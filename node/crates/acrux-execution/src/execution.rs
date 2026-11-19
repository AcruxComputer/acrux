//! SVM execution and the account-state commitment used by every validator.
use crate::selection::{self, Candidate, Selection};
use anyhow::{anyhow, bail, Context, Result};
use litesvm::LiteSVM;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_sdk::{
    account::{Account, ReadableAccount},
    clock::Clock,
    hash::Hash,
    pubkey::Pubkey,
    system_program,
    transaction::VersionedTransaction,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};

pub const RESERVE_LAMPORTS: u64 = 1_000_000;
pub const INCLUSION_LAMPORTS: u64 = 1_000;
pub const PER_PROPOSER_WINDOW_LIMIT: u64 = RESERVE_LAMPORTS / 6;
pub const BASE_FEE_PER_SIGNATURE: u64 = 5_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fees {
    pub reserve_lamports: u64,
    pub inclusion_lamports: u64,
    pub base_fee_per_signature: u64,
    pub base_burn_bps: u16,
}
impl Default for Fees {
    fn default() -> Self {
        Self {
            reserve_lamports: RESERVE_LAMPORTS,
            inclusion_lamports: INCLUSION_LAMPORTS,
            base_fee_per_signature: BASE_FEE_PER_SIGNATURE,
            base_burn_bps: 5000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TxHeader {
    pub id: String,
    pub fee_payer: String,
    pub priority_fee_per_cu: String,
    pub compute_limit: u64,
    pub writable_accounts: Vec<String>,
    pub target_slot: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatedTx {
    pub bytes: Vec<u8>,
    pub header: TxHeader,
    pub signature_count: u64,
}

#[derive(Clone, Debug)]
pub struct ExecutionItem {
    pub transaction: ValidatedTx,
    pub source_proposers: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionResult {
    pub id: String,
    pub success: bool,
    pub logs: Vec<String>,
    pub compute_units: u64,
    pub error: Option<String>,
    pub base_fee: u64,
    pub priority_fee: u64,
    /// The message's accounts, in its key order, before this transaction's base and priority
    /// fees and after it: Solana's preBalances and postBalances, and the token accounts among them.
    #[serde(skip)]
    pub before: AccountView,
    #[serde(skip)]
    pub after: AccountView,
}

/// Lamports of each account key, and each key's token account (SPL Token or Token-2022) by index.
#[derive(Clone, Debug, Default)]
pub struct AccountView {
    pub lamports: Vec<u64>,
    pub tokens: Vec<(u8, Account)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotExecution {
    pub merged_order: Vec<TxHeader>,
    pub schedule: Selection,
    pub results: Vec<ExecutionResult>,
    pub state_hash: String,
}

pub struct GenesisProgram {
    pub address: String,
    pub path: PathBuf,
}

/// A deposit on the Solana bridge, credited on Acrux: the bridge's sequence number, the Solana mint
/// and its decimals, the Acrux recipient and the amount the vault received. Carried in a proposer's
/// block once the deposit is finalized on Solana; every validator checks it against the deposit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeCredit {
    pub sequence: u64,
    pub solana_mint: String,
    pub decimals: u8,
    pub recipient: String,
    pub amount: u64,
}

/// Why a finalized deposit was consumed without minting on Acrux. The ordinal is persisted by
/// bincode in SkippedBridgeCredit, so append variants instead of reordering them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BridgeCreditSkipReason {
    InvalidSolanaMint,
    InvalidRecipient,
    MintOwner,
    MintLayout,
    MintLamportsOverflow,
    SupplyOverflow,
    RecipientOwner,
    RecipientLayout,
    RecipientLamportsOverflow,
    BalanceOverflow,
    SequenceOverflow,
}

/// A skipped deposit remains in committed account state for inspection. It cannot be reapplied:
/// the same sequence counter advances for successful and skipped deposits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedBridgeCredit {
    pub credit: BridgeCredit,
    pub reason: BridgeCreditSkipReason,
}

/// The bridge-owned account holding bincode SkippedBridgeCredit, without an Anchor discriminator.
pub fn bridge_skipped_credit(bridge: &Pubkey, sequence: u64) -> Pubkey {
    Pubkey::find_program_address(&[b"skipped_credit", &sequence.to_le_bytes()], bridge).0
}

/// Where the bridge's tokens live on Acrux, all derived from the Solana bridge program's address:
/// a mint per Solana mint at `["mint", solana mint]`, minted by `["authority"]`, and the next
/// sequence to credit in the account at `["credited"]`, so it is part of the state.
pub fn bridge_mint(bridge: &Pubkey, solana_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"mint", solana_mint.as_ref()], bridge).0
}
fn bridge_authority(bridge: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"authority"], bridge).0
}
fn bridge_counter(bridge: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"credited"], bridge).0
}

/// A program Solana runs at a fixed address, placed at the same address on Acrux from a named slot,
/// the way Solana adds programs at fixed addresses: its accounts (program data first, the program
/// last) are written into the state before the first slot executed at or after `slot`.
#[derive(Clone, Debug)]
pub struct Placement {
    pub name: String,
    pub slot: u64,
    pub accounts: Vec<(Pubkey, Account)>,
}

/// A Solana mint's withdrawal floor, installed from the chain's bridge rule.
#[derive(Clone, Debug)]
pub struct MinimumWithdrawal {
    pub mint: Pubkey,
    pub amount: u64,
}

pub fn bridge_minimum(bridge: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"minimum", mint.as_ref()], bridge).0
}

/// A named-slot rule converting the bridge's ACRUX token into the chain's native lamports.
#[derive(Clone, Debug)]
pub struct NativeCutover {
    pub slot: u64,
    pub solana_mint: Pubkey,
    pub lamports_per_base_unit: u64,
}

#[derive(Clone, Debug)]
pub struct NativeSupply {
    pub cutover_slot: u64,
    pub solana_mint: Pubkey,
    pub lamports_per_base_unit: u64,
    pub reserve_lamports: u64,
    pub credited_lamports: u64,
    pub withdrawn_lamports: u64,
    pub circulating_lamports: u64,
}

pub fn bridge_native(bridge: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"native"], bridge).0
}

pub fn bridge_native_burn(bridge: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"native_burn"], bridge).0
}

fn native_wallet(account: &Account) -> bool {
    account.owner == system_program::id() && account.data.is_empty() && !account.executable
}

pub trait Executor {
    fn validate_transaction(&self, bytes: &[u8], target_slot: u64) -> Result<ValidatedTx>;
    fn execute_slot(&mut self, slot: u64, items: Vec<ExecutionItem>) -> Result<SlotExecution>;
    fn state_hash(&self) -> String;
}

#[derive(Clone)]
pub struct ExecutorCheckpoint {
    accounts: BTreeMap<Pubkey, Arc<Account>>,
    account_hashes: BTreeMap<Pubkey, [u8; 32]>,
    accumulator: [u8; 32],
    total_lamports: u128,
    executed: BTreeMap<String, u64>,
    recent: VecDeque<(Hash, u64)>,
    inclusion_used: BTreeMap<(u64, Pubkey, u8), u64>,
    fees: Fees,
}

impl ExecutorCheckpoint {
    /// The state's lattice hash in base58: for the genesis checkpoint, the chain's genesis hash.
    pub fn hash(&self) -> String {
        bs58::encode(self.accumulator).into_string()
    }

    /// The accounts this state holds that `earlier` does not hold as they are, and the addresses
    /// it no longer holds: what a store of `earlier` writes to become this state.
    pub fn account_changes(&self, earlier: Option<&ExecutorCheckpoint>) -> (Vec<(Pubkey, Arc<Account>)>, Vec<Pubkey>) {
        let empty = BTreeMap::new();
        let before = earlier.map_or(&empty, |e| &e.accounts);
        let changed = self.accounts.iter().filter(|(k, a)| before.get(k).map_or(true, |b| !Arc::ptr_eq(a, b) && b != *a)).map(|(k, a)| (*k, a.clone())).collect();
        let removed = before.keys().filter(|k| !self.accounts.contains_key(k)).copied().collect();
        (changed, removed)
    }

    /// The same for the executed transaction ids.
    pub fn executed_changes(&self, earlier: Option<&ExecutorCheckpoint>) -> (Vec<(String, u64)>, Vec<String>) {
        let empty = BTreeMap::new();
        let before = earlier.map_or(&empty, |e| &e.executed);
        let added = self.executed.iter().filter(|(k, v)| before.get(*k) != Some(*v)).map(|(k, v)| (k.clone(), *v)).collect();
        let removed = before.keys().filter(|k| !self.executed.contains_key(*k)).cloned().collect();
        (added, removed)
    }
}

/// An executor's state at a cblock, as a follower loads it: every non-executable account, the
/// replay window and the fee accounting. Program accounts come from the follower's own genesis.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorSnapshot {
    pub accounts: Vec<(String, Account)>,
    pub executed: Vec<(String, u64)>,
    pub recent: Vec<(String, u64)>,
    pub inclusion_used: Vec<(u64, String, u8, u64)>,
    pub fees: Fees,
    pub track_capacity_cu: u64,
    pub state_hash: String,
}

/// A transaction may reference any blockhash of the last minute of slots, as on Solana.
pub const BLOCKHASH_AGE_SLOTS: u64 = 1_200;

pub struct SvmExecutor {
    svm: LiteSVM,
    accounts: BTreeMap<Pubkey, Arc<Account>>,
    account_hashes: BTreeMap<Pubkey, [u8; 32]>,
    accumulator: [u8; 32],
    /// Derived from the committed accounts; it is not part of the state hash or disk format.
    total_lamports: u128,
    proposers: BTreeMap<u8, Pubkey>,
    inclusion_used: BTreeMap<(u64, Pubkey, u8), u64>,
    /// Executed transaction ids, each with the slot of the blockhash it referenced; an id is kept
    /// while that blockhash is still valid, which is as long as a replay could land.
    executed: BTreeMap<String, u64>,
    /// The valid blockhashes, newest last: one per executed slot, derived from the state.
    recent: VecDeque<(Hash, u64)>,
    /// The builtin and genesis programs whose accounts enter the state hash at genesis.
    allowed_programs: BTreeSet<Pubkey>,
    fees: Fees,
    track_capacity_cu: u64,
    /// Each placement's code-account indexes. Mutable pool/config accounts do not trigger re-placement.
    placements: Vec<(Placement, Vec<usize>)>,
    /// The Solana bridge program's address, when the chain credits its deposits.
    bridge: Option<Pubkey>,
    /// The credits the next executed slot applies, before its transactions.
    credits: Vec<BridgeCredit>,
    /// Rule-owned minimum accounts enter committed state at the bridge activation slot.
    bridge_minimums: Option<(u64, Vec<MinimumWithdrawal>)>,
    native_cutover: Option<NativeCutover>,
}

impl SvmExecutor {
    pub fn checkpoint(&self) -> ExecutorCheckpoint {
        ExecutorCheckpoint {
            accounts: self.accounts.clone(),
            account_hashes: self.account_hashes.clone(),
            accumulator: self.accumulator,
            total_lamports: self.total_lamports,
            executed: self.executed.clone(),
            recent: self.recent.clone(),
            inclusion_used: self.inclusion_used.clone(),
            fees: self.fees.clone(),
        }
    }

    /// The state a follower starts from. Executables come from the follower's own build (builtins,
    /// the SPL programs, the genesis programs), except the program accounts of programs deployed on
    /// the chain through the upgradeable loader, which are carried as any account is.
    pub fn snapshot(&self, checkpoint: &ExecutorCheckpoint) -> ExecutorSnapshot {
        ExecutorSnapshot {
            accounts: checkpoint
                .accounts
                .iter()
                .filter(|(_, a)| !a.executable || a.owner == solana_sdk::bpf_loader_upgradeable::id())
                .map(|(k, a)| (k.to_string(), a.as_ref().clone()))
                .collect(),
            executed: checkpoint.executed.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            recent: checkpoint.recent.iter().map(|(h, s)| (h.to_string(), *s)).collect(),
            inclusion_used: checkpoint.inclusion_used.iter().map(|((w, p, q), v)| (*w, p.to_string(), *q, *v)).collect(),
            fees: checkpoint.fees.clone(),
            track_capacity_cu: self.track_capacity_cu,
            state_hash: hex::encode(checkpoint.accumulator),
        }
    }

    /// A checkpoint's state without its accounts and executed ids, which a store keeps by key.
    pub fn snapshot_meta(&self, checkpoint: &ExecutorCheckpoint) -> ExecutorSnapshot {
        ExecutorSnapshot {
            accounts: vec![],
            executed: vec![],
            recent: checkpoint.recent.iter().map(|(h, s)| (h.to_string(), *s)).collect(),
            inclusion_used: checkpoint.inclusion_used.iter().map(|((w, p, q), v)| (*w, p.to_string(), *q, *v)).collect(),
            fees: checkpoint.fees.clone(),
            track_capacity_cu: self.track_capacity_cu,
            state_hash: hex::encode(checkpoint.accumulator),
        }
    }

    /// An executor holding exactly a saved state, every account in it executables included: the
    /// state a restart continues from. Its state hash is the saved one when every account came
    /// across intact and was saved under this build's rules; the caller checks.
    pub fn from_state(programs: &[GenesisProgram], state: ExecutorSnapshot) -> Result<Self> {
        let mut this = Self::new(Vec::<(String, u64)>::new(), programs)?;
        let mut accounts = state.accounts.into_iter().map(|(k, a)| Ok((Pubkey::from_str(&k)?, a))).collect::<Result<Vec<_>>>()?;
        let keep = accounts.iter().map(|(k, _)| *k).collect::<BTreeSet<_>>();
        for key in this.accounts.keys().filter(|k| !keep.contains(k)).copied().collect::<Vec<_>>() {
            this.put(key, Account::default())?;
        }
        // A program loads from its program data account, so every executable comes after the rest.
        accounts.sort_by_key(|(k, a)| (a.executable, *k));
        for (key, account) in accounts {
            this.put(key, account)?;
        }
        this.executed = state.executed.into_iter().collect();
        this.recent = state.recent.into_iter().map(|(h, s)| Ok((Hash::from_str(&h)?, s))).collect::<Result<_>>()?;
        this.inclusion_used = state.inclusion_used.into_iter().map(|(w, p, q, v)| Ok(((w, Pubkey::from_str(&p)?, q), v))).collect::<Result<_>>()?;
        this.fees = state.fees;
        this.track_capacity_cu = state.track_capacity_cu;
        Ok(this)
    }

    /// Replaces this executor's state with a snapshot's; the result's state hash is the snapshot's
    /// when every account came across intact.
    pub fn load(&mut self, snapshot: ExecutorSnapshot) -> Result<()> {
        let keep = snapshot.accounts.iter().map(|(k, _)| Pubkey::from_str(k)).collect::<Result<BTreeSet<_>, _>>()?;
        for key in self.accounts.iter().filter(|(k, a)| !a.executable && !keep.contains(k)).map(|(k, _)| *k).collect::<Vec<_>>() {
            self.put(key, Account { lamports: 0, data: vec![], owner: system_program::id(), executable: false, rent_epoch: 0 })?;
        }
        // A deployed program's account is loaded after its program data account, which it names.
        let (programs, accounts): (Vec<_>, Vec<_>) = snapshot.accounts.into_iter().partition(|(_, a)| a.executable);
        for (key, account) in accounts.into_iter().chain(programs) {
            self.put(Pubkey::from_str(&key)?, account)?;
        }
        self.executed = snapshot.executed.into_iter().collect();
        self.recent = snapshot.recent.into_iter().map(|(h, s)| Ok((Hash::from_str(&h)?, s))).collect::<Result<_>>()?;
        self.inclusion_used = snapshot.inclusion_used.into_iter().map(|(w, p, q, v)| Ok(((w, Pubkey::from_str(&p)?, q), v))).collect::<Result<_>>()?;
        self.fees = snapshot.fees;
        self.track_capacity_cu = snapshot.track_capacity_cu;
        Ok(())
    }

    pub fn restore(&mut self, checkpoint: ExecutorCheckpoint) -> Result<()> {
        for key in self
            .account_hashes
            .keys()
            .filter(|key| !checkpoint.accounts.contains_key(key))
            .copied()
            .collect::<Vec<_>>()
        {
            self.svm.set_account(
                key,
                Account {
                    lamports: 0,
                    data: vec![],
                    owner: system_program::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )?;
        }
        for (key, account) in &checkpoint.accounts {
            if !self
                .accounts
                .get(key)
                .map_or(false, |current| Arc::ptr_eq(current, account))
            {
                self.svm.set_account(*key, account.as_ref().clone())?;
            }
        }
        self.accounts = checkpoint.accounts;
        self.account_hashes = checkpoint.account_hashes;
        self.accumulator = checkpoint.accumulator;
        self.total_lamports = checkpoint.total_lamports;
        self.executed = checkpoint.executed;
        self.recent = checkpoint.recent;
        self.inclusion_used = checkpoint.inclusion_used;
        self.fees = checkpoint.fees;
        Ok(())
    }

    fn remember_executed(&mut self, id: String, bytes: &[u8]) {
        let slot = recent_blockhash(bytes).and_then(|hash| self.blockhash_slot(&hash)).unwrap_or(0);
        self.executed.insert(id, slot);
    }

    fn blockhash_slot(&self, hash: &Hash) -> Option<u64> {
        self.recent.iter().rev().find(|(h, _)| h == hash).map(|(_, slot)| *slot)
    }

    /// After each slot: a new blockhash from the previous one and the state, and the oldest out.
    fn advance_blockhash(&mut self, slot: u64) {
        let mut digest = Sha256::new();
        digest.update(self.recent.back().map_or([0; 32], |(h, _)| h.to_bytes()));
        digest.update(self.accumulator);
        digest.update(slot.to_le_bytes());
        self.recent.push_back((Hash::new_from_array(digest.finalize().into()), slot));
        while self.recent.front().is_some_and(|(_, s)| s + BLOCKHASH_AGE_SLOTS < slot) {
            self.recent.pop_front();
        }
        let oldest = self.recent.front().map_or(0, |(_, s)| *s);
        self.executed.retain(|_, referenced| *referenced >= oldest);
    }

    pub fn new(
        balances: impl IntoIterator<Item = (String, u64)>,
        programs: &[GenesisProgram],
    ) -> Result<Self> {
        // Transaction history is maintained by transaction ID below. Blockhash checking
        // remains enabled; sigverify is also enabled when simulating the signed bytes.
        // Every feature on but loader v4 and the end of new loader v3 deployments, neither of
        // them on Solana mainnet: `solana program deploy` deploys through loader v3.
        let mut features = solana_sdk::feature_set::FeatureSet::all_enabled();
        features.deactivate(&solana_sdk::feature_set::enable_loader_v4::id());
        features.deactivate(&solana_sdk::feature_set::disable_new_loader_v3_deployments::id());
        let svm = LiteSVM::default()
            .with_feature_set(features)
            .with_builtins()
            .with_sysvars()
            .with_precompiles()
            .with_spl_programs()
            .with_sigverify(true)
            // Acrux checks the blockhash itself, against its own recent blockhashes.
            .with_blockhash_check(false)
            .with_transaction_history(0);
        let mut this = Self {
            svm,
            accounts: BTreeMap::new(),
            account_hashes: BTreeMap::new(),
            accumulator: [0; 32],
            total_lamports: 0,
            proposers: BTreeMap::new(),
            inclusion_used: BTreeMap::new(),
            executed: BTreeMap::new(),
            recent: VecDeque::new(),
            allowed_programs: BTreeSet::from([system_program::id()]),
            fees: Fees::default(),
            track_capacity_cu: selection::TRACK_CAPACITY_CU,
            placements: vec![],
            bridge: None,
            credits: vec![],
            bridge_minimums: None,
            native_cutover: None,
        };
        for program in [
            "ComputeBudget111111111111111111111111111111",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
        ] {
            this.allowed_programs.insert(Pubkey::from_str(program)?);
        }
        for program in programs {
            let key = Pubkey::from_str(&program.address)?;
            this.svm
                .add_program_from_file(key, &program.path)
                .with_context(|| format!("Load genesis program {}", program.address))?;
            this.allowed_programs.insert(key);
            this.refresh(key);
        }
        for (address, lamports) in balances {
            this.fund(&address, lamports)?;
        }
        for key in this.allowed_programs.clone() {
            this.refresh(key);
        }
        let mut clock = this.svm.get_sysvar::<Clock>();
        clock.epoch_start_timestamp = 1_794_787_200;
        clock.unix_timestamp = 1_794_787_200;
        this.svm.set_sysvar(&clock);
        for address in [
            "SysvarC1ock11111111111111111111111111111111",
            "SysvarEpochSchedu1e111111111111111111111111",
            "SysvarFees111111111111111111111111111111111",
            "SysvarRecentB1ockHashes11111111111111111111",
            "SysvarRent111111111111111111111111111111111",
            "SysvarS1otHashes111111111111111111111111111",
            "SysvarS1otHistory11111111111111111111111111",
            "SysvarStakeHistory1111111111111111111111111",
            "SysvarLastRestartS1ot1111111111111111111111",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
            "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo",
            "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
        ] {
            this.refresh(Pubkey::from_str(address)?);
        }
        this.set_slot_clock(0)?;
        this.recent.push_back((this.svm.latest_blockhash(), 0));
        Ok(this)
    }

    pub fn set_proposers(&mut self, proposers: &[(u8, String)]) -> Result<()> {
        self.proposers = proposers
            .iter()
            .map(|(index, address)| Ok((*index, Pubkey::from_str(address)?)))
            .collect::<Result<_>>()?;
        Ok(())
    }
    pub fn set_fees(&mut self, fees: Fees) -> Result<()> {
        if fees.base_burn_bps > 10_000 {
            bail!("Base-fee burn must be at most 10000 bps");
        }
        self.fees = fees;
        Ok(())
    }
    pub fn set_track_capacity(&mut self, capacity_cu: u64) -> Result<()> {
        if capacity_cu == 0 {
            bail!("Track capacity must be positive");
        }
        self.track_capacity_cu = capacity_cu;
        Ok(())
    }
    /// The blockhash clients sign with: eight slots back, so every validator already holds it
    /// however far its execution trails this one's.
    /// What a message costs on Acrux sent to `copies` proposers: the base fee per signature, the
    /// priority fee its compute budget sets, and an inclusion fee per copy. None when its blockhash
    /// is no longer held, as Solana's `getFeeForMessage` answers.
    pub fn fee_for_message(&self, message: &solana_sdk::message::VersionedMessage, copies: u64) -> Result<Option<u64>> {
        if self.blockhash_slot(message.recent_blockhash()).is_none() {
            return Ok(None);
        }
        let (limit, price) = compute_budget(message)?;
        let base = u64::from(message.header().num_required_signatures) * self.fees.base_fee_per_signature;
        Ok(Some(base + priority_fee(price, limit)? + copies * self.fees.inclusion_lamports))
    }

    /// Runs a transaction against the current state without keeping anything, the way Solana's
    /// `simulateTransaction` does: with or without its signatures checked, and with its blockhash
    /// replaced by the latest one when asked. Returns the error, logs, compute units and the
    /// blockhash it ran with when replaced.
    pub fn simulate(&mut self, bytes: &[u8], sig_verify: bool, replace_blockhash: bool) -> Result<Simulation> {
        use bincode::Options;
        let mut tx: VersionedTransaction = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(1232)
            .reject_trailing_bytes()
            .deserialize(bytes)
            .context("Invalid Solana transaction bytes")?;
        let mut replaced = None;
        if replace_blockhash {
            let hash = Hash::from_str(&self.latest_blockhash())?;
            tx.message.set_recent_blockhash(hash);
            replaced = Some(hash.to_string());
        } else if self.blockhash_slot(tx.message.recent_blockhash()).is_none() {
            return Ok(Simulation { err: Some(solana_sdk::transaction::TransactionError::BlockhashNotFound), logs: vec![], units: 0, replaced });
        }
        let checked = !sig_verify || replace_blockhash;
        if checked {
            self.svm = std::mem::take(&mut self.svm).with_sigverify(false);
        }
        let outcome = self.svm.simulate_transaction(tx);
        if checked {
            self.svm = std::mem::take(&mut self.svm).with_sigverify(true);
        }
        Ok(match outcome {
            Ok(info) => Simulation { err: None, logs: info.meta.logs, units: info.meta.compute_units_consumed, replaced },
            Err(failed) => Simulation { err: Some(failed.err), logs: failed.meta.logs, units: failed.meta.compute_units_consumed, replaced },
        })
    }

    pub fn latest_blockhash(&self) -> String {
        let index = self.recent.len().saturating_sub(9);
        self.recent[index].0.to_string()
    }
    pub fn balance(&self, address: &str) -> Result<u64> {
        Ok(self
            .svm
            .get_balance(&Pubkey::from_str(address)?)
            .unwrap_or(0))
    }
    pub fn account(&self, address: &str) -> Result<Option<Account>> {
        Ok(self.svm.get_account(&Pubkey::from_str(address)?))
    }
    pub fn fund(&mut self, address: &str, lamports: u64) -> Result<()> {
        let key = Pubkey::from_str(address)?;
        let mut account = self.svm.get_account(&key).unwrap_or_else(|| Account {
            lamports: 0,
            data: vec![],
            owner: system_program::id(),
            executable: false,
            rent_epoch: 0,
        });
        account.lamports = account
            .lamports
            .checked_add(lamports)
            .context("Balance overflow")?;
        self.put(key, account)
    }
    pub fn set_account(&mut self, address: &str, account: Account) -> Result<()> {
        self.put(Pubkey::from_str(address)?, account)
    }

    pub fn charge_inclusion(
        &mut self,
        payer: &str,
        proposer: u8,
        window: u64,
        copies: u64,
    ) -> Result<()> {
        let payer = Pubkey::from_str(payer)?;
        let destination = *self
            .proposers
            .get(&proposer)
            .context("Missing proposer fee recipient")?;
        let amount = copies
            .checked_mul(self.fees.inclusion_lamports)
            .context("Inclusion fee overflow")?;
        let used = self
            .inclusion_used
            .get(&(window, payer, proposer))
            .copied()
            .unwrap_or(0);
        if used.checked_add(amount).context("Inclusion fee overflow")?
            > self.fees.reserve_lamports / 6
        {
            bail!("Fee-payer inclusion rate exceeds reserve/(2k)");
        }
        self.debit(payer, amount, false)?;
        self.credit(destination, amount)?;
        self.inclusion_used
            .insert((window, payer, proposer), used + amount);
        self.inclusion_used.retain(|(w, _, _), _| *w + 2 >= window);
        Ok(())
    }

    /// Which of a slot's included copies are charged, in block order: a copy whose fee payer can't
    /// pay its inclusion fee, or is past its per-window rate, is dropped, never a reason to reject
    /// the block. Every validator sees the same state and order, so every one drops the same copies.
    pub fn admit_inclusions(&self, items: &[(String, u8, u64)], window: u64) -> Result<Vec<bool>> {
        let mut rates = BTreeMap::<(Pubkey, u8), u64>::new();
        let mut balances = BTreeMap::<Pubkey, u64>::new();
        let mut admitted = Vec::with_capacity(items.len());
        for (payer, proposer, copies) in items {
            let payer = Pubkey::from_str(payer)?;
            if !self.proposers.contains_key(proposer) {
                bail!("Missing proposer fee recipient");
            }
            let amount = copies.saturating_mul(self.fees.inclusion_lamports);
            let rate = rates.entry((payer, *proposer)).or_insert(
                self.inclusion_used.get(&(window, payer, *proposer)).copied().unwrap_or(0),
            );
            let balance = balances
                .entry(payer)
                .or_insert_with(|| self.svm.get_balance(&payer).unwrap_or(0));
            let ok = rate.saturating_add(amount) <= self.fees.reserve_lamports / 6 && *balance >= amount;
            if ok {
                *rate += amount;
                *balance -= amount;
            }
            admitted.push(ok);
        }
        Ok(admitted)
    }

    /// Whether a payer could pay `copies` inclusion fees to `proposer` in `window` now; the
    /// admission and packing check, the same rule `admit_inclusions` applies.
    pub fn can_pay_inclusion(&self, payer: &str, proposer: u8, window: u64, copies: u64) -> bool {
        self.admit_inclusions(&[(payer.to_owned(), proposer, copies)], window)
            .map_or(false, |admitted| admitted[0])
    }

    pub fn blockhash_known(&self, hash: &str) -> bool {
        Hash::from_str(hash).is_ok_and(|hash| self.blockhash_slot(&hash).is_some())
    }

    pub fn blockhash_valid(&self, bytes: &[u8]) -> bool {
        recent_blockhash(bytes).is_some_and(|hash| self.blockhash_slot(&hash).is_some())
    }

    pub fn is_executed(&self, id: &str) -> bool {
        self.executed.contains_key(id)
    }

    /// Admission: the structural check plus a blockhash this chain still holds.
    pub fn validate_transaction(&self, bytes: &[u8], target_slot: u64) -> Result<ValidatedTx> {
        let valid = self.check_transaction(bytes, target_slot)?;
        if !self.blockhash_valid(bytes) {
            bail!("Blockhash not found: fetch a recent one with getLatestBlockhash");
        }
        Ok(valid)
    }

    /// What a block carries must pass: size, signatures, programs and compute budget. Whether the
    /// blockhash is still valid depends on when the slot executes, so execution decides it.
    pub fn check_transaction(&self, bytes: &[u8], target_slot: u64) -> Result<ValidatedTx> {
        if bytes.len() > 1232 {
            bail!("Transaction exceeds Solana packet size");
        }
        use bincode::Options;
        let tx: VersionedTransaction = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(1232)
            .reject_trailing_bytes()
            .deserialize(bytes)
            .context("Invalid Solana transaction bytes")?;
        tx.sanitize().context("Malformed Solana transaction")?;
        tx.verify_and_hash_message()
            .context("Invalid transaction signature")?;
        if !tx
            .message
            .address_table_lookups()
            .map_or(true, |lookups| lookups.is_empty())
        {
            bail!("Address lookup tables are not configured in this genesis");
        }
        let keys = tx.message.static_account_keys();
        let payer = *keys.first().context("Transaction has no fee payer")?;
        let mut compute_limit = None;
        let mut price = None;
        let mut normal_instructions = 0u64;
        let compute_program = Pubkey::from_str("ComputeBudget111111111111111111111111111111")?;
        for ix in tx.message.instructions() {
            let program = keys
                .get(ix.program_id_index as usize)
                .context("Bad program index")?;
            if *program == compute_program {
                match ix.data.as_slice() {
                    [2, rest @ ..] if rest.len() == 4 => {
                        if compute_limit.is_some() {
                            bail!("Duplicate compute limit");
                        }
                        compute_limit = Some(u32::from_le_bytes(rest.try_into().unwrap()) as u64);
                    }
                    [3, rest @ ..] if rest.len() == 8 => {
                        if price.is_some() {
                            bail!("Duplicate compute price");
                        }
                        price = Some(u64::from_le_bytes(rest.try_into().unwrap()));
                    }
                    // RequestHeapFrame and SetLoadedAccountsDataSizeLimit: the SVM applies them.
                    [1, rest @ ..] | [4, rest @ ..] if rest.len() == 4 => {}
                    _ => bail!("Unsupported compute budget instruction"),
                }
            } else {
                normal_instructions += 1;
            }
        }
        let compute_limit = compute_limit.unwrap_or((normal_instructions * 200_000).min(1_400_000));
        if compute_limit == 0 || compute_limit > 1_400_000 {
            bail!("Invalid compute limit");
        }
        let writable_accounts = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| tx.message.is_maybe_writable(*i, None))
            .map(|(_, key)| key.to_string())
            .collect();
        let id = hex::encode(Sha256::digest(tx.message.serialize()));
        Ok(ValidatedTx {
            bytes: bytes.to_vec(),
            signature_count: tx.signatures.len() as u64,
            header: TxHeader {
                id,
                fee_payer: payer.to_string(),
                priority_fee_per_cu: price.unwrap_or(0).to_string(),
                compute_limit,
                writable_accounts,
                target_slot,
            },
        })
    }

    pub fn set_placements(&mut self, placements: Vec<Placement>) {
        self.placements = placements
            .into_iter()
            .map(|p| {
                let program_index = p.accounts.len().checked_sub(1).expect("a placement holds its program account");
                let program = &p.accounts[program_index].1;
                let mut code = vec![program_index];
                if program.owner == solana_sdk::bpf_loader_upgradeable::id() {
                    let state:solana_sdk::bpf_loader_upgradeable::UpgradeableLoaderState = bincode::deserialize(&program.data).expect("an upgradeable placement has a Program account");
                    let solana_sdk::bpf_loader_upgradeable::UpgradeableLoaderState::Program { programdata_address } = state else {
                        panic!("an executable upgradeable placement must reference ProgramData");
                    };
                    code.push(p.accounts.iter().position(|(key,_)| *key == programdata_address).expect("a placement includes its referenced ProgramData"));
                }
                (p, code)
            })
            .collect();
    }

    /// Writes every due placement whose program or program-data account differs from the rule.
    /// The same on every validator and follower, and again after a rollback past it.
    fn apply_placements(&mut self, slot: u64) -> Result<()> {
        for i in 0..self.placements.len() {
            let (placement, code) = &self.placements[i];
            let program = placement.accounts.last().expect("a placement holds its program account").0;
            // An append-only manifest may place a later version at the same address. Once it is
            // due, do not reload the older bytecode before restoring the newer one on every slot.
            let newest = self.placements.iter().enumerate().filter(|(_, (p,_))| p.slot <= slot && p.accounts.last().is_some_and(|(key,_)| *key == program))
                .max_by_key(|(index,(p,_))| (p.slot,*index)).map(|(index,_)| index);
            if newest != Some(i) { continue }
            let placed = code.iter().all(|index| {
                let (key,expected) = &placement.accounts[*index];
                self.accounts.get(key).is_some_and(|current| current.owner == expected.owner && current.executable == expected.executable && current.data == expected.data)
            });
            if placed {
                continue;
            }
            for (key, mut account) in placement.accounts.clone() {
                if self.native_cutover.as_ref().is_some_and(|rule| slot >= rule.slot) {
                    account.lamports = account.lamports.max(self.svm.get_account(&key).map_or(0,|current| current.lamports));
                }
                self.put(key, account)?;
            }
        }
        Ok(())
    }

    pub fn set_bridge(&mut self, bridge: Pubkey) {
        self.bridge = Some(bridge);
    }

    pub fn set_bridge_minimums(&mut self, from_slot: u64, minimums: Vec<MinimumWithdrawal>) {
        self.bridge_minimums = Some((from_slot, minimums));
    }

    pub fn set_native_cutover(&mut self, rule: NativeCutover) {
        self.native_cutover = Some(rule);
    }

    /// Canonical execution and followers call this after restoring the parent, before inclusion
    /// fees. Only the first native activation moves these steps earlier; historical ordering and
    /// placements arriving after the cutover keep their normal execute_slot ordering.
    pub fn activate_native_cutover(&mut self, slot: u64) -> Result<()> {
        if self.native_cutover.as_ref().is_none_or(|rule| slot < rule.slot) { return Ok(()) }
        if self.native_gate()?.is_some() { return Ok(()) }
        self.apply_placements(slot)?;
        self.apply_bridge_minimums(slot)?;
        self.apply_native_cutover(slot)
    }

    /// The native marker is ordinary hashed bridge-owned state, so restore/load alone determines
    /// whether conversion has already happened. The program can change only its withdrawn field.
    fn native_gate(&self) -> Result<Option<Account>> {
        let Some(bridge) = self.bridge else { return Ok(None) };
        let Some(account) = self.svm.get_account(&bridge_native(&bridge)) else { return Ok(None) };
        if native_wallet(&account) { return Ok(None) }
        anyhow::ensure!(account.owner == bridge && !account.executable && account.data.len() == 80
            && account.data[..8] == Sha256::digest(b"account:NativeGate")[..8], "Invalid native bridge gate");
        let scale = u64::from_le_bytes(account.data[40..48].try_into().unwrap());
        let credited = u64::from_le_bytes(account.data[48..56].try_into().unwrap());
        let withdrawn = u64::from_le_bytes(account.data[56..64].try_into().unwrap());
        anyhow::ensure!(scale > 0 && withdrawn <= credited, "Invalid native bridge accounting");
        if let Some(rule) = &self.native_cutover {
            anyhow::ensure!(account.data[8..40] == rule.solana_mint.to_bytes()
                && scale == rule.lamports_per_base_unit && account.data[72..80] == rule.slot.to_le_bytes(), "Native bridge rule differs from committed cutover");
        }
        Ok(Some(account))
    }

    pub fn native_supply(&self) -> Result<Option<NativeSupply>> {
        let Some(account) = self.native_gate()? else { return Ok(None) };
        let value = |offset| u64::from_le_bytes(account.data[offset..offset + 8].try_into().unwrap());
        let reserve_lamports = value(64);
        Ok(Some(NativeSupply {
            cutover_slot: value(72),
            solana_mint: Pubkey::new_from_array(account.data[8..40].try_into().unwrap()),
            lamports_per_base_unit: value(40),
            reserve_lamports,
            credited_lamports: value(48),
            withdrawn_lamports: value(56),
            circulating_lamports: self.total_lamports()?.checked_sub(reserve_lamports).context("Native supply fell below the fixed cutover reserve")?,
        }))
    }

    fn apply_native_cutover(&mut self, slot: u64) -> Result<()> {
        let Some(rule) = self.native_cutover.clone().filter(|rule| slot >= rule.slot) else { return Ok(()) };
        let bridge = self.bridge.context("Native cutover requires the bridge program")?;
        anyhow::ensure!(rule.lamports_per_base_unit > 0, "Native cutover scale must be positive");
        if self.native_gate()?.is_some() { return Ok(()) }
        let gate_key = bridge_native(&bridge);
        let burn_key = bridge_native_burn(&bridge);
        for key in [gate_key,burn_key] {
            anyhow::ensure!(self.svm.get_account(&key).as_ref().is_none_or(native_wallet), "Native cutover PDA has an incompatible account");
        }
        let mint_key = bridge_mint(&bridge, &rule.solana_mint);
        let mut changes = BTreeMap::new();
        let mut gains = BTreeMap::<Pubkey,u64>::new();
        let mut credited = 0u64;
        let mut reserve = 0u64;
        for (key, current) in &self.accounts {
            let mut account = current.as_ref().clone();
            if native_wallet(&account) {
                account.lamports = 0;
            } else {
                reserve = reserve.checked_add(account.lamports).context("Native cutover reserve overflow")?;
            }
            if account.owner == spl_token_id() && account.data.len() == 165 && account.data[..32] == mint_key.to_bytes() {
                anyhow::ensure!(matches!(account.data[108],1|2), "Native cutover token account is not initialized");
                let owner = Pubkey::new_from_array(account.data[32..64].try_into().unwrap());
                let amount = u64::from_le_bytes(account.data[64..72].try_into().unwrap());
                let lamports = amount.checked_mul(rule.lamports_per_base_unit).context("Native cutover token scaling overflow")?;
                credited = credited.checked_add(lamports).context("Native cutover credited overflow")?;
                let gain = gains.entry(owner).or_default();
                *gain = gain.checked_add(lamports).context("Native cutover holder overflow")?;
                account.data[64..72].fill(0);
            }
            if *key == mint_key {
                anyhow::ensure!(account.owner == spl_token_id() && !account.executable && account.data.len() == 82 && account.data[45] == 1, "Native cutover mint is invalid");
                account.data[36..44].fill(0);
            }
            changes.insert(*key,account);
        }
        // These two rule-owned accounts exist before reserve capture; their rent is not backed
        // native value and must not appear in the circulating amount.
        let gate_rent = self.rent_exempt_minimum(80);
        let burn_rent = self.rent_exempt_minimum(0);
        reserve = reserve.checked_add(gate_rent).and_then(|n| n.checked_add(burn_rent)).context("Native cutover reserve overflow")?;
        let mut data = Sha256::digest(b"account:NativeGate")[..8].to_vec();
        data.extend_from_slice(rule.solana_mint.as_ref());
        for value in [rule.lamports_per_base_unit,credited,0,reserve,rule.slot] { data.extend_from_slice(&value.to_le_bytes()); }
        changes.insert(gate_key,Account { lamports:gate_rent,data,owner:bridge,executable:false,rent_epoch:0 });
        changes.insert(burn_key,Account { lamports:burn_rent,data:vec![],owner:bridge,executable:false,rent_epoch:0 });
        for (owner,lamports) in gains {
            let account = changes.entry(owner).or_default();
            account.lamports = account.lamports.checked_add(lamports).context("Native cutover wallet overflow")?;
        }
        for (key,account) in changes { self.put(key,account)?; }
        Ok(())
    }

    fn clear_native_burn(&mut self) -> Result<()> {
        if self.native_gate()?.is_none() { return Ok(()) }
        let bridge = self.bridge.unwrap();
        let key = bridge_native_burn(&bridge);
        let mut account = self.svm.get_account(&key).context("Native burn account missing")?;
        let rent = self.rent_exempt_minimum(0);
        anyhow::ensure!(account.owner == bridge && !account.executable && account.data.is_empty() && account.lamports >= rent, "Invalid native burn account");
        if account.lamports != rent {
            account.lamports = rent;
            self.put(key,account)?;
        }
        Ok(())
    }

    /// Installs each configured floor before transactions, including after a rollback or a
    /// follower's load. The program has no instruction that can change these accounts.
    fn apply_bridge_minimums(&mut self, slot: u64) -> Result<()> {
        let Some(bridge) = self.bridge else { return Ok(()) };
        let Some((from_slot, minimums)) = &self.bridge_minimums else { return Ok(()) };
        if slot < *from_slot { return Ok(()) }
        for minimum in minimums.clone() {
            let key = bridge_minimum(&bridge, &minimum.mint);
            let mut data = Sha256::digest(b"account:MinimumWithdrawal")[..8].to_vec();
            data.extend_from_slice(&minimum.amount.to_le_bytes());
            let current = self.svm.get_account(&key);
            let lamports = current.as_ref().map_or(0, |a| a.lamports).max(self.rent_exempt_minimum(data.len()));
            let account = Account { lamports, data, owner: bridge, executable: false, rent_epoch: 0 };
            if current.as_ref() != Some(&account) {
                self.put(key, account)?;
            }
        }
        Ok(())
    }

    /// The withdrawals burned in cblock `cblock` (its four slots), in sequence, read from the bridge
    /// program's records on Acrux: `(sequence, Solana mint, Solana recipient, amount)`.
    pub fn withdrawals(&self, cblock: u64) -> Vec<(u64, [u8; 32], [u8; 32], u64)> {
        let Some(bridge) = self.bridge else { return vec![] };
        let u64_at = |d: &[u8], o: usize| d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
        // Outbound: discriminator, withdrawals made.
        let made = self.svm.get_account(&Pubkey::find_program_address(&[b"outbound"], &bridge).0).and_then(|a| u64_at(&a.data, 8)).unwrap_or(0);
        let mut found = vec![];
        // Sequences rise with slots: walk back from the newest until before this cblock.
        for sequence in (0..made).rev() {
            let record = Pubkey::find_program_address(&[b"withdrawal", &sequence.to_le_bytes()], &bridge).0;
            // Withdrawal: discriminator, sequence, owner, recipient, Solana mint, amount, slot, bump.
            let Some(d) = self.svm.get_account(&record).map(|a| a.data) else { break };
            let (Some(amount), Some(slot)) = (u64_at(&d, 112), u64_at(&d, 120)) else { break };
            if slot / 4 < cblock {
                break;
            }
            if slot / 4 == cblock {
                found.push((sequence, d[80..112].try_into().unwrap(), d[48..80].try_into().unwrap(), amount));
            }
        }
        found.reverse();
        found
    }

    /// The next bridge sequence to credit: what the counter account holds, 0 before the first.
    pub fn next_credit(&self) -> u64 {
        self.bridge
            .and_then(|b| self.svm.get_account(&bridge_counter(&b)))
            .and_then(|a| a.data.get(..8).map(|d| u64::from_le_bytes(d.try_into().unwrap())))
            .unwrap_or(0)
    }

    /// The credits the next executed slot applies.
    pub fn set_credits(&mut self, credits: Vec<BridgeCredit>) {
        self.credits = credits;
    }

    /// Prepares both accounts before changing state. Only a non-executable, empty system account
    /// may become a bridged mint or ATA; its lamports remain alongside the allocated rent.
    fn prepare_bridge_credit(&self, bridge: &Pubkey, credit: &BridgeCredit) -> std::result::Result<[(Pubkey, Account); 2], BridgeCreditSkipReason> {
        use BridgeCreditSkipReason::*;
        let token = spl_token_id();
        let solana_mint = Pubkey::from_str(&credit.solana_mint).map_err(|_| InvalidSolanaMint)?;
        let recipient = Pubkey::from_str(&credit.recipient).map_err(|_| InvalidRecipient)?;
        let mint = bridge_mint(bridge, &solana_mint);
        let authority = bridge_authority(bridge);
        let shell = |account: &Account| account.owner == system_program::id() && account.data.is_empty() && !account.executable;
        let mut mint_account = match self.svm.get_account(&mint) {
            Some(account) if account.owner == token => {
                let d = &account.data;
                if account.executable || d.len() != 82 || d[..4] != 1u32.to_le_bytes()
                    || d[4..36] != authority.to_bytes() || d[44] != credit.decimals || d[45] != 1
                    || d[46..50] != 0u32.to_le_bytes() {
                    return Err(MintLayout);
                }
                account
            }
            account if account.as_ref().is_none_or(&shell) => {
                let existing = account.map_or(0, |a| a.lamports);
                let lamports = self.rent_exempt_minimum(82).checked_add(existing).ok_or(MintLamportsOverflow)?;
                let mut data = vec![0u8; 82];
                data[..4].copy_from_slice(&1u32.to_le_bytes());
                data[4..36].copy_from_slice(authority.as_ref());
                data[44] = credit.decimals;
                data[45] = 1;
                Account { lamports, data, owner: token, executable: false, rent_epoch: 0 }
            }
            _ => return Err(MintOwner),
        };
        let supply = u64::from_le_bytes(mint_account.data[36..44].try_into().unwrap())
            .checked_add(credit.amount).ok_or(SupplyOverflow)?;
        let ata = Pubkey::find_program_address(
            &[recipient.as_ref(), token.as_ref(), mint.as_ref()],
            &Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap(),
        ).0;
        let mut holder = match self.svm.get_account(&ata) {
            Some(account) if account.owner == token => {
                let d = &account.data;
                if account.executable || d.len() != 165 || d[..32] != mint.to_bytes()
                    || d[32..64] != recipient.to_bytes() || d[108] != 1
                    || !matches!(d[72..76], [0, 0, 0, 0] | [1, 0, 0, 0])
                    || d[109..113] != 0u32.to_le_bytes()
                    || !matches!(d[129..133], [0, 0, 0, 0] | [1, 0, 0, 0]) {
                    return Err(RecipientLayout);
                }
                account
            }
            account if account.as_ref().is_none_or(&shell) => {
                let existing = account.map_or(0, |a| a.lamports);
                let lamports = self.rent_exempt_minimum(165).checked_add(existing).ok_or(RecipientLamportsOverflow)?;
                let mut data = vec![0u8; 165];
                data[..32].copy_from_slice(mint.as_ref());
                data[32..64].copy_from_slice(recipient.as_ref());
                data[108] = 1;
                Account { lamports, data, owner: token, executable: false, rent_epoch: 0 }
            }
            _ => return Err(RecipientOwner),
        };
        let amount = u64::from_le_bytes(holder.data[64..72].try_into().unwrap())
            .checked_add(credit.amount).ok_or(BalanceOverflow)?;
        mint_account.data[36..44].copy_from_slice(&supply.to_le_bytes());
        holder.data[64..72].copy_from_slice(&amount.to_le_bytes());
        Ok([(mint, mint_account), (ata, holder)])
    }

    fn prepare_native_credit(&self, bridge: &Pubkey, mut gate: Account, credit: &BridgeCredit) -> std::result::Result<[(Pubkey, Account); 2], BridgeCreditSkipReason> {
        use BridgeCreditSkipReason::*;
        let recipient = Pubkey::from_str(&credit.recipient).map_err(|_| InvalidRecipient)?;
        let mut wallet = self.svm.get_account(&recipient).unwrap_or_default();
        if wallet.owner != system_program::id() { return Err(RecipientOwner) }
        if wallet.executable || !wallet.data.is_empty() { return Err(RecipientLayout) }
        let scale = u64::from_le_bytes(gate.data[40..48].try_into().unwrap());
        let lamports = credit.amount.checked_mul(scale).ok_or(BalanceOverflow)?;
        let credited = u64::from_le_bytes(gate.data[48..56].try_into().unwrap()).checked_add(lamports).ok_or(SupplyOverflow)?;
        wallet.lamports = wallet.lamports.checked_add(lamports).ok_or(BalanceOverflow)?;
        gate.data[48..56].copy_from_slice(&credited.to_le_bytes());
        Ok([(recipient,wallet),(bridge_native(bridge),gate)])
    }

    /// Applies each deposit once, in sequence. Invalid destination state or arithmetic records a
    /// skipped deposit and advances the counter, so the same finalized deposit cannot halt slots.
    fn apply_credits(&mut self) -> Result<()> {
        let mut credits = std::mem::take(&mut self.credits);
        credits.sort_by_key(|c| c.sequence);
        let Some(bridge) = self.bridge else { return Ok(()) };
        for credit in credits {
            let next = self.next_credit();
            if credit.sequence != next {
                continue;
            }
            let native = self.native_gate()?;
            let prepared = if next == u64::MAX {
                Err(BridgeCreditSkipReason::SequenceOverflow)
            } else if let Some(gate) = native.filter(|gate| Pubkey::from_str(&credit.solana_mint).ok().is_some_and(|mint| gate.data[8..40] == mint.to_bytes())) {
                self.prepare_native_credit(&bridge,gate,&credit)
            } else {
                self.prepare_bridge_credit(&bridge, &credit)
            };
            match prepared {
                Ok(accounts) => {
                    // Both layouts and every checked addition passed; these are ordinary data
                    // accounts at PDAs, so LiteSVM cannot reject them as executables or sysvars.
                    for (key, account) in accounts {
                        self.put(key, account)?;
                    }
                }
                Err(reason) => {
                    let key = bridge_skipped_credit(&bridge, credit.sequence);
                    let data = bincode::serialize(&SkippedBridgeCredit { credit, reason })?;
                    let lamports = self.svm.get_account(&key).map_or(0, |a| a.lamports).max(self.rent_exempt_minimum(data.len()));
                    self.put(key, Account { lamports, data, owner: bridge, executable: false, rent_epoch: 0 })?;
                }
            }
            let key = bridge_counter(&bridge);
            let lamports = self.svm.get_account(&key).map_or(0, |a| a.lamports).max(self.rent_exempt_minimum(8));
            // u64::MAX cannot be a finalized deposit sequence (the on-chain counter refuses its
            // increment). Saturate malformed input rather than panic at this internal boundary.
            self.put(key, Account { lamports, data: next.saturating_add(1).to_le_bytes().to_vec(), owner: bridge, executable: false, rent_epoch: 0 })?;
        }
        Ok(())
    }

    pub fn execute_slot(&mut self, slot: u64, items: Vec<ExecutionItem>) -> Result<SlotExecution> {
        self.set_slot_clock(slot)?;
        self.apply_placements(slot)?;
        self.apply_bridge_minimums(slot)?;
        self.apply_native_cutover(slot)?;
        self.apply_credits()?;
        let mut merged = BTreeMap::<String, ExecutionItem>::new();
        for item in items {
            // Recheck signed bytes at the execution boundary; a public header cannot
            // redirect fees or writable-account locks away from the signed message.
            let checked = self.check_transaction(
                &item.transaction.bytes,
                item.transaction.header.target_slot,
            )?;
            if checked.header != item.transaction.header {
                bail!("Public transaction header does not match signed message");
            }
            if self.executed.contains_key(&checked.header.id) {
                continue;
            }
            let entry = merged
                .entry(checked.header.id.clone())
                .or_insert_with(|| ExecutionItem {
                    transaction: checked,
                    source_proposers: Vec::new(),
                });
            entry.source_proposers.extend(item.source_proposers);
        }
        for item in merged.values_mut() {
            item.source_proposers.sort_unstable();
            item.source_proposers.dedup();
            if item.source_proposers.is_empty()
                || item
                    .source_proposers
                    .iter()
                    .any(|index| !self.proposers.contains_key(index))
            {
                bail!("Transaction has an invalid source proposer set");
            }
        }
        let mut ordered: Vec<_> = merged
            .values()
            .map(|item| Candidate {
                id: item.transaction.header.id.clone(),
                priority_fee_per_cu: item
                    .transaction
                    .header
                    .priority_fee_per_cu
                    .parse()
                    .expect("Validated price"),
                compute_limit: item.transaction.header.compute_limit,
                writable_accounts: item.transaction.header.writable_accounts.clone(),
            })
            .collect();
        selection::sort_transactions(&mut ordered);
        let expired: BTreeSet<String> = merged
            .values()
            .filter(|item| !self.blockhash_valid(&item.transaction.bytes))
            .map(|item| item.transaction.header.id.clone())
            .collect();
        let mut charged_priority = BTreeMap::new();
        let schedule = selection::select_with_fees(&ordered, self.track_capacity_cu, |candidate| {
            if expired.contains(&candidate.id) {
                return false;
            }
            let item = &merged[&candidate.id];
            let amount = priority_fee(candidate.priority_fee_per_cu, candidate.compute_limit);
            let Ok(amount) = amount else {
                return false;
            };
            let payer =
                Pubkey::from_str(&item.transaction.header.fee_payer).expect("Validated payer");
            if self.debit(payer, amount, true).is_err() {
                return false;
            }
            if self.distribute(amount, &item.source_proposers).is_err() {
                return false;
            }
            charged_priority.insert(candidate.id.clone(), amount);
            true
        });
        let mut results: Vec<ExecutionResult> = expired
            .iter()
            .map(|id| ExecutionResult {
                id: id.clone(),
                success: false,
                logs: vec![],
                compute_units: 0,
                error: Some("Blockhash expired before the transaction executed".into()),
                base_fee: 0,
                priority_fee: 0,
                before: AccountView::default(),
                after: AccountView::default(),
            })
            .collect();
        for candidate in &ordered {
            if !schedule.selected_ids.contains(&candidate.id) {
                continue;
            }
            let item = &merged[&candidate.id];
            let priority_fee = charged_priority[&candidate.id];
            let base_fee = item
                .transaction
                .signature_count
                .checked_mul(self.fees.base_fee_per_signature)
                .context("Base fee overflow")?;
            let payer = Pubkey::from_str(&item.transaction.header.fee_payer)?;
            let tx: VersionedTransaction = bincode::deserialize(&item.transaction.bytes)?;
            let keys = tx.message.static_account_keys().to_vec();
            let mut before = self.account_view(&keys);
            // The priority fee left the payer when the slot's selection ran.
            before.lamports[0] += priority_fee;
            if let Err(error) = self.debit(payer, base_fee, true) {
                self.remember_executed(candidate.id.clone(), &item.transaction.bytes);
                results.push(ExecutionResult {
                    id: candidate.id.clone(),
                    success: false,
                    logs: vec![],
                    compute_units: 0,
                    error: Some(error.to_string()),
                    base_fee: 0,
                    priority_fee,
                    after: self.account_view(&keys),
                    before,
                });
                continue;
            }
            let burned =
                u64::try_from(u128::from(base_fee) * u128::from(self.fees.base_burn_bps) / 10_000)?;
            self.distribute(base_fee - burned, &item.source_proposers)?;
            let real_account = self
                .svm
                .get_account(&payer)
                .context("Fee payer disappeared")?;
            // LiteSVM charges Solana's per-signature fee and no prioritization fee. Reimburse
            // exactly that in the simulation input; NewCon fees above are then charged only once.
            let mut simulation_account = real_account.clone();
            let svm_base_fee = item
                .transaction
                .signature_count
                .checked_mul(BASE_FEE_PER_SIGNATURE)
                .context("SVM base fee overflow")?;
            simulation_account.lamports = simulation_account
                .lamports
                .checked_add(svm_base_fee)
                .context("Simulation balance overflow")?;
            self.svm.set_account(payer, simulation_account)?;
            let simulated = self.svm.simulate_transaction(tx);
            self.svm.set_account(payer, real_account)?;
            let outcome = match simulated {
                Ok(info) => {
                    let reserve_ok = info
                        .post_accounts
                        .iter()
                        .find(|(key, _)| *key == payer)
                        .map_or(true, |(_, account)| {
                            account.lamports() >= self.fees.reserve_lamports
                        });
                    if reserve_ok {
                        // Data before programs: LiteSVM loads an upgradeable program from its
                        // program data the moment the program account is written.
                        let mut accounts = info.post_accounts;
                        accounts.sort_by_key(|(_, account)| account.executable());
                        for (key, account) in accounts {
                            self.put(key, account.into())?;
                        }
                        ExecutionResult {
                            id: candidate.id.clone(),
                            success: true,
                            logs: info.meta.logs,
                            compute_units: info.meta.compute_units_consumed,
                            error: None,
                            base_fee,
                            priority_fee,
                            before: AccountView::default(),
                            after: AccountView::default(),
                        }
                    } else {
                        ExecutionResult {
                            id: candidate.id.clone(),
                            success: false,
                            logs: info.meta.logs,
                            compute_units: info.meta.compute_units_consumed,
                            error: Some("Execution would spend the fee-payer reserve".into()),
                            base_fee,
                            priority_fee,
                            before: AccountView::default(),
                            after: AccountView::default(),
                        }
                    }
                }
                Err(error) => ExecutionResult {
                    id: candidate.id.clone(),
                    success: false,
                    logs: error.meta.logs,
                    compute_units: error.meta.compute_units_consumed,
                    error: Some(error.err.to_string()),
                    base_fee,
                    priority_fee,
                    before: AccountView::default(),
                    after: AccountView::default(),
                },
            };
            let outcome = ExecutionResult { after: self.account_view(&keys), before, ..outcome };
            self.remember_executed(candidate.id.clone(), &item.transaction.bytes);
            results.push(outcome);
        }
        self.clear_native_burn()?;
        self.advance_blockhash(slot);
        Ok(SlotExecution {
            merged_order: ordered
                .iter()
                .map(|candidate| merged[&candidate.id].transaction.header.clone())
                .collect(),
            schedule,
            results,
            state_hash: self.state_hash(),
        })
    }

    fn account_view(&self, keys: &[Pubkey]) -> AccountView {
        let token = [spl_token_id(), spl_token_2022_id()];
        let mut view = AccountView::default();
        for (index, key) in keys.iter().enumerate() {
            let account = self.svm.get_account(key).unwrap_or_default();
            view.lamports.push(account.lamports);
            if token.contains(&account.owner) && account.data.len() >= 165 {
                view.tokens.push((index as u8, account));
            }
        }
        view
    }

    fn set_slot_clock(&mut self, slot: u64) -> Result<()> {
        let mut clock = self.svm.get_sysvar::<Clock>();
        clock.slot = slot;
        clock.epoch = slot / 432_000;
        clock.leader_schedule_epoch = clock.epoch;
        clock.epoch_start_timestamp = 1_794_787_200 + (clock.epoch * 432_000 / 20) as i64;
        clock.unix_timestamp = 1_794_787_200 + (slot / 20) as i64;
        self.svm.set_sysvar(&clock);
        self.refresh(solana_sdk::sysvar::clock::id());
        Ok(())
    }
    fn debit(&mut self, key: Pubkey, amount: u64, preserve_reserve: bool) -> Result<()> {
        let mut account = self
            .svm
            .get_account(&key)
            .context("Fee payer account not found")?;
        let remaining = account
            .lamports
            .checked_sub(amount)
            .context("Insufficient fee-payer balance")?;
        if preserve_reserve && remaining < self.fees.reserve_lamports {
            bail!("Fee would spend the fee-payer reserve");
        }
        account.lamports = remaining;
        self.put(key, account)
    }
    fn credit(&mut self, key: Pubkey, amount: u64) -> Result<()> {
        self.fund(&key.to_string(), amount)
    }
    fn distribute(&mut self, amount: u64, sources: &[u8]) -> Result<()> {
        let each = amount / sources.len() as u64;
        let remainder = amount % sources.len() as u64;
        for (i, proposer) in sources.iter().enumerate() {
            let key = *self
                .proposers
                .get(proposer)
                .context("Missing proposer fee recipient")?;
            self.credit(key, each + if i == 0 { remainder } else { 0 })?;
        }
        Ok(())
    }
    fn put(&mut self, key: Pubkey, account: Account) -> Result<()> {
        // An account left with no lamports no longer exists, as on Solana: its data and owner go
        // with it, so its address can be created again (a program deploy closes its buffer so).
        let account = if account.lamports == 0 { Account::default() } else { account };
        self.svm.set_account(key, account)?;
        self.refresh(key);
        Ok(())
    }
    fn refresh(&mut self, key: Pubkey) {
        if let Some(account) = self.svm.get_account(&key).filter(|account| account.lamports > 0) {
            let hash = account_digest(&key, &account);
            if self.account_hashes.get(&key) == Some(&hash) {
                return;
            }
            if let Some(old) = self.account_hashes.remove(&key) {
                subtract256(&mut self.accumulator, &old);
            }
            add256(&mut self.accumulator, &hash);
            self.account_hashes.insert(key, hash);
            self.total_lamports += account.lamports as u128;
            if let Some(old) = self.accounts.insert(key, Arc::new(account)) {
                self.total_lamports -= old.lamports as u128;
            }
        } else {
            if let Some(old) = self.account_hashes.remove(&key) {
                subtract256(&mut self.accumulator, &old);
            }
            if let Some(old) = self.accounts.remove(&key) {
                self.total_lamports -= old.lamports as u128;
            }
        }
    }
    /// Every account the given program owns, for `getProgramAccounts`.
    pub fn accounts_owned_by(&self, owner: &str) -> Result<Vec<(String, Account)>> {
        let owner = Pubkey::from_str(owner)?;
        Ok(self.accounts.iter().filter(|(_, a)| a.owner == owner).map(|(k, a)| (k.to_string(), a.as_ref().clone())).collect())
    }
    pub fn rent_exempt_minimum(&self, len: usize) -> u64 {
        self.svm.minimum_balance_for_rent_exemption(len)
    }
    pub fn account_count(&self) -> usize {
        self.accounts.len()
    }
    pub fn total_lamports(&self) -> Result<u64> {
        self.total_lamports.try_into().context("The total account lamports exceed u64")
    }
    pub fn state_hash(&self) -> String {
        hex::encode(self.accumulator)
    }
}

impl Executor for SvmExecutor {
    fn validate_transaction(&self, bytes: &[u8], target_slot: u64) -> Result<ValidatedTx> {
        SvmExecutor::validate_transaction(self, bytes, target_slot)
    }
    fn execute_slot(&mut self, slot: u64, items: Vec<ExecutionItem>) -> Result<SlotExecution> {
        SvmExecutor::execute_slot(self, slot, items)
    }
    fn state_hash(&self) -> String {
        SvmExecutor::state_hash(self)
    }
}

fn recent_blockhash(bytes: &[u8]) -> Option<Hash> {
    let tx: VersionedTransaction = bincode::deserialize(bytes).ok()?;
    Some(*tx.message.recent_blockhash())
}

pub struct Simulation {
    pub err: Option<solana_sdk::transaction::TransactionError>,
    pub logs: Vec<String>,
    pub units: u64,
    pub replaced: Option<String>,
}

/// A message's compute limit and price per CU, read from its ComputeBudget instructions the way
/// admission reads them.
fn compute_budget(message: &solana_sdk::message::VersionedMessage) -> Result<(u64, u64)> {
    let compute_program = Pubkey::from_str("ComputeBudget111111111111111111111111111111")?;
    let keys = message.static_account_keys();
    let (mut limit, mut price, mut normal) = (None, 0u64, 0u64);
    for ix in message.instructions() {
        if keys.get(ix.program_id_index as usize) == Some(&compute_program) {
            match ix.data.as_slice() {
                [2, rest @ ..] if rest.len() == 4 => limit = Some(u32::from_le_bytes(rest.try_into().unwrap()) as u64),
                [3, rest @ ..] if rest.len() == 8 => price = u64::from_le_bytes(rest.try_into().unwrap()),
                _ => {}
            }
        } else {
            normal += 1;
        }
    }
    Ok((limit.unwrap_or((normal * 200_000).min(1_400_000)), price))
}

pub fn priority_fee(micro_lamports_per_cu: u64, compute_limit: u64) -> Result<u64> {
    u64::try_from(
        (u128::from(micro_lamports_per_cu) * u128::from(compute_limit) + 999_999) / 1_000_000,
    )
    .map_err(|_| anyhow!("Priority fee overflow"))
}
/// The lattice hash of a set of accounts as every Acrux version so far computes it (each account's
/// digest summed mod 2^256), in hex: how a state from an earlier version is checked before it is
/// loaded under this one's rules.
pub fn lattice_hash<'a>(accounts: impl IntoIterator<Item = (Pubkey, &'a Account)>) -> String {
    let mut accumulator = [0; 32];
    for (key, account) in accounts {
        add256(&mut accumulator, &account_digest(&key, account));
    }
    hex::encode(accumulator)
}

fn account_digest(key: &Pubkey, account: &Account) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"acrux-account-v1");
    digest.update(key.as_ref());
    digest.update(account.lamports.to_le_bytes());
    digest.update(account.owner.as_ref());
    digest.update([account.executable as u8]);
    digest.update(account.rent_epoch.to_le_bytes());
    digest.update((account.data.len() as u64).to_le_bytes());
    digest.update(&account.data);
    digest.finalize().into()
}

fn spl_token_id() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
}

fn spl_token_2022_id() -> Pubkey {
    Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap()
}

fn add256(sum: &mut [u8; 32], rhs: &[u8; 32]) {
    let mut carry = 0u16;
    for (a, b) in sum.iter_mut().zip(rhs) {
        let value = u16::from(*a) + u16::from(*b) + carry;
        *a = value as u8;
        carry = value >> 8;
    }
}
fn subtract256(sum: &mut [u8; 32], rhs: &[u8; 32]) {
    let mut borrow = 0i16;
    for (a, b) in sum.iter_mut().zip(rhs) {
        let value = i16::from(*a) - i16::from(*b) - borrow;
        *a = value as u8;
        borrow = i16::from(value < 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        hash::Hash,
        signature::{Keypair, Signer},
        system_instruction,
        transaction::Transaction,
    };

    #[test]
    fn bridge_credit_preserves_prefunded_system_mint_and_ata() {
        let bridge = Pubkey::new_unique();
        let solana_mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let mint = bridge_mint(&bridge, &solana_mint);
        let ata = Pubkey::find_program_address(
            &[recipient.as_ref(), spl_token_id().as_ref(), mint.as_ref()],
            &Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap(),
        ).0;
        let credit = BridgeCredit { sequence: 0, solana_mint: solana_mint.to_string(), decimals: 6, recipient: recipient.to_string(), amount: 42 };
        let mut first = SvmExecutor::new([], &[]).unwrap();
        let mut second = SvmExecutor::new([], &[]).unwrap();
        for executor in [&mut first, &mut second] {
            executor.set_bridge(bridge);
            executor.put(mint, Account { lamports: 2_000_000, ..Account::default() }).unwrap();
            executor.put(ata, Account { lamports: 1_000_000, ..Account::default() }).unwrap();
            executor.set_credits(vec![credit.clone()]);
            executor.execute_slot(0, vec![]).unwrap();
            let mint_account = executor.svm.get_account(&mint).unwrap();
            let holder = executor.svm.get_account(&ata).unwrap();
            assert_eq!(mint_account.owner, spl_token_id());
            assert_eq!(holder.owner, spl_token_id());
            assert_eq!(mint_account.lamports, executor.rent_exempt_minimum(82) + 2_000_000);
            assert_eq!(holder.lamports, executor.rent_exempt_minimum(165) + 1_000_000);
            assert_eq!(u64::from_le_bytes(mint_account.data[36..44].try_into().unwrap()), 42);
            assert_eq!(u64::from_le_bytes(holder.data[64..72].try_into().unwrap()), 42);
            assert_eq!(executor.next_credit(), 1);
            // Reproposing a consumed deposit cannot mint it again.
            executor.set_credits(vec![credit.clone()]);
            executor.execute_slot(1, vec![]).unwrap();
            assert_eq!(executor.svm.get_account(&mint).unwrap(), mint_account);
            assert_eq!(executor.svm.get_account(&ata).unwrap(), holder);
        }
        assert_eq!(first.state_hash(), second.state_hash());
    }

    #[test]
    fn bridge_credit_skips_foreign_mint_and_the_next_slot_executes() {
        let bridge = Pubkey::new_unique();
        let solana_mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let mint = bridge_mint(&bridge, &solana_mint);
        let foreign = Account { lamports: 1_000_000, data: vec![7; 3], owner: Pubkey::new_unique(), executable: false, rent_epoch: 0 };
        let failed = BridgeCredit { sequence: 0, solana_mint: solana_mint.to_string(), decimals: 6, recipient: recipient.to_string(), amount: 42 };
        let mut executor = SvmExecutor::new([], &[]).unwrap();
        executor.set_bridge(bridge);
        executor.put(mint, foreign.clone()).unwrap();
        executor.set_credits(vec![failed.clone()]);
        executor.execute_slot(0, vec![]).unwrap();
        assert_eq!(executor.svm.get_account(&mint).unwrap(), foreign);
        assert_eq!(executor.next_credit(), 1);
        let key = bridge_skipped_credit(&bridge, 0);
        let record = executor.svm.get_account(&key).unwrap();
        assert_eq!(record.owner, bridge);
        let skipped: SkippedBridgeCredit = bincode::deserialize(&record.data).unwrap();
        assert_eq!(skipped, SkippedBridgeCredit { credit: failed.clone(), reason: BridgeCreditSkipReason::MintOwner });
        let checkpoint = executor.checkpoint();
        let mut saved = executor.snapshot_meta(&checkpoint);
        saved.accounts = checkpoint.account_changes(None).0.into_iter().map(|(k, a)| (k.to_string(), a.as_ref().clone())).collect();
        saved.executed = checkpoint.executed_changes(None).0;
        let mut restored = SvmExecutor::from_state(&[], saved).unwrap();
        restored.set_bridge(bridge);
        assert_eq!(restored.state_hash(), executor.state_hash());
        assert_eq!(restored.svm.get_account(&key).unwrap(), record);
        let next_mint = Pubkey::new_unique();
        let next = BridgeCredit { sequence: 1, solana_mint: next_mint.to_string(), decimals: 6, recipient: recipient.to_string(), amount: 9 };
        for executor in [&mut executor, &mut restored] {
            executor.set_credits(vec![failed.clone(), next.clone()]);
            executor.execute_slot(1, vec![]).unwrap();
            assert_eq!(executor.next_credit(), 2);
            assert_eq!(executor.svm.get_account(&mint).unwrap(), foreign);
            assert_eq!(executor.svm.get_account(&key).unwrap(), record);
            let credited = executor.svm.get_account(&bridge_mint(&bridge, &next_mint)).unwrap();
            assert_eq!(u64::from_le_bytes(credited.data[36..44].try_into().unwrap()), 9);
        }
        assert_eq!(restored.state_hash(), executor.state_hash());
    }

    #[test]
    fn bridge_credit_balance_overflow_skips_without_partial_mint_update() {
        let bridge = Pubkey::new_unique();
        let solana_mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let mint = bridge_mint(&bridge, &solana_mint);
        let ata = Pubkey::find_program_address(
            &[recipient.as_ref(), spl_token_id().as_ref(), mint.as_ref()],
            &Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap(),
        ).0;
        let mut executor = SvmExecutor::new([], &[]).unwrap();
        executor.set_bridge(bridge);
        let credit = BridgeCredit { sequence: 0, solana_mint: solana_mint.to_string(), decimals: 6, recipient: recipient.to_string(), amount: 1 };
        executor.set_credits(vec![credit.clone()]);
        executor.execute_slot(0, vec![]).unwrap();
        let mut holder = executor.svm.get_account(&ata).unwrap();
        holder.data[64..72].copy_from_slice(&u64::MAX.to_le_bytes());
        executor.put(ata, holder.clone()).unwrap();
        let mint_before = executor.svm.get_account(&mint).unwrap();
        let overflow = BridgeCredit { sequence: 1, ..credit };
        executor.set_credits(vec![overflow.clone()]);
        executor.execute_slot(1, vec![]).unwrap();
        assert_eq!(executor.svm.get_account(&mint).unwrap(), mint_before);
        assert_eq!(executor.svm.get_account(&ata).unwrap(), holder);
        assert_eq!(executor.next_credit(), 2);
        let record = executor.svm.get_account(&bridge_skipped_credit(&bridge, 1)).unwrap();
        assert_eq!(bincode::deserialize::<SkippedBridgeCredit>(&record.data).unwrap(), SkippedBridgeCredit { credit: overflow, reason: BridgeCreditSkipReason::BalanceOverflow });
        executor.execute_slot(2, vec![]).unwrap();
    }

    #[test]
    fn native_cutover_converts_all_holders_retires_wallets_and_replays_from_state() {
        use base64::Engine;
        let bridge = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let other_mint = Pubkey::new_unique();
        let alice = Pubkey::new_unique();
        let bob = Pubkey::new_unique();
        let faucet = Pubkey::new_unique();
        let validator = Pubkey::new_unique();
        let record_owner = Pubkey::new_unique();
        let nonce = Pubkey::new_unique();
        let rule = NativeCutover { slot:8,solana_mint:mint,lamports_per_base_unit:1000 };
        let mut executor = SvmExecutor::new([(alice.to_string(),9_000_000),(bob.to_string(),8_000_000),(faucet.to_string(),1_000_000_000),(validator.to_string(),7_000_000)],&[]).unwrap();
        executor.set_bridge(bridge);
        executor.set_native_cutover(rule.clone());
        executor.put(record_owner,Account { lamports:12_345,data:vec![77],owner:bridge,executable:false,rent_epoch:0 }).unwrap();
        executor.put(nonce,Account { lamports:123_456,data:vec![9],owner:system_program::id(),executable:false,rent_epoch:0 }).unwrap();
        // Both placements have the same program account. Only ProgramData changes at the cutover.
        let file:serde_json::Value = serde_json::from_slice(&std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../chain/placements/memo-4c2p.json")).unwrap()).unwrap();
        let mut accounts = file["accounts"].as_array().unwrap().iter().map(|a| (Pubkey::from_str(a["address"].as_str().unwrap()).unwrap(),Account {
            lamports:a["lamports"].as_u64().unwrap(),data:base64::engine::general_purpose::STANDARD.decode(a["data"].as_str().unwrap()).unwrap(),
            owner:Pubkey::from_str(a["owner"].as_str().unwrap()).unwrap(),executable:a["executable"].as_bool().unwrap(),rent_epoch:0,
        })).collect::<Vec<_>>();
        let configuration = Pubkey::new_unique();
        accounts.insert(accounts.len() - 1,(configuration,Account { lamports:4_000,data:vec![1,2],owner:accounts.last().unwrap().0,executable:false,rent_epoch:0 }));
        let mut upgraded = accounts.clone();
        upgraded[0].1.data[4..12].copy_from_slice(&8u64.to_le_bytes());
        executor.set_placements(vec![Placement { name:"memo before cutover".into(),slot:2,accounts:accounts.clone() },Placement { name:"memo at cutover".into(),slot:8,accounts:upgraded.clone() }]);
        executor.set_credits(vec![
            BridgeCredit { sequence:0,solana_mint:mint.to_string(),decimals:6,recipient:alice.to_string(),amount:30 },
            BridgeCredit { sequence:1,solana_mint:other_mint.to_string(),decimals:6,recipient:bob.to_string(),amount:9 },
        ]);
        executor.execute_slot(3,vec![]).unwrap();
        assert!(executor.native_supply().unwrap().is_none());
        let native_mint = bridge_mint(&bridge,&mint);
        let alice_ata = Pubkey::find_program_address(&[alice.as_ref(),spl_token_id().as_ref(),native_mint.as_ref()],&Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap()).0;
        let mut holder = executor.svm.get_account(&alice_ata).unwrap();
        holder.data[64..72].copy_from_slice(&10u64.to_le_bytes());
        executor.put(alice_ata,holder.clone()).unwrap();
        let frozen = Pubkey::new_unique();
        let delegated = Pubkey::new_unique();
        let program_held = Pubkey::new_unique();
        for (key,owner,amount,state) in [(frozen,alice,11,2),(delegated,bob,6,1),(program_held,record_owner,3,1)] {
            let mut account = holder.clone();
            account.data[32..64].copy_from_slice(owner.as_ref());
            account.data[64..72].copy_from_slice(&(amount as u64).to_le_bytes());
            account.data[108] = state;
            if key == delegated {
                account.data[72..76].copy_from_slice(&1u32.to_le_bytes());
                account.data[76..108].copy_from_slice(validator.as_ref());
                account.data[121..129].copy_from_slice(&6u64.to_le_bytes());
            }
            executor.put(key,account).unwrap();
        }
        executor.execute_slot(7,vec![]).unwrap();
        executor.fund(&upgraded[0].0.to_string(),5_000).unwrap();
        executor.fund(&accounts.last().unwrap().0.to_string(),6_000).unwrap();
        executor.set_proposers(&[(1,validator.to_string())]).unwrap();
        let before = executor.checkpoint();
        let other_before = executor.svm.get_account(&bridge_mint(&bridge,&other_mint)).unwrap();
        let reserve = before.accounts.values().filter(|a| !native_wallet(a)).map(|a| a.lamports).sum::<u64>() + executor.rent_exempt_minimum(80) + executor.rent_exempt_minimum(0);
        executor.activate_native_cutover(7).unwrap();
        assert!(executor.native_supply().unwrap().is_none(),"pre-inclusion activation leaves pre-cut slots unchanged");
        executor.activate_native_cutover(8).unwrap();
        let activated = executor.checkpoint();
        assert_eq!(executor.admit_inclusions(&[(alice.to_string(),1,1),(faucet.to_string(),1,1)],0).unwrap(),vec![true,false],"inclusion sees converted ACRUX and the retired faucet");
        executor.charge_inclusion(&alice.to_string(),1,0,1).unwrap();
        executor.execute_slot(8,vec![]).unwrap();
        assert_eq!(executor.balance(&alice.to_string()).unwrap(),20_000);
        assert_eq!(executor.balance(&validator.to_string()).unwrap(),1_000,"execute_slot must not retire inclusion fees charged after activation");
        executor.activate_native_cutover(8).unwrap();
        assert_eq!(executor.balance(&validator.to_string()).unwrap(),1_000);
        executor.restore(activated).unwrap();
        executor.execute_slot(8,vec![]).unwrap();
        let cutover_hash = executor.state_hash();
        let supply = executor.native_supply().unwrap().unwrap();
        assert_eq!((supply.credited_lamports,supply.withdrawn_lamports,supply.reserve_lamports,supply.circulating_lamports),(30_000,0,reserve,30_000));
        assert_eq!(executor.balance(&alice.to_string()).unwrap(),21_000);
        assert_eq!(executor.balance(&bob.to_string()).unwrap(),6_000);
        assert_eq!(executor.balance(&faucet.to_string()).unwrap(),0);
        assert_eq!(executor.balance(&validator.to_string()).unwrap(),0);
        let record = executor.svm.get_account(&record_owner).unwrap();
        assert_eq!((record.lamports,record.owner,record.data),(15_345,bridge,vec![77]));
        assert_eq!(executor.svm.get_account(&nonce).unwrap().lamports,123_456);
        for key in [alice_ata,frozen,delegated,program_held] {
            assert_eq!(executor.svm.get_account(&key).unwrap().data[64..72],[0;8]);
        }
        assert_eq!(executor.svm.get_account(&native_mint).unwrap().data[36..44],[0;8]);
        assert_eq!(executor.svm.get_account(&bridge_mint(&bridge,&other_mint)).unwrap(),other_before);
        let mut expected_data = upgraded[0].1.clone(); expected_data.lamports += 5_000;
        let mut expected_program = accounts.last().unwrap().1.clone(); expected_program.lamports += 6_000;
        assert_eq!(executor.svm.get_account(&upgraded[0].0).unwrap(),expected_data,"a data upgrade preserves existing rent and donations");
        assert_eq!(executor.svm.get_account(&accounts.last().unwrap().0).unwrap(),expected_program);
        executor.restore(before).unwrap();
        executor.execute_slot(8,vec![]).unwrap();
        assert_eq!(executor.state_hash(),cutover_hash,"rollback re-applies the conversion and data upgrade exactly once");
        let mut live_configuration = executor.svm.get_account(&configuration).unwrap();
        live_configuration.data[0] = 99;
        executor.put(configuration,live_configuration.clone()).unwrap();
        executor.fund(&upgraded[0].0.to_string(),7_777).unwrap();
        executor.fund(&accounts.last().unwrap().0.to_string(),8_888).unwrap();
        let donated_program_data = executor.svm.get_account(&upgraded[0].0).unwrap();
        let checkpoint = executor.checkpoint();
        let mut saved = executor.snapshot_meta(&checkpoint);
        saved.accounts = checkpoint.account_changes(None).0.into_iter().map(|(key,account)| (key.to_string(),account.as_ref().clone())).collect();
        saved.executed = checkpoint.executed_changes(None).0;
        let mut loaded = SvmExecutor::from_state(&[],saved).unwrap();
        loaded.set_bridge(bridge);
        loaded.set_native_cutover(rule);
        assert_eq!(loaded.native_supply().unwrap().unwrap().credited_lamports,30_000);
        for current in [&mut executor,&mut loaded] { current.execute_slot(9,vec![]).unwrap(); }
        assert_eq!(executor.state_hash(),loaded.state_hash(),"a loaded marker never converts holders again");
        assert_eq!(executor.svm.get_account(&configuration).unwrap(),live_configuration,"mutable placement state is not reset by a later slot");
        assert_eq!(executor.svm.get_account(&upgraded[0].0).unwrap(),donated_program_data,"donations do not trigger code re-placement");
        assert_eq!(loaded.balance(&alice.to_string()).unwrap(),21_000);
    }

    #[test]
    fn native_cutover_credits_once_skips_overflow_and_clears_burn_each_slot() {
        let bridge = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        let foreign = Pubkey::new_unique();
        let rule = NativeCutover { slot:4,solana_mint:mint,lamports_per_base_unit:1000 };
        let credit = |sequence,recipient:Pubkey,amount| BridgeCredit { sequence,solana_mint:mint.to_string(),decimals:6,recipient:recipient.to_string(),amount };
        let mut executor = SvmExecutor::new([],&[]).unwrap();
        executor.set_bridge(bridge);
        executor.set_native_cutover(rule.clone());
        executor.put(foreign,Account { lamports:1000,data:vec![7],owner:bridge,executable:false,rent_epoch:0 }).unwrap();
        executor.set_credits(vec![credit(0,owner,100)]);
        executor.execute_slot(3,vec![]).unwrap();
        executor.execute_slot(4,vec![]).unwrap();
        let mint_account = executor.svm.get_account(&bridge_mint(&bridge,&mint)).unwrap();
        executor.set_credits(vec![credit(1,owner,50),credit(1,owner,50)]);
        executor.execute_slot(5,vec![]).unwrap();
        assert_eq!(executor.next_credit(),2);
        assert_eq!(executor.balance(&owner.to_string()).unwrap(),150_000);
        assert_eq!(executor.native_supply().unwrap().unwrap().credited_lamports,150_000);
        assert_eq!(executor.svm.get_account(&bridge_mint(&bridge,&mint)).unwrap(),mint_account,"native deposits do not refill the bridged mint");
        executor.set_credits(vec![credit(2,owner,u64::MAX),credit(3,foreign,1),credit(4,owner,5)]);
        executor.execute_slot(6,vec![]).unwrap();
        assert_eq!(executor.next_credit(),5);
        assert_eq!(executor.balance(&owner.to_string()).unwrap(),155_000);
        for (sequence,reason) in [(2,BridgeCreditSkipReason::BalanceOverflow),(3,BridgeCreditSkipReason::RecipientOwner)] {
            let skipped:SkippedBridgeCredit = bincode::deserialize(&executor.svm.get_account(&bridge_skipped_credit(&bridge,sequence)).unwrap().data).unwrap();
            assert_eq!(skipped.reason,reason);
        }
        let checkpoint = executor.checkpoint();
        let gate_key = bridge_native(&bridge);
        let mut gate = executor.svm.get_account(&gate_key).unwrap();
        gate.data[48..56].copy_from_slice(&(u64::MAX - 10).to_le_bytes());
        executor.put(gate_key,gate.clone()).unwrap();
        executor.set_credits(vec![credit(5,owner,1)]);
        executor.execute_slot(7,vec![]).unwrap();
        assert_eq!(executor.svm.get_account(&gate_key).unwrap(),gate,"credited overflow never writes a partial wallet or gate update");
        let skipped:SkippedBridgeCredit = bincode::deserialize(&executor.svm.get_account(&bridge_skipped_credit(&bridge,5)).unwrap().data).unwrap();
        assert_eq!(skipped.reason,BridgeCreditSkipReason::SupplyOverflow);
        executor.restore(checkpoint.clone()).unwrap();
        let gate = executor.svm.get_account(&gate_key).unwrap();
        let mut full_wallet = executor.svm.get_account(&owner).unwrap();
        full_wallet.lamports = u64::MAX - 10;
        executor.put(owner,full_wallet.clone()).unwrap();
        executor.set_credits(vec![credit(5,owner,1)]);
        executor.execute_slot(7,vec![]).unwrap();
        assert_eq!(executor.svm.get_account(&owner).unwrap(),full_wallet);
        assert_eq!(executor.svm.get_account(&gate_key).unwrap(),gate,"wallet overflow cannot advance backed credits");
        let skipped:SkippedBridgeCredit = bincode::deserialize(&executor.svm.get_account(&bridge_skipped_credit(&bridge,5)).unwrap().data).unwrap();
        assert_eq!(skipped.reason,BridgeCreditSkipReason::BalanceOverflow);
        executor.restore(checkpoint).unwrap();
        let before = executor.total_lamports().unwrap();
        let burn_key = bridge_native_burn(&bridge);
        let mut burn = executor.svm.get_account(&burn_key).unwrap();
        let rent = burn.lamports;
        burn.lamports += 12_000;
        executor.put(burn_key,burn).unwrap();
        executor.execute_slot(7,vec![]).unwrap();
        assert_eq!(executor.svm.get_account(&burn_key).unwrap().lamports,rent);
        assert_eq!(executor.total_lamports().unwrap(),before,"end-of-slot burn removal updates the cached total");
        assert_eq!(executor.native_supply().unwrap().unwrap().credited_lamports,155_000);
        let mut no_rule = SvmExecutor::new([],&[]).unwrap();
        no_rule.set_bridge(bridge);
        no_rule.set_credits(vec![credit(0,owner,1)]);
        no_rule.execute_slot(100,vec![]).unwrap();
        assert!(no_rule.native_supply().unwrap().is_none());
        assert_eq!(no_rule.balance(&owner.to_string()).unwrap(),0,"without the rule a deposit remains an SPL credit");
        assert_eq!(u64::from_le_bytes(no_rule.svm.get_account(&bridge_mint(&bridge,&mint)).unwrap().data[36..44].try_into().unwrap()),1);
        let mut overflowing = SvmExecutor::new([(owner.to_string(),9_000_000)],&[]).unwrap();
        overflowing.set_bridge(bridge);
        overflowing.set_native_cutover(rule);
        overflowing.set_credits(vec![credit(0,owner,u64::MAX)]);
        overflowing.execute_slot(3,vec![]).unwrap();
        let held_mint = overflowing.svm.get_account(&bridge_mint(&bridge,&mint)).unwrap();
        assert!(overflowing.execute_slot(4,vec![]).is_err(),"cutover conversion refuses multiplication overflow");
        assert_eq!(overflowing.balance(&owner.to_string()).unwrap(),9_000_000,"a failed cutover never retires wallets partially");
        assert_eq!(overflowing.svm.get_account(&bridge_mint(&bridge,&mint)).unwrap(),held_mint);
        assert!(overflowing.native_supply().unwrap().is_none());
    }

    #[test]
    #[ignore = "requires chain/target/deploy/acrux_bridge.so built with withdraw_native"]
    fn native_withdrawal_sbf_burns_and_enforces_backed_cap() {
        use solana_sdk::instruction::{AccountMeta,Instruction};
        let bridge = Pubkey::from_str("EDm6HxGArNEgGfWRZwvj42f4euc9oPQWVUwUbkK3PEe5").unwrap();
        let programs = [GenesisProgram { address:bridge.to_string(),path:PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../chain/target/deploy/acrux_bridge.so") }];
        let owner = Keypair::new();
        let proposer = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let mut executor = SvmExecutor::new([(owner.pubkey().to_string(),100_000_000)],&programs).unwrap();
        executor.set_bridge(bridge);
        executor.set_bridge_minimums(4,vec![MinimumWithdrawal { mint,amount:10 }]);
        executor.set_native_cutover(NativeCutover { slot:4,solana_mint:mint,lamports_per_base_unit:1000 });
        executor.set_proposers(&[(1,proposer.to_string())]).unwrap();
        executor.set_credits(vec![BridgeCredit { sequence:0,solana_mint:mint.to_string(),decimals:6,recipient:owner.pubkey().to_string(),amount:200_000 }]);
        executor.execute_slot(3,vec![]).unwrap();
        executor.execute_slot(4,vec![]).unwrap();
        let native = bridge_native(&bridge);
        let burn = bridge_native_burn(&bridge);
        let outbound = Pubkey::find_program_address(&[b"outbound"],&bridge).0;
        let record = |sequence:u64| Pubkey::find_program_address(&[b"withdrawal",&sequence.to_le_bytes()],&bridge).0;
        let withdraw = |executor:&mut SvmExecutor,amount:u64,sequence:u64,slot:u64| {
            let mut data = Sha256::digest(b"global:withdraw_native")[..8].to_vec();
            data.extend_from_slice(&amount.to_le_bytes());
            data.extend_from_slice(recipient.as_ref());
            let instruction = Instruction { program_id:bridge,accounts:vec![
                AccountMeta::new(owner.pubkey(),true),AccountMeta::new(native,false),
                AccountMeta::new_readonly(bridge_minimum(&bridge,&mint),false),AccountMeta::new(burn,false),
                AccountMeta::new(outbound,false),AccountMeta::new(record(sequence),false),
                AccountMeta::new_readonly(system_program::id(),false),
            ],data };
            let tx = Transaction::new_signed_with_payer(&[instruction],Some(&owner.pubkey()),&[&owner],Hash::from_str(&executor.latest_blockhash()).unwrap());
            let transaction = executor.validate_transaction(&bincode::serialize(&tx).unwrap(),slot).unwrap();
            executor.execute_slot(slot,vec![ExecutionItem { transaction,source_proposers:vec![1] }]).unwrap().results.remove(0)
        };
        let before = executor.total_lamports().unwrap();
        let released = withdraw(&mut executor,20_000,0,5);
        assert!(released.success,"{released:?}");
        let supply = executor.native_supply().unwrap().unwrap();
        assert_eq!((supply.credited_lamports,supply.withdrawn_lamports),(200_000_000,20_000_000));
        assert_eq!(executor.svm.get_account(&burn).unwrap().lamports,executor.rent_exempt_minimum(0));
        assert_eq!(before - executor.total_lamports().unwrap(),20_000_000 + released.base_fee / 2);
        let withdrawal = executor.svm.get_account(&record(0)).unwrap();
        assert_eq!(withdrawal.data[80..112],mint.to_bytes());
        assert_eq!(withdrawal.data[112..120],20_000u64.to_le_bytes());
        assert_eq!(executor.withdrawals(1),vec![(0,mint.to_bytes(),recipient.to_bytes(),20_000)]);
        // Extra native balance can come from preserved legacy account rent. It does not enlarge
        // the hashed backing counter, even when the wallet can afford the attempted withdrawal.
        executor.fund(&owner.pubkey().to_string(),500_000_000).unwrap();
        let gate = executor.svm.get_account(&native).unwrap();
        let refused = withdraw(&mut executor,200_000,1,6);
        assert!(!refused.success,"the cap must reject native value beyond backed credits");
        assert_eq!(executor.svm.get_account(&native).unwrap(),gate);
        assert_eq!(executor.svm.get_account(&outbound).unwrap().data[8..16],1u64.to_le_bytes());
        assert!(executor.svm.get_account(&record(1)).is_none());
        assert_eq!(executor.svm.get_account(&burn).unwrap().lamports,executor.rent_exempt_minimum(0));
    }

    #[test]
    fn withdrawal_below_configured_minimum_does_not_burn_or_write_a_record() {
        use solana_sdk::instruction::{AccountMeta, Instruction};
        let bridge = Pubkey::from_str("EDm6HxGArNEgGfWRZwvj42f4euc9oPQWVUwUbkK3PEe5").unwrap();
        let programs = [GenesisProgram {
            address: bridge.to_string(),
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../chain/target/deploy/acrux_bridge.so"),
        }];
        let owner = Keypair::new();
        let solana_mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let proposer = Pubkey::new_unique();
        let ata_program = Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();
        let source = |solana_mint: &Pubkey| {
            Pubkey::find_program_address(&[owner.pubkey().as_ref(), spl_token_id().as_ref(), bridge_mint(&bridge, solana_mint).as_ref()], &ata_program).0
        };
        let outbound = Pubkey::find_program_address(&[b"outbound"], &bridge).0;
        let record = |sequence: u64| Pubkey::find_program_address(&[b"withdrawal", &sequence.to_le_bytes()], &bridge).0;
        let minimum = bridge_minimum(&bridge, &solana_mint);
        let mut executor = SvmExecutor::new([(owner.pubkey().to_string(), 100_000_000)], &programs).unwrap();
        executor.set_proposers(&[(1, proposer.to_string())]).unwrap();
        executor.set_bridge(bridge);
        executor.set_bridge_minimums(4, vec![MinimumWithdrawal { mint: solana_mint, amount: 10 }]);
        executor.execute_slot(3, vec![]).unwrap();
        assert!(executor.svm.get_account(&minimum).is_none());
        executor.set_credits(vec![BridgeCredit { sequence: 0, solana_mint: solana_mint.to_string(), decimals: 6, recipient: owner.pubkey().to_string(), amount: 100 }]);
        executor.execute_slot(4, vec![]).unwrap();
        let minimum_account = executor.svm.get_account(&minimum).unwrap();
        assert_eq!(minimum_account.owner, bridge);
        assert_eq!(&minimum_account.data[..8], &Sha256::digest(b"account:MinimumWithdrawal")[..8]);
        assert_eq!(minimum_account.data[8..], 10u64.to_le_bytes());
        let withdraw = |executor: &mut SvmExecutor, mint: Pubkey, amount: u64, sequence: u64, at: u64, minimum_key: Pubkey| {
            let mut data = Sha256::digest(b"global:withdraw")[..8].to_vec();
            data.extend_from_slice(mint.as_ref());
            data.extend_from_slice(&amount.to_le_bytes());
            data.extend_from_slice(recipient.as_ref());
            let instruction = Instruction { program_id: bridge, accounts: vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(bridge_mint(&bridge, &mint), false),
                AccountMeta::new_readonly(minimum_key, false),
                AccountMeta::new(source(&mint), false),
                AccountMeta::new(outbound, false),
                AccountMeta::new(record(sequence), false),
                AccountMeta::new_readonly(spl_token_id(), false),
                AccountMeta::new_readonly(system_program::id(), false),
            ], data };
            let transaction = Transaction::new_signed_with_payer(&[instruction], Some(&owner.pubkey()), &[&owner], Hash::from_str(&executor.latest_blockhash()).unwrap());
            let transaction = executor.validate_transaction(&bincode::serialize(&transaction).unwrap(), at).unwrap();
            executor.execute_slot(at, vec![ExecutionItem { transaction, source_proposers: vec![1] }]).unwrap().results.remove(0)
        };
        let mint_before = executor.svm.get_account(&bridge_mint(&bridge, &solana_mint)).unwrap();
        let source_before = executor.svm.get_account(&source(&solana_mint)).unwrap();
        let failed = withdraw(&mut executor, solana_mint, 9, 0, 5, minimum);
        assert!(!failed.success);
        assert!(failed.logs.iter().any(|line| line.contains("BelowMinimumWithdrawal")), "{failed:?}");
        assert_eq!(executor.svm.get_account(&bridge_mint(&bridge, &solana_mint)).unwrap(), mint_before);
        assert_eq!(executor.svm.get_account(&source(&solana_mint)).unwrap(), source_before);
        assert!(executor.svm.get_account(&outbound).is_none());
        assert!(executor.svm.get_account(&record(0)).is_none());
        let bypass = withdraw(&mut executor, solana_mint, 9, 0, 6, system_program::id());
        assert!(!bypass.success, "a caller cannot replace the configured minimum PDA");
        let paid = withdraw(&mut executor, solana_mint, 10, 0, 7, minimum);
        assert!(paid.success, "{paid:?}");
        assert_eq!(u64::from_le_bytes(executor.svm.get_account(&source(&solana_mint)).unwrap().data[64..72].try_into().unwrap()), 90);
        assert_eq!(executor.svm.get_account(&record(0)).unwrap().data[112..120], 10u64.to_le_bytes());
        assert_eq!(executor.svm.get_account(&minimum).unwrap(), minimum_account);

        // The floor is committed account state: loading a checkpoint keeps it and its hash.
        let checkpoint = executor.checkpoint();
        let mut saved = executor.snapshot_meta(&checkpoint);
        saved.accounts = checkpoint.account_changes(None).0.into_iter().map(|(key, account)| (key.to_string(), account.as_ref().clone())).collect();
        saved.executed = checkpoint.executed_changes(None).0;
        let mut restored = SvmExecutor::from_state(&programs, saved).unwrap();
        restored.set_bridge(bridge);
        restored.set_bridge_minimums(4, vec![MinimumWithdrawal { mint: solana_mint, amount: 10 }]);
        restored.set_proposers(&[(1, proposer.to_string())]).unwrap();
        assert_eq!(restored.svm.get_account(&minimum).unwrap(), minimum_account);
        assert_eq!(restored.state_hash(), executor.state_hash());
        for current in [&mut executor, &mut restored] { current.execute_slot(8, vec![]).unwrap(); }
        assert_eq!(restored.state_hash(), executor.state_hash());

        // A mint not named by the rule has no floor, even while another mint has one.
        let unnamed = Pubkey::new_unique();
        executor.set_credits(vec![BridgeCredit { sequence: 1, solana_mint: unnamed.to_string(), decimals: 6, recipient: owner.pubkey().to_string(), amount: 1 }]);
        executor.execute_slot(9, vec![]).unwrap();
        let no_floor = bridge_minimum(&bridge, &unnamed);
        assert!(executor.svm.get_account(&no_floor).is_none());
        let paid = withdraw(&mut executor, unnamed, 1, 1, 10, no_floor);
        assert!(paid.success, "{paid:?}");
    }

    #[test]
    fn real_transfer_exact_fees_and_deterministic_state() {
        let payer = Keypair::new();
        let receiver = Pubkey::new_unique();
        let proposer = Pubkey::new_unique();
        let balances = vec![
            (payer.pubkey().to_string(), 10_000_000),
            (receiver.to_string(), 1_000_000),
        ];
        let mut first = SvmExecutor::new(balances.clone(), &[]).unwrap();
        let mut second = SvmExecutor::new(balances, &[]).unwrap();
        for executor in [&mut first, &mut second] {
            executor
                .set_proposers(&[(1, proposer.to_string())])
                .unwrap();
        }
        let tx = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(
                &payer.pubkey(),
                &receiver,
                2_000_000,
            )],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::from_str(&first.latest_blockhash()).unwrap(),
        );
        let bytes = bincode::serialize(&tx).unwrap();
        let validated = first.validate_transaction(&bytes, 0).unwrap();
        let mut hashes = Vec::new();
        for executor in [&mut first, &mut second] {
            executor
                .charge_inclusion(&payer.pubkey().to_string(), 1, 0, 1)
                .unwrap();
            let result = executor
                .execute_slot(
                    0,
                    vec![ExecutionItem {
                        transaction: validated.clone(),
                        source_proposers: vec![1],
                    }],
                )
                .unwrap();
            assert!(result.results[0].success, "{:?}", result.results[0]);
            assert_eq!(executor.balance(&receiver.to_string()).unwrap(), 3_000_000);
            assert_eq!(
                executor.balance(&payer.pubkey().to_string()).unwrap(),
                7_994_000
            );
            assert_eq!(executor.balance(&proposer.to_string()).unwrap(), 3_500);
            hashes.push(result.state_hash);
        }
        assert_eq!(hashes[0], hashes[1]);
    }

    #[test]
    fn a_saved_state_restores_the_same_chain() {
        let payer = Keypair::new();
        let receiver = Pubkey::new_unique();
        let proposer = Pubkey::new_unique();
        let mut executor = SvmExecutor::new([(payer.pubkey().to_string(), 10_000_000)], &[]).unwrap();
        executor.set_proposers(&[(1, proposer.to_string())]).unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(&payer.pubkey(), &receiver, 2_000_000)],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::from_str(&executor.latest_blockhash()).unwrap(),
        );
        let bytes = bincode::serialize(&tx).unwrap();
        let validated = executor.validate_transaction(&bytes, 0).unwrap();
        executor.charge_inclusion(&payer.pubkey().to_string(), 1, 0, 1).unwrap();
        executor.execute_slot(0, vec![ExecutionItem { transaction: validated, source_proposers: vec![1] }]).unwrap();
        let checkpoint = executor.checkpoint();
        let mut state = executor.snapshot_meta(&checkpoint);
        state.accounts = checkpoint.account_changes(None).0.into_iter().map(|(k, a)| (k.to_string(), a.as_ref().clone())).collect();
        state.executed = checkpoint.executed_changes(None).0;
        assert!(state.accounts.iter().any(|(_, a)| a.executable));
        let mut restored = SvmExecutor::from_state(&[], state.clone()).unwrap();
        restored.set_proposers(&[(1, proposer.to_string())]).unwrap();
        assert_eq!(restored.state_hash(), executor.state_hash());
        assert_eq!(restored.balance(&receiver.to_string()).unwrap(), 2_000_000);
        assert_eq!(restored.latest_blockhash(), executor.latest_blockhash());
        let id = executor.check_transaction(&bytes, 1).unwrap().header.id;
        assert!(executor.is_executed(&id) && restored.is_executed(&id), "an executed transaction stays executed");
        for e in [&mut executor, &mut restored] {
            e.execute_slot(1, vec![]).unwrap();
        }
        assert_eq!(restored.state_hash(), executor.state_hash());
        // Nothing changed since: no account to write.
        assert!(executor.checkpoint().account_changes(Some(&restored.checkpoint())).1.is_empty());
        assert_eq!(lattice_hash(state.accounts.iter().map(|(k, a)| (Pubkey::from_str(k).unwrap(), a))), state.state_hash);
        state.accounts.pop();
        assert_ne!(SvmExecutor::from_state(&[], state.clone()).unwrap().state_hash(), state.state_hash, "a missing account changes the hash");
    }

    #[test]
    fn reserve_and_per_window_inclusion_rate() {
        let payer = Pubkey::new_unique();
        let proposer = Pubkey::new_unique();
        let mut executor = SvmExecutor::new([(payer.to_string(), 2_000_000)], &[]).unwrap();
        executor
            .set_proposers(&[(1, proposer.to_string())])
            .unwrap();
        executor
            .charge_inclusion(&payer.to_string(), 1, 0, 166)
            .unwrap();
        assert!(executor
            .charge_inclusion(&payer.to_string(), 1, 0, 1)
            .is_err());
        assert_eq!(priority_fee(1, 1).unwrap(), 1);
        assert!(executor.debit(payer, 1_000_000, true).is_err());
    }

    #[test]
    fn the_priority_fee_leaves_the_payer_once_and_reaches_the_proposer() {
        let payer = Keypair::new();
        let receiver = Pubkey::new_unique();
        let proposer = Pubkey::new_unique();
        let mut executor = SvmExecutor::new([(payer.pubkey().to_string(), 10_000_000)], &[]).unwrap();
        executor.set_proposers(&[(1, proposer.to_string())]).unwrap();
        // 1,000,000 micro-lamports per CU on 20,000 CU: 20,000 lamports of priority fee.
        let tx = Transaction::new_signed_with_payer(
            &[
                solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(20_000),
                solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(1_000_000),
                system_instruction::transfer(&payer.pubkey(), &receiver, 1_000_000),
            ],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::from_str(&executor.latest_blockhash()).unwrap(),
        );
        let validated = executor.validate_transaction(&bincode::serialize(&tx).unwrap(), 0).unwrap();
        executor.charge_inclusion(&payer.pubkey().to_string(), 1, 0, 1).unwrap();
        let result = executor
            .execute_slot(0, vec![ExecutionItem { transaction: validated, source_proposers: vec![1] }])
            .unwrap();
        assert!(result.results[0].success, "{:?}", result.results[0]);
        assert_eq!(result.results[0].priority_fee, 20_000);
        // 1,000,000 sent, 1,000 inclusion, 5,000 base, 20,000 priority.
        assert_eq!(executor.balance(&payer.pubkey().to_string()).unwrap(), 10_000_000 - 1_026_000);
        // The proposer: inclusion 1,000, half the base fee 2,500, the priority fee 20,000.
        assert_eq!(executor.balance(&proposer.to_string()).unwrap(), 23_500);
    }

    #[test]
    fn an_account_drained_to_zero_lamports_no_longer_exists() {
        let key = Pubkey::new_unique();
        let mut executor = SvmExecutor::new(Vec::<(String, u64)>::new(), &[]).unwrap();
        let empty = executor.state_hash();
        executor.put(key, Account { lamports: 1_000_000, data: vec![7; 64], owner: Pubkey::new_unique(), executable: false, rent_epoch: 0 }).unwrap();
        assert_ne!(executor.state_hash(), empty);
        executor.put(key, Account { lamports: 0, data: vec![7; 64], owner: Pubkey::new_unique(), executable: false, rent_epoch: 0 }).unwrap();
        let left = executor.svm.get_account(&key).unwrap_or_default();
        assert!(left.data.is_empty() && left.owner == system_program::id());
        assert_eq!(executor.state_hash(), empty);
    }

    #[test]
    fn an_unpayable_copy_is_dropped_and_the_block_stands() {
        let funded = Pubkey::new_unique();
        let unfunded = Pubkey::new_unique();
        let proposer = Pubkey::new_unique();
        let mut executor = SvmExecutor::new([(funded.to_string(), 2_000_000)], &[]).unwrap();
        executor.set_proposers(&[(1, proposer.to_string())]).unwrap();
        let items = [
            (funded.to_string(), 1, 1),
            (unfunded.to_string(), 1, 1),
            (funded.to_string(), 1, 1),
        ];
        assert_eq!(executor.admit_inclusions(&items, 0).unwrap(), vec![true, false, true]);
        assert!(!executor.can_pay_inclusion(&unfunded.to_string(), 1, 0, 1));
        // Past the per-window rate (reserve/6 = 166 copies) a copy is dropped, not refused.
        let burst = vec![(funded.to_string(), 1u8, 1u64); 167];
        let admitted = executor.admit_inclusions(&burst, 0).unwrap();
        assert_eq!(admitted.iter().filter(|ok| **ok).count(), 166);
        assert!(!admitted[166]);
    }

    #[test]
    fn a_transaction_whose_blockhash_ages_out_fails_without_stopping_the_slot() {
        let payer = Keypair::new();
        let proposer = Pubkey::new_unique();
        let mut executor = SvmExecutor::new([(payer.pubkey().to_string(), 10_000_000)], &[]).unwrap();
        executor.set_proposers(&[(1, proposer.to_string())]).unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[system_instruction::transfer(&payer.pubkey(), &Pubkey::new_unique(), 1_000_000)],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::from_str(&executor.latest_blockhash()).unwrap(),
        );
        let bytes = bincode::serialize(&tx).unwrap();
        let validated = executor.validate_transaction(&bytes, 0).unwrap();
        for slot in 0..BLOCKHASH_AGE_SLOTS + 2 {
            executor.execute_slot(slot, vec![]).unwrap();
        }
        assert!(executor.validate_transaction(&bytes, 0).is_err());
        assert!(executor.check_transaction(&bytes, 0).is_ok());
        let result = executor
            .execute_slot(
                BLOCKHASH_AGE_SLOTS + 2,
                vec![ExecutionItem { transaction: validated, source_proposers: vec![1] }],
            )
            .unwrap();
        assert!(!result.results[0].success);
        assert_eq!(result.results[0].base_fee + result.results[0].priority_fee, 0);
        assert_eq!(executor.balance(&payer.pubkey().to_string()).unwrap(), 10_000_000);
    }

    #[test]
    fn checkpoints_restore_transaction_history_across_branches() {
        let mut executor = SvmExecutor::new([], &[]).unwrap();
        executor.remember_executed("shared".into(), &[]);
        let shared = executor.checkpoint();
        executor.remember_executed("branch-a".into(), &[]);
        let a = executor.checkpoint();
        executor.restore(shared).unwrap();
        executor.remember_executed("branch-b".into(), &[]);
        let b = executor.checkpoint();
        assert_eq!(
            executor.executed.keys().cloned().collect::<BTreeSet<String>>(),
            BTreeSet::from(["shared".into(), "branch-b".into()])
        );
        executor.restore(a.clone()).unwrap();
        assert_eq!(
            executor.executed.keys().cloned().collect::<BTreeSet<String>>(),
            BTreeSet::from(["shared".into(), "branch-a".into()])
        );
        executor.restore(b).unwrap();
        executor.restore(a).unwrap();
        assert_eq!(
            executor.executed.keys().cloned().collect::<BTreeSet<String>>(),
            BTreeSet::from(["shared".into(), "branch-a".into()])
        );
    }
}
