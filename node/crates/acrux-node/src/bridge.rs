//! The deposit reader: finalized deposits on the Solana bridge program (`acrux_bridge`), read
//! through a Solana RPC (`ACRUX_BRIDGE_RPC`) every two seconds into the cache proposers carry
//! credits from and validators check them against.
use crate::execution::BridgeCredit;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use redb::{Database, ReadableTable, TableDefinition};
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::{Arc, Mutex},
};

type Deposits = Arc<Mutex<BTreeMap<u64, BridgeCredit>>>;
pub type ProofPrunes = Arc<Mutex<BTreeSet<u64>>>;

const PROOFS: TableDefinition<u64, &[u8]> = TableDefinition::new("proofs");

/// Shared readers decode one archived proof per request. The archive's page cache is bounded;
/// parsed proof payloads live only in the gateway's 4096-cblock rolling window.
pub struct ProofArchiveStore(Database);

impl ProofArchiveStore {
    pub fn get(&self, slot: u64) -> Result<Option<Value>> {
        let read = self.0.begin_read()?;
        let table = read.open_table(PROOFS)?;
        let proof = table.get(slot)?;
        proof.map(|proof| serde_json::from_slice(proof.value()).map_err(Into::into)).transpose()
    }
}

/// The durable archive is separate from the gateway's rolling window. Required proofs are always
/// stored, including above its shedding level. Epoch representatives never expire; an unpaid
/// withdrawal proof can be shed only after the relayer has durably saved every sequence in it,
/// its slot-based grace period has passed, and the archive remains above its shedding level.
pub struct ProofArchive {
    pub store: Arc<ProofArchiveStore>,
    pub epochs: BTreeMap<u64, u64>,
    /// All archived slots, including representatives with no withdrawals.
    pub withdrawals: BTreeMap<u64, Vec<u64>>,
    /// Activated epoch-end proofs, including zero burns, never expire or get age-shed.
    fee_burn_cblocks: BTreeSet<u64>,
    capacity: usize,
    grace_slots: u64,
    warned_above_level: bool,
    shed_since_log: usize,
}

impl ProofArchive {
    pub fn open(path: Option<&Path>, epoch_slots: u64, finalized_through: Option<u64>) -> Result<Self> {
        let capacity: usize = std::env::var("ACRUX_BRIDGE_PROOF_MAX")
            .context("ACRUX_BRIDGE_PROOF_MAX is required when the bridge is enabled")?
            .parse().context("ACRUX_BRIDGE_PROOF_MAX must be a positive integer")?;
        anyhow::ensure!(capacity > 0, "ACRUX_BRIDGE_PROOF_MAX must be a positive integer");
        let grace_slots: u64 = std::env::var("ACRUX_BRIDGE_PROOF_GRACE_SLOTS").unwrap_or_else(|_| "1728000".into())
            .parse().context("ACRUX_BRIDGE_PROOF_GRACE_SLOTS must be a positive integer")?;
        anyhow::ensure!(grace_slots > 0, "ACRUX_BRIDGE_PROOF_GRACE_SLOTS must be a positive integer");
        Self::load(path, epoch_slots, finalized_through, capacity, grace_slots)
    }

    fn load(path: Option<&Path>, epoch_slots: u64, finalized_through: Option<u64>, capacity: usize, grace_slots: u64) -> Result<Self> {
        let path = path.context("a bridge rule requires a persistent store for its proof archive")?;
        let mut builder = Database::builder();
        builder.set_cache_size(64 * 1024 * 1024);
        std::fs::create_dir_all(path.parent().context("bridge proof archive path")?)?;
        let db = builder.create(path)?;
        let mut archive = Self { store: Arc::new(ProofArchiveStore(db)), epochs: BTreeMap::new(), withdrawals: BTreeMap::new(), fee_burn_cblocks: BTreeSet::new(), capacity, grace_slots, warned_above_level: false, shed_since_log: 0 };
        {
            let w = archive.store.0.begin_write()?;
            {
                let mut table = w.open_table(PROOFS)?;
                let mut expired = vec![];
                for entry in table.iter()? {
                    let (slot, proof) = entry?;
                    let proof: Value = serde_json::from_slice(proof.value())?;
                    anyhow::ensure!(proof["epochSlots"].as_u64() == Some(epoch_slots), "saved bridge proofs use a different epochSlots");
                    if finalized_through.is_some_and(|cblock| slot.value() <= cblock) {
                        if handoff_proof(&proof)? {archive.epochs.entry(slot.value()*4/epoch_slots).or_insert(slot.value());}
                        archive.withdrawals.insert(slot.value(), withdrawal_sequences(&proof)?);
                        if fee_burn_epoch_end(&proof)? { archive.fee_burn_cblocks.insert(slot.value()); }
                    } else {
                        // A crash can leave a prepared proof ahead of the state checkpoint.
                        // It was never published and must not survive that checkpoint's rollback.
                        expired.push(slot.value());
                    }
                }
                for slot in expired {
                    table.remove(slot)?;
                }
            }
            w.commit()?;
        }
        archive.check_capacity();
        Ok(archive)
    }

    fn check_capacity(&mut self) {
        if self.withdrawals.len() > self.capacity && !self.warned_above_level {
            tracing::warn!(event = "bridge_proof_archive_full", capacity = self.capacity, held = self.withdrawals.len(),
                "bridge proof archive is above its shedding level; fresh withdrawals and epoch representatives remain durable");
            self.warned_above_level = true;
        }
    }

    /// Called only for finalized cblocks. New withdrawals and first proofs of epochs are always
    /// durable before their checkpoint, irrespective of the shedding level.
    pub fn keep(&mut self, slot: u64, epoch: u64, withdrawal: bool, proof: &Value) -> Result<bool> {
        let fee_burn_end = fee_burn_epoch_end(proof)?;
        let representative=handoff_proof(proof)? && !self.epochs.contains_key(&epoch);
        if !withdrawal && !fee_burn_end && !representative {
            return Ok(false);
        }
        let sequences = withdrawal_sequences(proof)?;
        let w = self.store.0.begin_write()?;
        {
            let mut table = w.open_table(PROOFS)?;
            table.insert(slot, serde_json::to_vec(proof)?.as_slice())?;
        }
        w.commit()?;
        self.withdrawals.insert(slot, sequences);
        if fee_burn_end { self.fee_burn_cblocks.insert(slot); }
        if representative {self.epochs.insert(epoch,slot);}
        self.check_capacity();
        Ok(true)
    }

    /// Shed oldest unpaid non-representatives only after the durable chain has advanced past the
    /// grace period, and only after every sequence is below the relayer's durable save cursor.
    /// A restart clears the acknowledgement: no age shedding resumes before the next report.
    pub fn shed_stale(&mut self, finalized_through: u64, saved_cursor: Option<u64>) -> Result<Vec<u64>> {
        let Some(saved_cursor) = saved_cursor else { return Ok(vec![]) };
        let excess = self.withdrawals.len().saturating_sub(self.capacity);
        if excess == 0 { return Ok(vec![]) }
        let representatives = self.epochs.values().copied().collect::<BTreeSet<_>>();
        let remove = self.withdrawals.iter()
            .take_while(|(slot, _)| finalized_through.saturating_sub(**slot).saturating_mul(4) > self.grace_slots)
            .filter(|(slot, sequences)| !sequences.is_empty() && sequences.iter().all(|sequence| *sequence < saved_cursor) && !representatives.contains(*slot) && !self.fee_burn_cblocks.contains(*slot))
            .take(excess.min(100)).map(|(slot, _)| *slot).collect::<Vec<_>>();
        self.remove(&remove)?;
        self.shed_since_log += remove.len();
        if self.shed_since_log >= 1024 {
            tracing::warn!(event = "bridge_proofs_shed", count = self.shed_since_log, held = self.withdrawals.len(), grace_slots = self.grace_slots,
                finalized_cblock = finalized_through, saved_cursor, "old saved bridge proofs shed above the retention level");
            self.shed_since_log = 0;
        }
        Ok(remove)
    }

    /// Only background reads agreed by both finalized RPCs can enqueue a paid proof here. An
    /// epoch's representative remains even after payment, because it can still hand that epoch on.
    pub fn prune_paid(&mut self, paid: &BTreeSet<u64>) -> Result<Vec<u64>> {
        let representatives = self.epochs.values().copied().collect::<BTreeSet<_>>();
        let remove = paid.iter().copied().filter(|slot| self.withdrawals.contains_key(slot) && !representatives.contains(slot) && !self.fee_burn_cblocks.contains(slot)).collect::<Vec<_>>();
        if remove.is_empty() { return Ok(remove) }
        self.remove(&remove)?;
        tracing::info!(event = "bridge_proofs_pruned", count = remove.len(), held = self.withdrawals.len(), "paid bridge proofs pruned");
        Ok(remove)
    }

    fn remove(&mut self, remove: &[u64]) -> Result<()> {
        if remove.is_empty() { return Ok(()) }
        let w = self.store.0.begin_write()?;
        { let mut table = w.open_table(PROOFS)?; for slot in remove { table.remove(*slot)?; } }
        w.commit()?;
        for slot in remove {
            self.withdrawals.remove(slot);
            self.fee_burn_cblocks.remove(slot);
        }
        self.check_capacity();
        Ok(())
    }
}

fn fee_burn_epoch_end(proof: &Value) -> Result<bool> {
    let Some(from_slot) = proof.get("feeBurnFromSlot").and_then(Value::as_u64) else { return Ok(false) };
    let slot = proof["consensusSlot"].as_u64().context("bridge proof consensusSlot")?;
    let epoch_slots = proof["epochSlots"].as_u64().context("bridge proof epochSlots")?;
    anyhow::ensure!(epoch_slots > 0 && epoch_slots % 4 == 0, "invalid fee-burn proof epoch length");
    let first_slot = slot.checked_mul(4).context("fee-burn proof slot overflow")?;
    Ok(first_slot >= from_slot && first_slot.checked_add(4).context("fee-burn proof end overflow")? % epoch_slots == 0)
}

/// Admission's pre-cut-off proofs deliberately carry no next set; they cannot hand an epoch on.
pub fn handoff_proof(proof:&Value)->Result<bool> {
    let active=proof["admissionFromSlot"].as_u64().is_some_and(|from|proof["consensusSlot"].as_u64().unwrap_or(0)*4>=from);
    // Before admission every cblock carried the fixed next set, including historical archives.
    if !active {return Ok(true)}
    for entry in proof["outbox"].as_array().context("bridge proof outbox")? {
        let entry:OutboxEntry=bincode::deserialize(&B64.decode(entry.as_str().context("outbox entry")?)?)?;
        if matches!(entry,OutboxEntry::ValidatorSet{..}) {return Ok(true)}
    }
    Ok(false)
}

fn withdrawal_sequences(proof: &Value) -> Result<Vec<u64>> {
    proof["outbox"].as_array().context("bridge proof outbox")?.iter().filter_map(|entry| {
        let decode = || -> Result<OutboxEntry> { Ok(bincode::deserialize(&B64.decode(entry.as_str().context("bridge proof outbox entry")?)?)?) };
        match decode() {
            Ok(OutboxEntry::Withdrawal { sequence, .. }) => Some(Ok(sequence)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        }
    }).collect()
}

/// Exactly two separately configured providers. Local checks may point both at one validator.
pub fn deposit_rpcs(value: &str) -> Result<[String; 2]> {
    let urls = value.split(',').map(str::trim).collect::<Vec<_>>();
    anyhow::ensure!(urls.len() == 2 && urls.iter().all(|url| !url.is_empty()), "ACRUX_BRIDGE_RPC must name two comma-separated Solana RPC URLs");
    for url in &urls {
        let parsed = reqwest::Url::parse(url).context("ACRUX_BRIDGE_RPC must contain HTTP URLs")?;
        anyhow::ensure!(matches!(parsed.scheme(), "http" | "https"), "ACRUX_BRIDGE_RPC must contain HTTP URLs");
    }
    Ok([urls[0].into(), urls[1].into()])
}

pub async fn read_deposits(urls: [String; 2], program: Pubkey, deposits: Deposits, gateway: Arc<tokio::sync::RwLock<crate::gateway::GatewayState>>, prunes: ProofPrunes) {
    let mut decimals = HashMap::new();
    loop {
        if read_new(&urls, &program, &deposits, &mut decimals).await.is_err() {
            // Provider errors can include keyed URLs. Neither addresses nor payloads go to logs.
            tracing::warn!("bridge deposit read failed: both RPCs must agree on finalized accounts");
        }
        // Snapshot only sequences, not proof payloads, and leave every RPC await outside the
        // gateway lock and the consensus actor. A failed/disputed bitmap never authorizes pruning.
        let candidates = {
            let state = gateway.read().await;
            let representatives = state.epoch_cblock_proofs.values().copied().collect::<BTreeSet<_>>();
            state.archived_withdrawals.iter().filter(|(slot, sequences)| !sequences.is_empty() && !representatives.contains(*slot))
                .map(|(slot, sequences)| (*slot, sequences.clone())).collect::<Vec<_>>()
        };
        match paid_proofs(&urls, &program, &candidates).await {
            Ok(paid) => prunes.lock().unwrap().extend(paid),
            Err(_) => tracing::warn!("bridge release read failed: both RPCs must agree on finalized bitmaps"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

const RELEASES_PER_BUCKET: u64 = 8192;
const RELEASED_BYTES: usize = 8 + 8 + 1024;

fn agreed_released(left: &Value, right: &Value, program: &Pubkey, bucket: u64) -> Result<Option<Vec<u8>>> {
    for account in [left, right].into_iter().filter(|account| !account.is_null()) {
        anyhow::ensure!(account["data"][1] == "base64", "released bitmap must use base64 encoding");
    }
    let bitmap = agree_values(left, right, Some(program))?;
    if let Some(data) = &bitmap {
        anyhow::ensure!(data.len() == RELEASED_BYTES && data.starts_with(&discriminator("Released")) && u64_at(data, 8)? == bucket,
            "invalid Released bitmap layout or bucket");
    }
    Ok(bitmap)
}

/// A proof with several withdrawals is disposable only once every bit is finalized and agrees.
async fn paid_proofs(urls: &[String; 2], program: &Pubkey, candidates: &[(u64, Vec<u64>)]) -> Result<BTreeSet<u64>> {
    let buckets = candidates.iter().flat_map(|(_, sequences)| sequences.iter().map(|sequence| sequence / RELEASES_PER_BUCKET)).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
    let mut bitmaps = BTreeMap::new();
    for batch in buckets.chunks(100) {
        let addresses = batch.iter().map(|bucket| Pubkey::find_program_address(&[b"released", &bucket.to_le_bytes()], program).0.to_string()).collect::<Vec<_>>();
        let params = json!([addresses, {"encoding":"base64", "commitment":"finalized"}]);
        let (left, right) = tokio::try_join!(crate::cli::rpc(&urls[0], "getMultipleAccounts", params.clone()), crate::cli::rpc(&urls[1], "getMultipleAccounts", params))?;
        let (left, right) = (left["value"].as_array().context("released bitmap accounts")?, right["value"].as_array().context("released bitmap accounts")?);
        anyhow::ensure!(left.len() == batch.len() && right.len() == batch.len(), "wrong released bitmap account count");
        for ((bucket, left), right) in batch.iter().zip(left).zip(right) {
            bitmaps.insert(*bucket, agreed_released(left, right, program, *bucket)?);
        }
    }
    Ok(candidates.iter().filter(|(_, sequences)| !sequences.is_empty() && sequences.iter().all(|sequence| {
        let bit = (sequence % RELEASES_PER_BUCKET) as usize;
        bitmaps[&(sequence / RELEASES_PER_BUCKET)].as_ref().is_some_and(|bitmap| bitmap[16 + bit / 8] & (1 << (bit % 8)) != 0)
    })).map(|(slot, _)| *slot).collect())
}

fn data(value: &Value) -> Result<Option<Vec<u8>>> {
    if value.is_null() { return Ok(None); }
    anyhow::ensure!(value["executable"] == false, "bridge data account must not be executable");
    Ok(Some(B64.decode(value["data"][0].as_str().context("account data")?)?))
}

/// A mismatch ends this read without adding the disputed deposit to the shared cache.
async fn agreed_account(urls: &[String; 2], address: &Pubkey, owner: Option<&Pubkey>) -> Result<Option<Vec<u8>>> {
    let params = json!([address.to_string(), {"encoding": "base64", "commitment": "finalized"}]);
    let (left, right) = tokio::try_join!(crate::cli::rpc(&urls[0], "getAccountInfo", params.clone()), crate::cli::rpc(&urls[1], "getAccountInfo", params))?;
    agree_values(&left["value"], &right["value"], owner)
}
fn agree_values(left: &Value, right: &Value, owner: Option<&Pubkey>) -> Result<Option<Vec<u8>>> {
    let (a, b) = (data(left)?, data(right)?);
    anyhow::ensure!(a == b && left["owner"] == right["owner"], "Solana RPCs disagree on finalized bridge account");
    if a.is_some() {
        if let Some(owner) = owner { anyhow::ensure!(left["owner"].as_str() == Some(owner.to_string().as_str()), "bridge account has wrong owner"); }
        else { anyhow::ensure!(matches!(left["owner"].as_str(), Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" | "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")), "deposit mint has wrong owner"); }
    }
    Ok(a)
}
fn u64_at(d: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(d.get(o..o + 8).context("short account")?.try_into()?))
}
fn key_at(d: &[u8], o: usize) -> Result<Pubkey> {
    Ok(Pubkey::new_from_array(d.get(o..o + 32).context("short account")?.try_into()?))
}
fn discriminator(name: &str) -> [u8; 8] { crate::crypto::hash(format!("account:{name}").as_bytes())[..8].try_into().unwrap() }

/// Records are immutable, so different finalized contexts are safe only when both records match.
async fn read_new(urls: &[String; 2], program: &Pubkey, deposits: &Deposits, decimals: &mut HashMap<Pubkey, u8>) -> Result<()> {
    let bridge = Pubkey::find_program_address(&[b"bridge"], program).0;
    let Some(state) = agreed_account(urls, &bridge, Some(program)).await? else { return Ok(()) };
    anyhow::ensure!(state.starts_with(&discriminator("Bridge")), "bad Bridge discriminator");
    let made = u64_at(&state, 8)?;
    let have = deposits.lock().unwrap().len() as u64;
    for start in (have..made).step_by(100) {
        let sequences = (start..made.min(start + 100)).collect::<Vec<_>>();
        let addresses = sequences.iter().map(|sequence| Pubkey::find_program_address(&[b"deposit", &sequence.to_le_bytes()], program).0.to_string()).collect::<Vec<_>>();
        let params = json!([addresses,{"encoding":"base64","commitment":"finalized"}]);
        let (left,right) = tokio::try_join!(crate::cli::rpc(&urls[0], "getMultipleAccounts", params.clone()),crate::cli::rpc(&urls[1], "getMultipleAccounts", params))?;
        let (left,right) = (left["value"].as_array().context("deposit accounts")?,right["value"].as_array().context("deposit accounts")?);
        anyhow::ensure!(left.len() == sequences.len() && right.len() == sequences.len(), "wrong deposit account count");
        for ((sequence,a),b) in sequences.into_iter().zip(left).zip(right) {
            let Some(record) = agree_values(a,b,Some(program))? else { return Ok(()) };
            anyhow::ensure!(record.len() == 129 && record.starts_with(&discriminator("DepositRecord")) && u64_at(&record, 8)? == sequence, "bad deposit record");
            let mint = key_at(&record, 80)?;
            if !decimals.contains_key(&mint) {
                let mint_data = agreed_account(urls, &mint, None).await?.context("deposit mint missing")?;
                anyhow::ensure!(mint_data.len() >= 82 && mint_data[45] == 1, "bad deposit mint layout");
                decimals.insert(mint, mint_data[44]);
            }
            let credit = BridgeCredit { sequence, solana_mint: mint.to_string(), decimals: decimals[&mint], recipient: key_at(&record, 48)?.to_string(), amount: u64_at(&record, 112)? };
            deposits.lock().unwrap().insert(sequence, credit);
        }
    }
    Ok(())
}

/// What a cblock's outbox holds for Solana's light client: the validator set of the next Acrux
/// epoch, in every cblock, and the withdrawals burned in the cblock, which Solana pays out.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum OutboxEntry {
    ValidatorSet { epoch: u64, validators: Vec<([u8; 32], u64)> },
    Withdrawal { sequence: u64, solana_mint: [u8; 32], recipient: [u8; 32], amount: u64 },
    FeeBurn { epoch: u64, amount: u64 },
}

/// The outbox's Merkle root: leaves are the SHA-256 of each entry's bincode, each level pairs
/// left to right hashing the two together, an odd last node moves up as it is; no entries is zero.
pub fn outbox_root(entries: &[OutboxEntry]) -> [u8; 32] {
    let mut level = entries.iter().map(|e| crate::crypto::hash(&bincode::serialize(e).expect("outbox entry encodes"))).collect::<Vec<_>>();
    if level.is_empty() {
        return [0; 32];
    }
    while level.len() > 1 {
        level = level.chunks(2).map(|pair| if pair.len() == 2 { crate::crypto::hash(&[pair[0], pair[1]].concat()) } else { pair[0] }).collect();
    }
    level[0]
}

/// The sibling hashes from leaf `index` up to the root, each with whether it sits on the left.
pub fn outbox_proof(entries: &[OutboxEntry], mut index: usize) -> Vec<(bool, [u8; 32])> {
    let mut level = entries.iter().map(|e| crate::crypto::hash(&bincode::serialize(e).expect("outbox entry encodes"))).collect::<Vec<_>>();
    let mut proof = vec![];
    while level.len() > 1 {
        let sibling = index ^ 1;
        if sibling < level.len() {
            proof.push((sibling < index, level[sibling]));
        }
        level = level.chunks(2).map(|pair| if pair.len() == 2 { crate::crypto::hash(&[pair[0], pair[1]].concat()) } else { pair[0] }).collect();
        index /= 2;
    }
    proof
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, routing::post, Json, Router};

    #[test]
    fn fee_burn_epoch_end_proofs_keep_live_encodings_and_survive_restart() {
        let set = OutboxEntry::ValidatorSet { epoch:7,validators:vec![([3;32],42)] };
        let mut expected = 0u32.to_le_bytes().to_vec();
        expected.extend(7u64.to_le_bytes()); expected.extend(1u64.to_le_bytes());
        expected.extend([3;32]); expected.extend(42u64.to_le_bytes());
        assert_eq!(bincode::serialize(&set).unwrap(),expected,"existing hand-off leaves remain byte-compatible");
        let withdrawal = OutboxEntry::Withdrawal { sequence:4,solana_mint:[2;32],recipient:[5;32],amount:9 };
        let mut expected = 1u32.to_le_bytes().to_vec();
        expected.extend(4u64.to_le_bytes()); expected.extend([2;32]); expected.extend([5;32]); expected.extend(9u64.to_le_bytes());
        assert_eq!(bincode::serialize(&withdrawal).unwrap(),expected,"existing release leaves remain byte-compatible");
        let fee = OutboxEntry::FeeBurn { epoch:1,amount:9 };
        let mut expected = 2u32.to_le_bytes().to_vec();
        expected.extend(1u64.to_le_bytes()); expected.extend(9u64.to_le_bytes());
        assert_eq!(bincode::serialize(&fee).unwrap(),expected);
        let entries = vec![set,withdrawal,fee];
        let mut leaf = crate::crypto::hash(&expected);
        for (left,sibling) in outbox_proof(&entries,2) {
            leaf = crate::crypto::hash(&if left { [sibling,leaf].concat() } else { [leaf,sibling].concat() });
        }
        assert_eq!(leaf,outbox_root(&entries));
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target").join(format!("fee-burn-outbox-{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&path).unwrap();
        let store = path.join("proofs.redb");
        {
            let mut archive = ProofArchive::load(Some(&store),16,Some(11),1,1).unwrap();
            for slot in [0,4,7,8,11] {
                let mut proof = stored_proof(slot,16,(slot == 7).then_some(1));
                proof["feeBurnFromSlot"] = json!(16);
                if slot == 7 { proof["outbox"].as_array_mut().unwrap().push(json!(B64.encode(bincode::serialize(&OutboxEntry::FeeBurn { epoch:1,amount:9 }).unwrap()))); }
                assert!(archive.keep(slot,slot * 4 / 16,slot == 7,&proof).unwrap());
            }
            assert!(archive.shed_stale(1000,Some(2)).unwrap().is_empty());
            assert!(archive.prune_paid(&BTreeSet::from([7])).unwrap().is_empty());
        }
        {
            let archive = ProofArchive::load(Some(&store),16,Some(11),1,1).unwrap();
            assert_eq!(archive.fee_burn_cblocks,BTreeSet::from([7,11]));
            assert!(archive.store.get(7).unwrap().is_some(),"the fee-burn proof remains after withdrawal payment");
            assert!(archive.store.get(11).unwrap().is_some(),"a zero epoch still has durable end proof for the cursor");
        }
        std::fs::remove_dir_all(path).unwrap();
    }

    fn stored_proof(slot: u64, epoch_slots: u64, sequence: Option<u64>) -> Value {
        let mut outbox = vec![OutboxEntry::ValidatorSet { epoch:slot * 4 / epoch_slots + 1, validators:vec![([1;32],1)] }];
        if let Some(sequence) = sequence { outbox.push(OutboxEntry::Withdrawal { sequence, solana_mint:[2;32], recipient:[3;32], amount:1 }); }
        json!({"consensusSlot":slot,"epochSlots":epoch_slots,"chainId":"archive-check","outbox":outbox.iter().map(|entry| B64.encode(bincode::serialize(entry).unwrap())).collect::<Vec<_>>()})
    }

    fn archive_path() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::current_dir().unwrap().join("target").join(format!("bridge-review-{}",rand::random::<u64>()));
        let path = dir.join("bridge-proofs.redb");
        (dir,path)
    }

    #[test]
    fn bridge_review_full_archive_keeps_new_epoch_representatives_after_restart() {
        let (dir,path) = archive_path();
        let next = stored_proof(108000,432000,None);
        {
            let mut archive = ProofArchive::load(Some(&path),432000,None,1,1728000).unwrap();
            assert!(archive.keep(7,0,false,&stored_proof(7,432000,None)).unwrap());
            assert!(archive.keep(108000,1,false,&next).unwrap());
            assert!(archive.shed_stale(2_000_000,Some(u64::MAX)).unwrap().is_empty(), "representatives never become shedding candidates");
        }
        let archive = ProofArchive::load(Some(&path),432000,Some(2_000_000),1,1728000).unwrap();
        assert_eq!(archive.epochs.get(&1),Some(&108000));
        assert_eq!(archive.store.get(108000).unwrap(),Some(next));
        drop(archive); std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bridge_review_full_archive_keeps_fresh_withdrawals_after_restart() {
        let (dir,path) = archive_path();
        let fresh = stored_proof(8,432000,Some(1));
        {
            let mut archive = ProofArchive::load(Some(&path),432000,None,1,1728000).unwrap();
            archive.keep(7,0,false,&stored_proof(7,432000,None)).unwrap();
            assert!(archive.keep(8,0,true,&fresh).unwrap());
            assert!(archive.shed_stale(10,Some(u64::MAX)).unwrap().is_empty());
        }
        let archive = ProofArchive::load(Some(&path),432000,Some(10),1,1728000).unwrap();
        assert_eq!(archive.store.get(8).unwrap(),Some(fresh));
        assert_eq!(archive.withdrawals.get(&8),Some(&vec![1]));
        drop(archive); std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bridge_review_above_level_sheds_only_old_unpaid_nonrepresentatives() {
        let (dir,path) = archive_path();
        {
            let mut archive = ProofArchive::load(Some(&path),80,None,2,40).unwrap();
            for (slot, sequence) in [(1,None),(2,Some(2)),(3,Some(3)),(10,Some(10)),(15,Some(15)),(20,None)] {
                archive.keep(slot,slot * 4 / 80,sequence.is_some(),&stored_proof(slot,80,sequence)).unwrap();
            }
            assert_eq!(archive.shed_stale(20,Some(u64::MAX)).unwrap(),vec![2,3], "oldest unpaid proofs are shed first");
            assert!(archive.withdrawals.contains_key(&10), "the exact grace boundary is still protected");
            assert!(archive.withdrawals.contains_key(&15), "young overflow must remain durable");
            assert_eq!(archive.shed_stale(21,Some(u64::MAX)).unwrap(),vec![10]);
            assert!(archive.shed_stale(21,Some(u64::MAX)).unwrap().is_empty());
            assert_eq!(archive.withdrawals.len(),3, "representatives and fresh proofs may exceed the soft level");
        }
        let archive = ProofArchive::load(Some(&path),80,Some(21),2,40).unwrap();
        assert_eq!(archive.withdrawals.keys().copied().collect::<Vec<_>>(),vec![1,15,20]);
        assert_eq!(archive.epochs.values().copied().collect::<Vec<_>>(),vec![1,20]);
        drop(archive); std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bridge_review_age_shedding_requires_every_withdrawal_below_saved_cursor() {
        let (dir,path) = archive_path();
        {
            let mut archive = ProofArchive::load(Some(&path),4000,None,1,4).unwrap();
            archive.keep(1,0,false,&stored_proof(1,4000,None)).unwrap();
            let mut multi = stored_proof(2,4000,Some(0));
            multi["outbox"].as_array_mut().unwrap().push(json!(B64.encode(bincode::serialize(&OutboxEntry::Withdrawal {
                sequence:2, solana_mint:[2;32], recipient:[3;32], amount:1,
            }).unwrap())));
            archive.keep(2,0,true,&multi).unwrap();
            archive.keep(3,0,true,&stored_proof(3,4000,Some(1))).unwrap();
            archive.keep(4,0,true,&stored_proof(4,4000,Some(3))).unwrap();
        }
        let mut archive = ProofArchive::load(Some(&path),4000,Some(100),1,4).unwrap();
        assert!(archive.shed_stale(100,None).unwrap().is_empty(), "a restart has no acknowledgement and cannot shed old proofs");
        assert!(archive.shed_stale(100,Some(0)).unwrap().is_empty());
        assert_eq!(archive.shed_stale(100,Some(2)).unwrap(),vec![3]);
        assert!(archive.store.get(2).unwrap().is_some(), "one saved sequence cannot shed a proof that also carries an unsaved sequence");
        assert_eq!(archive.shed_stale(100,Some(3)).unwrap(),vec![2]);
        assert!(archive.store.get(4).unwrap().is_some(), "the cursor itself is the next unsaved sequence");
        assert_eq!(archive.shed_stale(100,Some(4)).unwrap(),vec![4]);
        assert_eq!(archive.withdrawals.keys().copied().collect::<Vec<_>>(),vec![1]);
        assert!(archive.store.get(1).unwrap().is_some(), "the epoch representative is never shed");
        drop(archive); std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn bridge_review_prunes_only_after_all_release_bits_agree_on_both_finalized_rpcs() {
        let program = Pubkey::new_unique();
        let proof = |slot, sequences: &[u64]| json!({"consensusSlot":slot,"epochSlots":432000,"outbox":sequences.iter().map(|sequence| {
            B64.encode(bincode::serialize(&OutboxEntry::Withdrawal { sequence:*sequence, solana_mint:[1;32], recipient:[2;32], amount:1 }).unwrap())
        }).collect::<Vec<_>>()});
        let (dir,path) = archive_path();
        let mut archive = ProofArchive::load(Some(&path),432000,None,3,1728000).unwrap();
        archive.keep(7,0,true,&proof(7,&[0])).unwrap(); // epoch representative must remain after payment
        archive.keep(8,0,true,&proof(8,&[1,8192])).unwrap();
        archive.keep(9,0,true,&proof(9,&[2])).unwrap();
        let candidates = vec![(8,vec![1,8192]),(9,vec![2])];
        let bitmap = |bucket:u64, bits:&[usize]| {
            let mut data = vec![0;RELEASED_BYTES]; data[..8].copy_from_slice(&discriminator("Released")); data[8..16].copy_from_slice(&bucket.to_le_bytes());
            for bit in bits { data[16 + bit / 8] |= 1 << (bit % 8); }
            json!({"owner":program.to_string(),"executable":false,"data":[B64.encode(data),"base64"]})
        };
        let partial = vec![bitmap(0,&[0,1]),bitmap(1,&[])];
        let left_state = Arc::new(Mutex::new(partial.clone()));
        let right_state = Arc::new(Mutex::new(vec![bitmap(0,&[0]),bitmap(1,&[])]));
        async fn answer(State(accounts):State<Arc<Mutex<Vec<Value>>>>, Json(request):Json<Value>) -> Json<Value> {
            assert_eq!(request["method"],"getMultipleAccounts");
            assert_eq!(request["params"][1]["commitment"],"finalized");
            Json(json!({"jsonrpc":"2.0","id":1,"result":{"value":accounts.lock().unwrap().clone()}}))
        }
        let left_socket = tokio::net::TcpListener::bind("127.0.0.1:5043").await.unwrap();
        let right_socket = tokio::net::TcpListener::bind("127.0.0.1:5044").await.unwrap();
        let left_app = Router::new().route("/",post(answer)).with_state(left_state.clone());
        let right_app = Router::new().route("/",post(answer)).with_state(right_state.clone());
        let left = tokio::spawn(async move { axum::serve(left_socket,left_app).await.unwrap() });
        let right = tokio::spawn(async move { axum::serve(right_socket,right_app).await.unwrap() });
        let urls = ["http://127.0.0.1:5043".into(),"http://127.0.0.1:5044".into()];
        assert!(paid_proofs(&urls,&program,&candidates).await.is_err(), "one provider's paid bit cannot prune a proof");
        *right_state.lock().unwrap() = partial.clone();
        assert!(paid_proofs(&urls,&program,&candidates).await.unwrap().is_empty(), "every withdrawal in a proof must be paid");
        let mut invalid = vec![];
        let mut wrong_owner = partial.clone(); wrong_owner[0]["owner"] = Pubkey::new_unique().to_string().into(); invalid.push(wrong_owner);
        let mut wrong_discriminator = partial.clone(); let mut bytes = B64.decode(wrong_discriminator[0]["data"][0].as_str().unwrap()).unwrap(); bytes[0] ^= 1; wrong_discriminator[0]["data"][0] = B64.encode(bytes).into(); invalid.push(wrong_discriminator);
        let mut wrong_bucket = partial.clone(); wrong_bucket[0] = bitmap(1,&[0,1]); invalid.push(wrong_bucket);
        let mut short = partial.clone(); short[0]["data"][0] = B64.encode([0;16]).into(); invalid.push(short);
        for bad in invalid {
            *left_state.lock().unwrap() = bad.clone(); *right_state.lock().unwrap() = bad;
            assert!(paid_proofs(&urls,&program,&candidates).await.is_err(), "matching malformed bitmaps cannot prune proofs");
        }
        let complete = vec![bitmap(0,&[0,1]),bitmap(1,&[0])];
        *left_state.lock().unwrap() = complete.clone(); *right_state.lock().unwrap() = complete;
        let mut paid = paid_proofs(&urls,&program,&candidates).await.unwrap();
        assert_eq!(paid,BTreeSet::from([8]));
        paid.insert(7); // applying a stale candidate can never delete the epoch representative
        assert_eq!(archive.prune_paid(&paid).unwrap(),vec![8]);
        assert!(archive.withdrawals.contains_key(&7) && archive.withdrawals.contains_key(&9));
        assert!(archive.withdrawals.len() <= archive.capacity, "pruning a fully paid proof releases archive capacity");
        left.abort(); right.abort(); let _ = left.await; let _ = right.await;
        drop(archive); std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn bridge_review_two_rpcs_disagree_no_credit() {
        let program = Pubkey::new_unique();
        let bridge = Pubkey::find_program_address(&[b"bridge"], &program).0.to_string();
        let mint = Pubkey::new_unique();
        let recipient = Pubkey::new_unique();
        let mut state = discriminator("Bridge").to_vec(); state.extend(1u64.to_le_bytes());
        let mut record = vec![0;129]; record[..8].copy_from_slice(&discriminator("DepositRecord"));
        record[48..80].copy_from_slice(recipient.as_ref()); record[80..112].copy_from_slice(mint.as_ref()); record[112..120].copy_from_slice(&10u64.to_le_bytes());
        let account = |data: Vec<u8>, owner: String| json!({"owner":owner,"executable":false,"data":[B64.encode(data),"base64"]});
        let bridge_account = account(state, program.to_string());
        let first = account(record.clone(), program.to_string());
        record[112..120].copy_from_slice(&11u64.to_le_bytes());
        let second = account(record, program.to_string());
        let mut mint_data = vec![0;82]; mint_data[44]=6; mint_data[45]=1;
        let mint_account = account(mint_data,"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".into());
        async fn answer(State((bridge, state, mint, mint_state, record)): State<(String,Value,String,Value,Value)>, Json(request): Json<Value>) -> Json<Value> {
            let value = if request["method"] == "getMultipleAccounts" { json!([record]) }
                else if request["params"][0].as_str() == Some(bridge.as_str()) { state }
                else if request["params"][0].as_str() == Some(mint.as_str()) { mint_state }
                else { record };
            Json(json!({"jsonrpc":"2.0","id":1,"result":{"value":value}}))
        }
        let a = tokio::net::TcpListener::bind("127.0.0.1:5041").await.unwrap();
        let b = tokio::net::TcpListener::bind("127.0.0.1:5042").await.unwrap();
        let app_a = Router::new().route("/", post(answer)).with_state((bridge.clone(),bridge_account.clone(),mint.to_string(),mint_account.clone(),first));
        let app_b = Router::new().route("/", post(answer)).with_state((bridge,bridge_account,mint.to_string(),mint_account,second));
        let left = tokio::spawn(async move { axum::serve(a, app_a).await.unwrap() });
        let right = tokio::spawn(async move { axum::serve(b, app_b).await.unwrap() });
        let deposits = Arc::new(Mutex::new(BTreeMap::new()));
        let result = read_new(&["http://127.0.0.1:5041".into(),"http://127.0.0.1:5042".into()], &program, &deposits, &mut HashMap::new()).await;
        left.abort(); right.abort(); let _ = left.await; let _ = right.await;
        assert!(result.is_err());
        assert!(deposits.lock().unwrap().is_empty(), "a disputed finalized record must never become a credit");
    }
}
