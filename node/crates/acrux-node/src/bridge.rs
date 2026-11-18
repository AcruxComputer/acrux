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
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex},
};

type Deposits = Arc<Mutex<BTreeMap<u64, BridgeCredit>>>;

const PROOFS: TableDefinition<u64, &[u8]> = TableDefinition::new("proofs");

/// A bounded archive separate from the gateway's rolling window. It holds every withdrawal's
/// proof and one cblock per bridge epoch, so a stopped relayer can also hand missed epochs on.
/// A persistent net keeps it across restarts; the operator sizes its capacity for the downtime
/// and withdrawal volume they support. Capacity exhaustion fails before another state checkpoint; accepted proofs are never evicted.
pub struct ProofArchive {
    pub proofs: BTreeMap<u64, Value>,
    pub epochs: BTreeMap<u64, u64>,
    capacity: usize,
    epoch_slots: u64,
    db: Option<Database>,
}

impl ProofArchive {
    pub fn open(path: Option<&Path>, epoch_slots: u64, finalized_through: Option<u64>) -> Result<Self> {
        let capacity: usize = std::env::var("ACRUX_BRIDGE_PROOF_MAX")
            .context("ACRUX_BRIDGE_PROOF_MAX is required when the bridge is enabled")?
            .parse().context("ACRUX_BRIDGE_PROOF_MAX must be a positive integer")?;
        anyhow::ensure!(capacity > 0, "ACRUX_BRIDGE_PROOF_MAX must be a positive integer");
        let db = path.map(|path| -> Result<Database> {
            std::fs::create_dir_all(path.parent().context("bridge proof archive path")?)?;
            Ok(Database::create(path)?)
        }).transpose()?;
        let mut archive = Self { proofs: BTreeMap::new(), epochs: BTreeMap::new(), capacity, epoch_slots, db };
        if let Some(db) = &archive.db {
            let w = db.begin_write()?;
            {
                let mut table = w.open_table(PROOFS)?;
                let mut expired = vec![];
                for entry in table.iter()? {
                    let (slot, proof) = entry?;
                    let proof: Value = serde_json::from_slice(proof.value())?;
                    anyhow::ensure!(proof["epochSlots"].as_u64() == Some(epoch_slots), "saved bridge proofs use a different epochSlots");
                    if finalized_through.is_some_and(|cblock| slot.value() <= cblock) {
                        archive.proofs.insert(slot.value(), proof);
                    } else {
                        // A crash can leave a prepared proof ahead of the state checkpoint.
                        // It was never published and must not survive that checkpoint's rollback.
                        expired.push(slot.value());
                    }
                }
                anyhow::ensure!(archive.proofs.len() <= capacity, "Bridge proof archive exceeds ACRUX_BRIDGE_PROOF_MAX; increase the capacity; no saved proof was deleted");
                for slot in expired {
                    table.remove(slot)?;
                }
            }
            w.commit()?;
        }
        for slot in archive.proofs.keys() {
            archive.epochs.entry(slot * 4 / epoch_slots).or_insert(*slot);
        }
        Ok(archive)
    }

    /// Called only for finalized cblocks. Returns whether the archive changed, and expired slots.
    pub fn keep(&mut self, slot: u64, epoch: u64, withdrawal: bool, proof: &Value) -> Result<Option<Vec<u64>>> {
        if !withdrawal && self.epochs.contains_key(&epoch) {
            return Ok(None);
        }
        let expired = vec![];
        anyhow::ensure!(self.proofs.len() < self.capacity || self.proofs.contains_key(&slot),
            "Bridge proof archive is full; increase ACRUX_BRIDGE_PROOF_MAX before continuing; no saved proof was deleted");
        if let Some(db) = &self.db {
            let w = db.begin_write()?;
            {
                let mut table = w.open_table(PROOFS)?;
                table.insert(slot, serde_json::to_vec(proof)?.as_slice())?;
                for old in &expired { table.remove(*old)?; }
            }
            w.commit()?;
        }
        self.proofs.insert(slot, proof.clone());
        for old in &expired {
            self.proofs.remove(old);
            tracing::warn!(cblock = old, capacity = self.capacity, "bridge proof archive capacity reached; oldest proof expired");
        }
        self.epochs.clear();
        for held in self.proofs.keys() {
            self.epochs.entry(held * 4 / self.epoch_slots).or_insert(*held);
        }
        Ok(Some(expired))
    }
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

pub async fn read_deposits(urls: [String; 2], program: Pubkey, deposits: Deposits) {
    let mut decimals = HashMap::new();
    loop {
        if read_new(&urls, &program, &deposits, &mut decimals).await.is_err() {
            // Provider errors can include keyed URLs. Neither addresses nor payloads go to logs.
            tracing::warn!("bridge deposit read failed: both RPCs must agree on finalized accounts");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
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
