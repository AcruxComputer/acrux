//! A finalized Solana membership authorizes no extra privilege: the same identity signs the
//! existing Acrux join instruction for the same hub code, and pays its ordinary native rent/fees.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_sdk::{pubkey::Pubkey, signature::{read_keypair_file, Signer}};
use std::str::FromStr;

const MEMBERSHIP_LEN: usize = 154;
const HUB_LEN: usize = 113;

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset+8].try_into().unwrap())
}
fn key_at(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::new_from_array(data[offset..offset+32].try_into().unwrap())
}
fn account(value: &Value, owner: Pubkey, name: &str, length: usize) -> Result<Vec<u8>> {
    ensure!(!value.is_null(), "The finalized {name} account does not exist");
    ensure!(value["owner"].as_str() == Some(owner.to_string().as_str()) && value["executable"] == false,
        "The finalized {name} account has an invalid owner or is executable");
    ensure!(value["data"][1].as_str() == Some("base64"), "The finalized {name} account is not base64");
    let data = STANDARD.decode(value["data"][0].as_str().context("Missing account data")?)?;
    ensure!(data.len() == length, "Invalid finalized {name} account length");
    if name != "Clock" {
        ensure!(data[..8] == Sha256::digest(format!("account:{name}").as_bytes())[..8], "Invalid finalized {name} discriminator");
    }
    Ok(data)
}
fn membership(value: &Value, program: Pubkey, identity: Pubkey, bump: u8) -> Result<Vec<u8>> {
    let data = account(value,program,"Membership",MEMBERSHIP_LEN)?;
    ensure!(key_at(&data,8) == identity && data[153] == bump, "Solana membership identity or PDA bump differs");
    ensure!(data[96] <= 2, "Invalid Solana membership pending change");
    Ok(data)
}
fn hub(value: &Value, program: Pubkey, address: Pubkey, expected_code: &str) -> Result<Vec<u8>> {
    let data = account(value,program,"Hub",HUB_LEN)?;
    let code = data[16..24].split(|byte|*byte==0).next().unwrap();
    ensure!(code == expected_code.as_bytes() && data[16+code.len()..24].iter().all(|byte|*byte==0), "Solana and Acrux hub codes differ");
    let (pda,bump) = Pubkey::find_program_address(&[b"hub",expected_code.as_bytes()],&program);
    ensure!(pda == address && data[112] == bump, "Hub address or PDA bump differs");
    Ok(data)
}

async fn registry_rpc(url:&str,method:&str,params:Value)->Result<Value> {
    crate::cli::rpc(url,method,params).await.map_err(|_|anyhow::anyhow!("Finalized registry RPC {method} failed"))
}

pub async fn run(acrux_rpc: &str, solana_rpc: &str, program: &str, keypair_path: &str, code: &str) -> Result<()> {
    ensure!(!code.is_empty() && code.len() <= 8 && code.bytes().all(|byte|byte.is_ascii_uppercase()), "Hub code must contain one through eight uppercase letters");
    let program_key = Pubkey::from_str(program)?;
    let signer = read_keypair_file(keypair_path).map_err(|error|anyhow::anyhow!("Cannot read identity keypair: {error}"))?;
    let identity = signer.pubkey();
    let (member_address,member_bump) = Pubkey::find_program_address(&[b"member",identity.as_ref()],&program_key);
    let first = registry_rpc(solana_rpc,"getAccountInfo",json!([member_address.to_string(),{"encoding":"base64","commitment":"finalized"}])).await?;
    let first_slot = first["context"]["slot"].as_u64().context("Missing finalized Solana context")?;
    let initial = membership(&first["value"],program_key,identity,member_bump)?;
    let mut hub_addresses = vec![];
    for address in [key_at(&initial,40),key_at(&initial,97)] {
        if address != Pubkey::default() && !hub_addresses.contains(&address) { hub_addresses.push(address); }
    }
    let clock_address = solana_sdk::sysvar::clock::id();
    let keys = [member_address,clock_address].into_iter().chain(hub_addresses.iter().copied()).map(|key|key.to_string()).collect::<Vec<_>>();
    // Membership, Clock and the possible hubs come from one finalized bank. In particular a
    // move/leave cannot be evaluated against a stale getEpochInfo from before its boundary.
    let read = registry_rpc(solana_rpc,"getMultipleAccounts",json!([keys,{"encoding":"base64","commitment":"finalized","minContextSlot":first_slot}])).await?;
    ensure!(read["context"]["slot"].as_u64().is_some_and(|slot|slot >= first_slot), "Solana returned an earlier finalized bank");
    let values = read["value"].as_array().context("Missing finalized Solana accounts")?;
    ensure!(values.len() == 2+hub_addresses.len(), "Solana returned a different account count");
    let member = membership(&values[0],program_key,identity,member_bump)?;
    let clock = account(&values[1],solana_sdk::sysvar::id(),"Clock",40)?;
    let epoch = u64_at(&clock,16);
    let changed = member[96] != 0 && epoch >= u64_at(&member,137);
    let (selected,hub_id,effective) = if changed {
        ensure!(member[96] == 2, "The Solana membership has already left its hub");
        (key_at(&member,97),u64_at(&member,129),u64_at(&member,137))
    } else { (key_at(&member,40),u64_at(&member,72),u64_at(&member,88)) };
    ensure!(selected != Pubkey::default() && epoch >= effective, "Solana membership is not effective yet");
    let at = hub_addresses.iter().position(|address|*address == selected).context("Solana membership changed while it was read; run carry again")?;
    let solana_hub = hub(&values[at+2],program_key,selected,code)?;
    ensure!(u64_at(&solana_hub,8) == hub_id, "Solana membership names a released or recreated hub");
    let acrux_hub_address = Pubkey::find_program_address(&[b"hub",code.as_bytes()],&program_key).0;
    let acrux_hub = registry_rpc(acrux_rpc,"getAccountInfo",json!([acrux_hub_address.to_string(),{"encoding":"base64","commitment":"finalized"}])).await?;
    hub(&acrux_hub["value"],program_key,acrux_hub_address,code)?;
    println!("Carrying finalized Solana membership in {code} at epoch {epoch}; the same identity signs Acrux's ordinary join.");
    crate::cli::hub(acrux_rpc,program,keypair_path,crate::cli::HubAction::Join { code:code.to_owned() }).await
}
