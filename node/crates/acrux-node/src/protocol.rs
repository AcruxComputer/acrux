//! Algorithms 1 and 2: each actor owns its clock, signer, SVM, mempool and Votor.
use crate::{
    consensus::{self, Action, BlockRef, Votor},
    crypto::{self, Shred, ShredMeta},
    execution::{ExecutionItem, SvmExecutor, ValidatedTx},
    gateway::{GatewayCommand, GatewayState},
    schedule,
    storage::Store,
    transport::{LocalTransport, Peer, Received},
    types::*,
    view,
};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use bincode::Options;
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, RwLock};

/// Two minutes of 50 ms slots for followers catching up over an ordinary remote connection.
const FOLLOWER_SLOTS: u64 = 2_400;

#[derive(Default)]
struct LocalSlot {
    start: Option<Instant>,
    own_commit: Option<Commitment>,
    key: Option<[u8; 32]>,
    received: BTreeMap<usize, (Instant, Commitment, Vec<u8>)>,
    attest: BTreeMap<usize, (Attestation, Vec<u8>)>,
    shreds: BTreeMap<(usize, u32), Vec<Shred>>,
    cipher: BTreeMap<(usize, u32), Vec<u8>>,
    meta: BTreeMap<(usize, u32), ShredMeta>,
    payload: BTreeMap<usize, Vec<ValidatedTx>>,
    equivocation: bool,
    invalid_reason: Option<String>,
    executed: bool,
    agreed: Option<Vec<usize>>,
    state_hash: Option<String>,
}
impl LocalSlot {
    /// Retained block allocations, including shred/transaction structs in vectors. Map nodes,
    /// allocator metadata and other protocol state are excluded; process RSS is measured outside.
    fn block_buffer_bytes(&self) -> usize {
        let shreds = self.shreds.values().map(|shreds| shreds.capacity() * std::mem::size_of::<Shred>() + shreds.iter().map(|s| {
            s.data.capacity() + s.signature.capacity() + s.proof.capacity() * 32
        }).sum::<usize>()).sum::<usize>();
        let payload = self.payload.values().map(|txs| txs.capacity() * std::mem::size_of::<ValidatedTx>() + txs.iter().map(|tx| {
            let h = &tx.header;
            tx.bytes.capacity() + h.id.capacity() + h.fee_payer.capacity() + h.priority_fee_per_cu.capacity()
                + h.writable_accounts.capacity() * std::mem::size_of::<String>() + h.writable_accounts.iter().map(String::capacity).sum::<usize>()
        }).sum::<usize>()).sum::<usize>();
        shreds + payload + self.cipher.values().map(Vec::capacity).sum::<usize>()
            + self.meta.len() * std::mem::size_of::<ShredMeta>()
            + self.attest.values().map(|(a, sig)| a.pi.capacity() * std::mem::size_of::<Option<[u8; 32]>>() + sig.capacity()).sum::<usize>()
    }
}
#[derive(Clone, Copy)]
struct WindowRun {
    start: Instant,
    parent: Option<BlockRef>,
    next_slot: usize,
    skipped_from: Option<u64>,
}
struct EarlyBlock {
    key: [u8; 32],
    roots: Vec<[u8; 32]>,
    next_slice: u32,
}
struct Node {
    id: usize,
    key: SigningKey,
    executor: Option<SvmExecutor>,
    votor: Votor,
    mempool: BTreeMap<String, ValidatedTx>,
    /// Transactions this node carried, by id: the slot, its role and the copy, until they execute.
    in_flight: BTreeMap<String, (u64, usize, ValidatedTx)>,
    fragment_mempool: BTreeMap<(String, u8), crate::threshold::VerifiedFragment>,
    early: BTreeMap<u64, EarlyBlock>,
    threshold: crate::threshold::ThresholdCache,
    threshold_checkpoints: BTreeMap<u64, crate::threshold::ThresholdCache>,
    slots: BTreeMap<u64, LocalSlot>,
    windows: BTreeMap<u64, WindowRun>,
    votes: BTreeMap<Vec<u8>, SignedVote>,
    seen_certificates: BTreeSet<(u64, consensus::CertificateKind, Option<[u8; 32]>)>,
    voted: BTreeSet<u64>,
    last_executed: Option<u64>,
    executed_blocks: BTreeMap<u64, BlockRef>,
    checkpoints: BTreeMap<u64, crate::execution::ExecutorCheckpoint>,
    genesis: crate::execution::ExecutorCheckpoint,
    pending_starts: Vec<(u64, Option<BlockRef>)>,
}
#[derive(Default)]
struct NodeSet(Vec<Option<Node>>);
impl NodeSet {
    fn iter(&self) -> impl Iterator<Item = &Node> {
        self.0.iter().filter_map(Option::as_ref)
    }
    fn len(&self) -> usize {
        self.0.iter().filter(|n| n.is_some()).count()
    }
}
impl std::ops::Index<usize> for NodeSet {
    type Output = Node;
    fn index(&self, index: usize) -> &Node {
        self.0[index].as_ref().expect("actor owns node")
    }
}
impl std::ops::IndexMut<usize> for NodeSet {
    fn index_mut(&mut self, index: usize) -> &mut Node {
        self.0[index].as_mut().expect("actor owns node")
    }
}
struct Outbox {
    from: usize,
    to: Vec<usize>,
    body: Wire,
}
type Telemetry = crate::observer::Event;
struct Step {
    out: Vec<Outbox>,
    events: Vec<Telemetry>,
    persist: Vec<(String, Value)>,
}
impl Step {
    fn new() -> Self {
        Self {
            out: vec![],
            events: vec![],
            persist: vec![],
        }
    }
    fn event(&mut self, slot: u64, at: u64, kind: &'static str, data: Value) {
        self.events.push(Telemetry {
            slot: Some(slot),
            anchor: "slot",
            at,
            kind,
            data,
        })
    }
}
/// Public schedule inputs accompany follower snapshots; weights themselves come only from
/// finalized, hashed stake snapshots. Consensus certificate voting keeps its genesis weights.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProposerConfig {
    pub seed: String,
    pub windows_per_epoch: u64,
    pub identities: Vec<String>,
    pub hubs: Vec<schedule::Hub>,
    pub epoch_slots: u64,
    pub stake_from_slot: Option<u64>,
    pub genesis_stake_lamports: Option<u64>,
}

pub(crate) struct ProposerSchedules {
    pub config: ProposerConfig,
    legacy: schedule::Schedule,
    epochs: BTreeMap<u64, (Vec<u64>, schedule::Schedule)>,
}
impl ProposerSchedules {
    pub fn new(config: ProposerConfig) -> Result<Self> {
        ensure!(config.windows_per_epoch > 0 && config.epoch_slots > 0 && (config.stake_from_slot.is_none() || config.epoch_slots % 16 == 0), "Invalid proposer schedule lengths");
        let legacy = schedule::build_schedule(0, &config.seed, &config.hubs, config.identities.len() as u64 * 100_000_000, config.windows_per_epoch)?;
        let mut this = Self { config, legacy, epochs: BTreeMap::new() };
        if let (Some(slot), Some(weight)) = (this.config.stake_from_slot, this.config.genesis_stake_lamports) {
            let epoch = slot / this.config.epoch_slots;
            let weights = vec![weight; this.config.identities.len()];
            this.learn(epoch, weights.clone())?;
            this.learn(epoch + 1, weights)?;
        }
        Ok(this)
    }
    pub fn learn(&mut self, epoch: u64, weights: Vec<u64>) -> Result<()> {
        ensure!(weights.len() == self.config.identities.len(), "Stake snapshot has another validator count");
        if let Some((known, _)) = self.epochs.get(&epoch) {
            ensure!(known == &weights, "Finalized stake snapshots disagree for epoch {epoch}");
            return Ok(());
        }
        let total = weights.iter().try_fold(0u64, |sum, weight| sum.checked_add(*weight)).context("Proposer stake overflow")?;
        let mut hubs = self.config.hubs.clone();
        for member in hubs.iter_mut().flat_map(|hub| hub.members.iter_mut()) {
            let index = self.config.identities.iter().position(|identity| identity == &member.identity).context("Unknown proposer identity")?;
            member.stake = weights[index];
        }
        let drawn = schedule::build_schedule(epoch, &self.config.seed, &hubs, total, self.config.windows_per_epoch)?;
        self.epochs.insert(epoch, (weights, drawn));
        Ok(())
    }
    fn active(&self, window: u64) -> bool {
        self.config.stake_from_slot.is_some_and(|slot| window.saturating_mul(16) >= slot)
    }
    pub fn ready(&self, window: u64) -> bool {
        !self.active(window) || self.epochs.contains_key(&(window * 16 / self.config.epoch_slots))
    }
    pub fn window(&self, number: u64) -> Result<schedule::Window> {
        let schedule = if self.active(number) {
            &self.epochs.get(&(number * 16 / self.config.epoch_slots)).context("The proposer epoch's stake snapshot is not finalized yet")?.1
        } else { &self.legacy };
        let mut window = schedule.windows[number as usize % schedule.windows.len()].clone();
        window.number = number;
        Ok(window)
    }
    pub fn weights(&self, number: u64) -> Result<Vec<u64>> {
        if self.active(number) { Ok(self.epochs.get(&(number * 16 / self.config.epoch_slots)).context("The proposer epoch's stake snapshot is not finalized yet")?.0.clone()) }
        else { Ok(vec![100_000_000; self.config.identities.len()]) }
    }
    pub fn ids(&self, number: u64) -> Result<Vec<usize>> {
        self.window(number)?.proposers.iter().map(|identity| self.config.identities.iter().position(|known| known == identity).context("Unknown drawn proposer")).collect()
    }
    fn epoch(&self, epoch: u64) -> Result<Value> {
        let (weights, schedule) = if self.config.stake_from_slot.is_some_and(|slot| epoch >= slot / self.config.epoch_slots) {
            let (weights, schedule) = self.epochs.get(&epoch).context("The proposer epoch's stake snapshot is not finalized yet")?;
            (weights.clone(), schedule)
        } else { (vec![100_000_000; self.config.identities.len()], &self.legacy) };
        Ok(json!({"epoch":epoch,"weights":self.config.identities.iter().zip(weights).map(|(identity,stake)| json!({"identity":identity,"stake":stake.to_string()})).collect::<Vec<_>>(),"schedule":schedule}))
    }
}

#[cfg(test)]
mod proposer_stake_tests {
    use super::*;
    use solana_sdk::{account::Account, pubkey::Pubkey, stake::state::{Delegation, Meta, Stake, StakeStateV2}};

    fn config() -> ProposerConfig {
        let identities = (0..12).map(|index| Pubkey::new_from_array([index + 1;32]).to_string()).collect::<Vec<_>>();
        let hubs = ["FRA","NYC","TYO"].into_iter().enumerate().map(|(hub,code)| schedule::Hub {
            code:code.into(),region:code.into(),members:identities[hub * 4..hub * 4 + 4].iter().map(|identity| schedule::Member { identity:identity.clone(),stake:100_000_000 }).collect(),
        }).collect();
        ProposerConfig { seed:"9".into(),windows_per_epoch:96,identities,hubs,epoch_slots:256,stake_from_slot:Some(512),genesis_stake_lamports:Some(100_000_000) }
    }
    fn delegation(executor: &mut SvmExecutor, config: &ProposerConfig, deactivation_epoch: u64) {
        let vote = crate::execution::genesis_vote_address(&config.identities[0].parse().unwrap());
        let state = StakeStateV2::Stake(Meta::default(),Stake { delegation:Delegation {
            voter_pubkey:vote,stake:1_000_000_000,activation_epoch:2,deactivation_epoch,..Delegation::default()
        },credits_observed:0 },Default::default());
        let mut data = bincode::serialize(&state).unwrap();
        data.resize(StakeStateV2::size_of(),0);
        executor.set_account(&Pubkey::new_from_array([99;32]).to_string(),Account { lamports:1_000_000_000,data,owner:solana_sdk::stake::program::id(),executable:false,rent_epoch:0 }).unwrap();
    }
    fn executor(config: &ProposerConfig) -> SvmExecutor {
        let mut executor = SvmExecutor::new(Vec::<(String,u64)>::new(),&[]).unwrap();
        executor.set_stake_config(crate::execution::StakeConfig {
            from_slot:512,epoch_slots:256,genesis_stake_lamports:100_000_000,identities:config.identities.iter().map(|identity| identity.parse().unwrap()).collect(),
        });
        executor
    }
    #[test]
    fn delegated_stake_changes_epoch_e_plus_two_proposer_draw() {
        let config = config();
        let mut executor = executor(&config);
        let mut schedules = ProposerSchedules::new(config).unwrap();
        let current = serde_json::to_vec(&schedules.epoch(2).unwrap()).unwrap();
        delegation(&mut executor,&schedules.config,u64::MAX);
        assert!(!schedules.ready(4 * 256 / 16));
        schedules.learn(4,executor.stake_weights(4).unwrap()).unwrap();
        assert_eq!(serde_json::to_vec(&schedules.epoch(2).unwrap()).unwrap(),current,"delegation cannot rewrite the current or already-frozen next epoch");
        assert_eq!(executor.stake_weights(2).unwrap()[0],100_000_000,"same-epoch activation is excluded");
        let baseline = schedules.epochs.get(&3).unwrap().1.shares.iter().find(|hub| hub.hub_code == "FRA").unwrap().proposer_stake;
        let weighted = schedules.epochs.get(&4).unwrap().1.shares.iter().find(|hub| hub.hub_code == "FRA").unwrap().proposer_stake;
        assert!(weighted > baseline,"Algorithm3 must cap and draw from the delegated weights");
        assert_eq!(schedules.weights(4 * 256 / 16).unwrap()[0],1_100_000_000);
    }
    #[test]
    fn deactivated_stake_leaves_next_snapshot_without_rewriting_the_current_draw() {
        let config = config();
        let mut executor = executor(&config);
        let mut schedules = ProposerSchedules::new(config).unwrap();
        delegation(&mut executor,&schedules.config,u64::MAX);
        schedules.learn(4,executor.stake_weights(4).unwrap()).unwrap();
        let frozen = serde_json::to_vec(&schedules.epoch(4).unwrap()).unwrap();
        delegation(&mut executor,&schedules.config,4);
        assert_eq!(executor.stake_weights(4).unwrap()[0],1_100_000_000,"deactivation remains active in its own epoch");
        schedules.learn(5,executor.stake_weights(5).unwrap()).unwrap();
        assert_eq!(schedules.weights(5 * 256 / 16).unwrap(),vec![100_000_000;12]);
        assert_eq!(serde_json::to_vec(&schedules.epoch(4).unwrap()).unwrap(),frozen);
    }
    #[test]
    fn pre_stake_draw_is_byte_identical_to_the_legacy_schedule() {
        let mut config = config();
        config.stake_from_slot = Some(4096);
        let expected = schedule::build_schedule(0,&config.seed,&config.hubs,1_200_000_000,96).unwrap();
        let configured = ProposerSchedules::new(config.clone()).unwrap();
        config.stake_from_slot = None;
        config.genesis_stake_lamports = None;
        let disabled = ProposerSchedules::new(config).unwrap();
        for number in 0..192 {
            let mut legacy = expected.windows[number as usize % 96].clone();
            legacy.number = number;
            assert_eq!(bincode::serialize(&configured.window(number).unwrap()).unwrap(),bincode::serialize(&legacy).unwrap());
            assert_eq!(bincode::serialize(&disabled.window(number).unwrap()).unwrap(),bincode::serialize(&legacy).unwrap());
        }
    }
}

pub struct Network {
    config: Configuration,
    profile: Arc<crate::profiling::Profiling>,
    keys: Vec<SigningKey>,
    identities: Vec<String>,
    stakes: Vec<u64>,
    proposers: Arc<std::sync::RwLock<ProposerSchedules>>,
    nodes: NodeSet,
    transport: Arc<LocalTransport>,
    exchange: Arc<crate::exchange::ExchangeEngine>,
    incoming: mpsc::Receiver<Received>,
    gateway: Arc<RwLock<GatewayState>>,
    commands: mpsc::Receiver<GatewayCommand>,
    store: Store,
    persistence: mpsc::Sender<(String, Value)>,
    observer: crate::observer::Observer,
    origin: Instant,
    wall: u64,
    samples: Vec<u64>,
    final_times: Vec<u64>,
    finalized: BTreeSet<u64>,
    skipped: BTreeSet<u64>,
    slot_views: BTreeMap<u64, Value>,
    senders: Vec<Keypair>,
    traffic_seq: u64,
    received_packets: u64,
    last_slot_started: Option<Instant>,
    maximum_execution_skew: u64,
    within_window_samples: Vec<u64>,
    handover_samples: Vec<u64>,
    active: Vec<usize>,
    admissions: BTreeMap<String, [u8; 32]>,
    /// Admission keys by target slot, dropped once their blockhash can no longer be valid.
    admission_slots: BTreeMap<u64, Vec<String>>,
    progress: Arc<std::sync::Mutex<Progress>>,
    observed_final: Arc<std::sync::Mutex<BTreeSet<u64>>>,
    /// Finalization events waiting for their cblock's state to reach the disk (`state::durable`).
    held_finals: Vec<Telemetry>,
    /// Validator 0's cblock hash preimages (all but the outbox root) and outboxes, for the proofs.
    cblock_preimages: BTreeMap<u64, (Vec<u8>, Vec<crate::bridge::OutboxEntry>)>,
    bridge_proofs: Option<crate::bridge::ProofArchive>,
    bridge_prunes: crate::bridge::ProofPrunes,
    /// Prepared and archived before the state checkpoint is saved, published after it is durable.
    pending_bridge_proofs: BTreeMap<u64, (Value, bool)>,
    /// The finalized frontier whose ancestor proofs are prepared and whose checkpoint is queued.
    proof_prepared_through: Option<u64>,
    /// Finalized Solana bridge deposits by sequence, as the deposit reader found them.
    bridge_deposits: Arc<std::sync::Mutex<BTreeMap<u64, crate::execution::BridgeCredit>>>,
    /// Solana's `getRecentPerformanceSamples`: one sample a minute of the slots and transactions
    /// validator 0 executed, newest last, at most 720 (twelve hours).
    performance: VecDeque<Value>,
    performance_mark: (Instant, u64, u64),
    executed_count: (u64, u64),
    observed_skip: Arc<std::sync::Mutex<BTreeSet<u64>>>,
    /// Per window: when each node voted for the previous window's last cblock and emitted ParentReady.
    handover_trace: Arc<std::sync::Mutex<BTreeMap<u64, Vec<(usize, &'static str, Instant)>>>>,
    state_checks: Arc<std::sync::Mutex<StateChecks>>,
    io_handle: tokio::runtime::Handle,
    /// Attestation arrivals at validators, drained into the facts once a second.
    attestation_arrivals: Arc<std::sync::Mutex<Vec<(u64, Instant)>>>,
    /// Node zero's actor keeps the rolling facts the gateway serves.
    facts: Option<Box<crate::facts::Facts>>,
    facts_cursor: FactsCursor,
    genesis: Option<Arc<crate::genesis::Genesis>>,
    /// Fault Lab: evidence gathered while the scenario runs, and the transaction it marks.
    lab_evidence: Arc<std::sync::Mutex<Vec<Value>>>,
    lab_marked: Arc<std::sync::Mutex<Option<String>>>,
    lab_injected: bool,
    /// Solana JSON-RPC: transaction signatures sent through `sendTransaction`, to their ids.
    signature_ids: BTreeMap<String, String>,
    /// Solana JSON-RPC: every executed transaction by signature, and the signatures by address.
    solana: crate::solana::SolanaIndex,
    /// Solana JSON-RPC: what each `sendTransaction` was submitted with, until it executes.
    solana_sent: BTreeMap<String, Value>,
    /// Validator 0's actor writes each finalized state through this, when the net persists.
    state_writer: Option<std::sync::mpsc::Sender<crate::state::Pending>>,
    counter_base: (u64, u64),
    genesis_hash: String,
    /// The kept transactions, and validator 0's actor's writer to them: from `keep_from` on, the
    /// slots of the finalized chain not yet looked at; `built_in` the fee payers never kept.
    kept: Option<Arc<crate::history::History>>,
    kept_writer: Option<std::sync::mpsc::Sender<Vec<(String, crate::history::Kept)>>>,
    keep_from: u64,
    built_in: BTreeSet<String>,
}
#[derive(Default, Clone, Copy)]
struct FactsCursor {
    commit_slot: u64,
    spread_index: usize,
    final_index: usize,
    updated: Option<Instant>,
}
/// What proposer `q` carries in slot `s`: the transactions and fragments due by `s` whose blockhash
/// the chain still holds and whose payer can pay this copy's inclusion fee; the rest are dropped,
/// so no honest block carries what validators would not charge. Each carried transaction stays in
/// flight until it executes, so one lost with a skipped slot goes out again.
/// How far ahead of the latest started slot a send made at the gateway (`sendTransaction`, the
/// faucet) is aimed. The gateway sits beside the proposers, so the next slot is in reach; a copy
/// that misses its slot's block is carried by the next, as any due transaction is.
const SEND_LEAD_SLOTS: u64 = 1;
/// What a faucet grant's payer needs beyond the grant: fees, two token accounts' rent, the reserve.
const FAUCET_MARGIN: u64 = 10_000_000;
/// What one built-in traffic transaction costs its sender, measured: the base fee, the inclusion fee
/// and its priority fee.
const TRAFFIC_TX_LAMPORTS: u64 = 8_020;
/// Days of its own traffic a sender keeps when it pays faucet grants.
const SENDER_RESERVE_DAYS: u64 = 30;
/// What a validator's identity started with; the fees it earned above it can pay faucet grants.
const VALIDATOR_START_LAMPORTS: u64 = 10_000_000_000;

/// A block carries at most this many bytes of transactions and fragments; what is due beyond it
/// stays in the mempool for the next slots. Kept well under the 1 MiB block and network message
/// limits, with room for the window handover that carries four slots of shreds.
pub const BLOCK_BYTES: u64 = 64 * 1024;

/// The due items that fit `budget`, highest priority fee per CU first, then by key.
fn within_budget<K: Ord + Clone>(mut due: Vec<(K, u128, u64)>, budget: u64) -> Vec<K> {
    due.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut used = 0;
    due.into_iter()
        .filter(|(_, _, size)| {
            let fits = used + size <= budget;
            if fits {
                used += size;
            }
            fits
        })
        .map(|(key, _, _)| key)
        .collect()
}

/// Splits a handover's shreds into parts of at most `HANDOVER_PART_BYTES` each; always at least one.
const HANDOVER_PART_BYTES: u64 = 768 * 1024;
fn handover_parts(shreds: Vec<Shred>) -> Vec<Vec<Shred>> {
    let mut parts = vec![vec![]];
    let mut used = 0;
    for shred in shreds {
        let size = bincode::serialized_size(&shred).unwrap_or(0);
        if used + size > HANDOVER_PART_BYTES && !parts.last().unwrap().is_empty() {
            parts.push(vec![]);
            used = 0;
        }
        used += size;
        parts.last_mut().unwrap().push(shred);
    }
    parts
}

fn pack(
    node: &mut Node,
    s: u64,
    q: usize,
    censored: Option<&String>,
) -> (Vec<ValidatedTx>, Vec<crate::threshold::VerifiedFragment>) {
    let executor = node.executor.as_ref().unwrap();
    let priority = |h: &crate::execution::TxHeader| h.priority_fee_per_cu.parse::<u128>().unwrap_or(0);
    let due = node
        .mempool
        .iter()
        .filter(|(id, tx)| tx.header.target_slot <= s && Some(*id) != censored)
        .map(|(id, tx)| (id.clone(), priority(&tx.header), bincode::serialized_size(tx).unwrap_or(u64::MAX)))
        .collect::<Vec<_>>();
    let due_fragments = node
        .fragment_mempool
        .iter()
        .filter(|(_, f)| f.header.target_slot <= s)
        .map(|(key, f)| (key.clone(), priority(&f.header), bincode::serialized_size(f).unwrap_or(u64::MAX)))
        .collect::<Vec<_>>();
    // Fragments first: a threshold transaction needs t of them together, so they take the budget's
    // first half and plain transactions the rest of it.
    let fragment_keys = within_budget(due_fragments, BLOCK_BYTES / 2);
    let used = fragment_keys
        .iter()
        .map(|key| bincode::serialized_size(&node.fragment_mempool[key]).unwrap_or(0))
        .sum::<u64>();
    let txs = within_budget(due, BLOCK_BYTES - used)
        .into_iter()
        .filter_map(|id| node.mempool.remove(&id))
        .filter(|tx| !executor.is_executed(&tx.header.id) && executor.blockhash_valid(&tx.bytes))
        .collect::<Vec<_>>();
    let fragments = fragment_keys
        .into_iter()
        .filter_map(|key| node.fragment_mempool.remove(&key))
        .collect::<Vec<_>>();
    let items = txs
        .iter()
        .map(|tx| (tx.header.fee_payer.clone(), q as u8, 1u64))
        .chain(fragments.iter().map(|f| (f.header.fee_payer.clone(), q as u8, 1u64)))
        .collect::<Vec<_>>();
    let mut admitted = executor
        .admit_inclusions(&items, s / 16)
        .unwrap_or_else(|_| vec![false; items.len()])
        .into_iter();
    let txs = txs
        .into_iter()
        .filter(|_| admitted.next().unwrap())
        .collect::<Vec<_>>();
    let fragments = fragments
        .into_iter()
        .filter(|_| admitted.next().unwrap())
        .collect();
    for tx in &txs {
        node.in_flight.insert(tx.header.id.clone(), (s, q, tx.clone()));
    }
    (txs, fragments)
}
fn encode<T: serde::Serialize>(v: &T) -> Result<Vec<u8>> {
    Ok(bincode::serialize(v)?)
}
/// Finalization of `tip` also finalizes every ancestor back to the prepared frontier. Window
/// parents can skip cblocks, so following parents is required instead of iterating slot numbers.
fn finalized_ancestors(tip: u64, after: Option<u64>, mut parent: impl FnMut(u64) -> Result<Option<u64>>) -> Result<Vec<u64>> {
    let mut chain = vec![];
    let mut at = Some(tip);
    while let Some(c) = at.filter(|c| after.is_none_or(|done| *c > done)) {
        chain.push(c);
        at = parent(c)?;
        ensure!(at.is_none_or(|previous| previous < c), "Finalized bridge ancestry does not move backward");
    }
    chain.reverse();
    Ok(chain)
}
fn bridge_proof_quorum<'a>(votes: impl Iterator<Item = &'a SignedVote>, block: BlockRef, stakes: &[u64]) -> bool {
    let (mut notar, mut finalize) = (0u32, 0u32);
    for signed in votes.filter(|signed| signed.vote.slot == block.slot && signed.vote.hash == Some(block.hash)) {
        match signed.vote.kind {
            consensus::VoteKind::Notar => notar |= 1 << signed.vote.validator,
            consensus::VoteKind::Finalize => finalize |= 1 << signed.vote.validator,
            _ => {}
        }
    }
    let stake = |bits: u32| stakes.iter().enumerate().filter(|(i, _)| bits >> i & 1 == 1).map(|(_, stake)| *stake as u128).sum::<u128>();
    let total = stakes.iter().map(|stake| *stake as u128).sum::<u128>();
    stake(finalize) * 100 >= total * 60 || stake(notar) * 100 >= total * 80
}

#[cfg(test)]
mod bridge_finality_tests {
    use super::*;

    #[test]
    fn bridge_ancestor_withdrawal_needs_its_proof_before_a_descendant_checkpoint() {
        // Cblock 12 starts a new window on 7; 8..11 were skipped. A burn in 7 is in 14's state,
        // even when consensus announces only 14's finalization.
        let parents = BTreeMap::from([(7, Some(6)), (12, Some(7)), (13, Some(12)), (14, Some(13))]);
        let chain = finalized_ancestors(14, Some(6), |c| parents.get(&c).copied().context("missing ancestor")).unwrap();
        assert_eq!(chain, [7, 12, 13, 14]);
        let stakes = vec![1u64; 12];
        let block = |slot| BlockRef { slot, hash: [slot as u8; 32] };
        let signed = |slot, validator, kind| SignedVote {
            vote: consensus::Vote { validator, slot, hash: Some(block(slot).hash), kind }, signature: vec![],
        };
        let mut votes = vec![];
        for slot in [12, 13, 14] {
            votes.extend((0..8).map(|validator| signed(slot, validator, consensus::VoteKind::Finalize)));
        }
        // Eight Notar votes are enough to extend 7 in consensus, but not to certify it on Solana.
        votes.extend((0..8).map(|validator| signed(7, validator, consensus::VoteKind::Notar)));
        assert!(bridge_proof_quorum(votes.iter(), block(14), &stakes));
        assert!(!chain.iter().all(|c| bridge_proof_quorum(votes.iter(), block(*c), &stakes)));
        // Explicit Finalize votes on the known finalized ancestor make the entire chain relayable.
        votes.extend((0..8).map(|validator| signed(7, validator, consensus::VoteKind::Finalize)));
        assert!(chain.iter().all(|c| bridge_proof_quorum(votes.iter(), block(*c), &stakes)));
    }
}
fn decode<T: serde::de::DeserializeOwned>(v: &[u8]) -> Result<T> {
    Ok(bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(1024 * 1024)
        .reject_trailing_bytes()
        .deserialize(v)?)
}
fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_micros() as u64
}
fn offset(at: Instant, start: Instant) -> u64 {
    at.saturating_duration_since(start).as_micros() as u64
}
impl Network {
    pub async fn new(
        config: Configuration,
        gateway: Arc<RwLock<GatewayState>>,
        commands: mpsc::Receiver<GatewayCommand>,
    ) -> Result<Self> {
        let profile = crate::profiling::Profiling::new();
        let genesis = config
            .genesis_path
            .as_deref()
            .map(|path| crate::genesis::Genesis::load(Path::new(path)).map(Arc::new))
            .transpose()?;
        let bridge_rpc_urls = if genesis.as_ref().is_some_and(|genesis| genesis.bridge.is_some()) {
            crate::validate_bridge_identity(std::env::var("ACRUX_IDENTITY_SEED").ok().as_deref())?;
            let urls = std::env::var("ACRUX_BRIDGE_RPC").context("ACRUX_BRIDGE_RPC is required when a bridge rule is configured")?;
            Some(crate::bridge::deposit_rpcs(&urls)?)
        } else { None };
        let keys = (0..12).map(|i| crate::identity_key(i)).collect::<Vec<_>>();
        let identities = keys
            .iter()
            .map(|k| bs58::encode(k.verifying_key().as_bytes()).into_string())
            .collect::<Vec<_>>();
        let stakes = vec![100_000_000u64; 12];
        // The hubs, their places and their members come from the chain's own Hub Registry in genesis.
        let registry = match &genesis {
            Some(genesis) => genesis.registry_hubs()?,
            None => vec![],
        };
        let registry = if registry.is_empty() {
            [("FRA", 50.0379, 8.5622), ("NYC", 40.6413, -73.7781), ("TYO", 35.5494, 139.7798)]
                .iter()
                .enumerate()
                .map(|(i, (code, latitude, longitude))| crate::genesis::RegistryHub { code: (*code).into(), latitude: *latitude, longitude: *longitude, members: identities[i * 4..i * 4 + 4].to_vec() })
                .collect()
        } else {
            registry
        };
        let hub_of = |identity: &str| registry.iter().find(|h| h.members.iter().any(|m| m == identity));
        let peers = (0..12)
            .map(|id| {
                let hub = hub_of(&identities[id]).with_context(|| format!("Validator {} is in no hub of the registry", identities[id]))?;
                Ok(Peer { id, hub: hub.code.clone(), latitude: hub.latitude, longitude: hub.longitude, stake: stakes[id] })
            })
            .collect::<Result<Vec<_>>>()?;
        let hubs = registry
            .iter()
            .map(|h| schedule::Hub {
                code: h.code.clone(),
                region: crate::genesis::region_of(h.latitude, h.longitude).into(),
                members: h
                    .members
                    .iter()
                    .filter_map(|m| identities.iter().position(|i| i == m))
                    .map(|j| schedule::Member { identity: identities[j].clone(), stake: stakes[j] })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let stake_rule = genesis.as_ref().and_then(|g| g.bridge.as_ref()).map(|bridge| bridge.stake_activation()).transpose()?.flatten();
        let proposer_config = ProposerConfig {
            seed: config.seed.to_string(), windows_per_epoch: config.windows_per_epoch as u64,
            identities: identities.clone(), hubs: hubs.clone(),
            epoch_slots: genesis.as_ref().and_then(|g| g.bridge.as_ref()).map_or(432_000, |bridge| bridge.epoch_slots),
            stake_from_slot: stake_rule.map(|rule| rule.0), genesis_stake_lamports: stake_rule.map(|rule| rule.1),
        };
        // A net that persists continues from its saved state, when it has one: the last finalized
        // cblock, its accounts, the executed ids and recent blockhashes, and the traffic's keys.
        let state_store = if config.persist {
            Some(crate::state::StateStore::open(&Path::new(&config.store_path).join("state.redb"))?)
        } else {
            None
        };
        let saved = match &state_store {
            Some(store) => store.load()?,
            None => None,
        };
        let bridge_proof_path = Path::new(&config.store_path).join("bridge-proofs.redb");
        let bridge_proofs = genesis.as_ref().and_then(|g| g.bridge.as_ref())
            .map(|bridge| crate::bridge::ProofArchive::open(config.persist.then_some(bridge_proof_path.as_path()), bridge.epoch_slots, saved.as_ref().map(|state| state.meta.cblock)))
            .transpose()?;
        if let Some(archive) = &bridge_proofs {
            let mut state = gateway.write().await;
            state.bridge_archive = Some(archive.store.clone());
            state.archived_withdrawals = archive.withdrawals.clone();
            state.epoch_cblock_proofs = archive.epochs.clone();
            state.bridge_program = genesis.as_ref().and_then(|g| g.bridge.as_ref()).map(|rule| rule.program.to_string());
            state.bridge_epoch_slots = genesis.as_ref().and_then(|g| g.bridge.as_ref()).map(|rule| rule.epoch_slots);
            state.fee_burn_from_slot = genesis.as_ref().and_then(|g| g.bridge.as_ref()).and_then(|rule| rule.fee_burn_from_slot);
        }
        let resumed = match &saved {
            Some(saved) => Some(BlockRef {
                slot: saved.meta.cblock,
                hash: hex::decode(&saved.meta.hash)?.try_into().map_err(|_| anyhow::anyhow!("saved cblock hash"))?,
            }),
            None => None,
        };
        let senders = match &saved {
            Some(saved) => saved.meta.senders.iter().map(|k| Keypair::from_base58_string(k)).collect::<Vec<_>>(),
            None => (0..24).map(|_| Keypair::new()).collect::<Vec<_>>(),
        };
        let balances = identities
            .iter()
            .cloned()
            .map(|s| (s, 10_000_000_000u64))
            .chain(
                senders
                    .iter()
                    .map(|s| (s.pubkey().to_string(), 100_000_000_000u64)),
            )
            .collect::<Vec<_>>();
        let (transport, incoming) = LocalTransport::bind(
            peers.clone(),
            config.base_port,
            config.seed,
            profile.clone(),
        )
        .await?;
        let exchange = crate::exchange::ExchangeEngine::new(
            transport.clone(),
            keys.clone(),
            gateway.clone(),
            profile.clone(),
            config.fault.clone(),
        );
        let programs = config
            .genesis_programs
            .iter()
            .map(|entry| {
                let (address, path) = entry
                    .split_once('=')
                    .context("Use --program ADDRESS=path")?;
                Ok(crate::execution::GenesisProgram {
                    address: address.into(),
                    path: path.into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut programs = programs;
        if let Some(genesis) = &genesis {
            programs.extend(genesis.programs.iter().map(|(address, path)| crate::execution::GenesisProgram { address: address.clone(), path: path.clone() }));
        }
        let mut nodes = vec![];
        for id in 0..12 {
            let mut votor = Votor::new(id, stakes.clone())?;
            let executor = match (&saved, resumed) {
                (Some(saved), Some(block)) => {
                    votor.resume(block);
                    let mut executor = SvmExecutor::from_state(&programs, saved.state.clone())?;
                    executor.set_placements(genesis.iter().flat_map(|g| g.placements.clone()).collect());
                    if let Some(bridge) = genesis.as_ref().and_then(|g| g.bridge.as_ref()) {
                        executor.set_bridge(bridge.program);
                        executor.set_bridge_minimums(bridge.from_slot, bridge.min_withdrawal.iter().map(|(mint, amount)| crate::execution::MinimumWithdrawal { mint: *mint, amount: *amount }).collect());
                        if let Some(native) = bridge.native_cutover()? { executor.set_native_cutover(native); }
                        if let Some(fee_burn) = bridge.fee_burn_activation()? { executor.set_fee_burn_config(fee_burn); }
                        if let Some((from_slot, genesis_stake_lamports)) = bridge.stake_activation()? {
                            executor.set_stake_config(crate::execution::StakeConfig { from_slot, epoch_slots: bridge.epoch_slots, genesis_stake_lamports, identities: identities.iter().map(|identity| identity.parse()).collect::<std::result::Result<Vec<_>, _>>()? });
                        }
                    }
                    anyhow::ensure!(executor.state_hash() == saved.state.state_hash, "the saved state at cblock {} hashes to {}, not to its saved hash {}", block.slot, executor.state_hash(), saved.state.state_hash);
                    executor
                }
                _ => {
                    votor.genesis();
                    let mut executor = SvmExecutor::new(balances.clone(), &programs)?;
                    executor.set_placements(genesis.iter().flat_map(|g| g.placements.clone()).collect());
                    if let Some(bridge) = genesis.as_ref().and_then(|g| g.bridge.as_ref()) {
                        executor.set_bridge(bridge.program);
                        executor.set_bridge_minimums(bridge.from_slot, bridge.min_withdrawal.iter().map(|(mint, amount)| crate::execution::MinimumWithdrawal { mint: *mint, amount: *amount }).collect());
                        if let Some(native) = bridge.native_cutover()? { executor.set_native_cutover(native); }
                        if let Some(fee_burn) = bridge.fee_burn_activation()? { executor.set_fee_burn_config(fee_burn); }
                        if let Some((from_slot, genesis_stake_lamports)) = bridge.stake_activation()? {
                            executor.set_stake_config(crate::execution::StakeConfig { from_slot, epoch_slots: bridge.epoch_slots, genesis_stake_lamports, identities: identities.iter().map(|identity| identity.parse()).collect::<std::result::Result<Vec<_>, _>>()? });
                        }
                    }
                    executor.set_fees(crate::execution::Fees {
                        reserve_lamports: config.reserve,
                        inclusion_lamports: config.inclusion_fee,
                        base_fee_per_signature: config.base_fee,
                        base_burn_bps: crate::execution::Fees::default().base_burn_bps,
                    })?;
                    executor.set_track_capacity(config.track_capacity_cu)?;
                    for (address, account) in genesis.iter().flat_map(|g| g.accounts.iter()) {
                        executor.set_account(address, account.clone())?;
                    }
                    executor
                }
            };
            let genesis = executor.checkpoint();
            nodes.push(Node {
                id,
                key: keys[id].clone(),
                executor: Some(executor),
                votor,
                mempool: BTreeMap::new(),
                in_flight: BTreeMap::new(),
                fragment_mempool: BTreeMap::new(),
                early: BTreeMap::new(),
                threshold: crate::threshold::ThresholdCache::new(config.fragment_expiry_slots)?,
                threshold_checkpoints: BTreeMap::new(),
                slots: BTreeMap::new(),
                windows: BTreeMap::new(),
                votes: BTreeMap::new(),
                seen_certificates: BTreeSet::new(),
                voted: BTreeSet::new(),
                last_executed: None,
                executed_blocks: BTreeMap::new(),
                checkpoints: BTreeMap::new(),
                genesis,
                pending_starts: vec![(0, None)],
            });
            if let (Some(saved), Some(block)) = (&saved, resumed) {
                let node = nodes.last_mut().unwrap();
                node.checkpoints.insert(block.slot, node.genesis.clone());
                node.threshold_checkpoints.insert(block.slot, saved.meta.threshold.clone());
                node.threshold = saved.meta.threshold.clone();
                node.executed_blocks.insert(block.slot, block);
                node.last_executed = Some(block.slot * 4 + 3);
                node.pending_starts = vec![(block.slot / 4 + 1, Some(block))];
            }
        }
        let genesis_hash = match &saved {
            Some(saved) => saved.meta.genesis_hash.clone(),
            None => nodes[0].genesis.hash(),
        };
        let counter_base = saved.as_ref().map_or((0, 0), |s| (s.meta.finalized_cblocks, s.meta.skips));
        {
            let mut gateway = gateway.write().await;
            gateway.counter_base = counter_base;
            gateway.finalized_cblock = resumed.map(|block| block.slot);
        }
        if let Some(saved) = &saved {
            tracing::warn!(cblock = saved.meta.cblock, slot = saved.meta.cblock * 4 + 3, state = %saved.state.state_hash, "continuing the chain from its saved state");
        }
        let kept = if config.persist {
            Some(Arc::new(crate::history::History::open(&Path::new(&config.store_path).join("history.redb"), config.keep_days, config.keep_max)?))
        } else {
            None
        };
        gateway.write().await.kept = kept.clone();
        let mut proposer_schedules = ProposerSchedules::new(proposer_config)?;
        for (epoch, weights) in nodes[0].executor.as_ref().unwrap().stake_schedule_snapshots(&nodes[0].genesis)? { proposer_schedules.learn(epoch, weights)?; }
        let proposers = Arc::new(std::sync::RwLock::new(proposer_schedules));
        let kept_writer = kept.clone().map(crate::history::spawn_writer).transpose()?;
        let keep_from = resumed.map_or(0, |block| block.slot * 4 + 4);
        let built_in = senders
            .iter()
            .map(|k| k.pubkey().to_string())
            .chain(genesis.iter().map(|g| g.faucet.pubkey().to_string()))
            .collect::<BTreeSet<_>>();
        let state_writer = match state_store {
            Some(store) => Some(crate::state::spawn_writer(store, saved.as_ref().map(|_| nodes[0].genesis.clone()), resumed.map_or(0, |block| block.slot))?),
            None => None,
        };
        {
            let mut g = gateway.write().await;
            g.parameters = json!({"k":3,"kappa":4,"w":16,"deltaLocalUs":10000,"deltaBlockUs":50000,"deltaGlobalUs":250000,"timeoutUs":750000,"tracks":4,"reserveLamports":config.reserve.to_string(),"trackCapacityCu":config.track_capacity_cu,"fragmentExpirySlots":config.fragment_expiry_slots,"alpha":0.5,"minimumHubMembers":4,"minimumHubStake":(stakes.iter().sum::<u64>()/100).to_string()});
        }
        std::fs::create_dir_all(&config.store_path)?;
        std::fs::write(
            Path::new(&config.store_path).join("genesis.json"),
            serde_json::to_vec_pretty(
                &json!({"config":config,"identities":identities,"stakes":stakes,"hubs":hubs}),
            )?,
        )?;
        let origin = Instant::now();
        let wall = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
        // Each start is a fresh genesis with its own store; the previous run's store is kept beside it.
        let mut stores = std::fs::read_dir(&config.store_path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("net-") && n.ends_with(".redb")))
            .collect::<Vec<_>>();
        stores.sort();
        for old in stores.iter().rev().skip(1) {
            std::fs::remove_file(old)?;
        }
        let store = Store::open(&Path::new(&config.store_path).join(format!("net-{wall}.redb")))?;
        gateway.write().await.store = Some(store.clone());
        let (persistence, writes) = mpsc::channel::<(String, Value)>(4096);
        let disk = store.clone();
        let retain = config.retain_slots;
        tokio::task::spawn_blocking(move || {
            if let Err(error) = disk.write_batches(writes, retain) {
                tracing::error!(%error,"storage writer failed");
            }
        });
        let observer = crate::observer::Observer::start(
            gateway.clone(), persistence.clone(), identities.clone(), exchange.clone(), profile.clone(),
        );
        let bridge_deposits = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let bridge_prunes = Arc::new(std::sync::Mutex::new(BTreeSet::new()));
        if let (Some(bridge), Some(urls)) = (genesis.as_ref().and_then(|g| g.bridge.clone()), bridge_rpc_urls) {
            tokio::spawn(crate::bridge::read_deposits(urls, bridge.program, bridge_deposits.clone(), gateway.clone(), bridge_prunes.clone()));
        }
        Ok(Self {
            config,
            profile,
            keys,
            identities,
            stakes,
            proposers,
            nodes: NodeSet(nodes.into_iter().map(Some).collect()),
            transport,
            exchange,
            incoming,
            gateway,
            commands,
            store,
            persistence,
            observer,
            origin,
            wall,
            samples: vec![],
            final_times: vec![],
            finalized: BTreeSet::new(),
            skipped: BTreeSet::new(),
            slot_views: BTreeMap::new(),
            senders,
            traffic_seq: 0,
            received_packets: 0,
            last_slot_started: None,
            maximum_execution_skew: 0,
            within_window_samples: vec![],
            handover_samples: vec![],
            active: (0..12).collect(),
            admissions: BTreeMap::new(),
            admission_slots: BTreeMap::new(),
            progress: Arc::new(std::sync::Mutex::new(Progress::default())),
            observed_final: Arc::new(std::sync::Mutex::new(BTreeSet::new())),
            held_finals: Vec::new(),
            cblock_preimages: BTreeMap::new(),
            bridge_proofs,
            bridge_prunes,
            pending_bridge_proofs: BTreeMap::new(),
            proof_prepared_through: resumed.map(|block| block.slot),
            bridge_deposits: bridge_deposits.clone(),
            performance: VecDeque::new(),
            performance_mark: (Instant::now(), 0, 0),
            executed_count: (0, 0),
            observed_skip: Arc::new(std::sync::Mutex::new(BTreeSet::new())),
            handover_trace: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            state_checks: Arc::new(std::sync::Mutex::new(StateChecks::default())),
            io_handle: tokio::runtime::Handle::current(),
            attestation_arrivals: Arc::new(std::sync::Mutex::new(Vec::new())),
            facts: None,
            facts_cursor: FactsCursor::default(),
            genesis,
            lab_evidence: Arc::new(std::sync::Mutex::new(Vec::new())),
            lab_marked: Arc::new(std::sync::Mutex::new(None)),
            lab_injected: false,
            signature_ids: BTreeMap::new(),
            solana: Default::default(),
            solana_sent: BTreeMap::new(),
            state_writer,
            counter_base,
            genesis_hash,
            kept,
            kept_writer,
            keep_from,
            built_in,
        })
    }
    fn window(&self, number: u64) -> Result<schedule::Window> { self.proposers.read().unwrap().window(number) }
    fn proposer_ids(&self, number: u64) -> Result<Vec<usize>> { self.proposers.read().unwrap().ids(number) }
    fn schedule_ready(&self, number: u64) -> bool { self.proposers.read().unwrap().ready(number) }
    /// Cblock `i`'s outbox: every cblock carries the next epoch's validator set, so any one of an
    /// epoch certified on Solana hands its light client on, whichever slots were skipped.
    fn outbox(&self, i: u64) -> Vec<crate::bridge::OutboxEntry> {
        let epoch_slots = self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).expect("bridge outbox requires bridge rule").epoch_slots;
        let validators = self.identities.iter().zip(&self.stakes).map(|(identity, stake)| {
            let key: [u8; 32] = bs58::decode(identity).into_vec().expect("identity is base58").try_into().expect("identity is 32 bytes");
            (key, *stake)
        }).collect();
        vec![crate::bridge::OutboxEntry::ValidatorSet { epoch: i * 4 / epoch_slots + 1, validators }]
    }
    /// Validator 0, once cblock `c` is final: its hash preimage, outbox and the signed votes that
    /// finalized it (Finalize, or Notar for a fast finalization), for Solana's light client.
    fn prepare_proof(&mut self, c: u64) -> Result<()> {
        let (preimage, outbox) = self.cblock_preimages.remove(&c).context("Finalized bridge cblock is missing its proof material")?;
        let block = self.nodes[0].executed_blocks.get(&c).copied().context("Finalized bridge cblock is missing its executed hash")?;
        let epoch_slots = self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).expect("bridge proof requires bridge rule").epoch_slots;
        let votes = self.nodes[0]
            .votes
            .values()
            .filter(|v| v.vote.slot == c && v.vote.hash == Some(block.hash) && matches!(v.vote.kind, consensus::VoteKind::Notar | consensus::VoteKind::Finalize))
            .map(|v| json!({"validator": v.vote.validator, "kind": if v.vote.kind == consensus::VoteKind::Finalize { "finalize" } else { "notar" }, "signature": B64.encode(&v.signature)}))
            .collect::<Vec<_>>();
        let proof = json!({
            "consensusSlot": c, "epochSlots": epoch_slots, "chainId": self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).expect("bridge proof requires bridge rule").chain_id, "hash": hex::encode(block.hash), "preimage": B64.encode(&preimage),
            "feeBurnFromSlot": self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).and_then(|rule| rule.fee_burn_from_slot),
            "nativeMint": self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).and_then(|rule| rule.native_mint).map(|mint| mint.to_string()),
            "outboxRoot": hex::encode(crate::bridge::outbox_root(&outbox)),
            "outbox": outbox.iter().map(|e| B64.encode(bincode::serialize(e).expect("outbox entry encodes"))).collect::<Vec<_>>(),
            "validators": self.identities, "stakes": self.stakes.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "votes": votes,
        });
        let withdrawal = outbox.iter().any(|entry| matches!(entry, crate::bridge::OutboxEntry::Withdrawal { .. }));
        let archive = self.bridge_proofs.as_mut().expect("bridge proof archive configured");
        let archived = archive.keep(c, c * 4 / epoch_slots, withdrawal, &proof)?;
        self.pending_bridge_proofs.insert(c, (proof, archived));
        Ok(())
    }
    fn bridge_ancestors(&self, id: usize, cblock: u64, after: Option<u64>) -> Result<Vec<u64>> {
        finalized_ancestors(cblock, after, |c| {
            ensure!(self.nodes[id].executed_blocks.contains_key(&c), "Finalized bridge ancestor {c} is missing its executed hash");
            if c % 4 == 0 {
                Ok(self.nodes[id].windows.get(&(c / 4)).context("Finalized bridge ancestor window is missing")?.parent.map(|p| p.slot))
            } else { Ok(Some(c - 1)) }
        })
    }
    /// A finalized descendant commits its ancestors too. Give those ancestors their own signed
    /// Finalize votes, which the Solana light client can certify without an ancestry instruction.
    fn sign_finalized_ancestors(&mut self, id: usize, block: BlockRef, step: &mut Step) -> Result<()> {
        if !self.bridge_open(block.slot * 4) { return Ok(()) }
        let first = self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).unwrap().from_slot.div_ceil(4);
        let after = [self.proof_prepared_through, first.checked_sub(1), self.nodes[id].votor.pruned_below().checked_sub(1)].into_iter().flatten().max();
        for c in self.bridge_ancestors(id, block.slot, after)? {
            let ancestor = self.nodes[id].executed_blocks[&c];
            if bridge_proof_quorum(self.nodes[id].votes.values(), ancestor, &self.stakes) { continue }
            let hash = ancestor.hash;
            let vote = consensus::Vote { validator: id, slot: c, hash: Some(hash), kind: consensus::VoteKind::Finalize };
            if !self.nodes[id].votes.contains_key(&encode(&vote)?) {
                self.consensus_actions(id, vec![Action::SendVote(vote)], step)?;
            }
        }
        Ok(())
    }
    /// No checkpoint may retain an ancestor's burn until its relayable proof is prepared.
    /// Archive capacity only affects retention; it never gates this consensus checkpoint.
    /// Incoming votes wake dispatch again; held finalization events remain unpublished meanwhile.
    fn prepare_finalized_chain(&mut self, cblock: u64) -> Result<bool> {
        if !self.bridge_open(cblock * 4) { return Ok(true) }
        let first = self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).unwrap().from_slot.div_ceil(4);
        let after = [self.proof_prepared_through, first.checked_sub(1)].into_iter().flatten().max();
        let chain = self.bridge_ancestors(0, cblock, after)?;
        for c in &chain {
            let block = self.nodes[0].executed_blocks[c];
            if !bridge_proof_quorum(self.nodes[0].votes.values(), block, &self.stakes) { return Ok(false) }
        }
        for c in chain { self.prepare_proof(c)?; }
        Ok(true)
    }
    /// Publishes only proofs whose execution state is durable. Preparing the archive before the
    /// state checkpoint ensures a restart cannot keep a burn while losing its withdrawal proof.
    async fn record_proof(&mut self, c: u64) {
        let Some((proof, archived)) = self.pending_bridge_proofs.remove(&c) else { return };
        let archive = self.bridge_proofs.as_ref().expect("bridge proof archive configured");
        let mut gateway = self.gateway.write().await;
        if archived {
            if archive.withdrawals.contains_key(&c) {
                gateway.archived_withdrawals.insert(c, archive.withdrawals[&c].clone());
                let epoch = c * 4 / proof["epochSlots"].as_u64().expect("proof epochSlots");
                gateway.epoch_cblock_proofs.entry(epoch).or_insert(c);
            }
        }
        gateway.keep_cblock_proof(c, proof);
    }
    /// Whether slot `s`'s blocks may carry bridge credits.
    fn bridge_open(&self, s: u64) -> bool {
        self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).is_some_and(|b| s >= b.from_slot)
    }
    /// The finalized deposits validator `id` has not credited yet, in sequence, a few a block.
    fn credits_due(&self, id: usize, s: u64) -> Vec<crate::execution::BridgeCredit> {
        if !self.bridge_open(s) {
            return vec![];
        }
        let next = self.nodes[id].executor.as_ref().unwrap().next_credit();
        self.bridge_deposits.lock().unwrap().range(next..).take(16).map(|(_, c)| c.clone()).collect()
    }
    fn window_json(&self, number: u64) -> Result<Value> {
        Ok(view::window_json(
            number,
            &self.window(number)?,
            &self.identities,
            &self.proposers.read().unwrap().weights(number)?,
            if self.config.crash_window == Some(number) {
                Some(self.config.crash_proposer)
            } else {
                None
            },
        ))
    }
    pub async fn run(mut self) -> Result<()> {
        let mut senders = Vec::new();
        let mut receivers = Vec::new();
        for _ in 0..12 {
            let (tx, rx) = mpsc::channel(4096);
            senders.push(tx);
            receivers.push(rx);
        }
        let verification_keys = self
            .keys
            .iter()
            .map(|k| k.verifying_key())
            .collect::<Vec<_>>();
        let schedule_ids = self.proposers.clone();
        let window_bases = Arc::new(std::sync::Mutex::new(
            BTreeMap::<(usize, u64), Instant>::new(),
        ));
        let (_empty, replacement) = mpsc::channel(1);
        let mut incoming = std::mem::replace(&mut self.incoming, replacement);
        let ingress_profile = self.profile.clone();
        let ingress_exchange = self.exchange.clone();
        let ingress_bases = window_bases.clone();
        // Each validator verifies its own data on its own queue, so a validator that falls behind
        // delays only itself, never the other validators' intake or the commitment path.
        let mut data_txs = Vec::with_capacity(12);
        for destination in senders.iter().cloned() {
            let (data_tx, mut receiver) = mpsc::unbounded_channel::<Received>();
            data_txs.push(data_tx);
            let keys = verification_keys.clone();
            let schedule = schedule_ids.clone();
            let profile = ingress_profile.clone();
            tokio::spawn(async move {
                loop {
                    let mut packets = Vec::with_capacity(128);
                    if receiver.recv_many(&mut packets, 128).await == 0 {
                        break;
                    }
                    let keys = keys.clone();
                    let schedule = schedule.clone();
                    let profile = profile.clone();
                    let verified = tokio::task::spawn_blocking(move || {
                        packets
                            .into_iter()
                            .filter_map(|packet| {
                                profile.record(
                                    Some(packet.to),
                                    "verify.queue_wait",
                                    packet.received_at.elapsed(),
                                );
                                let _phase = profile.measure(Some(packet.to), "verify.signature");
                                match authenticate(packet, &keys, &schedule) {
                                    Ok(value) => Some(value),
                                    Err(error) => {
                                        tracing::warn!(%error,"Rejected wire message");
                                        None
                                    }
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .await
                    .expect("wire worker panicked");
                    for (packet, envelope) in verified {
                        if destination
                            .send((packet, envelope))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
        // This tag only selects the cheap authentication path; it never authenticates a packet.
        // Envelope starts with fixed-width u64 `from`, followed by the fixed-width Wire tag.
        let commit_tag = encode(&Envelope {
            from: 0,
            body: Wire::Commit(Commitment {
                slot: 0,
                proposer: 1,
                hash: [0; 32],
                key_hash: [0; 32],
            }),
            signature: vec![],
        })?[8..12]
            .to_vec();
        tokio::spawn(async move {
            while let Some(packet) = incoming.recv().await {
                if packet.payload.get(8..12) != Some(commit_tag.as_slice()) {
                    if data_txs[packet.to].send(packet).is_err() {
                        return;
                    }
                    continue;
                }
                ingress_profile.record(
                    Some(packet.to),
                    "verify.control_queue_wait",
                    packet.received_at.elapsed(),
                );
                let verified = {
                    let _phase =
                        ingress_profile.measure(Some(packet.to), "verify.control_signature");
                    authenticate(packet, &verification_keys, &schedule_ids)
                };
                let (packet, envelope) = match verified {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(%error,"Rejected commitment");
                        continue;
                    }
                };
                let exchange = ingress_exchange.clone();
                let bases = ingress_bases.clone();
                let destination = senders[packet.to].clone();
                // Recording the exchange must not await a validator's full data queue.
                tokio::spawn(async move {
                    if let Wire::Commit(c) = &envelope.body {
                        let base = bases
                            .lock()
                            .unwrap()
                            .get(&(packet.to, c.slot / 16))
                            .copied();
                        // Algorithm 1, lines 20-22 hold whether or not this proposer has started
                        // the window yet; the base only anchors the event's times.
                        let base = base
                            .map(|b| b + std::time::Duration::from_micros(c.slot % 16 * 50000))
                            .unwrap_or(packet.received_at);
                        exchange
                            .receive_commit(
                                packet.to,
                                envelope.from,
                                c.clone(),
                                envelope.signature.clone(),
                                packet.received_at,
                                base,
                            )
                            .await;
                    }
                    let _ = destination.send((packet, envelope)).await;
                });
            }
        });
        let (no_commands, empty_commands) = mpsc::channel(1);
        drop(no_commands);
        let mut command_receiver = std::mem::replace(&mut self.commands, empty_commands);
        let mut actors = tokio::task::JoinSet::new();
        let all_nodes = std::mem::take(&mut self.nodes.0);
        for (id, node) in all_nodes.into_iter().enumerate() {
            let mut placeholders = (0..12).map(|_| None).collect::<Vec<Option<Node>>>();
            placeholders[id] = node;
            let nodes = NodeSet(placeholders);
            let actor = NodeRunner {
                id,
                network: Box::new(Network {
                    config: self.config.clone(),
                    profile: self.profile.clone(),
                    keys: self.keys.clone(),
                    identities: self.identities.clone(),
                    stakes: self.stakes.clone(),
                    proposers: self.proposers.clone(),
                    nodes: NodeSet::default(),
                    transport: self.transport.clone(),
                    exchange: self.exchange.clone(),
                    incoming: mpsc::channel(1).1,
                    gateway: self.gateway.clone(),
                    commands: mpsc::channel(1).1,
                    store: self.store.clone(),
                    persistence: self.persistence.clone(),
                    observer: self.observer.clone(),
                    origin: self.origin,
                    wall: self.wall,
                    samples: vec![],
                    final_times: vec![],
                    finalized: BTreeSet::new(),
                    skipped: BTreeSet::new(),
                    slot_views: BTreeMap::new(),
                    senders: if id == 0 {
                        std::mem::take(&mut self.senders)
                    } else {
                        vec![]
                    },
                    traffic_seq: 0,
                    received_packets: 0,
                    last_slot_started: None,
                    maximum_execution_skew: 0,
                    within_window_samples: vec![],
                    handover_samples: vec![],
                    active: vec![id],
                    admissions: BTreeMap::new(),
            admission_slots: BTreeMap::new(),
                    progress: self.progress.clone(),
                    observed_final: self.observed_final.clone(),
                    held_finals: Vec::new(),
                    cblock_preimages: BTreeMap::new(),
                    bridge_proofs: if id == 0 { self.bridge_proofs.take() } else { None },
                    bridge_prunes: self.bridge_prunes.clone(),
                    pending_bridge_proofs: BTreeMap::new(),
                    proof_prepared_through: self.proof_prepared_through,
                    bridge_deposits: self.bridge_deposits.clone(),
                    performance: VecDeque::new(),
                    performance_mark: (Instant::now(), 0, 0),
                    executed_count: (0, 0),
                    observed_skip: self.observed_skip.clone(),
                    handover_trace: self.handover_trace.clone(),
                    state_checks: self.state_checks.clone(),
                    io_handle: self.io_handle.clone(),
                    attestation_arrivals: self.attestation_arrivals.clone(),
                    facts: (id == 0).then(|| Box::new(crate::facts::Facts::new(self.transport.peers()))),
                    facts_cursor: FactsCursor::default(),
                    genesis: self.genesis.clone(),
                    lab_evidence: self.lab_evidence.clone(),
                    lab_marked: self.lab_marked.clone(),
                    lab_injected: false,
                    signature_ids: BTreeMap::new(),
                    solana: Default::default(),
                    solana_sent: BTreeMap::new(),
                    state_writer: if id == 0 { self.state_writer.take() } else { None },
                    counter_base: self.counter_base,
                    genesis_hash: self.genesis_hash.clone(),
                    kept: self.kept.clone(),
                    kept_writer: if id == 0 { self.kept_writer.take() } else { None },
                    keep_from: self.keep_from,
                    built_in: self.built_in.clone(),
                }),
                nodes,
            };
            let rx = receivers.remove(0);
            let bases = window_bases.clone();
            let commands = if id == 0 {
                let (_tx, empty) = mpsc::channel(1);
                Some(std::mem::replace(&mut command_receiver, empty))
            } else {
                None
            };
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name(format!("acrux-validator-{id}"))
                .spawn(move || {
                    crate::raise_thread_priority();
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("validator runtime");
                    let result = runtime.block_on(actor.run(rx, commands, bases));
                    let _ = done_tx.send(result);
                })?;
            actors.spawn(async move { done_rx.await.context("Validator thread stopped")? });
        }
        let mut finished = Vec::new();
        while let Some(result) = actors.join_next().await {
            finished.push(result??);
        }
        self.transport.close().await;
        for (id, mut actor) in finished {
            if id == 0 {
                self.samples = std::mem::take(&mut actor.samples);
                self.within_window_samples = std::mem::take(&mut actor.within_window_samples);
                self.handover_samples = std::mem::take(&mut actor.handover_samples);
                self.facts = actor.facts.take();
                self.final_times = std::mem::take(&mut actor.final_times);
                self.finalized = std::mem::take(&mut actor.finalized);
                self.skipped = std::mem::take(&mut actor.skipped);
                self.traffic_seq = actor.traffic_seq;
            }
            self.nodes.0.push(actor.nodes.0.swap_remove(id));
        }
        self.nodes.0.sort_by_key(|node| node.as_ref().unwrap().id);
        self.lab_conclude().await?;
        self.observer.flush().await?;
        self.report().await?;
        Ok(())
    }
    async fn dispatch(&mut self, node: usize, step: Step) -> Result<()> {
        if node == 0 && self.bridge_proofs.is_some() {
            let paid = {
                let mut queued = self.bridge_prunes.lock().unwrap();
                let batch = queued.iter().take(100).copied().collect::<BTreeSet<_>>();
                for slot in &batch { queued.remove(slot); }
                batch
            };
            if let Some(archive) = &mut self.bridge_proofs {
                let removed = archive.prune_paid(&paid)?;
                if !removed.is_empty() {
                    let mut gateway = self.gateway.write().await;
                    for slot in removed {
                        gateway.archived_withdrawals.remove(&slot);
                    }
                    gateway.epoch_cblock_proofs = archive.epochs.clone();
                }
            }
        }
        if step.out.is_empty() && step.events.is_empty() && step.persist.is_empty() && self.held_finals.is_empty() {
            return Ok(());
        }
        let _phase = self.profile.measure(Some(node), "dispatch.total");
        for message in step.out {
            let from = message.from;
            let signer = self.keys[from].clone();
            let body = message.body;
            let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                let signature = crypto::sign_bytes(&signer, &encode(&body)?);
                encode(&Envelope {
                    from,
                    body,
                    signature,
                })
            })
            .await??;
            for to in message.to {
                let transport = self.transport.clone();
                let bytes = bytes.clone();
                self.io_handle.spawn(async move {
                    if let Err(e) = transport.send(message.from, to, bytes).await {
                        tracing::warn!(error=%e,"QUIC send failed")
                    }
                });
            }
        }
        for event in &step.events {
            if let Some(slot) = event.slot {
                if event.kind == "cblock.certificate" && event.data["kind"] == "skip" {
                    self.skipped.insert(slot / 4);
                    self.observed_skip.lock().unwrap().insert(slot / 4);
                }
                if node == 0 && event.kind == "cblock.finalized" { self.finalized.insert(slot / 4); }
                // These local seeds only support node zero's own slot-start and execution views.
                // The observer owns the complete projection, including other validators' arrivals.
                if node == 0 && matches!(event.kind, "slot.started" | "slot.executed") {
                    self.slot_views.insert(slot, event.data.clone());
                }
                if node == 0 && event.kind == "slot.executed" {
                    let merged = event.data["mergedOrder"].as_array().map_or(0, |m| m.iter().filter(|t| t["selected"] != false).count() as u64);
                    self.executed_count = (self.executed_count.0.max(slot), self.executed_count.1 + merged);
                }
            }
        }
        if node == 0 {
            let waiting = self.finalized.iter().copied().filter(|c| self.proof_prepared_through.is_none_or(|done| *c > done)).collect::<Vec<_>>();
            for c in waiting {
                if !self.prepare_finalized_chain(c)? { break }
                self.save_state(c)?;
                self.proof_prepared_through = Some(c);
                // Accepted ancestors were consumed above; older remaining material belongs to
                // skipped/abandoned branches and cannot contain a committed withdrawal.
                self.cblock_preimages.retain(|slot, _| *slot > c);
            }
        }
        if node == 0 && self.performance_mark.0.elapsed() >= std::time::Duration::from_secs(60) {
            let (at, slot, transactions) = self.performance_mark;
            let (now_slot, now_transactions) = self.executed_count;
            if slot > 0 {
                self.performance.push_back(json!({
                    "slot": now_slot, "numSlots": now_slot - slot, "numTransactions": now_transactions - transactions,
                    "numNonVoteTransactions": now_transactions - transactions, "samplePeriodSecs": at.elapsed().as_secs(),
                }));
                if self.performance.len() > 720 {
                    self.performance.pop_front();
                }
            }
            self.performance_mark = (Instant::now(), now_slot, now_transactions);
        }
        while self.slot_views.len() > 64 {
            let first = *self.slot_views.keys().next().unwrap();
            self.slot_views.remove(&first);
        }
        // A cblock is reported final (the event stream, statuses, kept transactions) only once its
        // state is on disk, so a crash never takes back what was reported final.
        let durable = crate::state::durable();
        let (mut events, held): (Vec<_>, Vec<_>) = step
            .events
            .into_iter()
            .partition(|e| e.kind != "cblock.finalized" || e.slot.map_or(true, |slot| slot / 4 <= durable));
        self.held_finals.extend(held);
        let (released, held): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held_finals)
            .into_iter()
            .partition(|e| e.slot.map_or(true, |slot| slot / 4 <= durable));
        self.held_finals = held;
        events.extend(released);
        if node == 0 {
            for event in events.iter().filter(|e| e.kind == "cblock.finalized") {
                let cblock = event.slot.unwrap() / 4;
                if self.observed_final.lock().unwrap().insert(cblock) {
                    self.keep_finalized(cblock).await;
                    self.final_times.push(event.at);
                }
            }
            let prepared = self.pending_bridge_proofs.keys().copied().take_while(|c| *c <= durable).collect::<Vec<_>>();
            for c in prepared { self.record_proof(c).await; }
            // Age is measured against persisted finality, after publishing every prepared
            // ancestor. A crash cannot shed an old proof using a frontier that was never saved.
            let saved_cursor = self.gateway.read().await.bridge_saved_cursor;
            if let (Some(archive), Some(prepared_through)) = (&mut self.bridge_proofs, self.proof_prepared_through) {
                let removed = archive.shed_stale(durable.min(prepared_through), saved_cursor)?;
                if !removed.is_empty() {
                    let mut gateway = self.gateway.write().await;
                    for slot in removed {
                        gateway.archived_withdrawals.remove(&slot);
                    }
                    gateway.epoch_cblock_proofs = archive.epochs.clone();
                }
            }
        }
        let queued = Instant::now();
        self.observer.submit(node, events, step.persist).await?;
        self.profile.record(Some(node), "observer.queue_wait", queued.elapsed());
        Ok(())
    }
    /// Fault Lab: sends the scenario's transaction a few slots before the faulty window.
    async fn lab_inject(&mut self) -> Result<()> {
        let Some(fault) = self.config.fault.clone().filter(|f| matches!(f.scenario.as_str(), "censorship" | "private-front-run")) else {
            self.lab_injected = true;
            return Ok(());
        };
        if self.latest_started() + 6 < fault.window * 16 {
            return Ok(());
        }
        self.lab_injected = true;
        let target = fault.window * 16 + 2;
        let sender = &self.senders[self.senders.len() - 1];
        let recipient = self.senders[self.senders.len() - 2].pubkey();
        let tx = Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(20_000),
                ComputeBudgetInstruction::set_compute_unit_price(5_000),
                system_instruction::transfer(&sender.pubkey(), &recipient, 1_000_000),
            ],
            Some(&sender.pubkey()),
            &[sender],
            self.nodes[0].executor.as_ref().unwrap().latest_blockhash().parse()?,
        );
        let bytes = encode(&tx)?;
        let valid = self.nodes[0].executor.as_ref().unwrap().validate_transaction(&bytes, target)?;
        *self.lab_marked.lock().unwrap() = Some(valid.header.id.clone());
        if fault.scenario == "censorship" {
            self.record_sent(&valid.header, "plain", 3, 1).await?;
            let mut step = Step::new();
            step.out.push(Outbox { from: 0, to: self.proposer_ids(fault.window)?, body: Wire::Submit(valid) });
            return self.dispatch(0, step).await;
        }
        // Private front-run: a 2-of-3 transaction; the attacking receiver holds one fragment.
        let signer = SigningKey::from_bytes(&sender.to_bytes()[..32].try_into()?);
        let fragments = crate::threshold::seal_for_receivers(&bytes, valid.header.clone(), &signer, 3, 2, &[1, 2, 3])?;
        let held = fragments.iter().find(|f| f.receiver as usize == fault.attacker).context("attacker fragment")?;
        let mut alone = crate::threshold::ThresholdCache::new(self.config.fragment_expiry_slots)?;
        let attempt = alone.accept(target, held.receiver, crate::threshold::FragmentSubmission::parse(serde_json::to_value(held)?)?)?;
        if let crate::threshold::AcceptOutcome::Pending { accepted, threshold, .. } = attempt {
            self.lab_evidence.lock().unwrap().push(json!({"kind":"unreadable_fragment","slot":target,"proposer":fault.attacker,"txId":valid.header.id,"description":format!("Receiver {} holds {accepted} of the {threshold} fragments it would need: neither the ciphertext nor the key can be rebuilt, so it cannot read the transaction before the slot commits.", fault.attacker),"hashes":[],"signatures":[],"observedFragments":accepted,"threshold":threshold}));
        }
        for fragment in fragments {
            self.submit_fragment(serde_json::to_value(fragment)?).await?;
        }
        Ok(())
    }
    /// Fault Lab: reads the run's outcome from the net's own records and writes `outcome.json`.
    async fn lab_conclude(&mut self) -> Result<()> {
        let Some(fault) = self.config.fault.clone() else { return Ok(()) };
        let w = fault.window;
        let mut evidence = self.lab_evidence.lock().unwrap().clone();
        let skipped = (w * 4..w * 4 + 4).filter(|i| self.skipped.contains(i)).collect::<Vec<_>>();
        let skip = |slot: u64, why: &str| json!({"kind":"skip_certificate","slot":slot,"proposer":null,"txId":null,"description":why,"hashes":[],"signatures":[],"observedFragments":null,"threshold":null});
        let marked = self.lab_marked.lock().unwrap().clone();
        let marked_record = match &marked {
            Some(id) => self.store.get(&format!("transaction/{id}"))?,
            None => None,
        };
        match fault.scenario.as_str() {
            "last-look" => {
                if let Some(late) = self.exchange.omitted_commitments().into_iter().find(|o| o["slot"].as_u64().is_some_and(|s| s / 16 == w) && o["omittedProposers"].as_array().is_some_and(|p| p.contains(&json!(fault.attacker)))) {
                    evidence.push(json!({"kind":"late_block_dropped","slot":late["slot"],"proposer":fault.attacker,"txId":null,"description":format!("Proposer {} committed 28 ms late; its co-proposers attested it as late (deadline {} us after start), so its block can only be dropped.", fault.attacker, 20_000),"hashes":[],"signatures":[],"observedFragments":null,"threshold":null}));
                }
                if let Some(i) = skipped.first() {
                    evidence.push(skip(i * 4, "The late proposer's own attestation disagrees with its co-proposers' vectors, so no set is agreed and the cblock is skip-certified."));
                }
            }
            "split-commitments" => {
                if let Some(i) = skipped.first() {
                    evidence.push(skip(i * 4, "Co-proposer 1 attested the attacker's commitment and co-proposer 2 never received it: the vectors differ, no set is agreed, and the cblock is skip-certified."));
                }
            }
            "equivocation" => {
                if let Some(i) = skipped.first() {
                    evidence.push(skip(i * 4, "Validators that saw the slashing proof cannot agree on the slot (Agreed (d)), and the cblock is skip-certified."));
                }
            }
            "crash" => {
                let first = w * 16;
                if self.nodes[0].slots.get(&first).and_then(|s| s.agreed.clone()) == Some(vec![1, 2]) {
                    evidence.push(json!({"kind":"two_proposers_continue","slot":first,"proposer":fault.attacker,"txId":null,"description":format!("Proposer {} sent nothing for the whole window; both co-proposers attested it as missing, so slot {first} agreed on P = {{1,2}} and the window continued on two proposers.", fault.attacker),"hashes":[],"signatures":[],"observedFragments":null,"threshold":null}));
                }
            }
            "censorship" => {
                if let (Some(id), Some(tx)) = (&marked, &marked_record) {
                    let carriers = tx["fragments"].as_array().map(|f| f.iter().filter_map(|x| x["proposer"].as_u64()).collect::<Vec<_>>()).unwrap_or_default();
                    if matches!(tx["state"].as_str(), Some("executed" | "finalized")) && carriers == vec![fault.attacker as u64] {
                        evidence.push(json!({"kind":"transaction_included","slot":tx["executedSlot"],"proposer":fault.attacker,"txId":id,"description":format!("Proposers 1 and 2 left the transaction out; proposer {} carried it, its block was in the agreed set, and the transaction executed in slot {} ({}).", fault.attacker, tx["executedSlot"], tx["state"].as_str().unwrap()),"hashes":[],"signatures":[],"observedFragments":null,"threshold":null}));
                    }
                }
            }
            "private-front-run" => {
                let executed = marked_record.as_ref().is_some_and(|tx| matches!(tx["state"].as_str(), Some("executed" | "finalized")));
                if !executed {
                    evidence.retain(|e| e["kind"] != "unreadable_fragment");
                }
            }
            other => anyhow::bail!("Unknown Fault Lab scenario {other}"),
        }
        let expected: &[&str] = match fault.scenario.as_str() {
            "last-look" => &["late_block_dropped", "skip_certificate"],
            "censorship" => &["transaction_included"],
            "crash" => &["two_proposers_continue"],
            "equivocation" => &["slashing_proof", "skip_certificate"],
            "split-commitments" => &["skip_certificate"],
            _ => &["unreadable_fragment"],
        };
        let missing = expected.iter().filter(|k| !evidence.iter().any(|e| e["kind"] == **k)).collect::<Vec<_>>();
        let outcome = if missing.is_empty() {
            json!({"status":"done","outcome":evidence.iter().map(|e| e["description"].as_str().unwrap_or_default()).collect::<Vec<_>>().join(" "),"evidence":evidence,"failure":null})
        } else {
            json!({"status":"failed","outcome":null,"evidence":evidence,"failure":format!("The run ended without the expected evidence: {}", missing.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", "))})
        };
        {
            let mut g = self.gateway.write().await;
            for e in outcome["evidence"].as_array().unwrap() {
                g.emit(None, e["slot"].as_u64(), "slot", 0, "lab.evidence", e.clone());
            }
        }
        std::fs::write(Path::new(&fault.out).join("outcome.json"), serde_json::to_vec_pretty(&outcome)?)?;
        Ok(())
    }
    /// Once a second, node zero folds the latest commitments, attestations and finalizations
    /// into the rolling facts and publishes them to the gateway.
    async fn update_facts(&mut self) {
        let now = Instant::now();
        if self.facts_cursor.updated.is_some_and(|at| now.duration_since(at) < std::time::Duration::from_secs(1)) {
            return;
        }
        self.facts_cursor.updated = Some(now);
        let until = self.latest_started().saturating_sub(4);
        let commits = self.exchange.commit_times();
        let mut gaps = vec![];
        for slot in self.facts_cursor.commit_slot + 1..=until {
            let (Some(at), Some(previous)) = (commits.get(&slot), commits.get(&(slot - 1))) else { continue };
            let link = if slot % 16 == 0 { self.window(slot / 16 - 1).ok().zip(self.window(slot / 16).ok()).map(|(a,b)| format!("{}-{}",a.hub_code,b.hub_code)) } else { None };
            gaps.push((link, offset(*at, *previous)));
        }
        self.facts_cursor.commit_slot = self.facts_cursor.commit_slot.max(until);
        let (spreads, spread_index) = self.exchange.spreads_from(self.facts_cursor.spread_index);
        self.facts_cursor.spread_index = spread_index;
        let arrivals = std::mem::take(&mut *self.attestation_arrivals.lock().unwrap());
        let finals = self.final_times[self.facts_cursor.final_index..].to_vec();
        self.facts_cursor.final_index = self.final_times.len();
        let facts = self.facts.as_mut().unwrap();
        for (link, gap) in gaps {
            match link {
                Some(link) => facts.handover(link, gap),
                None => facts.within_window(gap),
            }
        }
        spreads.into_iter().for_each(|s| facts.commit_spread(s));
        for (slot, at) in arrivals {
            if let Some(block) = commits.get(&slot) {
                let relative = if at >= *block { offset(at, *block) as i64 } else { -(offset(*block, at) as i64) };
                facts.attestation(relative);
            }
        }
        finals.into_iter().for_each(|f| facts.finalization(f));
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let json = facts.json(utc(wall));
        self.exchange.prune_commit_times(until.saturating_sub(64));
        let node = &self.nodes[0];
        let retained = node.slots.iter().filter(|(_, local)| local.executed && !local.cipher.is_empty()).map(|(slot, _)| *slot).collect::<Vec<_>>();
        let bytes = node.slots.values().map(LocalSlot::block_buffer_bytes).sum::<usize>() as u64;
        let mut gateway = self.gateway.write().await;
        gateway.facts = json;
        gateway.follower_retained_slots = retained.len() as u64;
        gateway.follower_earliest_slot = retained.first().copied();
        gateway.follower_retained_block_bytes = bytes;
    }
    async fn tick(&mut self) -> Result<()> {
        let _phase = self.profile.measure(None, "protocol.tick");
        if self.facts.is_some() {
            self.update_facts().await;
            if !self.lab_injected {
                self.lab_inject().await?;
            }
        }
        for id in self.active.clone() {
            let starts = std::mem::take(&mut self.nodes[id].pending_starts);
            for (number, parent) in starts {
                self.start_window(id, number, parent).await?;
            }
        }
        for id in self.active.clone() {
            let starts = std::mem::take(&mut self.nodes[id].pending_starts);
            for (number, parent) in starts {
                self.start_window(id, number, parent).await?;
            }
            // Catching up: a validator that fell more than two windows behind (its threads were
            // starved, or it restarted mid-run) executes and votes the older windows it has not
            // finished, oldest first, eight a tick, before the two it is working on. It produces
            // no blocks for them: their slots are long past.
            // Only forward from where it stands: windows that end after its last executed slot
            // and are not finished (each cblock executed or certified skipped). Windows behind
            // that point were settled or abandoned; going back to one would roll its state back.
            let older = self.nodes[id].windows.len().saturating_sub(2);
            let executed_to = self.nodes[id].last_executed;
            let behind = self.nodes[id]
                .windows
                .keys()
                .take(older)
                .copied()
                .filter(|w| executed_to.is_some_and(|last| w * 16 + 15 > last))
                .filter(|w| {
                    !(w * 4..w * 4 + 4).all(|i| {
                        self.nodes[id].executed_blocks.contains_key(&i)
                            || self.nodes[id].seen_certificates.contains(&(i, consensus::CertificateKind::Skip, None))
                    })
                })
                .take(8)
                .collect::<Vec<_>>();
            for w in behind {
                self.try_execute(id, w)
                    .await
                    .with_context(|| format!("validator {id} catching up window {w}"))?;
                self.try_vote(id, w)
                    .await
                    .with_context(|| format!("validator {id} catching up votes of window {w}"))?;
            }
            let windows = self.nodes[id]
                .windows
                .keys()
                .rev()
                .take(2)
                .copied()
                .filter(|w| {
                    !(w * 4..w * 4 + 4).all(|i| self.nodes[id].voted.contains(&i))
                        || (w * 4..w * 4 + 4).any(|i| {
                            !self.nodes[id].executed_blocks.contains_key(&i)
                                && (i * 4..i * 4 + 4).all(|s| {
                                    self.nodes[id]
                                        .slots
                                        .get(&s)
                                        .map_or(false, |local| local.executed)
                                })
                                && !self.nodes[id].seen_certificates.contains(&(
                                    i,
                                    consensus::CertificateKind::Skip,
                                    None,
                                ))
                        })
                        || (self.nodes[id].windows.keys().next_back() == Some(w)
                            && !(w * 16..w * 16 + 16).all(|s| {
                                self.nodes[id]
                                    .slots
                                    .get(&s)
                                    .map_or(false, |slot| slot.executed)
                            }))
                })
                .collect::<Vec<_>>();
            for w in windows {
                let ids = self.proposer_ids(w)?;
                let index = ids.iter().position(|x| *x == id);
                let mut produce = vec![];
                {
                    let run = self.nodes[id].windows.get_mut(&w).unwrap();
                    while run.next_slot < 16 && elapsed(run.start) >= run.next_slot as u64 * 50000 {
                        let s = w * 16 + run.next_slot as u64;
                        run.next_slot += 1;
                        produce.push(s);
                    }
                }
                for s in produce {
                    if id == 0 {
                        let now = Instant::now();
                        if let Some(last) = self.last_slot_started {
                            let span = offset(now, last);
                            self.samples.push(span);
                            if s % 16 == 0 {
                                self.handover_samples.push(span);
                            } else {
                                self.within_window_samples.push(span);
                            }
                        }
                        self.last_slot_started = Some(now);
                        let view = self.slot_views[&s].clone();
                        let mut st = Step::new();
                        st.event(s, 0, "slot.started", view);
                        self.dispatch(id, st).await?;
                    }
                    if index.is_some()
                        && !(self.config.crash_window == Some(w)
                            && index.unwrap() + 1 == self.config.crash_proposer)
                    {
                        self.begin_block(id, s).await?;
                    }
                }
                self.try_execute(id, w)
                    .await
                    .with_context(|| format!("validator {id} executing window {w}"))?;
                self.try_vote(id, w)
                    .await
                    .with_context(|| format!("validator {id} voting window {w}"))?;
            }
        }
        for id in self.active.clone() {
            // Validator 0 serves followers; other validators retain their short execution window.
            let held = if id == 0 { FOLLOWER_SLOTS } else { 64 };
            let node = &mut self.nodes[id];
            if let Some(last) = node.last_executed {
                for (slot, local) in node
                    .slots
                    .range_mut(last.saturating_sub(held + 32)..last.saturating_sub(held - 1))
                {
                    if *slot + held <= last {
                        local.shreds.clear();
                        local.cipher.clear();
                        local.meta.clear();
                        local.payload.clear();
                    }
                }
            }
        }
        for id in self.active.clone() {
            let proof_floor = (id == 0).then(|| self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).map(|rule| {
                self.proof_prepared_through.map_or(rule.from_slot.div_ceil(4), |done| done + 1)
            })).flatten();
            let node = &mut self.nodes[id];
            if let Some((&window, run)) = node.windows.iter().next_back() {
                let parent = run.parent.map(|p| p.slot);
                // Everything from the last finalized cblock on: after any run of skipped windows
                // the next window's parent is that cblock or a later one.
                let floor = node.votor.finalized().map_or(0, |f| f.slot).min(proof_floor.unwrap_or(u64::MAX));
                node.checkpoints
                    .retain(|slot, _| *slot + 8 >= window * 4 || *slot >= floor || Some(*slot) == parent);
                node.threshold_checkpoints
                    .retain(|slot, _| *slot + 8 >= window * 4 || *slot >= floor || Some(*slot) == parent);
            }
            // Keep node zero's local slots, windows and parent chain for the same follower window.
            if let Some(finalized) = node.votor.finalized() {
                let held_cblocks = if id == 0 { FOLLOWER_SLOTS / 4 } else { 64 };
                let below = finalized.slot.saturating_sub(held_cblocks).min(proof_floor.unwrap_or(u64::MAX));
                if below > node.votor.pruned_below() {
                    node.votor.prune(below);
                    node.slots = node.slots.split_off(&(below * 4));
                    node.votes.retain(|_, v| v.vote.slot >= below);
                    node.seen_certificates = node.seen_certificates.split_off(&(below, consensus::CertificateKind::Notarization, None));
                    node.voted = node.voted.split_off(&below);
                    node.executed_blocks = node.executed_blocks.split_off(&below);
                    node.windows = node.windows.split_off(&(below / 4));
                }
            }
        }
        let positions = self
            .nodes
            .iter()
            .map(|node| node.last_executed.map_or(0, |slot| slot + 1))
            .collect::<Vec<_>>();
        self.maximum_execution_skew = self
            .maximum_execution_skew
            .max(positions.iter().max().unwrap() - positions.iter().min().unwrap());
        let mut progress = self.progress.lock().unwrap();
        for id in &self.active {
            let node = &self.nodes[*id];
            progress.executed[*id] = node.last_executed.map_or(0, |s| s + 1);
            progress.started[*id] = node
                .windows
                .iter()
                .next_back()
                .map_or(0, |(w, r)| w * 16 + r.next_slot as u64);
        }
        let minimum = *progress.executed.iter().min().unwrap();
        let maximum = *progress.executed.iter().max().unwrap();
        progress.maximum_skew = progress.maximum_skew.max(maximum - minimum);
        progress.maximum_lag = progress.maximum_lag.max(
            progress
                .started
                .iter()
                .zip(progress.executed)
                .map(|(s, e)| s.saturating_sub(e))
                .max()
                .unwrap(),
        );
        Ok(())
    }
    async fn start_window(
        &mut self,
        id: usize,
        number: u64,
        parent: Option<BlockRef>,
    ) -> Result<()> {
        if self.nodes[id].windows.contains_key(&number) {
            return Ok(());
        }
        if !self.schedule_ready(number) {
            self.nodes[id].pending_starts.push((number, parent));
            return Ok(());
        }
        let start = Instant::now();
        self.nodes[id].windows.insert(
            number,
            WindowRun {
                start,
                parent,
                next_slot: 0,
                skipped_from: None,
            },
        );
        let w = self.window_json(number)?;
        let mut step = Step::new();
        // A transaction this node carried two windows ago that has still not executed went down
        // with a skipped slot: it goes to this window's proposer in the role that carried it.
        let first = number * 16;
        let stale = self.nodes[id]
            .in_flight
            .iter()
            .filter(|(_, (slot, _, _))| slot + 32 <= first)
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in stale {
            let (_, role, tx) = self.nodes[id].in_flight.remove(&key).unwrap();
            let executor = self.nodes[id].executor.as_ref().unwrap();
            if executor.is_executed(&key) || !executor.blockhash_valid(&tx.bytes) {
                continue;
            }
            let to = vec![self.proposer_ids(number)?[role - 1]];
            step.out.push(Outbox { from: id, to, body: Wire::Submit(tx) });
        }
        // A proposer of the window before hands on what it did not get to include (its window was
        // skipped, or the transaction arrived late) to the proposer in its role of this window.
        if number > 0 {
            if let Some(role) = self.proposer_ids(number - 1)?.iter().position(|x| *x == id) {
                let to = vec![self.proposer_ids(number)?[role]];
                let first = number * 16;
                let node = &mut self.nodes[id];
                let late = node.mempool.iter().filter(|(_, t)| t.header.target_slot < first).map(|(k, _)| k.clone()).collect::<Vec<_>>();
                for key in late {
                    let tx = node.mempool.remove(&key).unwrap();
                    step.out.push(Outbox { from: id, to: to.clone(), body: Wire::Submit(tx) });
                }
                let late = node.fragment_mempool.iter().filter(|(_, f)| f.header.target_slot < first).map(|(k, _)| k.clone()).collect::<Vec<_>>();
                for key in late {
                    let fragment = node.fragment_mempool.remove(&key).unwrap();
                    step.out.push(Outbox { from: id, to: to.clone(), body: Wire::SubmitFragment(fragment) });
                }
            }
        }
        step.events.push(Telemetry {
            slot: Some(number * 16),
            anchor: "window",
            at: 0,
            kind: "window.started",
            data: w.clone(),
        });
        for s in number * 16..number * 16 + 16 {
            let wall = utc(self.wall + offset(start, self.origin) / 1000 + (s % 16) * 50);
            let v = view::pending_slot(s, w.clone(), wall);
            self.nodes[id].slots.entry(s).or_default();
            if id == 0 {
                self.slot_views.insert(s, v.clone());
            }
            if s % 16 == 0 {
                step.event(s, 0, "slot.started", v);
            }
        }
        self.dispatch(id, step).await?;
        if id == 0 {
            let upcoming = (number + 1..number + 9)
                .take_while(|n| self.schedule_ready(*n))
                .map(|n| self.window_json(n))
                .collect::<Result<Vec<_>>>()?;
            let mut g = self.gateway.write().await;
            g.upcoming_windows = upcoming;
        }
        Ok(())
    }
    async fn begin_block(&mut self, id: usize, s: u64) -> Result<()> {
        let _phase = self.profile.measure(Some(id), "proposer.build");
        let ids = self.proposer_ids(s / 16)?;
        let q = ids.iter().position(|x| *x == id).unwrap() + 1;
        if s % 16 == 0 {
            if let Some(early) = self.nodes[id].early.remove(&(s / 16)) {
                return self.complete_early(id, s, q, early).await;
            }
        }
        let key = crypto::fresh_key();
        // Censorship scenario: every proposer of the window but the honest one omits the marked transaction.
        let censored = self
            .config
            .fault
            .as_ref()
            .filter(|f| f.scenario == "censorship" && f.window == s / 16 && f.attacker != q)
            .and_then(|_| self.lab_marked.lock().unwrap().clone());
        self.retire_builtin(id, s);
        let node = &mut self.nodes[id];
        let (txs, fragments) = pack(node, s, q, censored.as_ref());
        let credits = self.credits_due(id, s);
        let payload = BlockPayload { transactions: txs, fragments, credits }.encode(self.bridge_open(s))?;
        let node = &mut self.nodes[id];
        ensure!(
            payload.len() <= 1024 * 1024,
            "Proposer block exceeds local size limit"
        );
        let parent = if s % 16 == 0 {
            node.windows[&(s / 16)].parent
        } else {
            None
        };
        let signer = node.key.clone();
        let (commit, packets) = tokio::task::spawn_blocking(move || -> Result<_> {
            let chunks = payload
                .chunks(38_000)
                .map(|c| c.to_vec())
                .collect::<Vec<_>>();
            let count = chunks.len();
            let mut roots = vec![];
            let mut packets = vec![];
            for (slice, plain) in chunks.into_iter().enumerate() {
                let cipher = crypto::encrypt_slice(&key, s, q, slice as u32, &plain)?;
                let meta = ShredMeta {
                    slot: s,
                    proposer: q,
                    slice: slice as u32,
                    final_slice: slice + 1 == count,
                    key_hash: crypto::hash(&key),
                    parent: if slice + 1 == count {
                        parent.map(|p| (p.slot, p.hash))
                    } else {
                        None
                    },
                    encrypted_len: cipher.len() as u32,
                };
                let shreds = crypto::encode_slice(&signer, &meta, &cipher)?;
                roots.push(shreds[0].root);
                packets.push(shreds);
            }
            let commit = Commitment {
                slot: s,
                proposer: q,
                hash: crypto::hash(&encode(&BlockCommitment {
                    slot: s,
                    proposer: q,
                    key_hash: crypto::hash(&key),
                    parent,
                    roots,
                })?),
                key_hash: crypto::hash(&key),
            };

            Ok((commit, packets))
        })
        .await??;
        let node = &mut self.nodes[id];
        let local = node.slots.entry(s).or_default();
        local.key = Some(key);
        local.own_commit = Some(commit.clone());
        let transport = self.transport.clone();
        let signer = node.key.clone();
        // A task streams each slice before the final commitment boundary, without waiting for votes.
        let start =
            node.windows[&(s / 16)].start + std::time::Duration::from_micros(s % 16 * 50000);
        self.exchange
            .launch(id, s, q, commit, key, start, self.proposer_ids(s / 16)?);
        let mut pending = vec![];
        for (index, shreds) in packets.into_iter().enumerate() {
            pending.push((index, shreds));
        }
        let gateway = self.gateway.clone();
        let identity = self.identities[id].clone();
        let total = pending.len();
        tokio::spawn(async move {
            for (index, shreds) in pending {
                let when = start
                    + std::time::Duration::from_micros(
                        ((index + 1) as u64 * 50000 / total as u64).saturating_sub(1000),
                    );
                tokio::time::sleep_until(when.into()).await;
                let mut groups: BTreeMap<usize, Vec<Shred>> = BTreeMap::new();
                for shred in &shreds {
                    groups
                        .entry(transport.relay(s, index as u32, q, shred.index))
                        .or_default()
                        .push(shred.clone());
                }
                for (to, group) in groups {
                    let body = Wire::Shreds(group);
                    let signature = crypto::sign_bytes(&signer, &encode(&body).unwrap());
                    let bytes = encode(&Envelope {
                        from: id,
                        body,
                        signature,
                    })
                    .unwrap();
                    let t = transport.clone();
                    tokio::spawn(async move {
                        let _ = t.send(id, to, bytes).await;
                    });
                }
                let first = &shreds[0];
                let data = json!({"proposer":q,"slice":{"index":index,"shredCount":64,"rebuildShreds":32,"bytes":first.meta.encrypted_len,"sentUs":offset(Instant::now(),start),"arrivedUs":0,"merkleRoot":hex::encode(first.root)}});
                let mut g = gateway.write().await;
                if let Some(view) = g.slots.get_mut(&s) {
                    crate::projection::apply(view, "block.slice", &data);
                }
                g.emit(
                    Some(identity.clone()),
                    Some(s),
                    "slot",
                    offset(Instant::now(), start),
                    "block.slice",
                    data,
                );
            }
        });
        Ok(())
    }
    async fn prepare_early(&mut self, id: usize, window: u64) -> Result<()> {
        if !self.schedule_ready(window) { return Ok(()) }
        if self.nodes[id].windows.contains_key(&window)
            || self.nodes[id].early.contains_key(&window)
        {
            return Ok(());
        }
        let q = self
            .proposer_ids(window)?
            .iter()
            .position(|n| *n == id)
            .unwrap()
            + 1;
        if self.config.crash_window == Some(window) && q == self.config.crash_proposer {
            return Ok(());
        }
        let slot = window * 16;
        let key = crypto::fresh_key();
        self.retire_builtin(id, slot);
        let node = &mut self.nodes[id];
        let (transactions, fragments) = pack(node, slot, q, None);
        let credits = self.credits_due(id, slot);
        let payload = BlockPayload { transactions, fragments, credits }.encode(self.bridge_open(slot))?;
        ensure!(
            payload.len() <= 1024 * 1024,
            "Proposer block exceeds local size limit"
        );
        let mut roots = vec![];
        let mut next_slice = 0;
        for plain in payload.chunks(38000) {
            let cipher = crypto::encrypt_slice(&key, slot, q, next_slice, plain)?;
            let meta = ShredMeta {
                slot,
                proposer: q,
                slice: next_slice,
                final_slice: false,
                key_hash: crypto::hash(&key),
                parent: None,
                encrypted_len: cipher.len() as u32,
            };
            let shreds = crypto::encode_slice(&self.keys[id], &meta, &cipher)?;
            roots.push(shreds[0].root);
            self.send_slice(id, slot, q, next_slice, shreds).await?;
            next_slice += 1;
        }
        self.nodes[id].early.insert(
            window,
            EarlyBlock {
                key,
                roots,
                next_slice,
            },
        );
        Ok(())
    }
    async fn complete_early(
        &mut self,
        id: usize,
        s: u64,
        q: usize,
        mut early: EarlyBlock,
    ) -> Result<()> {
        let run = self.nodes[id].windows[&(s / 16)];
        let cipher = crypto::encrypt_slice(&early.key, s, q, early.next_slice, &[])?;
        let meta = ShredMeta {
            slot: s,
            proposer: q,
            slice: early.next_slice,
            final_slice: true,
            key_hash: crypto::hash(&early.key),
            parent: run.parent.map(|p| (p.slot, p.hash)),
            encrypted_len: cipher.len() as u32,
        };
        let shreds = crypto::encode_slice(&self.keys[id], &meta, &cipher)?;
        early.roots.push(shreds[0].root);
        let commit = Commitment {
            slot: s,
            proposer: q,
            key_hash: crypto::hash(&early.key),
            hash: crypto::hash(&encode(&BlockCommitment {
                slot: s,
                proposer: q,
                key_hash: crypto::hash(&early.key),
                parent: run.parent,
                roots: early.roots,
            })?),
        };
        self.nodes[id].slots.get_mut(&s).unwrap().own_commit = Some(commit.clone());
        self.nodes[id].slots.get_mut(&s).unwrap().key = Some(early.key);
        self.exchange.launch(
            id,
            s,
            q,
            commit,
            early.key,
            run.start,
            self.proposer_ids(s / 16)?,
        );
        let transport = self.transport.clone();
        let signer = self.keys[id].clone();
        let gateway = self.gateway.clone();
        let identity = self.identities[id].clone();
        tokio::spawn(async move {
            tokio::time::sleep_until((run.start + std::time::Duration::from_micros(49000)).into())
                .await;
            let mut groups: BTreeMap<usize, Vec<Shred>> = BTreeMap::new();
            for shred in &shreds {
                groups
                    .entry(transport.relay(s, early.next_slice, q, shred.index))
                    .or_default()
                    .push(shred.clone());
            }
            for (to, group) in groups {
                let t = transport.clone();
                let body = Wire::Shreds(group);
                let sig = crypto::sign_bytes(&signer, &encode(&body).unwrap());
                let bytes = encode(&Envelope {
                    from: id,
                    body,
                    signature: sig,
                })
                .unwrap();
                tokio::spawn(async move {
                    let _ = t.send(id, to, bytes).await;
                });
            }
            let data = json!({"proposer":q,"slice":{"index":early.next_slice,"shredCount":64,"rebuildShreds":32,"bytes":shreds[0].meta.encrypted_len,"sentUs":offset(Instant::now(),run.start),"arrivedUs":0,"merkleRoot":hex::encode(shreds[0].root)}});
            let mut g = gateway.write().await;
            if let Some(view) = g.slots.get_mut(&s) {
                crate::projection::apply(view, "block.slice", &data);
            }
            g.emit(
                Some(identity),
                Some(s),
                "slot",
                offset(Instant::now(), run.start),
                "block.slice",
                data,
            );
        });
        Ok(())
    }
    async fn send_slice(
        &self,
        id: usize,
        slot: u64,
        q: usize,
        index: u32,
        shreds: Vec<Shred>,
    ) -> Result<()> {
        let mut groups: BTreeMap<usize, Vec<Shred>> = BTreeMap::new();
        for shred in shreds {
            groups
                .entry(self.transport.relay(slot, index, q, shred.index))
                .or_default()
                .push(shred);
        }
        for (to, group) in groups {
            let body = Wire::Shreds(group);
            let signature = crypto::sign_bytes(&self.keys[id], &encode(&body)?);
            let bytes = encode(&Envelope {
                from: id,
                body,
                signature,
            })?;
            let transport = self.transport.clone();
            tokio::spawn(async move {
                let _ = transport.send(id, to, bytes).await;
            });
        }
        Ok(())
    }
    async fn receive(&mut self, packet: Received, e: Envelope) -> Result<()> {
        self.profile.record(
            Some(packet.to),
            "receive.actor_delay",
            packet.received_at.elapsed(),
        );
        let _phase = self.profile.measure(Some(packet.to), "receive.process");
        self.received_packets += 1;
        let id = packet.to;
        let mut step = Step::new();
        match e.body {
            Wire::Submit(tx) => {
                // Judged by the slot it targets: every actor, not only node zero's, admits submissions.
                if self.native_cutover_slot().is_some_and(|cut| tx.header.target_slot >= cut) && self.built_in.contains(&tx.header.fee_payer) { return Ok(()) }
                let valid = self.nodes[id]
                    .executor
                    .as_ref()
                    .unwrap()
                    .validate_transaction(&tx.bytes, tx.header.target_slot)?;
                ensure!(
                    valid.header == tx.header,
                    "Transaction header differs from signed message"
                );
                let target = tx.header.target_slot / 16;
                let window = self.nodes[id].windows.keys().next_back().copied().unwrap_or(0).max(target);
                let role = self.proposer_ids(window)?.iter().position(|x| *x == id).map_or(1, |q| q + 1);
                ensure!(
                    self.nodes[id].executor.as_ref().unwrap().can_pay_inclusion(&tx.header.fee_payer, role as u8, window, 1),
                    "Fee payer cannot pay the inclusion fee"
                );
                self.nodes[id].mempool.insert(tx.header.id.clone(), tx);
            }
            Wire::SubmitFragment(fragment) => {
                let fragment = fragment.revalidate()?;
                // Its target window's proposer in the fragment's role, or that role in a later
                // window the fragment was handed on to.
                let target = fragment.header.target_slot / 16;
                let latest = self.nodes[id].windows.keys().next_back().copied().unwrap_or(0).max(target);
                ensure!(
                    (target..=(latest + 1).min(target + 64))
                        .map(|w| self.proposer_ids(w)).collect::<Result<Vec<_>>>()?.iter().any(|ids| ids[fragment.receiver as usize - 1] == id),
                    "Fragment delivered to wrong proposer"
                );
                self.nodes[id]
                    .fragment_mempool
                    .insert((fragment.header.id.clone(), fragment.index), fragment);
            }
            Wire::Shreds(shreds) => {
                for shred in &shreds {
                    ensure!(self.proposer_ids(shred.meta.slot / 16)?.get(shred.meta.proposer.saturating_sub(1)) == Some(&e.from), "Wrong proposer shred");
                }
                step.out.push(Outbox {
                    from: id,
                    to: (0..12).collect(),
                    body: Wire::Relay(shreds),
                });
            }
            Wire::Relay(shreds) => {
                for shred in shreds {
                    let s = shred.meta.slot;
                    let q = shred.meta.proposer;
                    ensure!((1..=3).contains(&q), "Invalid proposer");

                    let local = self.nodes[id].slots.entry(s).or_default();
                    let index = (q, shred.meta.slice);
                    let pile = local.shreds.entry(index).or_default();
                    if let Some(a) = pile.iter().find(|a| a.index == shred.index) {
                        if a.root != shred.root || a.meta != shred.meta || a.data != shred.data {
                            local.equivocation = true;
                            self.store.put(
                                &format!("equivocation/block/{s}/{q}"),
                                &json!({"first":a,"second":shred}),
                            )?;
                        }
                        continue;
                    }
                    if pile
                        .first()
                        .map_or(false, |a| a.root != shred.root || a.meta != shred.meta)
                    {
                        local.equivocation = true;
                        self.store.put(
                            &format!("equivocation/block/{s}/{q}"),
                            &json!({"first":pile[0],"second":shred}),
                        )?;
                        continue;
                    }
                    pile.push(shred.clone());
                    if pile.len() == 32 {
                        let _phase = self.profile.measure(Some(id), "rotor.reconstruct");
                        let pieces = pile.clone();
                        let cipher = tokio::task::spawn_blocking(move || {
                            crypto::reconstruct_verified_slice(&pieces)
                        })
                        .await??;
                        local.cipher.insert(index, cipher);
                        let meta = shred.meta.clone();
                        local.meta.insert(index, shred.meta);
                        if id == 0 {
                            let base = self.nodes[id].windows.get(&(s / 16)).map(|r| {
                                r.start + std::time::Duration::from_micros(s % 16 * 50000)
                            });
                            if let Some(base) = base {
                                let mut g = self.gateway.write().await;
                                if let Some(view) = g.slots.get_mut(&s) {
                                    if let Some(slice) =
                                        view["blocks"][q - 1]["slices"].as_array_mut().and_then(
                                            |v| v.iter_mut().find(|x| x["index"] == meta.slice),
                                        )
                                    {
                                        slice["arrivedUs"] =
                                            json!(offset(packet.received_at, base));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Wire::Commit(c) => {
                if let Some(run) = self.nodes[id].windows.get(&(c.slot / 16)) {
                    self.exchange
                        .receive_commit(
                            id,
                            e.from,
                            c.clone(),
                            e.signature.clone(),
                            packet.received_at,
                            run.start + std::time::Duration::from_micros(c.slot % 16 * 50000),
                        )
                        .await;
                }
                let s = c.slot;
                let ids = self.proposer_ids(s / 16)?;
                ensure!(
                    ids.get(c.proposer.saturating_sub(1)) == Some(&e.from),
                    "Incorrect commitment proposer"
                );
                let node = &mut self.nodes[id];
                if !node.windows.contains_key(&(s / 16)) {
                    return Ok(());
                }
                let now = packet.received_at;
                let local = node.slots.entry(s).or_default();
                if let Some((_, prev, _)) = local.received.get(&c.proposer) {
                    if prev.hash != c.hash {
                        local.equivocation = true;
                        step.persist.push((format!("equivocation/{s}/{}",c.proposer),json!({"first":prev,"second":c,"firstSignature":local.received[&c.proposer].2,"secondSignature":e.signature})));
                    }
                    if !step.persist.is_empty() {
                        for (k, v) in &step.persist {
                            self.store.put(k, v)?;
                        }
                    }
                    return Ok(());
                }
                local.start = Some(local.start.map_or(now, |a| a.min(now)));
                local
                    .received
                    .insert(c.proposer, (now, c.clone(), e.signature.clone()));
            }
            Wire::Attest(a) => {
                let s = a.slot;
                let ids = self.proposer_ids(s / 16)?;
                ensure!(
                    ids.get(a.proposer.saturating_sub(1)) == Some(&e.from) && a.pi.len() == 3,
                    "Invalid attester"
                );
                let local = self.nodes[id].slots.entry(s).or_default();
                if let Some((old, _)) = local.attest.get(&a.proposer) {
                    if old.pi != a.pi || old.key != a.key {
                        local.equivocation = true;
                        self.store.put(&format!("equivocation/attestation/{s}/{}",a.proposer),&json!({"first":old,"second":a,"firstSignature":local.attest[&a.proposer].1,"secondSignature":e.signature}))?;
                    }
                    return Ok(());
                }
                local
                    .attest
                    .insert(a.proposer, (a.clone(), e.signature.clone()));
                self.attestation_arrivals.lock().unwrap().push((s, packet.received_at));
                let base = self.nodes[id]
                    .windows
                    .get(&(s / 16))
                    .map(|w| w.start + std::time::Duration::from_micros(s % 16 * 50000));
                if let Some(base) = base {
                    let us = offset(packet.received_at, base);
                    step.event(s,us,"attestation.arrived",json!({"attestation":attestation_value(&a,&e.signature,0,vec![]),"validator":self.identities[id],"arrivedUs":us}));
                }
            }
            Wire::Equivocation { first, second } => {
                // Authenticated: both commitments are signed by their slot's proposer and differ.
                let s = first.0.slot;
                let local = self.nodes[id].slots.entry(s).or_default();
                local.equivocation = true;
                let mut evidence = self.lab_evidence.lock().unwrap();
                if !evidence.iter().any(|e| e["kind"] == "slashing_proof") {
                    evidence.push(json!({"kind":"slashing_proof","slot":s,"proposer":first.0.proposer,"txId":null,"description":"Two commitments for one slot, both signed by the same proposer: a slashing proof. Agreement fails and the cblock is skipped.","hashes":[hex::encode(first.0.hash),hex::encode(second.0.hash)],"signatures":[bs58::encode(&first.1).into_string(),bs58::encode(&second.1).into_string()],"observedFragments":null,"threshold":null}));
                }
            }
            Wire::Vote(signed) => {
                if self.nodes[id].votes.contains_key(&encode(&signed.vote)?) {
                    return Ok(());
                }
                ensure!(signed.vote.validator == e.from, "Wrong vote sender");
                let message = vote_message(&signed.vote, self.genesis.as_ref().and_then(|g| g.bridge.as_ref()))?;
                let digest = crypto::hash(&[self.keys[e.from].verifying_key().as_bytes().as_slice(), &message, &signed.signature].concat());
                if !verified_cached(digest) {
                    crypto::verify_bytes(
                        &self.keys[e.from].verifying_key(),
                        &message,
                        &signed.signature,
                    )?;
                    remember_verified(digest);
                }
                self.nodes[id]
                    .votes
                    .insert(encode(&signed.vote)?, signed.clone());
                let actions = self.nodes[id]
                    .votor
                    .receive(consensus::Message::Vote(signed.vote))?;
                self.consensus_actions(id, actions, &mut step)?;
            }
            Wire::Certificate {
                certificate,
                proofs,
            } => {
                if self.nodes[id].seen_certificates.contains(&(
                    certificate.slot,
                    certificate.kind,
                    certificate.hash,
                )) {
                    return Ok(());
                }
                ensure!(
                    certificate.votes.len() == proofs.len(),
                    "Certificate missing proofs"
                );
                for (v, p) in certificate.votes.iter().zip(&proofs) {
                    ensure!(
                        v == &p.vote && v.validator < 12,
                        "Mismatched certificate vote"
                    );
                    let message = vote_message(v, self.genesis.as_ref().and_then(|g| g.bridge.as_ref()))?;
                    let digest = crypto::hash(&[self.keys[v.validator].verifying_key().as_bytes().as_slice(), &message, &p.signature].concat());
                    if !verified_cached(digest) {
                        crypto::verify_bytes(
                            &self.keys[v.validator].verifying_key(),
                            &message,
                            &p.signature,
                        )?;
                        remember_verified(digest);
                    }
                    self.nodes[id].votes.insert(encode(v)?, p.clone());
                }
                let at = self.consensus_time(id, certificate.slot);
                step.events.push(Telemetry {
                    slot: Some(certificate.slot * 4),
                    anchor: "consensus",
                    at,
                    kind: "cblock.certificate",
                    data: view::certificate_json(
                        &certificate,
                        &self.identities,
                        &self.stakes,
                        at,
                        self.first_skipped_consensus_slot(id, certificate.slot),
                    ),
                });
                self.nodes[id].seen_certificates.insert((
                    certificate.slot,
                    certificate.kind,
                    certificate.hash,
                ));
                let forward = Wire::Certificate {
                    certificate: certificate.clone(),
                    proofs: proofs.clone(),
                };
                let actions = self.nodes[id]
                    .votor
                    .receive(consensus::Message::Certificate(certificate.clone()))?;
                let mut targets = self.proposer_ids(certificate.slot / 4)?;
                if self.schedule_ready(certificate.slot / 4 + 1) { targets.extend(self.proposer_ids(certificate.slot / 4 + 1)?); }
                targets.sort_unstable();
                targets.dedup();
                if targets.contains(&id) {
                    step.out.push(Outbox {
                        from: id,
                        to: targets,
                        body: forward,
                    });
                }
                self.consensus_actions(id, actions, &mut step)?;
            }
            Wire::Handover {
                slot,
                parent,
                shreds,
                attestations,
            } => {
                ensure!(
                    slot % 16 == 15 && parent.slot == slot / 4,
                    "Invalid handover"
                );
                if self.nodes[id].executed_blocks.get(&parent.slot) == Some(&parent) {
                    return Ok(());
                }
                for shred in shreds {
                    ensure!(
                        shred.meta.slot / 4 == slot / 4 && (1..=3).contains(&shred.meta.proposer),
                        "Invalid handover shred slot"
                    );
                    let origin = self.proposer_ids(shred.meta.slot / 16)?[shred.meta.proposer - 1];
                    if self.nodes[id]
                        .slots
                        .get(&shred.meta.slot)
                        .and_then(|local| {
                            local.shreds.get(&(shred.meta.proposer, shred.meta.slice))
                        })
                        .map_or(false, |pile| {
                            pile.iter().any(|old| {
                                old.index == shred.index
                                    && old.signature == shred.signature
                                    && old.data == shred.data
                                    && old.meta == shred.meta
                                    && old.root == shred.root
                            })
                        })
                    {
                        continue;
                    }
                    let _ = origin; // shred signatures were verified by the ingress worker
                    let local = self.nodes[id].slots.entry(shred.meta.slot).or_default();
                    let key = (shred.meta.proposer, shred.meta.slice);
                    let pile = local.shreds.entry(key).or_default();
                    if let Some(first) = pile.first() {
                        ensure!(
                            first.root == shred.root && first.meta == shred.meta,
                            "Conflicting handover shred"
                        );
                    }
                    if !pile.iter().any(|s| s.index == shred.index) {
                        pile.push(shred);
                    }
                    if pile.len() >= 32 && !local.cipher.contains_key(&key) {
                        let parts = pile.clone();
                        let cipher = tokio::task::spawn_blocking(move || {
                            crypto::reconstruct_verified_slice(&parts)
                        })
                        .await??;
                        local.cipher.insert(key, cipher);
                        local.meta.insert(key, pile[0].meta.clone());
                    }
                }
                for (att, signature) in attestations {
                    ensure!(
                        att.slot / 4 == slot / 4
                            && (1..=3).contains(&att.proposer)
                            && att.pi.len() == 3,
                        "Invalid handover attestation"
                    );
                    let origin = self.proposer_ids(att.slot / 16)?[att.proposer - 1];
                    crypto::verify_bytes(
                        &self.keys[origin].verifying_key(),
                        &encode(&Wire::Attest(att.clone()))?,
                        &signature,
                    )?;
                    let local = self.nodes[id].slots.entry(att.slot).or_default();
                    if let Some((old, _)) = local.attest.get(&att.proposer) {
                        if old.pi != att.pi || old.key != att.key {
                            local.equivocation = true;
                        }
                    } else {
                        local.attest.insert(att.proposer, (att, signature));
                    }
                }
            }
        }
        self.dispatch(id, step).await
    }
    async fn try_execute(&mut self, id: usize, w: u64) -> Result<()> {
        let _phase = self.profile.measure(Some(id), "validator.scan");
        let certified_skip = self.nodes[id]
            .seen_certificates
            .range(
                (w * 4, consensus::CertificateKind::Notarization, None)
                    ..=(w * 4 + 3, consensus::CertificateKind::Skip, Some([255; 32])),
            )
            .filter(|(_, kind, _)| *kind == consensus::CertificateKind::Skip)
            .map(|(i, _, _)| *i)
            .min();
        let ids = self.proposer_ids(w)?;
        let recipients = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (i as u8 + 1, self.identities[*id].clone()))
            .collect::<Vec<_>>();
        self.nodes[id]
            .executor
            .as_mut()
            .unwrap()
            .set_proposers(&recipients)?;
        for s in w * 16..w * 16 + 16 {
            if certified_skip.map_or(false, |i| s / 4 >= i) {
                break;
            }
            if self.nodes[id].slots.get(&s).map_or(false, |x| x.executed) {
                continue;
            }
            if s % 16 != 0
                && !self.nodes[id]
                    .slots
                    .get(&(s - 1))
                    .map_or(false, |x| x.executed)
            {
                break;
            }
            let Some(local) = self.nodes[id].slots.get(&s) else {
                break;
            };
            let attestations = local
                .attest
                .values()
                .map(|(a, _)| a.clone())
                .collect::<Vec<_>>();
            let agree_start = Instant::now();
            let agreed = crate::agreement::agreed(&attestations, local.equivocation);
            self.profile
                .record(Some(id), "validator.agree", agree_start.elapsed());
            let Some(accepted) = agreed else {
                break;
            };
            let pi = attestations[0].pi.clone();
            if s % 16 != 0 && self.nodes[id].slots[&(s - 1)].agreed.as_ref() != Some(&accepted) {
                break;
            }
            let mut payloads = vec![];
            let mut fragment_payloads = vec![];
            let mut slot_credits: Vec<crate::execution::BridgeCredit> = vec![];
            let bridge_open = self.bridge_open(s);
            let deposits = self.bridge_deposits.lock().unwrap().clone();
            let mut parents = vec![];
            let mut complete = true;
            let mut invalid_reason = None;
            for q in &accepted {
                let finals = local
                    .meta
                    .iter()
                    .filter(|((p, _), m)| p == q && m.final_slice)
                    .collect::<Vec<_>>();
                if finals.len() != 1 {
                    complete = false;
                    break;
                }
                let last = finals[0].1.slice;
                let mut plaintext = vec![];
                let mut roots = vec![];
                for slice in 0..=last {
                    let key = (*q, slice);
                    let Some(cipher) = local.cipher.get(&key) else {
                        complete = false;
                        break;
                    };
                    let meta = &local.meta[&key];
                    let att = &local.attest[q].0;
                    if crypto::hash(&att.key) != meta.key_hash {
                        invalid_reason =
                            Some("Released key does not match block key commitment".to_owned());
                        complete = false;
                        break;
                    }
                    let decrypt_start = Instant::now();
                    match crypto::decrypt_slice(&att.key, s, *q, slice, cipher) {
                        Ok(bytes) => plaintext.extend(bytes),
                        Err(error) => {
                            invalid_reason = Some(error.to_string());
                            complete = false;
                            break;
                        }
                    }
                    self.profile
                        .record(Some(id), "validator.decrypt", decrypt_start.elapsed());
                    roots.push(local.shreds[&key][0].root);
                }
                if !complete {
                    break;
                }
                let parent = finals[0]
                    .1
                    .parent
                    .map(|(slot, hash)| BlockRef { slot, hash });
                parents.push((*q, parent));
                let expected = crypto::hash(&encode(&BlockCommitment {
                    slot: s,
                    proposer: *q,
                    key_hash: finals[0].1.key_hash,
                    parent,
                    roots,
                })?);
                if pi[*q - 1] != Some(expected) {
                    invalid_reason = Some("Block commitment does not match attestation".to_owned());
                    complete = false;
                    break;
                }
                let checked = (|| -> Result<_> {
                    let mut block = BlockPayload::decode(&plaintext)?;
                    // Every credit is a finalized deposit, from the bridge's first slot on.
                    ensure!(block.credits.is_empty() || bridge_open, "Bridge credit before the bridge's first slot");
                    for credit in &block.credits {
                        ensure!(deposits.get(&credit.sequence) == Some(credit), "Bridge credit with no finalized deposit");
                    }
                    for t in &mut block.transactions {
                        let valid = self.nodes[id]
                            .executor
                            .as_ref()
                            .unwrap()
                            .check_transaction(&t.bytes, t.header.target_slot)?;
                        ensure!(
                            valid.header == t.header && valid.header.target_slot <= s,
                            "Signed header or target mismatch"
                        );
                        *t = valid;
                    }
                    for fragment in block.fragments {
                        let fragment = fragment.revalidate()?;
                        ensure!(
                            fragment.receiver as usize == *q,
                            "Fragment carried by wrong proposer"
                        );
                        fragment_payloads.push((*q as u8, fragment));
                    }
                    Ok((block.transactions, block.credits))
                })();
                match checked {
                    Ok((transactions, credits)) => {
                        payloads.push((*q, transactions));
                        slot_credits.extend(credits);
                    }
                    Err(error) => {
                        invalid_reason = Some(error.to_string());
                        complete = false;
                        break;
                    }
                }
            }
            if !complete {
                if let Some(reason) = invalid_reason {
                    self.nodes[id].slots.get_mut(&s).unwrap().invalid_reason = Some(reason);
                }
                break;
            }
            let selected_parent = if s % 16 == 0 {
                parents
                    .iter()
                    .min_by_key(|(q, p)| {
                        (
                            std::cmp::Reverse(parents.iter().filter(|(_, x)| x == p).count()),
                            *q,
                        )
                    })
                    .unwrap()
                    .1
            } else {
                self.nodes[id].windows[&w].parent
            };
            if s % 16 == 0 {
                if let Some(p) = selected_parent {
                    if self.nodes[id].executed_blocks.get(&p.slot) != Some(&p) {
                        break;
                    }
                }
                self.nodes[id].windows.get_mut(&w).unwrap().parent = selected_parent;
                let checkpoint = match selected_parent {
                    Some(parent) => self.nodes[id]
                        .checkpoints
                        .get(&parent.slot)
                        .cloned()
                        .context("Missing executed parent checkpoint")?,
                    None => self.nodes[id].genesis.clone(),
                };
                self.nodes[id]
                    .executor
                    .as_mut()
                    .unwrap()
                    .restore(checkpoint)?;
                self.nodes[id].threshold = match selected_parent {
                    Some(parent) => self.nodes[id]
                        .threshold_checkpoints
                        .get(&parent.slot)
                        .cloned()
                        .context("Missing parent fragment checkpoint")?,
                    None => {
                        crate::threshold::ThresholdCache::new(self.config.fragment_expiry_slots)?
                    }
                };
            }
            self.nodes[id].slots.get_mut(&s).unwrap().payload = payloads.iter().cloned().collect();
            // Restore the selected parent first, then retire legacy money before this slot's
            // inclusion fees. A rollback restores the marker along with every other account.
            self.nodes[id].executor.as_mut().unwrap().activate_native_cutover(s)?;
            self.nodes[id].executor.as_mut().unwrap().prepare_stake_epoch(s)?;
            let mut validation = payloads
                .iter()
                .flat_map(|(q, txs)| {
                    txs.iter()
                        .map(move |tx| (tx.header.fee_payer.clone(), *q as u8, 1u64))
                })
                .collect::<Vec<_>>();
            validation.extend(
                fragment_payloads
                    .iter()
                    .map(|(q, f)| (f.header.fee_payer.clone(), *q, 1)),
            );
            // A copy whose payer can't pay its inclusion fee is dropped by every validator alike.
            let mut admitted = self.nodes[id]
                .executor
                .as_ref()
                .unwrap()
                .admit_inclusions(&validation, w)?
                .into_iter();
            let mut items = vec![];
            let mut source: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            for (q, txs) in payloads {
                for tx in txs {
                    if !admitted.next().unwrap() {
                        continue;
                    }
                    self.nodes[id].executor.as_mut().unwrap().charge_inclusion(
                        &tx.header.fee_payer,
                        q as u8,
                        w,
                        1,
                    )?;
                    source
                        .entry(tx.header.id.clone())
                        .or_default()
                        .push(q as u8);
                    items.push(ExecutionItem {
                        transaction: tx,
                        source_proposers: vec![q as u8],
                    });
                }
            }
            let mut fragment_info = BTreeMap::new();
            let mut pending_updates = vec![];
            let expired_ids = self.nodes[id].threshold.expire(s);
            for (q, fragment) in fragment_payloads {
                if !admitted.next().unwrap() {
                    continue;
                }
                self.nodes[id].executor.as_mut().unwrap().charge_inclusion(
                    &fragment.header.fee_payer,
                    q,
                    w,
                    1,
                )?;
                let header = fragment.header.clone();
                let n = fragment.n;
                let t = fragment.t;
                let included_us = offset(
                    Instant::now(),
                    self.nodes[id].windows[&w].start
                        + std::time::Duration::from_micros(s % 16 * 50000),
                );
                let accepted_fragment =
                    self.nodes[id]
                        .threshold
                        .accept_at(s, included_us, q, fragment);
                let accepted_fragment = match accepted_fragment {
                    Ok(value) => value,
                    Err(error) => {
                        self.nodes[id].threshold.reject(&header.id);
                        let mut failed = crate::submission::transaction_sent(
                            &header,
                            "fragments",
                            n,
                            t,
                            &self.window_json(w)?["proposers"].as_array().unwrap(),
                        );
                        failed["state"] = json!("failed");
                        failed["failure"] = json!(error.to_string());
                        pending_updates.push(failed);
                        continue;
                    }
                };
                match accepted_fragment {
                    crate::threshold::AcceptOutcome::Pending { inclusions, .. } => {
                        pending_updates.push(threshold_transaction(
                            &header,
                            n,
                            t,
                            &inclusions,
                            "partly_included",
                            None,
                            None,
                            None,
                            &self.window_json(w)?,
                            self.config.fragment_expiry_slots,
                        ));
                    }
                    crate::threshold::AcceptOutcome::Decoded {
                        header,
                        bytes,
                        inclusions,
                    } => {
                        let tx = match crate::threshold::validate_decoded(
                            self.nodes[id].executor.as_ref().unwrap(),
                            &header,
                            &bytes,
                        ) {
                            Ok(tx) => tx,
                            Err(error) => {
                                self.nodes[id].threshold.reject(&header.id);
                                let mut failed = threshold_transaction(
                                    &header,
                                    n,
                                    t,
                                    &inclusions,
                                    "failed",
                                    None,
                                    None,
                                    None,
                                    &self.window_json(w)?,
                                    self.config.fragment_expiry_slots,
                                );
                                failed["result"] = json!("error");
                                failed["failure"] = json!(error.to_string());
                                pending_updates.push(failed);
                                continue;
                            }
                        };
                        let mut sources = inclusions.iter().map(|i| i.proposer).collect::<Vec<_>>();
                        sources.sort_unstable();
                        sources.dedup();
                        source.insert(header.id.clone(), sources.clone());
                        fragment_info.insert(header.id.clone(), (n, t, inclusions));
                        items.push(ExecutionItem {
                            transaction: tx,
                            source_proposers: sources,
                        });
                    }
                    crate::threshold::AcceptOutcome::AlreadyDecoded => {}
                }
            }
            // Validator 0 keeps each transaction's signed bytes for Solana's view of it.
            let solana_bytes = if id == 0 {
                items.iter().map(|item| (item.transaction.header.id.clone(), item.transaction.bytes.clone())).collect::<BTreeMap<_, _>>()
            } else {
                BTreeMap::new()
            };
            let execute_start = Instant::now();
            let mut executor = self.nodes[id].executor.take().unwrap();
            slot_credits.sort_by_key(|c| c.sequence);
            slot_credits.dedup_by_key(|c| c.sequence);
            executor.set_credits(slot_credits);
            let (returned, result) = tokio::task::spawn_blocking(move || {
                let result = executor.execute_slot(s, items);
                (executor, result)
            })
            .await?;
            self.nodes[id].executor = Some(returned);
            let execution = result?;
            let executor = self.nodes[id].executor.as_ref().unwrap();
            let decimals = |mint: &str| executor.account(mint).ok().flatten().and_then(|m| m.data.get(44).copied());
            for result in execution.results.iter().filter(|_| id == 0) {
                let Some(bytes) = solana_bytes.get(&result.id) else { continue };
                let Some(signature) = bincode::deserialize::<solana_sdk::transaction::VersionedTransaction>(bytes).ok().and_then(|tx| tx.signatures.first().map(|s| s.to_string())) else { continue };
                let copies = source.get(&result.id).map_or(1, |s| s.len() as u64);
                self.signature_ids.insert(signature.clone(), result.id.clone());
                self.gateway.write().await.signatures.insert(signature.clone(), result.id.clone());
                self.solana_sent.remove(&signature);
                self.solana.record(signature, crate::solana::Executed {
                    id: result.id.clone(),
                    slot: s,
                    time: crate::solana::now(),
                    bytes: bytes.clone(),
                    fee: result.base_fee + result.priority_fee + copies * self.config.inclusion_fee,
                    err: result.error.clone(),
                    logs: result.logs.clone(),
                    units: result.compute_units,
                    pre_balances: {
                        // The inclusion fee left the payer when its copies were admitted.
                        let mut pre = result.before.lamports.clone();
                        if let Some(payer) = pre.first_mut() {
                            *payer += copies * self.config.inclusion_fee;
                        }
                        pre
                    },
                    post_balances: result.after.lamports.clone(),
                    pre_tokens: result.before.tokens.iter().filter_map(|(i, a)| crate::solana::TokenBalance::read(*i, a, &decimals)).collect(),
                    post_tokens: result.after.tokens.iter().filter_map(|(i, a)| crate::solana::TokenBalance::read(*i, a, &decimals)).collect(),
                });
            }
            if id == 0 && s % 1_000 == 0 {
                self.solana.prune(s.saturating_sub(self.config.retain_slots));
                // A signature still in flight drops out here and comes back when it executes.
                let solana = &self.solana;
                self.signature_ids.retain(|signature, _| solana.get(signature).is_some());
                self.solana_sent.retain(|signature, _| solana.get(signature).is_none());
                self.gateway.write().await.signatures.retain(|signature, _| solana.get(signature).is_some());
            }
            self.profile.record(
                Some(id),
                "validator.merge_select_execute",
                execute_start.elapsed(),
            );
            for header in &execution.merged_order {
                if !execution.schedule.selected_ids.contains(&header.id)
                    && self.nodes[id].in_flight.get(&header.id).is_some_and(|(slot, _, _)| *slot <= s)
                {
                    let (_, _, tx) = self.nodes[id].in_flight.remove(&header.id).unwrap();
                    self.nodes[id].mempool.insert(header.id.clone(), tx);
                }
            }
            let base =
                self.nodes[id].windows[&w].start + std::time::Duration::from_micros(s % 16 * 50000);
            let at = offset(Instant::now(), base);
            {
                let local = self.nodes[id].slots.get_mut(&s).unwrap();
                local.executed = true;
                local.agreed = Some(accepted.clone());
                local.state_hash = Some(execution.state_hash.clone());
            }
            self.nodes[id].last_executed = Some(s);
            {
                let mut checks = self.state_checks.lock().unwrap();
                let entry = checks
                    .pending
                    .entry(s)
                    .or_insert((execution.state_hash.clone(), 0));
                if entry.0 != execution.state_hash {
                    checks.mismatches += 1;
                    return Err(anyhow::anyhow!("State hash divergence at slot {s}"));
                } else {
                    entry.1 |= 1u16 << id;
                    if entry.1 == 4095 {
                        checks.checked += 1;
                        checks.pending.remove(&s);
                    }
                }
            }
            if s % 4 == 3 {
                let checkpoint = self.nodes[id].executor.as_mut().unwrap().checkpoint();
                self.nodes[id].checkpoints.insert(s / 4, checkpoint);
                let threshold = self.nodes[id].threshold.clone();
                self.nodes[id]
                    .threshold_checkpoints
                    .insert(s / 4, threshold);
            }
            let mut step = Step::new();
            step.event(s, at, "slot.agreed", json!({"agreedSet":accepted}));
            for q in &accepted {
                step.event(s,at,"block.decrypted",json!({"proposer":q,"transactionIds":execution.merged_order.iter().filter(|h|source[&h.id].contains(&(*q as u8))).map(|h|h.id.clone()).collect::<Vec<_>>()}));
            }
            let merged=execution.merged_order.iter().enumerate().map(|(position,h)|{let mut v=serde_json::to_value(h).unwrap();let selected=execution.schedule.selected_ids.contains(&h.id);let sources=&source[&h.id];v.as_object_mut().unwrap().extend(json!({"kind":if fragment_info.contains_key(&h.id){"fragments"}else{"plain"},"sourceProposers":sources,"n":fragment_info.get(&h.id).map(|f|f.0 as usize).unwrap_or(sources.len()),"t":fragment_info.get(&h.id).map(|f|f.1).unwrap_or(1),"fragmentIndices":fragment_info.get(&h.id).map(|f|f.2.iter().map(|i|i.index).collect::<Vec<_>>()).unwrap_or_default(),"position":position,"selected":selected,"exclusion":if selected{Value::Null}else{json!("capacity")}}).as_object().unwrap().clone());v}).collect::<Vec<_>>();
            step.event(s, at, "slot.merged", json!({"transactions":merged}));
            let sched = json!({"algorithm":4,"tracks":4,"capacityCu":self.config.track_capacity_cu,"placements":execution.schedule.placements,"gaps":execution.schedule.gaps});
            step.event(s, at, "slot.scheduled", sched.clone());
            let mut view = match self.slot_views.get(&s).cloned() {
                Some(view) => view,
                None => view::pending_slot(s,self.window_json(w)?,utc(self.wall + offset(base, self.origin) / 1000)),
            };
            view["status"] = json!("executed");
            view["agreedSet"] = json!(accepted);
            view["mergedOrder"] = json!(merged);
            view["schedule"] = sched;
            view["stateHash"] = json!(execution.state_hash);
            view["xblockHash"] = json!(hex::encode(crypto::hash(&encode(&(s, &pi))?)));
            view["executedUs"] = json!(at);
            if let Some(existing) = self.gateway.read().await.slots.get(&s) {
                view["blocks"] = existing["blocks"].clone();
                view["attestations"] = existing["attestations"].clone();
            }
            for block in view["blocks"].as_array_mut().unwrap() {
                block["status"] = json!(if accepted
                    .contains(&(block["proposer"].as_u64().unwrap() as usize))
                {
                    "accepted"
                } else {
                    "missing"
                });
            }
            step.event(s, at, "slot.executed", view.clone());
            if id == 0 {
                for expired in expired_ids {
                    if let Some(tx) = self.gateway.write().await.transactions.get_mut(&expired) {
                        tx["state"] = json!("expired");
                        tx["failure"] = json!("Fragment threshold was not reached before expiry");
                        pending_updates.push(tx.clone());
                    }
                }
                for tx in pending_updates {
                    let tx_id = tx["header"]["id"].as_str().unwrap().to_owned();
                    self.gateway
                        .write()
                        .await
                        .transactions
                        .insert(tx_id.clone(), tx.clone());
                    step.persist
                        .push((format!("transaction/{tx_id}"), tx.clone()));
                    step.event(s, at, "transaction.updated", tx);
                }
                for (position, h) in execution.merged_order.iter().enumerate() {
                    let outcome = execution.results.iter().find(|r| r.id == h.id);
                    // A plain transaction the merge did not place (its track was full) is not
                    // failed: its proposer carries it again in a later slot.
                    if outcome.is_none() && !fragment_info.contains_key(&h.id) {
                        continue;
                    }
                    let mut tx = json!({"header":h,"state":if outcome.map_or(false,|r|r.success){"executed"}else{"failed"},"kind":"plain","n":source[&h.id].len(),"t":1,"receivers":self.window_json(w)?["proposers"],"fragments":source[&h.id].iter().map(|p|json!({"index":*p-1,"receiver":p,"slot":s,"proposer":p,"includedUs":at})).collect::<Vec<_>>(),"includedCount":source[&h.id].len(),"readableSlot":s,"executedSlot":s,"mergedPosition":position,"result":if outcome.map_or(false,|r|r.success){"success"}else{"error"},"logs":outcome.map(|r|r.logs.clone()).unwrap_or_default(),"failure":outcome.and_then(|r|r.error.clone()),"expiresAfterSlot":null,"finalization":null});
                    if let Some((n, t, inclusions)) = fragment_info.get(&h.id) {
                        tx = threshold_transaction(
                            h,
                            *n,
                            *t,
                            inclusions,
                            if outcome.map_or(false, |r| r.success) {
                                "executed"
                            } else {
                                "failed"
                            },
                            Some(s),
                            Some(position),
                            outcome,
                            &self.window_json(w)?,
                            self.config.fragment_expiry_slots,
                        );
                    }
                    if let Some(result) = outcome {
                        // What the payer paid, in lamports (native ACRUX from the cut-over): each copy or
                        // fragment's inclusion fee when admitted, then the base and priority fees.
                        let copies = fragment_info.get(&h.id).map_or(source[&h.id].len(), |(_, _, inclusions)| inclusions.len()) as u64;
                        let inclusion = copies * self.config.inclusion_fee;
                        let burned = result.base_fee * u64::from(crate::execution::Fees::default().base_burn_bps) / 10_000;
                        tx["fees"] = json!({"base": result.base_fee.to_string(), "priority": result.priority_fee.to_string(), "inclusion": inclusion.to_string(),
                            "burned": burned.to_string(), "total": (result.base_fee + result.priority_fee + inclusion).to_string()});
                    }
                    step.persist
                        .push((format!("transaction/{}", h.id), tx.clone()));
                    self.gateway
                        .write()
                        .await
                        .transactions
                        .insert(h.id.clone(), tx.clone());
                    step.event(s, at, "transaction.updated", tx);
                }
            }
            self.dispatch(id, step).await?;
            if s % 16 == 15 && self.schedule_ready(w + 1) && self.proposer_ids(w + 1)?.contains(&id) {
                self.prepare_early(id, w + 1).await?;
            }
        }
        Ok(())
    }
    async fn try_vote(&mut self, id: usize, w: u64) -> Result<()> {
        let _phase = self.profile.measure(Some(id), "validator.vote");
        for i in w * 4..w * 4 + 4 {
            let already_voted = self.nodes[id].voted.contains(&i);
            if i % 4 != 0 && !self.nodes[id].voted.contains(&(i - 1)) {
                break;
            }
            let now = Instant::now();
            let run = self.nodes[id].windows[&w];
            let expired = elapsed(run.start) >= 750000 + (i - w * 4 + 1) * 200000 + 50000;
            let slots = (i * 4..i * 4 + 4).collect::<Vec<_>>();
            let all = slots
                .iter()
                .all(|s| self.nodes[id].slots.get(s).map_or(false, |x| x.executed));
            let same = all
                && slots.iter().all(|s| {
                    self.nodes[id].slots[s].agreed == self.nodes[id].slots[&slots[0]].agreed
                });
            let disagreement = slots.iter().any(|s| {
                self.nodes[id].slots.get(s).map_or(false, |x| {
                    x.equivocation
                        || x.invalid_reason.is_some()
                        || x.attest
                            .values()
                            .map(|(a, _)| &a.pi)
                            .collect::<BTreeSet<_>>()
                            .len()
                            > 1
                })
            });
            let mut step = Step::new();
            let actions;
            let certified_skip = self.nodes[id].seen_certificates.contains(&(
                i,
                consensus::CertificateKind::Skip,
                None,
            ));
            if all
                && same
                && !disagreement
                && !certified_skip
                && (!expired || already_voted)
                && !self.nodes[id].executed_blocks.contains_key(&i)
            {
                if !already_voted
                    && i % 4 == 0
                    && elapsed(run.start) < 570000
                    && self.nodes[id].slots[&(w * 16)].attest.len() < 3
                {
                    break;
                }
                let parent = if i % 4 == 0 {
                    run.parent
                } else {
                    self.nodes[id].executed_blocks.get(&(i - 1)).copied()
                };
                if i % 4 != 0 && parent.is_none() {
                    break;
                }
                let state = self.nodes[id].slots[&(i * 4 + 3)]
                    .state_hash
                    .clone()
                    .unwrap();
                let accepted = slots
                    .iter()
                    .map(|s| {
                        (
                            *s,
                            self.nodes[id].slots[s].agreed.clone(),
                            self.nodes[id].slots[s]
                                .attest
                                .values()
                                .next()
                                .unwrap()
                                .0
                                .pi
                                .clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                // From the bridge's first slot the hash also commits to the cblock's outbox root,
                // which Solana's light client proves outbox entries against.
                let preimage = encode(&(i, parent, accepted, &state))?;
                let hash = if self.bridge_open(i * 4) {
                    let mut outbox = self.outbox(i);
                    outbox.extend(self.nodes[id].executor.as_ref().unwrap().withdrawals(i).into_iter().map(|(sequence, solana_mint, recipient, amount)| {
                        crate::bridge::OutboxEntry::Withdrawal { sequence, solana_mint, recipient, amount }
                    }));
                    // Voting can trail execution: read the epoch close from this cblock's own
                    // hashed checkpoint, not the marker of a later executed epoch.
                    if let Some(burn) = self.nodes[id].executor.as_ref().unwrap().fee_burn_at(self.nodes[id].checkpoints.get(&i).context("Bridge cblock checkpoint is missing")?, i)? {
                        outbox.push(crate::bridge::OutboxEntry::FeeBurn { epoch:burn.epoch,amount:burn.amount });
                    }
                    let hash = crypto::hash(&[preimage.as_slice(), &crate::bridge::outbox_root(&outbox)].concat());
                    if id == 0 {
                        self.cblock_preimages.insert(i, (preimage, outbox));
                        ensure!(self.cblock_preimages.len() <= 4096, "Bridge proof material reached its pending cblock limit before finalization");
                    }
                    hash
                } else {
                    crypto::hash(&preimage)
                };
                let block = consensus::Block {
                    id: BlockRef { slot: i, hash },
                    parent,
                };
                self.nodes[id].executed_blocks.insert(i, block.id);
                if i % 4 == 3 && self.schedule_ready(w + 1) && self.proposer_ids(w)?.contains(&id) {
                    let next = self.proposer_ids(w + 1)?;
                    // 32 shreds rebuild a slice, so no more go; the rest is split into messages
                    // under the network limit, the attestations riding on the first.
                    let shreds = (i * 4..i * 4 + 4)
                        .flat_map(|slot| {
                            self.nodes[id].slots[&slot]
                                .shreds
                                .values()
                                .flat_map(|pile| pile.iter().take(32))
                                .cloned()
                        })
                        .collect::<Vec<_>>();
                    let mut attestations = (i * 4..i * 4 + 4)
                        .flat_map(|slot| self.nodes[id].slots[&slot].attest.values().cloned())
                        .collect::<Vec<_>>();
                    for part in handover_parts(shreds) {
                        step.out.push(Outbox {
                            from: id,
                            to: next.clone(),
                            body: Wire::Handover {
                                slot: i * 4 + 3,
                                parent: block.id,
                                shreds: part,
                                attestations: std::mem::take(&mut attestations),
                            },
                        });
                    }
                }
                actions = self.nodes[id].votor.local_block(block)?;
                let c = view::cblock_json(i, parent, &hex::encode(hash), &state, "pending");
                step.events.push(Telemetry {
                    slot: Some(i * 4),
                    anchor: "consensus",
                    at: offset(
                        now,
                        run.start + std::time::Duration::from_micros(i % 4 * 200000),
                    ),
                    kind: "cblock.assembled",
                    data: c,
                });
            } else if already_voted {
                continue;
            } else if expired || disagreement || run.skipped_from.is_some() {
                actions = self.nodes[id].votor.timeout(i)?;
                self.nodes[id]
                    .windows
                    .get_mut(&w)
                    .unwrap()
                    .skipped_from
                    .get_or_insert(i);
                let reason = if disagreement {
                    "disagreeing_vectors"
                } else {
                    "missing_attestation"
                };
                for s in i * 4..w * 16 + 16 {
                    let vectors = self.nodes[id]
                        .slots
                        .get(&s)
                        .map(|slot| {
                            slot.attest
                                .values()
                                .map(|(a, sig)| attestation_value(a, sig, 0, vec![]))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    let skip = json!({"reason":reason,"detail":"The validator cannot agree and execute this consensus block before its deadline.","disagreeingVectors":vectors,"affectedSlots":(i*4..w*16+16).collect::<Vec<_>>(),"atUs":offset(now,run.start)});
                    step.events.push(Telemetry {
                        slot: Some(s),
                        anchor: "window",
                        at: offset(now, run.start),
                        kind: "slot.skipped",
                        data: skip,
                    });
                }
            } else {
                break;
            }
            self.nodes[id].voted.insert(i);
            self.consensus_actions(id, actions, &mut step)?;
            self.dispatch(id, step).await?;
        }
        Ok(())
    }
    fn first_skipped_consensus_slot(&self, id: usize, slot: u64) -> Option<u64> {
        let first = slot / 4 * 4;
        let observed = self.nodes[id]
            .seen_certificates
            .range(
                (first, consensus::CertificateKind::Notarization, None)
                    ..=(slot, consensus::CertificateKind::Skip, Some([255; 32])),
            )
            .filter_map(|(i, kind, _)| (*kind == consensus::CertificateKind::Skip).then_some(*i));
        observed
            .chain(
                self.nodes[id]
                    .windows
                    .get(&(slot / 4))
                    .and_then(|w| w.skipped_from),
            )
            .filter(|i| *i >= first && *i < slot)
            .min()
    }
    fn consensus_actions(
        &mut self,
        id: usize,
        actions: Vec<Action>,
        step: &mut Step,
    ) -> Result<()> {
        for action in actions {
            match action {
                Action::SendVote(vote) => {
                    if vote.slot % 4 == 3 {
                        self.handover_trace.lock().unwrap().entry(vote.slot / 4 + 1).or_default().push((id, "vote", Instant::now()));
                    }
                    let message = vote_message(&vote, self.genesis.as_ref().and_then(|g| g.bridge.as_ref()))?;
                    let signature = crypto::sign_bytes(&self.keys[id], &message);
                    let signed = SignedVote {
                        vote: vote.clone(),
                        signature,
                    };
                    self.nodes[id].votes.insert(encode(&vote)?, signed.clone());
                    step.out.push(Outbox {
                        from: id,
                        to: (0..12).collect(),
                        body: Wire::Vote(signed),
                    });
                    let at = self.consensus_time(id, vote.slot);
                    step.events.push(Telemetry {
                        slot: Some(vote.slot * 4),
                        anchor: "consensus",
                        at,
                        kind: "cblock.vote",
                        data: view::vote_json(&vote, &self.identities, &self.stakes, at),
                    });
                }
                Action::SendCertificate(c) => {
                    self.nodes[id]
                        .seen_certificates
                        .insert((c.slot, c.kind, c.hash));
                    let proofs = c
                        .votes
                        .iter()
                        .map(|v| {
                            self.nodes[id]
                                .votes
                                .get(&encode(v).unwrap())
                                .cloned()
                                .context("Missing signed certificate vote")
                        })
                        .collect::<Result<Vec<_>>>()?;
                    step.out.push(Outbox {
                        from: id,
                        to: (0..12).collect(),
                        body: Wire::Certificate {
                            certificate: c.clone(),
                            proofs,
                        },
                    });
                    let at = self.consensus_time(id, c.slot);
                    step.events.push(Telemetry {
                        slot: Some(c.slot * 4),
                        anchor: "consensus",
                        at,
                        kind: "cblock.certificate",
                        data: view::certificate_json(
                            &c,
                            &self.identities,
                            &self.stakes,
                            at,
                            self.first_skipped_consensus_slot(id, c.slot),
                        ),
                    });
                }
                Action::Finalized(block) => {
                    if let Some(checkpoint) = self.nodes[id].checkpoints.get(&block.slot) {
                        let snapshots = self.nodes[id].executor.as_ref().unwrap().stake_schedule_snapshots(checkpoint)?;
                        let mut schedules = self.proposers.write().unwrap();
                        for (epoch, weights) in snapshots { schedules.learn(epoch, weights)?; }
                    }
                    self.sign_finalized_ancestors(id, block, step)?;
                    let at = self.consensus_time(id, block.slot);
                    let state = self.nodes[id]
                        .slots
                        .get(&(block.slot * 4 + 3))
                        .and_then(|x| x.state_hash.clone())
                        .context("Finalized cblock is missing its executed state hash")?;
                    let parent = if block.slot % 4 == 0 {
                        self.nodes[id]
                            .windows
                            .get(&(block.slot / 4))
                            .and_then(|r| r.parent)
                    } else {
                        self.nodes[id]
                            .executed_blocks
                            .get(&(block.slot - 1))
                            .copied()
                    };
                    let mut c = view::cblock_json(
                        block.slot,
                        parent,
                        &hex::encode(block.hash),
                        &state,
                        "finalized",
                    );
                    c["finalizedUs"] = json!(at);
                    step.events.push(Telemetry {
                        slot: Some(block.slot * 4),
                        anchor: "consensus",
                        at,
                        kind: "cblock.finalized",
                        data: c,
                    });
                }
                Action::ParentReady {
                    window_first_slot,
                    parent,
                } => {
                    let mut trace = self.handover_trace.lock().unwrap();
                    trace.entry(window_first_slot / 4).or_default().push((id, "ready", Instant::now()));
                    while trace.len() > 128 {
                        trace.pop_first();
                    }
                    drop(trace);
                    self.nodes[id]
                        .pending_starts
                        .push((window_first_slot / 4, parent))
                }
            }
        }
        Ok(())
    }
    fn consensus_time(&self, id: usize, i: u64) -> u64 {
        self.nodes[id]
            .windows
            .get(&(i / 4))
            .map(|w| elapsed(w.start).saturating_sub(i % 4 * 200000))
            .unwrap_or(0)
    }
    fn native_cutover_slot(&self) -> Option<u64> {
        self.genesis.as_ref().and_then(|g| g.bridge.as_ref()).and_then(|bridge| bridge.native_from_slot)
    }
    fn retire_builtin(&mut self, id: usize, slot: u64) {
        if !self.native_cutover_slot().is_some_and(|cut| slot >= cut) { return }
        // This public payer set is present on every actor, unlike node zero's private keys.
        let retired = &self.built_in;
        self.nodes[id].mempool.retain(|_, tx| !retired.contains(&tx.header.fee_payer));
    }
    async fn traffic(&mut self) -> Result<()> {
        let s = self.latest_started() + 3;
        if self.native_cutover_slot().is_some_and(|cut| s >= cut) { return Ok(()) }
        let ids = self.proposer_ids(s / 16)?;
        let n = self.traffic_seq as usize;
        self.traffic_seq += 1;
        let sender = &self.senders[n % self.senders.len()];
        let receiver = self.senders[(n + 1) % self.senders.len()].pubkey();
        let hash = self.nodes[0]
            .executor
            .as_ref()
            .unwrap()
            .latest_blockhash()
            .parse::<Hash>()?;
        let tx = Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(20000 + (n % 5) as u32 * 1000),
                ComputeBudgetInstruction::set_compute_unit_price(100 + (n % 17) as u64 * 100),
                system_instruction::transfer(&sender.pubkey(), &receiver, 1000 + (n % 100) as u64),
            ],
            Some(&sender.pubkey()),
            &[sender],
            hash,
        );
        let valid = self.nodes[0]
            .executor
            .as_ref()
            .unwrap()
            .validate_transaction(&encode(&tx)?, s)?;
        let mut step = Step::new();
        step.out.push(Outbox {
            from: 0,
            to: ids,
            body: Wire::Submit(valid),
        });
        self.dispatch(0, step).await
    }
    async fn command(&mut self, command: GatewayCommand) {
        match command {
            GatewayCommand::LatestBlockhash { reply } => {
                let _=reply.send(Ok(json!({"blockhash":self.nodes[0].executor.as_ref().unwrap().latest_blockhash(),"slot":self.nodes[0].last_executed.unwrap_or(0),"startedSlot":self.latest_started()})));
            }
            GatewayCommand::SubmitFragment { params, reply } => {
                let result = self.submit_fragment(params).await;
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            GatewayCommand::SubmitPlain { params, reply } => {
                let result = self.submit(params).await;
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            GatewayCommand::ExecutionSnapshot { reply } => {
                let result = (|| {
                    let node = &self.nodes[0];
                    let finalized = self.finalized_checkpoint()?;
                    let checkpoint = node.checkpoints.get(&finalized.slot).context("Finalized checkpoint not held")?;
                    let threshold = node.threshold_checkpoints.get(&finalized.slot).context("Finalized fragment cache not held")?;
                    let executor = node.executor.as_ref().unwrap().snapshot(checkpoint);
                    Ok::<_, anyhow::Error>(json!({
                        "consensusSlot": finalized.slot,
                        "lastSlot": finalized.slot * 4 + 3,
                        "anchor": {"consensusSlot": finalized.slot, "hash": hex::encode(finalized.hash), "stateHash": executor.state_hash},
                        "executor": executor,
                        "threshold": threshold,
                        "proposerConfig": self.proposers.read().unwrap().config,
                    }))
                })();
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            GatewayCommand::SlotBlocks { slot, reply } => {
                let result = self.slot_blocks(slot, false);
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            GatewayCommand::FollowerBlocks { from, count, reply } => {
                let blocks = (from..from + count).map(|slot| self.slot_blocks(slot, true).ok()).collect::<Vec<_>>();
                let _ = reply.send(Ok(json!(blocks)));
            }
            GatewayCommand::DemoPool { reply } => {
                let _ = reply.send(self.pool_reserves().map(|(genesis, reserves, slot)| genesis.demo_pool(reserves, slot)).map_err(|e| e.to_string()));
            }
            GatewayCommand::Quote { params, reply } => {
                let result = (|| {
                    let (genesis, reserves, slot) = self.pool_reserves()?;
                    let input = params["inputMint"].as_str().context("inputMint")?;
                    let amount = params["amountIn"].as_str().context("amountIn")?.parse::<u64>()?;
                    let slippage = params["slippageBps"].as_u64().context("slippageBps")?;
                    genesis.quote(reserves, input, amount, slippage, slot)
                })();
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            GatewayCommand::Balance { address, reply } => {
                let result=self.nodes[0].executor.as_ref().unwrap().balance(&address).map(|value|json!({"address":address,"lamports":value.to_string(),"reserveLamports":self.config.reserve.to_string(),"spendableLamports":value.saturating_sub(self.config.reserve).to_string(),"slot":self.nodes[0].last_executed.unwrap_or(0)})).map_err(|e|e.to_string());
                let _ = reply.send(result);
            }
            GatewayCommand::Solana { method, params, client, reply } => {
                let result = self.solana_rpc(&method, params, client).await;
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            GatewayCommand::Faucet { reply } => {
                let _ = reply.send(self.faucet_available().map_err(|e| e.to_string()));
            }
            GatewayCommand::ProposerEpoch { epoch, reply } => {
                let result = self.proposers.read().unwrap().epoch(epoch).map_err(|error| error.to_string());
                let _ = reply.send(result);
            }
            GatewayCommand::NativeSupply { reply } => {
                let result = (|| {
                    let supply = self.nodes[0].executor.as_ref().unwrap().native_supply()?;
                    Ok::<_, anyhow::Error>(supply.map_or(Value::Null, |supply| json!({
                        "cutoverSlot": supply.cutover_slot, "mint": supply.solana_mint.to_string(), "scale": supply.lamports_per_base_unit.to_string(),
                        "reserveLamports": supply.reserve_lamports.to_string(), "creditedLamports": supply.credited_lamports.to_string(),
                        "withdrawnLamports": supply.withdrawn_lamports.to_string(), "circulatingLamports": supply.circulating_lamports.to_string(),
                        "feeBurnedLamports": supply.fee_burned_lamports.to_string(),
                    })))
                })();
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            GatewayCommand::Credit {
                address,
                lamports,
                reply,
            } => {
                let result = self.faucet_transfer(&address, lamports).await;
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
        }
    }
    /// A snapshot must not expose finality beyond the durable checkpoint. Follow the actual
    /// parent chain, since a skipped window can make the preceding numeric cblock an orphan.
    fn finalized_checkpoint(&self) -> Result<BlockRef> {
        let node = &self.nodes[0];
        let mut block = node.votor.finalized().context("Nothing finalized yet")?;
        let durable = crate::state::durable();
        while block.slot > durable {
            block = if block.slot % 4 == 0 {
                node.windows.get(&(block.slot / 4)).and_then(|w| w.parent)
            } else {
                node.executed_blocks.get(&(block.slot - 1)).copied()
            }.context("No durable finalized ancestor held")?;
        }
        Ok(block)
    }

    fn slot_blocks(&self, slot: u64, signed_attestations: bool) -> Result<Value> {
        let local = self.nodes[0].slots.get(&slot).context("Slot not held")?;
        let agreed = local.agreed.as_ref().context("Slot not agreed")?;
        ensure!(!local.cipher.is_empty(), "Blocks no longer held");
        let blocks = agreed.iter().map(|q| {
            let slices = local.meta.iter().filter(|((p, _), _)| p == q).map(|((_, i), meta)| json!({
                "meta": meta,
                "cipher": local.cipher.get(&(*q, *i)).map(|c| B64.encode(c)),
                "root": local.shreds.get(&(*q, *i)).and_then(|s| s.first()).map(|s| hex::encode(s.root)),
            })).collect::<Vec<_>>();
            json!({"proposer": q, "slices": slices})
        }).collect::<Vec<_>>();
        let attestations = local.attest.values().map(|(a, signature)| {
            let mut attestation = json!({"proposer": a.proposer, "pi": a.pi.iter().map(|h| h.map(hex::encode)).collect::<Vec<_>>(), "key": B64.encode(a.key)});
            if signed_attestations { attestation["signature"] = json!(bs58::encode(signature).into_string()); }
            attestation
        }).collect::<Vec<_>>();
        Ok(json!({"slot": slot, "agreed": agreed, "blocks": blocks, "attestations": attestations}))
    }

    /// The demo pool's vault balances, read from node zero's executed state.
    fn pool_reserves(&self) -> Result<(Arc<crate::genesis::Genesis>, [u64; 2], u64)> {
        let genesis = self.genesis.clone().context("No demo pool in this net's genesis")?;
        let executor = self.nodes[0].executor.as_ref().unwrap();
        let amount = |vault: &str| -> Result<u64> { Ok(executor.account(vault)?.as_ref().map_or(0, crate::genesis::token_amount)) };
        let reserves = [amount(&genesis.pool.vault_a)?, amount(&genesis.pool.vault_b)?];
        Ok((genesis, reserves, self.nodes[0].last_executed.unwrap_or(0)))
    }
    /// What a traffic sender keeps for its own transactions: `SENDER_RESERVE_DAYS` of them.
    fn sender_reserve(&self) -> u64 {
        let per_sender_per_second = self.config.traffic_per_second as f64 / self.senders.len().max(1) as f64;
        (per_sender_per_second * 86_400.0 * SENDER_RESERVE_DAYS as f64) as u64 * TRAFFIC_TX_LAMPORTS
    }
    /// Who else can pay a faucet grant, with what each keeps: the validators' identities, the fees
    /// they earned above what they started with, and the traffic's senders, above their reserve.
    fn faucet_funders(&self) -> Vec<(Keypair, u64)> {
        let validators = self.keys.iter().map(|k| (solana_sdk::signer::keypair::keypair_from_seed(&k.to_bytes()).expect("a validator key is an ed25519 seed"), VALIDATOR_START_LAMPORTS));
        let senders = self.senders.iter().map(|k| (k.insecure_clone(), self.sender_reserve()));
        validators.chain(senders).collect()
    }
    /// What the faucet can still grant: the genesis faucet's balance and what every other funder holds
    /// above what it keeps.
    fn require_faucet(&self, slot: u64) -> Result<()> {
        if let Some(cut) = self.native_cutover_slot().filter(|cut| slot >= *cut) { anyhow::bail!("FAUCET_RETIRED: {cut}") }
        Ok(())
    }
    fn faucet_available(&self) -> Result<Value> {
        self.require_faucet(self.latest_started())?;
        let executor = self.nodes[0].executor.as_ref().unwrap();
        let funds = |k: &Keypair| executor.balance(&k.pubkey().to_string()).unwrap_or(0);
        let faucet = self.genesis.as_ref().map_or(0, |g| funds(&g.faucet));
        let funders = self.faucet_funders();
        let spare = |range: std::ops::Range<usize>| funders[range].iter().map(|(k, keep)| funds(k).saturating_sub(*keep)).sum::<u64>();
        let (validators, senders) = (spare(0..self.keys.len()), spare(self.keys.len()..funders.len()));
        Ok(json!({"availableLamports": (faucet + validators + senders).to_string(), "faucetLamports": faucet.to_string(),
            "validatorsSpareLamports": validators.to_string(), "sendersSpareLamports": senders.to_string(),
            "senderReserveLamports": self.sender_reserve().to_string(), "slot": self.nodes[0].last_executed.unwrap_or(0)}))
    }
    async fn faucet_transfer(&mut self, address: &str, lamports: u64) -> Result<Value> {
        let target = self.latest_started() + SEND_LEAD_SLOTS;
        self.require_faucet(target)?;
        let recipient: solana_sdk::pubkey::Pubkey = address.parse()?;
        let genesis = self.genesis.clone();
        // The grant's ACRUX comes from the genesis faucet while it can pay, then from whichever
        // validator or traffic sender has the most to spare; with no one able to pay, the grant is
        // refused, not sent.
        let executor = self.nodes[0].executor.as_ref().unwrap();
        let need = lamports + FAUCET_MARGIN;
        let funds = |k: &Keypair| executor.balance(&k.pubkey().to_string()).unwrap_or(0);
        let funders = self.faucet_funders();
        let funder = genesis.as_ref().map(|g| &g.faucet).filter(|f| funds(f) >= need)
            .or_else(|| funders.iter().map(|(k, keep)| (k, funds(k).saturating_sub(*keep))).max_by_key(|(_, spare)| *spare).filter(|(_, spare)| *spare >= need).map(|(k, _)| k))
            .context("FAUCET_EMPTY: The faucet cannot pay this grant.")?;
        let (signers, instructions): (Vec<&Keypair>, _) = match &genesis {
            // The genesis faucet also hands out demo tokens.
            Some(genesis) if funder.pubkey() == genesis.faucet.pubkey() => (vec![funder], genesis.faucet_instructions(&funder.pubkey(), &recipient, lamports)?),
            Some(genesis) => (vec![funder, &genesis.faucet], genesis.faucet_instructions(&funder.pubkey(), &recipient, lamports)?),
            None => (vec![funder], vec![system_instruction::transfer(&funder.pubkey(), &recipient, lamports)]),
        };
        let tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&funder.pubkey()),
            &signers,
            self.nodes[0]
                .executor
                .as_ref()
                .unwrap()
                .latest_blockhash()
                .parse()?,
        );
        let valid = self.nodes[0]
            .executor
            .as_ref()
            .unwrap()
            .validate_transaction(&encode(&tx)?, target)?;
        let transaction_id = valid.header.id.clone();
        let signature = tx.signatures[0].to_string();
        self.record_sent(&valid.header, "plain", 3, 1).await?;
        let mut step = Step::new();
        step.out.push(Outbox {
            from: 0,
            to: self.proposer_ids(target / 16)?,
            body: Wire::Submit(valid),
        });
        self.dispatch(0, step).await?;
        let balance = self.nodes[0].executor.as_ref().unwrap().balance(address)?;
        Ok(
            json!({"transactionId":transaction_id,"signature":signature,"balance":{"address":address,"lamports":balance.to_string(),"reserveLamports":self.config.reserve.to_string(),"spendableLamports":balance.saturating_sub(self.config.reserve).to_string(),"slot":self.nodes[0].last_executed.unwrap_or(0)}}),
        )
    }
    fn prune_admissions(&mut self) {
        let horizon = self.latest_started().saturating_sub(crate::execution::BLOCKHASH_AGE_SLOTS);
        while let Some(entry) = self.admission_slots.first_entry() {
            if *entry.key() >= horizon {
                break;
            }
            for key in entry.remove() {
                self.admissions.remove(&key);
            }
        }
    }
    /// Queues validator 0's state at a cblock it just finalized for the state writer.
    fn save_state(&self, cblock: u64) -> Result<()> {
        let Some(writer) = &self.state_writer else {
            return Ok(());
        };
        let node = &self.nodes[0];
        let (Some(block), Some(checkpoint), Some(threshold)) = (node.executed_blocks.get(&cblock), node.checkpoints.get(&cblock), node.threshold_checkpoints.get(&cblock)) else {
            ensure!(!self.bridge_open(cblock * 4), "Finalized bridge cblock is missing its state checkpoint");
            return Ok(());
        };
        let meta = crate::state::Meta {
            format: crate::state::FORMAT,
            cblock,
            hash: hex::encode(block.hash),
            executor: node.executor.as_ref().unwrap().snapshot_meta(checkpoint),
            threshold: threshold.clone(),
            senders: self.senders.iter().map(|k| k.to_base58_string()).collect(),
            finalized_cblocks: self.counter_base.0 + self.finalized.len() as u64,
            skips: self.counter_base.1 + self.skipped.len() as u64,
            genesis_hash: self.genesis_hash.clone(),
            saved_at_ms: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
        };
        writer.send(crate::state::Pending { checkpoint: checkpoint.clone(), meta }).map_err(|_| anyhow::anyhow!("State checkpoint writer stopped"))?;
        Ok(())
    }
    /// Hands the transactions people sent in the finalized chain through `cblock` to the kept
    /// transactions: the slots of skipped cblocks are passed over.
    async fn keep_finalized(&mut self, cblock: u64) {
        let Some(writer) = &self.kept_writer else {
            return;
        };
        let through = cblock * 4 + 3;
        if through < self.keep_from {
            return;
        }
        let mut chain = BTreeSet::new();
        let mut at = Some(cblock);
        while let Some(c) = at.filter(|c| c * 4 + 3 >= self.keep_from) {
            chain.insert(c);
            at = if c % 4 == 0 { self.nodes[0].windows.get(&(c / 4)).and_then(|w| w.parent).map(|p| p.slot) } else { Some(c - 1) };
        }
        let payer = |e: &crate::solana::Executed| {
            bincode::deserialize::<solana_sdk::transaction::VersionedTransaction>(&e.bytes).ok().and_then(|tx| tx.message.static_account_keys().first().map(|k| k.to_string()))
        };
        let gateway = self.gateway.read().await;
        // The record as the gateway will hold it finalized: with its cblock's finalization certificate.
        let certificate = |c: u64| {
            gateway.slots.get(&(c * 4)).and_then(|view| view["cblock"]["certificates"].as_array()).and_then(|certs| {
                certs.iter().rev().find(|cert| matches!(cert["kind"].as_str(), Some("fast_finalization" | "slow_finalization"))).cloned()
            })
        };
        let record = |e: &crate::solana::Executed| {
            let mut record = gateway.transactions.get(&e.id)?.clone();
            record["state"] = json!("finalized");
            record["finalization"] = certificate(e.slot / 4).or_else(|| certificate(cblock)).unwrap_or(Value::Null);
            Some(record.to_string())
        };
        let kept = self
            .solana
            .executed_between(self.keep_from, through)
            .into_iter()
            .filter(|(_, e)| chain.contains(&(e.slot / 4)) && payer(e).is_some_and(|p| !self.built_in.contains(&p)))
            .map(|(signature, e)| (signature.to_owned(), crate::history::Kept { executed: e.clone(), record: record(e) }))
            .collect::<Vec<_>>();
        drop(gateway);
        self.keep_from = through + 1;
        if !kept.is_empty() {
            let _ = writer.send(kept);
        }
    }
    fn latest_started(&self) -> u64 {
        let node = &self.nodes[0];
        node.windows.iter().next_back().map_or(0, |(w, run)| {
            w * 16 + (run.next_slot.saturating_sub(1) as u64).min(15)
        })
    }
    async fn record_sent(
        &mut self,
        header: &crate::execution::TxHeader,
        kind: &str,
        n: u8,
        t: u8,
    ) -> Result<()> {
        let receivers = self.window_json(header.target_slot / 16)?["proposers"]
            .as_array()
            .unwrap()
            .clone();
        let tx = crate::submission::transaction_sent(header, kind, n, t, &receivers);
        if self
            .gateway
            .read()
            .await
            .transactions
            .contains_key(&header.id)
        {
            return Ok(());
        }
        self.gateway
            .write()
            .await
            .transactions
            .insert(header.id.clone(), tx.clone());
        let mut step = Step::new();
        step.persist
            .push((format!("transaction/{}", header.id), tx.clone()));
        step.event(header.target_slot, 0, "transaction.updated", tx);
        self.dispatch(0, step).await
    }
    async fn submit_fragment(&mut self, params: Value) -> Result<Value> {
        let raw = params.clone();
        let fragment = crate::threshold::FragmentSubmission::parse(params)?;
        let s = fragment.header.target_slot;
        let q = fragment.receiver;
        let id = fragment.header.id.clone();
        let key = format!("{id}:{}", fragment.index);
        self.prune_admissions();
        if self.admissions.contains_key(&key) {
            crate::submission::remember(&mut self.admissions, &key, &raw)?;
            return Ok(json!({"id":id,"accepted":true,"targetSlot":s}));
        }
        let window = crate::submission::validate_target(self.latest_started(), s)?;
        ensure!(
            self.nodes[0].executor.as_ref().unwrap().can_pay_inclusion(&fragment.header.fee_payer, q, window, 1),
            "The fee payer cannot pay the inclusion fee: fund it first"
        );
        crate::submission::remember(&mut self.admissions, &key, &raw)?;
        self.admission_slots.entry(s).or_default().push(key);
        self.record_sent(&fragment.header, "fragments", fragment.n, fragment.t)
            .await?;
        let mut step = Step::new();
        step.out.push(Outbox {
            from: 0,
            to: vec![self.proposer_ids(window)?[q as usize - 1]],
            body: Wire::SubmitFragment(fragment),
        });
        self.dispatch(0, step).await?;
        Ok(json!({"id":id,"accepted":true,"targetSlot":s}))
    }
    async fn submit(&mut self, params: Value) -> Result<Value> {
        let bytes = B64.decode(
            params["transaction"]
                .as_str()
                .context("Missing transaction")?,
        )?;
        let s = params["header"]["targetSlot"]
            .as_u64()
            .context("Missing targetSlot")?;
        let valid = self.nodes[0]
            .executor
            .as_ref()
            .unwrap()
            .validate_transaction(&bytes, s)?;
        let key = valid.header.id.clone();
        self.prune_admissions();
        let mut roles = vec![];
        for q in params["receivers"]
            .as_array()
            .context("Missing receivers")?
        {
            let q = q.as_u64().context("Invalid receiver")? as usize;
            ensure!((1..=3).contains(&q), "Invalid receiver");
            roles.push(q);
        }
        if self.admissions.contains_key(&key) {
            crate::submission::remember(&mut self.admissions, &key, &params)?;
            // An identical retry of a transaction that has not executed goes out again, to the
            // proposers now due: its first copies may have gone down with a skipped slot.
            if !self.nodes[0].executor.as_ref().unwrap().is_executed(&key) {
                let ids = self.proposer_ids((self.latest_started() + 1) / 16)?;
                let mut step = Step::new();
                step.out.push(Outbox { from: 0, to: roles.iter().map(|q| ids[q - 1]).collect(), body: Wire::Submit(valid) });
                self.dispatch(0, step).await?;
            }
            return Ok(json!({"id":key,"accepted":true,"targetSlot":s}));
        }
        let window = crate::submission::validate_target(self.latest_started(), s)?;
        ensure!(
            serde_json::to_value(&valid.header)? == params["header"],
            "INVALID_INPUT"
        );
        let inclusions = roles.iter().map(|q| (valid.header.fee_payer.clone(), *q as u8, 1u64)).collect::<Vec<_>>();
        ensure!(
            self.nodes[0].executor.as_ref().unwrap().admit_inclusions(&inclusions, window)?.iter().all(|ok| *ok),
            "The fee payer cannot pay the inclusion fee: fund it first"
        );
        let ids = self.proposer_ids(window)?;
        let targets = roles.iter().map(|q| ids[q - 1]).collect::<Vec<_>>();
        let id = valid.header.id.clone();
        crate::submission::remember(&mut self.admissions, &id, &params)?;
        self.admission_slots.entry(s).or_default().push(id.clone());
        self.record_sent(&valid.header, "plain", targets.len() as u8, 1)
            .await?;
        let mut step = Step::new();
        step.out.push(Outbox {
            from: 0,
            to: targets,
            body: Wire::Submit(valid),
        });
        self.dispatch(0, step).await?;
        Ok(json!({"id":id,"accepted":true,"targetSlot":s}))
    }
    async fn report(&self) -> Result<()> {
        self.profile
            .write(&Path::new(&self.config.store_path).join("profile.json"))?;
        let all_heights = self
            .nodes
            .iter()
            .flat_map(|node| node.slots.keys().copied())
            .collect::<BTreeSet<_>>();
        let mut compared_heights = 0u64;
        let mut mismatching_heights = 0u64;
        let mut all_validators_heights = 0u64;
        for height in all_heights {
            let hashes = self
                .nodes
                .iter()
                .filter_map(|node| {
                    node.slots
                        .get(&height)
                        .and_then(|slot| slot.state_hash.as_ref())
                })
                .collect::<Vec<_>>();
            if hashes.len() >= 2 {
                compared_heights += 1;
                if hashes.iter().any(|hash| *hash != hashes[0]) {
                    mismatching_heights += 1;
                }
            }
            if hashes.len() == self.nodes.len() {
                all_validators_heights += 1;
            }
        }
        let positions = self
            .nodes
            .iter()
            .map(|node| node.last_executed.map_or(0, |slot| slot + 1))
            .collect::<Vec<_>>();
        let current_skew = positions.iter().max().unwrap() - positions.iter().min().unwrap();
        let validator_debug = self.nodes.iter().map(|node| {
            let latest = node.windows.iter().next_back();
            let first_unexecuted = node.slots.iter().find(|(_, slot)| !slot.executed).map(|(slot, _)| *slot);
            let latest_unexecuted = latest.and_then(|(window, _)| (*window * 16..*window * 16 + 16).find(|slot| !node.slots.get(slot).map_or(false, |local| local.executed)));
            let proposed_parents = latest_unexecuted.and_then(|slot| node.slots.get(&slot)).map(|local| local.meta.values().filter(|meta| meta.final_slice).map(|meta| meta.parent).collect::<Vec<_>>()).unwrap_or_default();
            let first_detail = latest_unexecuted.and_then(|slot| node.slots.get(&slot).map(|local| {
                json!({"slot":slot,"equivocation":local.equivocation,"attestations":local.attest.values().map(|(att,_)| json!({"proposer":att.proposer,"pi":att.pi.iter().map(|hash| hash.map(hex::encode)).collect::<Vec<_>>()})).collect::<Vec<_>>(),"cipherSlices":local.cipher.keys().collect::<Vec<_>>(),"finalSlices":local.meta.values().filter(|meta|meta.final_slice).map(|meta|json!({"proposer":meta.proposer,"slice":meta.slice})).collect::<Vec<_>>()})
            }));
            json!({"id":node.id,"latestWindow":latest.map(|(window,_)|*window),"parent":latest.and_then(|(_,run)|run.parent),"firstUnexecuted":first_unexecuted,"latestWindowFirstUnexecuted":latest_unexecuted,"proposedParents":proposed_parents,"firstUnexecutedDetail":first_detail,"highestExecutedBlock":node.executed_blocks.keys().next_back(),"latestVoted":node.voted.iter().next_back()})
        }).collect::<Vec<_>>();
        let (maximum_skew, maximum_lag) = {
            let p = self.progress.lock().unwrap();
            (p.maximum_skew, p.maximum_lag)
        };
        // Proposer-side pacing: the gap between the first commitments of consecutive slots.
        let commits = self.exchange.commit_times();
        let trace = self.handover_trace.lock().unwrap().clone();
        let handover_trace = trace.iter().rev().filter(|(w, _)| **w > 0).take(64).filter_map(|(w, events)| {
            let anchor = commits.get(&(w * 16 - 1))?;
            Some(json!({"window":w,"link":format!("{}-{}", self.window(w - 1).ok()?.hub_code, self.window(*w).ok()?.hub_code),"nextProposers":self.proposer_ids(*w).ok()?,"events":events.iter().map(|(node, kind, at)| json!([node, kind, at.saturating_duration_since(*anchor).as_micros() as u64])).collect::<Vec<_>>()}))
        }).collect::<Vec<_>>();
        let facts = self.facts.as_ref().map_or(Value::Null, |facts| facts.json(String::new()));
        let memory = {
            let g = self.gateway.read().await;
            let n = &self.nodes[0];
            json!({"gatewaySlots":g.slots.len(),"gatewayTransactions":g.transactions.len(),"gatewayTransactionsByCblock":g.transactions_by_cblock.values().map(|v|v.len()).sum::<usize>(),"gatewayHistory":g.history.len(),"node0Slots":n.slots.len(),"node0Votes":n.votes.len(),"node0Windows":n.windows.len(),"node0ExecutedBlocks":n.executed_blocks.len(),"node0SeenCertificates":n.seen_certificates.len(),"node0Votor":n.votor.sizes(),"node0Checkpoints":n.checkpoints.len(),"node0Mempool":n.mempool.len()})
        };
        let report = json!({"handoverTrace":handover_trace,"memory":memory,"proposerWithinWindowPacingUs":facts["slotPacingUs"],"proposerHandoverGapUs":facts["handoverUs"],"facts":facts,"slotPacingUs":stats(&self.samples),"withinWindowPacingUs":stats(&self.within_window_samples),"windowHandoverGapUs":stats(&self.handover_samples),"commitSpreadUs":stats(&self.exchange.spreads()),"omittedCommitments":self.exchange.omitted_commitments(),"finalizationUs":stats(&self.final_times),"finalizedCblocks":self.finalized.len(),"skippedCblocks":self.skipped.len(),"validatorLastExecuted":self.nodes.iter().map(|n|n.last_executed).collect::<Vec<_>>(),"validatorDebug":validator_debug,"validatorExecutedCblocks":self.nodes.iter().map(|n|n.executed_blocks.len()).collect::<Vec<_>>(),"validatorFinalizedConsensusSlot":self.nodes.iter().map(|n|n.votor.finalized().map(|block|block.slot)).collect::<Vec<_>>(),"generatedTransactions":self.traffic_seq,"maximumExecutionSkewSlots":maximum_skew,"maximumExecutionLagSlots":maximum_lag,"currentExecutionSkewSlots":current_skew,"comparedStateHashHeights":compared_heights,"mismatchingStateHashHeights":mismatching_heights,"onlineStateHashChecks":self.state_checks.lock().unwrap().checked,"allValidatorsStateHashHeights":all_validators_heights,"receivedPackets":self.received_packets,"observedAttestations":self.nodes.iter().map(|n|n.slots.values().map(|s|s.attest.len()).sum::<usize>()).collect::<Vec<_>>(),"observedShreds":self.nodes.iter().map(|n|n.slots.values().map(|s|s.shreds.values().map(|v|v.len()).sum::<usize>()).sum::<usize>()).collect::<Vec<_>>(),"stateHashes":self.nodes.iter().map(|n|n.executor.as_ref().unwrap().state_hash()).collect::<Vec<_>>(),"recentOwnVotes":self.nodes.iter().map(|n|n.votes.values().filter(|v|v.vote.validator==n.id&&n.windows.keys().next_back().map_or(true,|w|v.vote.slot+4>=w*4)).map(|v|v.vote.clone()).collect::<Vec<_>>()).collect::<Vec<_>>()});
        std::fs::write(
            Path::new(&self.config.store_path).join("last-run.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        println!(
            "{}",
            json!({"report":Path::new(&self.config.store_path).join("last-run.json"),"finalizedCblocks":self.finalized.len(),"skips":self.skipped.len(),"slotPacingUs":report["slotPacingUs"]})
        );
        Ok(())
    }
}
fn attestation_value(a: &Attestation, sig: &[u8], sent: u64, arrivals: Vec<Value>) -> Value {
    json!({"slot":a.slot,"proposer":a.proposer,"pi":a.pi.iter().map(|h|h.map(hex::encode)).collect::<Vec<_>>(),"key":B64.encode(a.key),"signature":bs58::encode(sig).into_string(),"sentUs":sent,"validatorArrivals":arrivals})
}
fn stats(values: &[u64]) -> Value {
    if values.is_empty() {
        return json!({"count":0});
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let n = v.len();
    json!({"count":n,"p50":v[(n-1)/2],"p99":v[(n*99).div_ceil(100)-1],"max":v[n-1]})
}
pub fn utc(ms: u64) -> String {
    let seconds = ms / 1000;
    let days = seconds / 86400;
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    year += if m <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60,
        ms % 1000
    )
}

fn authenticate(
    packet: Received,
    keys: &[ed25519_dalek::VerifyingKey],
    schedule: &std::sync::RwLock<ProposerSchedules>,
) -> Result<(Received, Envelope)> {
    let e: Envelope = decode(&packet.payload)?;
    let schedule = schedule.read().unwrap();
    ensure!(
        e.from == packet.from && e.from < keys.len(),
        "Wrong envelope signer"
    );
    let outer = crypto::hash(&packet.payload);
    if !verified_cached(outer) {
        crypto::verify_bytes(&keys[e.from], &encode(&e.body)?, &e.signature)?;
        remember_verified(outer);
    }
    match &e.body {
        Wire::Commit(commitment) => {
            ensure!(
                (1..=3).contains(&commitment.proposer)
                    && schedule.ids(commitment.slot / 16)?
                        .get(commitment.proposer - 1)
                        == Some(&e.from),
                "Incorrect commitment proposer"
            );
        }
        Wire::Shreds(shreds) | Wire::Relay(shreds) | Wire::Handover { shreds, .. } => {
            for shred in shreds {
                ensure!(
                    (1..=3).contains(&shred.meta.proposer),
                    "Invalid proposer role"
                );
                let signer = schedule.ids(shred.meta.slot / 16)?
                    [shred.meta.proposer - 1];
                let digest = crypto::hash(&encode(&(signer, shred))?);
                if !verified_cached(digest) {
                    crypto::verify_shred(shred, &keys[signer])?;
                    remember_verified(digest);
                }
            }
        }
        Wire::Equivocation { first, second } => {
            ensure!(
                first.0.slot == second.0.slot
                    && first.0.proposer == second.0.proposer
                    && first.0.hash != second.0.hash
                    && (1..=3).contains(&first.0.proposer),
                "Not an equivocation"
            );
            let origin = schedule.ids(first.0.slot / 16)?[first.0.proposer - 1];
            for (commitment, signature) in [first, second] {
                crypto::verify_bytes(&keys[origin], &encode(&Wire::Commit(commitment.clone()))?, signature)?;
            }
        }
        _ => {}
    }
    if let Wire::Handover { attestations, .. } = &e.body {
        for (a, sig) in attestations {
            ensure!(
                (1..=3).contains(&a.proposer) && a.pi.len() == 3,
                "Invalid handover attestation"
            );
            let q = schedule.ids(a.slot / 16)?[a.proposer - 1];
            crypto::verify_bytes(&keys[q], &encode(&Wire::Attest(a.clone()))?, sig)?;
        }
    }
    Ok((packet, e))
}

#[derive(Default)]
struct VerificationCache {
    set: std::collections::HashSet<[u8; 32]>,
    order: std::collections::VecDeque<[u8; 32]>,
}
static VERIFIED: std::sync::OnceLock<std::sync::Mutex<VerificationCache>> =
    std::sync::OnceLock::new();
fn verified_cached(hash: [u8; 32]) -> bool {
    VERIFIED
        .get_or_init(|| std::sync::Mutex::new(VerificationCache::default()))
        .lock()
        .unwrap()
        .set
        .contains(&hash)
}
fn remember_verified(hash: [u8; 32]) {
    let mut cache = VERIFIED
        .get_or_init(|| std::sync::Mutex::new(VerificationCache::default()))
        .lock()
        .unwrap();
    if cache.set.insert(hash) {
        cache.order.push_back(hash);
    }
    while cache.order.len() > 100_000 {
        let old = cache.order.pop_front().unwrap();
        cache.set.remove(&old);
    }
}

fn threshold_transaction(
    header: &crate::execution::TxHeader,
    n: u8,
    t: u8,
    inclusions: &[crate::threshold::Inclusion],
    state: &str,
    slot: Option<u64>,
    position: Option<usize>,
    outcome: Option<&crate::execution::ExecutionResult>,
    window: &Value,
    expiry: u64,
) -> Value {
    json!({"header":header,"state":state,"kind":"fragments","n":n,"t":t,"receivers":window["proposers"],
        "fragments":inclusions.iter().map(|i|json!({"index":i.index,"receiver":i.receiver,"slot":i.slot,"proposer":i.proposer,"includedUs":i.at_us})).collect::<Vec<_>>(),
        "includedCount":inclusions.len(),"readableSlot":slot,"executedSlot":slot,"mergedPosition":position,
        "result":outcome.map(|r|if r.success{"success"}else{"error"}),"logs":outcome.map(|r|r.logs.clone()).unwrap_or_default(),"failure":outcome.and_then(|r|r.error.clone()),
        "expiresAfterSlot":inclusions.first().map(|i|i.slot+expiry),"finalization":null})
}

#[derive(Default)]
struct StateChecks {
    pending: BTreeMap<u64, (String, u16)>,
    checked: u64,
    mismatches: u64,
}
#[derive(Default)]
struct Progress {
    started: [u64; 12],
    executed: [u64; 12],
    maximum_skew: u64,
    maximum_lag: u64,
}
struct NodeRunner {
    id: usize,
    network: Box<Network>,
    nodes: NodeSet,
}
impl NodeRunner {
    async fn run(
        mut self,
        mut incoming: mpsc::Receiver<(Received, Envelope)>,
        mut commands: Option<mpsc::Receiver<GatewayCommand>>,
        bases: Arc<std::sync::Mutex<BTreeMap<(usize, u64), Instant>>>,
    ) -> Result<(usize, Box<Network>)> {
        self.network.nodes = std::mem::take(&mut self.nodes);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut traffic = tokio::time::interval(std::time::Duration::from_micros(
            1_000_000 / self.network.config.traffic_per_second.max(1),
        ));
        traffic.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let stop = tokio::time::sleep(std::time::Duration::from_secs(
            self.network.config.run_seconds.unwrap_or(u64::MAX / 1000),
        ));
        tokio::pin!(stop);
        loop {
            tokio::select! {
              _=&mut stop=>break,
              _=tokio::signal::ctrl_c()=>break,
              packet=incoming.recv()=>{if let Some((packet,envelope))=packet{if let Err(error)=self.network.receive(packet,envelope).await{tracing::warn!(%error,"Rejected protocol input");} for _ in 0..7{let Ok((packet,envelope))=incoming.try_recv()else{break;};if let Err(error)=self.network.receive(packet,envelope).await{tracing::warn!(%error,"Rejected protocol input");}} }else{break;}},
              _=tick.tick()=>{self.network.tick().await?;let node=&self.network.nodes[self.id];let mut map=bases.lock().unwrap();for(w,r)in node.windows.iter().rev().take(2){map.insert((self.id,*w),r.start);}},
              _=traffic.tick(),if self.id==0&&self.network.config.traffic_per_second>0=>self.network.traffic().await?,
              command=async {match &mut commands{Some(rx)=>rx.recv().await,None=>std::future::pending().await}}=>{if let Some(command)=command{self.network.command(command).await;}}
            }
        }
        Ok((self.id, self.network))
    }
}

/// Solana's JSON-RPC, the part a wallet, web3.js or the `acrux hub` commands use: reads of
/// accounts, balances, blockhashes and the epoch, and `sendTransaction` with its status.
impl Network {
    async fn solana_rpc(&mut self, method: &str, params: Value, client: Option<std::net::IpAddr>) -> Result<Value> {
        let arg = |i: usize| params.get(i).cloned().unwrap_or(Value::Null);
        let slot = self.nodes[0].last_executed.unwrap_or(0);
        let context = json!({"slot": slot, "apiVersion": "2.2.1"});
        let executor = self.nodes[0].executor.as_ref().unwrap();
        // A mint's decimals, for token amounts in jsonParsed.
        let decimals = |mint: &str| executor.account(mint).ok().flatten().filter(|a| a.data.len() >= 82).map(|a| a.data[44]);
        let encoding = |config: &Value| config["encoding"].as_str().unwrap_or("base64").to_owned();
        let account_json = |a: &solana_sdk::account::Account, encoding: &str| crate::solana::account_json(a, encoding, decimals);
        Ok(match method {
            "getHealth" => json!("ok"),
            "getVersion" => json!({"solana-core": concat!("acrux-", env!("CARGO_PKG_VERSION")), "feature-set": 0}),
            "getSlot" | "getBlockHeight" => json!(slot),
            "getEpochInfo" => {
                let config = &self.proposers.read().unwrap().config;
                let slots = if config.stake_from_slot.is_some_and(|from| slot >= from) { config.epoch_slots } else { 432_000 };
                json!({"absoluteSlot":slot,"blockHeight":slot,"epoch":slot / slots,"slotIndex":slot % slots,"slotsInEpoch":slots})
            },
            "getLatestBlockhash" => json!({"context": context, "value": {"blockhash": executor.latest_blockhash(), "lastValidBlockHeight": slot + crate::execution::BLOCKHASH_AGE_SLOTS - 8}}),
            "getBalance" => json!({"context": context, "value": executor.balance(arg(0).as_str().context("address")?)?}),
            "getSupply" => {
                let total = executor.total_lamports()?;
                let reserve = executor.native_supply()?.map_or(0, |supply| supply.reserve_lamports);
                let circulating = total.checked_sub(reserve).context("Native supply is below its fixed reserve")?;
                json!({"context": context, "value": {"total": total, "circulating": circulating, "nonCirculating": reserve, "nonCirculatingAccounts": []}})
            }
            "getAccountInfo" => {
                let encoding = encoding(&arg(1));
                json!({"context": context, "value": executor.account(arg(0).as_str().context("address")?)?.filter(|a| a.lamports > 0).map(|a| account_json(&a, &encoding))})
            }
            "getMultipleAccounts" => {
                let encoding = encoding(&arg(1));
                let keys = arg(0).as_array().context("addresses")?.iter().map(|k| k.as_str().context("address").map(str::to_owned)).collect::<Result<Vec<_>>>()?;
                let values = keys.iter().map(|k| Ok(executor.account(k)?.filter(|a| a.lamports > 0).map(|a| account_json(&a, &encoding)))).collect::<Result<Vec<_>>>()?;
                json!({"context": context, "value": values})
            }
            "getProgramAccounts" => {
                let encoding = encoding(&arg(1));
                json!(executor.accounts_owned_by(arg(0).as_str().context("program")?)?.into_iter().filter(|(_, a)| a.lamports > 0).map(|(k, a)| json!({"pubkey": k, "account": account_json(&a, &encoding)})).collect::<Vec<_>>())
            }
            // A wallet's token list: the owner's accounts under SPL Token or Token-2022, by mint or program.
            "getTokenAccountsByOwner" => {
                let owner = <solana_sdk::pubkey::Pubkey as std::str::FromStr>::from_str(arg(0).as_str().context("owner")?)?;
                let filter = arg(1);
                let encoding = encoding(&arg(2));
                let mint = filter["mint"].as_str().map(<solana_sdk::pubkey::Pubkey as std::str::FromStr>::from_str).transpose()?;
                let programs = match filter["programId"].as_str() {
                    Some(program) => vec![program.to_owned()],
                    None => vec![crate::solana::TOKEN.to_owned(), crate::solana::TOKEN_2022.to_owned()],
                };
                let mut accounts = vec![];
                for program in programs {
                    for (key, account) in executor.accounts_owned_by(&program)? {
                        let data = &account.data;
                        if data.len() >= 165 && data[108] != 0 && data[32..64] == owner.to_bytes() && mint.is_none_or(|m| data[0..32] == m.to_bytes()) {
                            accounts.push(json!({"pubkey": key, "account": account_json(&account, &encoding)}));
                        }
                    }
                }
                json!({"context": context, "value": accounts})
            }
            "getTokenAccountBalance" => {
                let account = executor.account(arg(0).as_str().context("account")?)?.context("Invalid param: could not find account")?;
                anyhow::ensure!(account.data.len() >= 165, "Invalid param: not a Token account");
                let mint = bs58::encode(&account.data[0..32]).into_string();
                let d = decimals(&mint).context("Invalid param: mint could not be read")?;
                json!({"context": context, "value": crate::solana::token_amount(u64::from_le_bytes(account.data[64..72].try_into()?), d)})
            }
            "getTokenSupply" => {
                let mint = executor.account(arg(0).as_str().context("mint")?)?.context("Invalid param: could not find mint")?;
                anyhow::ensure!(mint.data.len() >= 82, "Invalid param: not a mint");
                json!({"context": context, "value": crate::solana::token_amount(u64::from_le_bytes(mint.data[36..44].try_into()?), mint.data[44])})
            }
            "getMinimumBalanceForRentExemption" => json!(executor.rent_exempt_minimum(arg(0).as_u64().context("data length")? as usize)),
            "getSignaturesForAddress" => {
                let address = arg(0).as_str().context("address")?.to_owned();
                let address = address.as_str();
                let config = arg(1);
                let limit = config["limit"].as_u64().unwrap_or(1_000).min(1_000) as usize;
                // A transaction the gateway no longer holds in memory was finalized long ago.
                let gateway = self.gateway.read().await;
                let finalized = |id: &str| gateway.transactions.get(id).map_or(true, |tx| tx["state"] == "finalized" || !tx["finalization"].is_null());
                let (before, until) = (config["before"].as_str(), config["until"].as_str());
                let entry = |signature: &str, executed: &crate::solana::Executed, finalized: bool| json!({
                    "signature": signature,
                    "slot": executed.slot,
                    "err": executed.err.as_ref().map(|_| crate::solana::FAILED.clone()),
                    "memo": null,
                    "blockTime": executed.time,
                    "confirmationStatus": if finalized { "finalized" } else { "processed" },
                });
                let held = self.solana.signatures_for(address, limit, before, until);
                let mut out = held.iter().map(|(signature, executed)| entry(signature, executed, finalized(&executed.id))).collect::<Vec<_>>();
                // Older than what the gateway holds: the kept transactions.
                let until_held = until.is_some_and(|u| self.solana.get(u).is_some());
                if let (Some(kept), true) = (&self.kept, out.len() < limit && !until_held) {
                    let older_than = match held.last() {
                        Some((_, executed)) => Some(executed.slot),
                        None => match before {
                            Some(before) => match self.solana.get(before) {
                                Some(executed) => Some(executed.slot),
                                None => kept.get(before)?.map(|k| k.executed.slot),
                            },
                            None => None,
                        },
                    };
                    let seen = held.iter().map(|(s, _)| *s).collect::<BTreeSet<_>>();
                    for (signature, k) in kept.signatures_for(address, older_than, &|s| seen.contains(s), until, limit - out.len())? {
                        out.push(entry(&signature, &k.executed, true));
                    }
                }
                json!(out)
            }
            "getVoteAccounts" => {
                let schedules = self.proposers.read().unwrap();
                let active = schedules.config.stake_from_slot.is_some_and(|from| slot >= from);
                let weights = if active { executor.stake_weights(slot / schedules.config.epoch_slots)? } else { self.stakes.clone() };
                let current = self.identities.iter().zip(weights).map(|(identity, stake)| {
                    if !active {
                        return Ok::<_, anyhow::Error>(json!({"nodePubkey":identity,"votePubkey":identity,"activatedStake":stake,"epochVoteAccount":true,"commission":0,"lastVote":slot,"rootSlot":slot}));
                    }
                    let vote = crate::execution::genesis_vote_address(&identity.parse()?);
                    let account = executor.account(&vote.to_string())?.context("Genesis vote account is missing")?;
                    ensure!(account.owner == solana_sdk::vote::program::id() && !account.executable, "Invalid genesis vote account");
                    let state = solana_sdk::vote::state::VoteState::deserialize(&account.data)?;
                    ensure!(state.node_pubkey.to_string() == *identity, "Genesis vote account names another validator");
                    Ok(json!({"nodePubkey":identity,"votePubkey":vote.to_string(),"activatedStake":stake,"epochVoteAccount":true,
                        "commission":state.commission,"lastVote":state.last_voted_slot().unwrap_or(0),"rootSlot":state.root_slot,"epochCredits":state.epoch_credits}))
                }).collect::<Result<Vec<_>>>()?;
                json!({"current":current,"delinquent":[]})
            },
            "getStakeMinimumDelegation" => json!({"context":context,"value":executor.get_stake_minimum_delegation()?.context("The Stake program is not active")?}),
            "getStakeActivation" => {
                use solana_sdk::stake::state::StakeStateV2;
                let account = executor.account(arg(0).as_str().context("stake account")?)?.context("Stake account not found")?;
                ensure!(account.owner == solana_sdk::stake::program::id() && !account.executable && account.data.len() == StakeStateV2::size_of(), "Invalid Stake account");
                let schedules = self.proposers.read().unwrap();
                let config = arg(1);
                ensure!(config.is_null() || config.is_object(), "Invalid Stake activation configuration");
                let epoch_slots = if schedules.config.stake_from_slot.is_some_and(|from| slot >= from) { schedules.config.epoch_slots } else { 432_000 };
                let epoch = match config.get("epoch") {
                    Some(value) => value.as_u64().context("Stake activation epoch must be an unsigned integer")?,
                    None => slot / epoch_slots,
                };
                match bincode::deserialize::<StakeStateV2>(&account.data)? {
                    StakeStateV2::Stake(meta, stake, _) => {
                        let status = stake.delegation.stake_activating_and_deactivating(epoch,&solana_sdk::stake_history::StakeHistory::default(),None);
                        let state = if status.deactivating > 0 { "deactivating" } else if status.activating > 0 { "activating" }
                            else if status.effective > 0 { "active" } else { "inactive" };
                        json!({"state":state,"active":status.effective,"inactive":account.lamports.saturating_sub(meta.rent_exempt_reserve).saturating_sub(status.effective)})
                    },
                    StakeStateV2::Initialized(meta) => json!({"state":"inactive","active":0,"inactive":account.lamports.saturating_sub(meta.rent_exempt_reserve)}),
                    _ => anyhow::bail!("Stake account is not initialized"),
                }
            },
            "sendTransaction" => {
                let encoded = arg(0).as_str().context("transaction")?.to_owned();
                anyhow::ensure!(encoded.len() <= 1_684, "Transaction exceeds Solana packet size");
                let bytes = match arg(1)["encoding"].as_str() {
                    Some("base64") => B64.decode(&encoded)?,
                    _ => bs58::decode(&encoded).into_vec()?,
                };
                let tx: solana_sdk::transaction::VersionedTransaction = bincode::deserialize(&bytes)?;
                let signature = tx.signatures.first().context("unsigned transaction")?.to_string();
                // A resend answers the signature, as Solana does; one that has not executed yet goes
                // out again exactly as it was first submitted.
                if self.solana.get(&signature).is_some() {
                    return Ok(json!(signature));
                }
                if let Some(params) = self.solana_sent.get(&signature).cloned() {
                    self.submit(params).await?;
                    return Ok(json!(signature));
                }
                let target = self.latest_started() + SEND_LEAD_SLOTS;
                let valid = executor.validate_transaction(&bytes, target).map_err(|e| anyhow::anyhow!("Transaction simulation failed: {e}"))?;
                if arg(1)["skipPreflight"] != json!(true) {
                    let limits = crate::limits::limits();
                    anyhow::ensure!(limits.simulation_ready(client), "RATE_LIMITED: too much simulation from this address; slow down or send with skipPreflight");
                    let simulated = self.nodes[0].executor.as_mut().unwrap().simulate(&bytes, true, false)?;
                    limits.simulation_charge(client, simulated.units);
                    if let Some(err) = simulated.err {
                        anyhow::bail!("Transaction simulation failed: {err}. {}", simulated.logs.join(" "));
                    }
                }
                let params = json!({"header": valid.header, "transaction": B64.encode(&bytes), "receivers": [1, 2, 3]});
                self.submit(params.clone()).await?;
                self.signature_ids.insert(signature.clone(), valid.header.id.clone());
                self.solana_sent.insert(signature.clone(), params);
                json!(signature)
            }
            "getGenesisHash" => json!(self.genesis_hash),
            // Newest first, as Solana answers; Acrux has no vote transactions, so every one counts.
            "getRecentPerformanceSamples" => {
                let limit = arg(0).as_u64().unwrap_or(720).min(720) as usize;
                json!(self.performance.iter().rev().take(limit).cloned().collect::<Vec<_>>())
            }
            // The slot's wall-clock time: slots run every 50 ms behind the latest executed one.
            "getBlockTime" => {
                let asked = arg(0).as_u64().context("slot")?;
                anyhow::ensure!(asked <= slot, "Slot {asked} has not executed yet");
                json!(crate::solana::now() - ((slot - asked) / 20) as i64)
            }
            "isBlockhashValid" => json!({"context": context, "value": executor.blockhash_known(arg(0).as_str().unwrap_or(""))}),
            // Every slot's merge orders by priority fee, but none is required to land: the floor is 0.
            "getRecentPrioritizationFees" => json!([{"slot": slot, "prioritizationFee": 0}]),
            "getFeeForMessage" => {
                let encoded = arg(0).as_str().context("message")?.to_owned();
                anyhow::ensure!(encoded.len() <= 1_684, "Message exceeds Solana packet size");
                let message: solana_sdk::message::VersionedMessage = bincode::deserialize(&B64.decode(encoded)?).context("Invalid message")?;
                // sendTransaction sends to all three of the target slot's proposers.
                json!({"context": context, "value": executor.fee_for_message(&message, 3)?})
            }
            "simulateTransaction" => {
                let encoded = arg(0).as_str().context("transaction")?.to_owned();
                anyhow::ensure!(encoded.len() <= 1_684, "Transaction exceeds Solana packet size");
                let config = arg(1);
                let bytes = match config["encoding"].as_str() {
                    Some("base64") => B64.decode(&encoded)?,
                    _ => bs58::decode(&encoded).into_vec()?,
                };
                let replace = config["replaceRecentBlockhash"] == json!(true);
                let limits = crate::limits::limits();
                anyhow::ensure!(limits.simulation_ready(client), "RATE_LIMITED: too much simulation from this address; slow down");
                let sim = self.nodes[0].executor.as_mut().unwrap().simulate(&bytes, config["sigVerify"] == json!(true), replace)?;
                limits.simulation_charge(client, sim.units);
                json!({"context": context, "value": {
                    "err": sim.err.map(|e| serde_json::to_value(e).unwrap()), "logs": sim.logs, "accounts": null, "unitsConsumed": sim.units, "returnData": null,
                    "innerInstructions": null,
                    "replacementBlockhash": sim.replaced.map(|hash| json!({"blockhash": hash, "lastValidBlockHeight": slot + crate::execution::BLOCKHASH_AGE_SLOTS - 8})),
                }})
            }
            "getTransaction" => {
                let signature = arg(0).as_str().context("signature")?.to_owned();
                let signature = signature.as_str();
                let config = arg(1);
                let encoding = config.as_str().or(config["encoding"].as_str()).unwrap_or("json");
                // A read: `confirmed` and `processed` return it once executed, `finalized` (the default)
                // once its cblock is.
                let confirmed = config["commitment"] == json!("confirmed") || config["commitment"] == json!("processed");
                match self.solana.get(signature) {
                    Some(executed) => {
                        let state = match self.gateway.read().await.transactions.get(&executed.id) {
                            Some(tx) => tx["state"].as_str().map(str::to_owned),
                            None => self.store.get(&format!("transaction/{}", executed.id))?.and_then(|tx: Value| tx["state"].as_str().map(str::to_owned)),
                        };
                        if confirmed || state.as_deref() == Some("finalized") {
                            crate::solana::transaction_json(executed, encoding)
                        } else {
                            Value::Null
                        }
                    }
                    None => match &self.kept {
                        Some(kept) => kept.get(signature)?.map_or(Value::Null, |k| crate::solana::transaction_json(&k.executed, encoding)),
                        None => Value::Null,
                    },
                }
            }
            "getSignatureStatuses" => {
                let mut statuses = vec![];
                for signature in arg(0).as_array().context("signatures")? {
                    let id = signature.as_str().and_then(|s| self.signature_ids.get(s)).cloned();
                    // A record the gateway no longer holds in memory was finalized and moved to the store.
                    let (record, stored) = match id {
                        Some(id) => match self.gateway.read().await.transactions.get(&id).cloned() {
                            Some(tx) => (Some(tx), false),
                            None => (self.store.get(&format!("transaction/{id}"))?, true),
                        },
                        None => (None, false),
                    };
                    if record.is_none() {
                        if let (Some(kept), Some(signature)) = (&self.kept, signature.as_str()) {
                            if let Some(entry) = kept.get(signature)? {
                                let failed = entry.executed.err.is_some();
                                statuses.push(json!({
                                    "slot": entry.executed.slot,
                                    "confirmations": null,
                                    "err": if failed { crate::solana::FAILED.clone() } else { Value::Null },
                                    "status": if failed { json!({"Err": *crate::solana::FAILED}) } else { json!({"Ok": null}) },
                                    "confirmationStatus": "finalized",
                                }));
                                continue;
                            }
                        }
                    }
                    // A failed transaction is final once its cblock is, and then stored with its finalization.
                    let is_final = |tx: &Value| tx["state"] == "finalized" || (tx["state"] == "failed" && (stored || !tx["finalization"].is_null()));
                    statuses.push(match record.as_ref().and_then(|tx| tx["state"].as_str()) {
                        Some("executed" | "finalized" | "failed") => {
                            let state = if is_final(record.as_ref().unwrap()) { "finalized" } else { "executed" };
                            json!({
                            "slot": record.as_ref().unwrap()["executedSlot"],
                            "confirmations": if state == "finalized" { Value::Null } else { json!(0) },
                            "err": if record.as_ref().unwrap()["result"] == "success" { Value::Null } else { crate::solana::FAILED.clone() },
                            "status": if record.as_ref().unwrap()["result"] == "success" { json!({"Ok": null}) } else { json!({"Err": *crate::solana::FAILED}) },
                            // `confirmed` on Solana means a supermajority voted and it will not roll back; an executed
                            // transaction here can still go with a skipped cblock, so it is `processed` until finalized.
                            "confirmationStatus": if state == "finalized" { "finalized" } else { "processed" },
                        })
                        }
                        _ => Value::Null,
                    });
                }
                json!({"context": context, "value": statuses})
            }
            other => anyhow::bail!("Method not found: {other}"),
        })
    }
}
