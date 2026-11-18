//! The deposit reader: finalized deposits on the Solana bridge program (`acrux_bridge`), read
//! through a Solana RPC (`ACRUX_BRIDGE_RPC`) every two seconds into the cache proposers carry
//! credits from and validators check them against.
use crate::execution::BridgeCredit;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

type Deposits = Arc<Mutex<BTreeMap<u64, BridgeCredit>>>;

pub async fn read_deposits(url: String, program: Pubkey, deposits: Deposits) {
    let mut decimals = HashMap::new();
    loop {
        if let Err(error) = read_new(&url, &program, &deposits, &mut decimals).await {
            // The URL may carry a key: the log names its host only.
            let host = url.split("://").nth(1).unwrap_or(&url).split(['/', '?']).next().unwrap_or("the RPC");
            tracing::warn!(error = %error.to_string().replace(&url, host), "reading bridge deposits failed");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn account(url: &str, address: &Pubkey) -> Result<Option<Vec<u8>>> {
    let value = crate::cli::rpc(url, "getAccountInfo", json!([address.to_string(), {"encoding": "base64", "commitment": "finalized"}])).await?;
    data(&value["value"])
}

fn data(value: &Value) -> Result<Option<Vec<u8>>> {
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(B64.decode(value["data"][0].as_str().context("account data")?)?))
}

fn u64_at(d: &[u8], o: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(d.get(o..o + 8).context("short account")?.try_into()?))
}

fn key_at(d: &[u8], o: usize) -> Result<Pubkey> {
    Ok(Pubkey::new_from_array(d.get(o..o + 32).context("short account")?.try_into()?))
}

/// Reads every finalized deposit past those already cached, in sequence.
async fn read_new(url: &str, program: &Pubkey, deposits: &Deposits, decimals: &mut HashMap<Pubkey, u8>) -> Result<()> {
    let bridge = Pubkey::find_program_address(&[b"bridge"], program).0;
    // Bridge: discriminator, deposits made.
    let Some(state) = account(url, &bridge).await? else { return Ok(()) };
    let made = u64_at(&state, 8)?;
    let have = deposits.lock().unwrap().len() as u64;
    for start in (have..made).step_by(100) {
        let sequences = (start..made.min(start + 100)).collect::<Vec<_>>();
        let addresses = sequences.iter().map(|s| Pubkey::find_program_address(&[b"deposit", &s.to_le_bytes()], program).0.to_string()).collect::<Vec<_>>();
        let found = crate::cli::rpc(url, "getMultipleAccounts", json!([addresses, {"encoding": "base64", "commitment": "finalized"}])).await?;
        for value in found["value"].as_array().context("accounts")? {
            // A record not finalized yet ends this read; the next one starts from it.
            let Some(record) = data(value)? else { return Ok(()) };
            // DepositRecord: discriminator, sequence, depositor, recipient, mint, amount, slot, bump.
            let mint = key_at(&record, 80)?;
            if !decimals.contains_key(&mint) {
                let mint_data = account(url, &mint).await?.context("deposit mint missing")?;
                decimals.insert(mint, *mint_data.get(44).context("short mint")?);
            }
            let credit = BridgeCredit {
                sequence: u64_at(&record, 8)?,
                solana_mint: mint.to_string(),
                decimals: decimals[&mint],
                recipient: key_at(&record, 48)?.to_string(),
                amount: u64_at(&record, 112)?,
            };
            deposits.lock().unwrap().insert(credit.sequence, credit);
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
