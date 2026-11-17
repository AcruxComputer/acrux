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
    executed: BTreeMap<String, u64>,
    recent: VecDeque<(Hash, u64)>,
    inclusion_used: BTreeMap<(u64, Pubkey, u8), u64>,
    fees: Fees,
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
    proposers: BTreeMap<u8, Pubkey>,
    inclusion_used: BTreeMap<(u64, Pubkey, u8), u64>,
    /// Executed transaction ids, each with the slot of the blockhash it referenced; an id is kept
    /// while that blockhash is still valid, which is as long as a replay could land.
    executed: BTreeMap<String, u64>,
    /// The valid blockhashes, newest last: one per executed slot, derived from the state.
    recent: VecDeque<(Hash, u64)>,
    allowed_programs: BTreeSet<Pubkey>,
    fees: Fees,
    track_capacity_cu: u64,
}

impl SvmExecutor {
    pub fn checkpoint(&self) -> ExecutorCheckpoint {
        ExecutorCheckpoint {
            accounts: self.accounts.clone(),
            account_hashes: self.account_hashes.clone(),
            accumulator: self.accumulator,
            executed: self.executed.clone(),
            recent: self.recent.clone(),
            inclusion_used: self.inclusion_used.clone(),
            fees: self.fees.clone(),
        }
    }

    pub fn snapshot(&self, checkpoint: &ExecutorCheckpoint) -> ExecutorSnapshot {
        ExecutorSnapshot {
            accounts: checkpoint.accounts.iter().filter(|(_, a)| !a.executable).map(|(k, a)| (k.to_string(), a.as_ref().clone())).collect(),
            executed: checkpoint.executed.iter().map(|(k, v)| (k.clone(), *v)).collect(),
            recent: checkpoint.recent.iter().map(|(h, s)| (h.to_string(), *s)).collect(),
            inclusion_used: checkpoint.inclusion_used.iter().map(|((w, p, q), v)| (*w, p.to_string(), *q, *v)).collect(),
            fees: checkpoint.fees.clone(),
            track_capacity_cu: self.track_capacity_cu,
            state_hash: hex::encode(checkpoint.accumulator),
        }
    }

    /// Replaces this executor's state with a snapshot's; the result's state hash is the snapshot's
    /// when every account came across intact.
    pub fn load(&mut self, snapshot: ExecutorSnapshot) -> Result<()> {
        let keep = snapshot.accounts.iter().map(|(k, _)| Pubkey::from_str(k)).collect::<Result<BTreeSet<_>, _>>()?;
        for key in self.accounts.iter().filter(|(k, a)| !a.executable && !keep.contains(k)).map(|(k, _)| *k).collect::<Vec<_>>() {
            self.put(key, Account { lamports: 0, data: vec![], owner: system_program::id(), executable: false, rent_epoch: 0 })?;
        }
        for (key, account) in snapshot.accounts {
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
        let svm = LiteSVM::default()
            .with_feature_set(solana_sdk::feature_set::FeatureSet::all_enabled())
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
            proposers: BTreeMap::new(),
            inclusion_used: BTreeMap::new(),
            executed: BTreeMap::new(),
            recent: VecDeque::new(),
            allowed_programs: BTreeSet::from([system_program::id()]),
            fees: Fees::default(),
            track_capacity_cu: selection::TRACK_CAPACITY_CU,
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
            if !self.allowed_programs.contains(program) {
                bail!("Program is not loaded in the Acrux genesis: {program}");
            }
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

    pub fn execute_slot(&mut self, slot: u64, items: Vec<ExecutionItem>) -> Result<SlotExecution> {
        self.set_slot_clock(slot)?;
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
            let tx: VersionedTransaction = bincode::deserialize(&item.transaction.bytes)?;
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
                        for (key, account) in info.post_accounts {
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
                },
            };
            self.remember_executed(candidate.id.clone(), &item.transaction.bytes);
            results.push(outcome);
        }
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
        self.svm.set_account(key, account)?;
        self.refresh(key);
        Ok(())
    }
    fn refresh(&mut self, key: Pubkey) {
        if let Some(account) = self.svm.get_account(&key) {
            let mut digest = Sha256::new();
            digest.update(b"acrux-account-v1");
            digest.update(key.as_ref());
            digest.update(account.lamports.to_le_bytes());
            digest.update(account.owner.as_ref());
            digest.update([account.executable as u8]);
            digest.update(account.rent_epoch.to_le_bytes());
            digest.update((account.data.len() as u64).to_le_bytes());
            digest.update(&account.data);
            let hash: [u8; 32] = digest.finalize().into();
            if self.account_hashes.get(&key) == Some(&hash) {
                return;
            }
            if let Some(old) = self.account_hashes.remove(&key) {
                subtract256(&mut self.accumulator, &old);
            }
            add256(&mut self.accumulator, &hash);
            self.account_hashes.insert(key, hash);
            self.accounts.insert(key, Arc::new(account));
        } else {
            if let Some(old) = self.account_hashes.remove(&key) {
                subtract256(&mut self.accumulator, &old);
            }
            self.accounts.remove(&key);
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

pub fn priority_fee(micro_lamports_per_cu: u64, compute_limit: u64) -> Result<u64> {
    u64::try_from(
        (u128::from(micro_lamports_per_cu) * u128::from(compute_limit) + 999_999) / 1_000_000,
    )
    .map_err(|_| anyhow!("Priority fee overflow"))
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
