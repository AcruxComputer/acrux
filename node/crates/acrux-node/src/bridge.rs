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
