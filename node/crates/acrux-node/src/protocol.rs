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
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, RwLock};

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
pub struct Network {
    config: Configuration,
    profile: Arc<crate::profiling::Profiling>,
    keys: Vec<SigningKey>,
    identities: Vec<String>,
    stakes: Vec<u64>,
    schedule: Vec<schedule::Window>,
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
fn pack(
    node: &mut Node,
    s: u64,
    q: usize,
    censored: Option<&String>,
) -> (Vec<ValidatedTx>, Vec<crate::threshold::VerifiedFragment>) {
    let executor = node.executor.as_ref().unwrap();
    let due = node
        .mempool
        .iter()
        .filter(|(id, tx)| tx.header.target_slot <= s && Some(*id) != censored)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let txs = due
        .into_iter()
        .filter_map(|id| node.mempool.remove(&id))
        .filter(|tx| !executor.is_executed(&tx.header.id) && executor.blockhash_valid(&tx.bytes))
        .collect::<Vec<_>>();
    let due = node
        .fragment_mempool
        .iter()
        .filter(|(_, f)| f.header.target_slot <= s)
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let fragments = due
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
        let keys = (0..12).map(|i| crate::identity_key(i)).collect::<Vec<_>>();
        let identities = keys
            .iter()
            .map(|k| bs58::encode(k.verifying_key().as_bytes()).into_string())
            .collect::<Vec<_>>();
        let stakes = vec![100_000_000u64; 12];
        let genesis = config
            .genesis_path
            .as_deref()
            .map(|path| crate::genesis::Genesis::load(Path::new(path)).map(Arc::new))
            .transpose()?;
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
        let planned = schedule::build_schedule(
            0,
            &config.seed.to_string(),
            &hubs,
            stakes.iter().sum(),
            config.windows_per_epoch as u64,
        )?;
        let senders = (0..24).map(|_| Keypair::new()).collect::<Vec<_>>();
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
            votor.genesis();
            let mut executor = SvmExecutor::new(balances.clone(), &programs)?;
            executor.set_fees(crate::execution::Fees {
                reserve_lamports: config.reserve,
                inclusion_lamports: config.inclusion_fee,
                base_fee_per_signature: config.base_fee,
                base_burn_bps: 5000,
            })?;
            executor.set_track_capacity(config.track_capacity_cu)?;
            for (address, account) in genesis.iter().flat_map(|g| g.accounts.iter()) {
                executor.set_account(address, account.clone())?;
            }
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
        }
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
        Ok(Self {
            config,
            profile,
            keys,
            identities,
            stakes,
            schedule: planned.windows,
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
        })
    }
    fn window(&self, number: u64) -> schedule::Window {
        let mut w = self.schedule[number as usize % self.schedule.len()].clone();
        w.number = number;
        w
    }
    fn proposer_ids(&self, number: u64) -> Vec<usize> {
        self.window(number)
            .proposers
            .iter()
            .map(|a| self.identities.iter().position(|x| x == a).unwrap())
            .collect()
    }
    fn window_json(&self, number: u64) -> Value {
        view::window_json(
            number,
            &self.window(number),
            &self.identities,
            &self.stakes,
            if self.config.crash_window == Some(number) {
                Some(self.config.crash_proposer)
            } else {
                None
            },
        )
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
        let schedule_ids = Arc::new(
            self.schedule
                .iter()
                .map(|w| {
                    w.proposers
                        .iter()
                        .map(|a| self.identities.iter().position(|id| id == a).unwrap())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>(),
        );
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
                    schedule: self.schedule.clone(),
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
        if step.out.is_empty() && step.events.is_empty() && step.persist.is_empty() {
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
                if node == 0 && event.kind == "cblock.finalized" && self.finalized.insert(slot / 4) {
                    self.final_times.push(event.at);
                    self.observed_final.lock().unwrap().insert(slot / 4);
                }
                // These local seeds only support node zero's own slot-start and execution views.
                // The observer owns the complete projection, including other validators' arrivals.
                if node == 0 && matches!(event.kind, "slot.started" | "slot.executed") {
                    self.slot_views.insert(slot, event.data.clone());
                }
            }
        }
        while self.slot_views.len() > 64 {
            let first = *self.slot_views.keys().next().unwrap();
            self.slot_views.remove(&first);
        }
        let queued = Instant::now();
        self.observer.submit(node, step.events, step.persist).await?;
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
            step.out.push(Outbox { from: 0, to: self.proposer_ids(fault.window), body: Wire::Submit(valid) });
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
            let link = (slot % 16 == 0).then(|| format!("{}-{}", self.window(slot / 16 - 1).hub_code, self.window(slot / 16).hub_code));
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
        self.gateway.write().await.facts = json;
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
                let ids = self.proposer_ids(w);
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
            // Validator 0 serves followers through the gateway, so it holds blocks for 256 slots.
            let held = if id == 0 { 256 } else { 64 };
            let node = &mut self.nodes[id];
            if let Some(last) = node.last_executed {
                for (slot, local) in node
                    .slots
                    .range_mut(last.saturating_sub(held + 32)..last.saturating_sub(held))
                {
                    if *slot + held < last {
                        local.shreds.clear();
                        local.cipher.clear();
                        local.meta.clear();
                        local.payload.clear();
                    }
                }
            }
        }
        for id in self.active.clone() {
            let node = &mut self.nodes[id];
            if let Some((&window, run)) = node.windows.iter().next_back() {
                let parent = run.parent.map(|p| p.slot);
                node.checkpoints
                    .retain(|slot, _| *slot + 8 >= window * 4 || Some(*slot) == parent);
                node.threshold_checkpoints
                    .retain(|slot, _| *slot + 8 >= window * 4 || Some(*slot) == parent);
            }
            // Everything 64 consensus slots below finality is settled and already in the store.
            if let Some(finalized) = node.votor.finalized() {
                let below = finalized.slot.saturating_sub(64);
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
        let w = self.window_json(number);
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
            let to = vec![self.proposer_ids(number)[role - 1]];
            step.out.push(Outbox { from: id, to, body: Wire::Submit(tx) });
        }
        // A proposer of the window before hands on what it did not get to include (its window was
        // skipped, or the transaction arrived late) to the proposer in its role of this window.
        if number > 0 {
            if let Some(role) = self.proposer_ids(number - 1).iter().position(|x| *x == id) {
                let to = vec![self.proposer_ids(number)[role]];
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
                .map(|n| self.window_json(n))
                .collect();
            let mut g = self.gateway.write().await;
            g.upcoming_windows = upcoming;
        }
        Ok(())
    }
    async fn begin_block(&mut self, id: usize, s: u64) -> Result<()> {
        let _phase = self.profile.measure(Some(id), "proposer.build");
        let ids = self.proposer_ids(s / 16);
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
        let node = &mut self.nodes[id];
        let (txs, fragments) = pack(node, s, q, censored.as_ref());
        let payload = encode(&BlockPayload {
            transactions: txs,
            fragments,
        })?;
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
            .launch(id, s, q, commit, key, start, self.proposer_ids(s / 16));
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
        if self.nodes[id].windows.contains_key(&window)
            || self.nodes[id].early.contains_key(&window)
        {
            return Ok(());
        }
        let q = self
            .proposer_ids(window)
            .iter()
            .position(|n| *n == id)
            .unwrap()
            + 1;
        if self.config.crash_window == Some(window) && q == self.config.crash_proposer {
            return Ok(());
        }
        let slot = window * 16;
        let key = crypto::fresh_key();
        let node = &mut self.nodes[id];
        let (transactions, fragments) = pack(node, slot, q, None);
        let payload = encode(&BlockPayload {
            transactions,
            fragments,
        })?;
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
            self.proposer_ids(s / 16),
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
                let role = self.proposer_ids(window).iter().position(|x| *x == id).map_or(1, |q| q + 1);
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
                        .any(|w| self.proposer_ids(w)[fragment.receiver as usize - 1] == id),
                    "Fragment delivered to wrong proposer"
                );
                self.nodes[id]
                    .fragment_mempool
                    .insert((fragment.header.id.clone(), fragment.index), fragment);
            }
            Wire::Shreds(shreds) => {
                ensure!(
                    shreds.iter().all(|x| self
                        .proposer_ids(x.meta.slot / 16)
                        .get(x.meta.proposer.saturating_sub(1))
                        == Some(&e.from)),
                    "Wrong proposer shred"
                );
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
                let ids = self.proposer_ids(s / 16);
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
                let ids = self.proposer_ids(s / 16);
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
                let digest = crypto::hash(&encode(&signed)?);
                if !verified_cached(digest) {
                    crypto::verify_bytes(
                        &self.keys[e.from].verifying_key(),
                        &encode(&signed.vote)?,
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
                    let digest = crypto::hash(&encode(p)?);
                    if !verified_cached(digest) {
                        crypto::verify_bytes(
                            &self.keys[v.validator].verifying_key(),
                            &encode(v)?,
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
                let mut targets = self.proposer_ids(certificate.slot / 4);
                targets.extend(self.proposer_ids(certificate.slot / 4 + 1));
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
                    let origin = self.proposer_ids(shred.meta.slot / 16)[shred.meta.proposer - 1];
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
                    let origin = self.proposer_ids(att.slot / 16)[att.proposer - 1];
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
        let ids = self.proposer_ids(w);
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
                    let mut block: BlockPayload = decode(&plaintext)?;
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
                    Ok(block.transactions)
                })();
                match checked {
                    Ok(transactions) => payloads.push((*q, transactions)),
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
                            &self.window_json(w)["proposers"].as_array().unwrap(),
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
                            &self.window_json(w),
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
                                    &self.window_json(w),
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
            let (returned, result) = tokio::task::spawn_blocking(move || {
                let result = executor.execute_slot(s, items);
                (executor, result)
            })
            .await?;
            self.nodes[id].executor = Some(returned);
            let execution = result?;
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
                    bytes: bytes.clone(),
                    fee: result.base_fee + result.priority_fee + copies * self.config.inclusion_fee,
                    err: result.error.clone(),
                    logs: result.logs.clone(),
                    units: result.compute_units,
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
            let mut view = self.slot_views.get(&s).cloned().unwrap_or_else(|| {
                view::pending_slot(
                    s,
                    self.window_json(w),
                    utc(self.wall + offset(base, self.origin) / 1000),
                )
            });
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
                    let mut tx = json!({"header":h,"state":if outcome.map_or(false,|r|r.success){"executed"}else{"failed"},"kind":"plain","n":source[&h.id].len(),"t":1,"receivers":self.window_json(w)["proposers"],"fragments":source[&h.id].iter().map(|p|json!({"index":*p-1,"receiver":p,"slot":s,"proposer":p,"includedUs":at})).collect::<Vec<_>>(),"includedCount":source[&h.id].len(),"readableSlot":s,"executedSlot":s,"mergedPosition":position,"result":if outcome.map_or(false,|r|r.success){"success"}else{"error"},"logs":outcome.map(|r|r.logs.clone()).unwrap_or_default(),"failure":outcome.and_then(|r|r.error.clone()),"expiresAfterSlot":null,"finalization":null});
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
                            &self.window_json(w),
                            self.config.fragment_expiry_slots,
                        );
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
            if s % 16 == 15 && self.proposer_ids(w + 1).contains(&id) {
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
                let hash = crypto::hash(&encode(&(i, parent, accepted, &state))?);
                let block = consensus::Block {
                    id: BlockRef { slot: i, hash },
                    parent,
                };
                self.nodes[id].executed_blocks.insert(i, block.id);
                if i % 4 == 3 && self.proposer_ids(w).contains(&id) {
                    let next = self.proposer_ids(w + 1);
                    step.out.push(Outbox {
                        from: id,
                        to: next,
                        body: Wire::Handover {
                            slot: i * 4 + 3,
                            parent: block.id,
                            shreds: (i * 4..i * 4 + 4)
                                .flat_map(|slot| {
                                    self.nodes[id].slots[&slot]
                                        .shreds
                                        .values()
                                        .flatten()
                                        .cloned()
                                })
                                .collect(),
                            attestations: (i * 4..i * 4 + 4)
                                .flat_map(|slot| {
                                    self.nodes[id].slots[&slot].attest.values().cloned()
                                })
                                .collect(),
                        },
                    });
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
                    let signature = crypto::sign_bytes(&self.keys[id], &encode(&vote)?);
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
    async fn traffic(&mut self) -> Result<()> {
        let s = self.latest_started() + 3;
        let ids = self.proposer_ids(s / 16);
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
                    let finalized = node.votor.finalized().context("Nothing finalized yet")?;
                    let checkpoint = node.checkpoints.get(&finalized.slot).context("Finalized checkpoint not held")?;
                    let threshold = node.threshold_checkpoints.get(&finalized.slot).context("Finalized fragment cache not held")?;
                    Ok::<_, anyhow::Error>(json!({
                        "consensusSlot": finalized.slot,
                        "lastSlot": finalized.slot * 4 + 3,
                        "executor": node.executor.as_ref().unwrap().snapshot(checkpoint),
                        "threshold": threshold,
                    }))
                })();
                let _ = reply.send(result.map_err(|e| e.to_string()));
            }
            GatewayCommand::SlotBlocks { slot, reply } => {
                let result = (|| {
                    let local = self.nodes[0].slots.get(&slot).context("Slot not held")?;
                    let agreed = local.agreed.clone().context("Slot not agreed")?;
                    ensure!(!local.cipher.is_empty(), "Blocks no longer held");
                    let blocks = agreed.iter().map(|q| {
                        let slices = local.meta.iter().filter(|((p, _), _)| p == q).map(|((_, i), meta)| json!({
                            "meta": meta,
                            "cipher": local.cipher.get(&(*q, *i)).map(|c| B64.encode(c)),
                            "root": local.shreds.get(&(*q, *i)).and_then(|s| s.first()).map(|s| hex::encode(s.root)),
                        })).collect::<Vec<_>>();
                        json!({"proposer": q, "slices": slices})
                    }).collect::<Vec<_>>();
                    let attestations = local.attest.values().map(|(a, _)| json!({"proposer": a.proposer, "pi": a.pi.iter().map(|h| h.map(hex::encode)).collect::<Vec<_>>(), "key": B64.encode(a.key)})).collect::<Vec<_>>();
                    Ok::<_, anyhow::Error>(json!({"slot": slot, "agreed": agreed, "blocks": blocks, "attestations": attestations}))
                })();
                let _ = reply.send(result.map_err(|e| e.to_string()));
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
            GatewayCommand::Solana { method, params, reply } => {
                let result = self.solana_rpc(&method, params).await;
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
    /// The demo pool's vault balances, read from node zero's executed state.
    fn pool_reserves(&self) -> Result<(Arc<crate::genesis::Genesis>, [u64; 2], u64)> {
        let genesis = self.genesis.clone().context("No demo pool in this net's genesis")?;
        let executor = self.nodes[0].executor.as_ref().unwrap();
        let amount = |vault: &str| -> Result<u64> { Ok(executor.account(vault)?.as_ref().map_or(0, crate::genesis::token_amount)) };
        let reserves = [amount(&genesis.pool.vault_a)?, amount(&genesis.pool.vault_b)?];
        Ok((genesis, reserves, self.nodes[0].last_executed.unwrap_or(0)))
    }
    async fn faucet_transfer(&mut self, address: &str, lamports: u64) -> Result<Value> {
        let target = self.latest_started() + 3;
        let recipient: solana_sdk::pubkey::Pubkey = address.parse()?;
        let genesis = self.genesis.clone();
        let (payer, instructions) = match &genesis {
            // The genesis faucet also hands out demo tokens.
            Some(genesis) => (&genesis.faucet, genesis.faucet_instructions(&recipient, lamports)?),
            None => (&self.senders[0], vec![system_instruction::transfer(&self.senders[0].pubkey(), &recipient, lamports)]),
        };
        let tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&payer.pubkey()),
            &[payer],
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
            to: self.proposer_ids(target / 16),
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
        let receivers = self.window_json(header.target_slot / 16)["proposers"]
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
            to: vec![self.proposer_ids(window)[q as usize - 1]],
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
                let ids = self.proposer_ids((self.latest_started() + 1) / 16);
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
        let ids = self.proposer_ids(window);
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
            Some(json!({"window":w,"link":format!("{}-{}", self.window(w - 1).hub_code, self.window(*w).hub_code),"nextProposers":self.proposer_ids(*w),"events":events.iter().map(|(node, kind, at)| json!([node, kind, at.saturating_duration_since(*anchor).as_micros() as u64])).collect::<Vec<_>>()}))
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
fn utc(ms: u64) -> String {
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
    schedule: &[Vec<usize>],
) -> Result<(Received, Envelope)> {
    let e: Envelope = decode(&packet.payload)?;
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
                    && schedule[(commitment.slot / 16) as usize % schedule.len()]
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
                let signer = schedule[(shred.meta.slot / 16) as usize % schedule.len()]
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
            let origin = schedule[(first.0.slot / 16) as usize % schedule.len()][first.0.proposer - 1];
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
            let q = schedule[(a.slot / 16) as usize % schedule.len()][a.proposer - 1];
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
    async fn solana_rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let arg = |i: usize| params.get(i).cloned().unwrap_or(Value::Null);
        let slot = self.nodes[0].last_executed.unwrap_or(0);
        let context = json!({"slot": slot, "apiVersion": "2.2.1"});
        let executor = self.nodes[0].executor.as_ref().unwrap();
        let account_json = |a: &solana_sdk::account::Account| json!({
            "lamports": a.lamports, "owner": a.owner.to_string(), "data": [B64.encode(&a.data), "base64"],
            "executable": a.executable, "rentEpoch": u64::MAX, "space": a.data.len(),
        });
        Ok(match method {
            "getHealth" => json!("ok"),
            "getVersion" => json!({"solana-core": concat!("acrux-", env!("CARGO_PKG_VERSION")), "feature-set": 0}),
            "getSlot" | "getBlockHeight" => json!(slot),
            "getEpochInfo" => json!({"absoluteSlot": slot, "blockHeight": slot, "epoch": slot / 432_000, "slotIndex": slot % 432_000, "slotsInEpoch": 432_000}),
            "getLatestBlockhash" => json!({"context": context, "value": {"blockhash": executor.latest_blockhash(), "lastValidBlockHeight": slot + crate::execution::BLOCKHASH_AGE_SLOTS - 8}}),
            "getBalance" => json!({"context": context, "value": executor.balance(arg(0).as_str().context("address")?)?}),
            "getAccountInfo" => json!({"context": context, "value": executor.account(arg(0).as_str().context("address")?)?.filter(|a| a.lamports > 0).as_ref().map(account_json)}),
            "getMultipleAccounts" => {
                let keys = arg(0).as_array().context("addresses")?.iter().map(|k| k.as_str().context("address").map(str::to_owned)).collect::<Result<Vec<_>>>()?;
                let values = keys.iter().map(|k| Ok(executor.account(k)?.filter(|a| a.lamports > 0).as_ref().map(account_json))).collect::<Result<Vec<_>>>()?;
                json!({"context": context, "value": values})
            }
            "getProgramAccounts" => json!(executor.accounts_owned_by(arg(0).as_str().context("program")?)?.into_iter().filter(|(_, a)| a.lamports > 0).map(|(k, a)| json!({"pubkey": k, "account": account_json(&a)})).collect::<Vec<_>>()),
            "getMinimumBalanceForRentExemption" => json!(executor.rent_exempt_minimum(arg(0).as_u64().context("data length")? as usize)),
            "getSignaturesForAddress" => {
                let address = arg(0).as_str().context("address")?.to_owned();
                let address = address.as_str();
                let config = arg(1);
                let limit = config["limit"].as_u64().unwrap_or(1_000).min(1_000) as usize;
                // A transaction the gateway no longer holds in memory was finalized long ago.
                let gateway = self.gateway.read().await;
                let finalized = |id: &str| gateway.transactions.get(id).map_or(true, |tx| tx["state"] == "finalized");
                json!(self.solana.signatures_for(address, limit, config["before"].as_str(), config["until"].as_str()).into_iter().map(|(signature, executed)| json!({
                    "signature": signature,
                    "slot": executed.slot,
                    "err": executed.err.as_ref().map(|_| crate::solana::FAILED.clone()),
                    "memo": null,
                    "blockTime": crate::solana::block_time(executed.slot),
                    "confirmationStatus": if finalized(&executed.id) { "finalized" } else { "confirmed" },
                })).collect::<Vec<_>>())
            }
            "getVoteAccounts" => json!({"current": self.identities.iter().zip(&self.stakes).map(|(identity, stake)| json!({"nodePubkey": identity, "votePubkey": identity, "activatedStake": stake, "epochVoteAccount": true, "commission": 0, "lastVote": slot, "rootSlot": slot})).collect::<Vec<_>>(), "delinquent": []}),
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
                let target = self.latest_started() + 3;
                let valid = executor.validate_transaction(&bytes, target).map_err(|e| anyhow::anyhow!("Transaction simulation failed: {e}"))?;
                if arg(1)["skipPreflight"] != json!(true) {
                    let simulated = self.nodes[0].executor.as_mut().unwrap().simulate(&bytes, true, false)?;
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
            "getGenesisHash" => json!(self.nodes[0].genesis.hash()),
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
                let sim = self.nodes[0].executor.as_mut().unwrap().simulate(&bytes, config["sigVerify"] == json!(true), replace)?;
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
                    None => Value::Null,
                }
            }
            "getSignatureStatuses" => {
                let mut statuses = vec![];
                for signature in arg(0).as_array().context("signatures")? {
                    let id = signature.as_str().and_then(|s| self.signature_ids.get(s)).cloned();
                    let record = match id {
                        Some(id) => match self.gateway.read().await.transactions.get(&id).cloned() {
                            Some(tx) => Some(tx),
                            None => self.store.get(&format!("transaction/{id}"))?,
                        },
                        None => None,
                    };
                    statuses.push(match record.as_ref().and_then(|tx| tx["state"].as_str()) {
                        Some(state @ ("executed" | "finalized" | "failed")) => json!({
                            "slot": record.as_ref().unwrap()["executedSlot"],
                            "confirmations": if state == "finalized" { Value::Null } else { json!(0) },
                            "err": if record.as_ref().unwrap()["result"] == "success" { Value::Null } else { crate::solana::FAILED.clone() },
                            "status": if record.as_ref().unwrap()["result"] == "success" { json!({"Ok": null}) } else { json!({"Err": *crate::solana::FAILED}) },
                            "confirmationStatus": if state == "finalized" { "finalized" } else { "confirmed" },
                        }),
                        _ => Value::Null,
                    });
                }
                json!({"context": context, "value": statuses})
            }
            other => anyhow::bail!("Method not found: {other}"),
        })
    }
}
