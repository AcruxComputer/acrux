//! `acrux replay`: one recorded Solana mainnet block run through Acrux's merge and selection
//! (Algorithm 4, four tracks), printed beside the order its leader gave it. The block comes from a
//! file holding a `getBlock` result or from a Solana RPC (versions legacy, 0 and 1); Acrux slot after Acrux slot takes what
//! the one before could not place, until the whole block is placed.
use crate::selection::{select_with_fees, sort_transactions, Candidate, Selection, TRACKS, TRACK_CAPACITY_CU};
use anyhow::{Context, Result};
use serde_json::{json, Value};

const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
const VOTE: &str = "Vote111111111111111111111111111111111111111";

/// A block's transactions as Acrux candidates, in the leader's order, and how many vote
/// transactions were left out (Acrux has none).
pub fn candidates(block: &Value) -> Result<(Vec<Candidate>, usize)> {
    let mut out = vec![];
    let mut votes = 0;
    for entry in block["transactions"].as_array().context("the block has no transactions list")? {
        let message = &entry["transaction"]["message"];
        let header = &message["header"];
        let signed = header["numRequiredSignatures"].as_u64().context("header")? as usize;
        let readonly_signed = header["numReadonlySignedAccounts"].as_u64().context("header")? as usize;
        let readonly_unsigned = header["numReadonlyUnsignedAccounts"].as_u64().context("header")? as usize;
        let statics = message["accountKeys"].as_array().context("accountKeys")?.iter().map(|k| k.as_str().unwrap_or_default().to_owned()).collect::<Vec<_>>();
        let loaded = |kind: &str| entry["meta"]["loadedAddresses"][kind].as_array().map(|keys| keys.iter().filter_map(|k| k.as_str().map(str::to_owned)).collect::<Vec<_>>()).unwrap_or_default();
        let (loaded_writable, loaded_readonly) = (loaded("writable"), loaded("readonly"));
        let keys = statics.iter().chain(&loaded_writable).chain(&loaded_readonly).cloned().collect::<Vec<_>>();
        let instructions = message["instructions"].as_array().context("instructions")?;
        let programs = instructions.iter().filter_map(|ix| ix["programIdIndex"].as_u64().map(|i| i as usize)).collect::<Vec<_>>();
        if programs.iter().any(|&i| keys.get(i).is_some_and(|k| k == VOTE)) {
            votes += 1;
            continue;
        }
        // ComputeBudget tags 2 (limit) and 3 (price), read as validate_transaction reads them.
        let (mut limit, mut price, mut normal) = (None, 0u64, 0u64);
        for ix in instructions {
            let program = ix["programIdIndex"].as_u64().and_then(|i| keys.get(i as usize)).map(String::as_str);
            if program != Some(COMPUTE_BUDGET) {
                normal += 1;
                continue;
            }
            let data = bs58::decode(ix["data"].as_str().unwrap_or_default()).into_vec().unwrap_or_default();
            match data.as_slice() {
                [2, rest @ ..] if rest.len() == 4 => limit = Some(u32::from_le_bytes(rest.try_into().unwrap()) as u64),
                [3, rest @ ..] if rest.len() == 8 => price = u64::from_le_bytes(rest.try_into().unwrap()),
                _ => {}
            }
        }
        // A v1 transaction carries its budget in its message: a CU limit and a total priority fee
        // in lamports, taken here per CU in micro-lamports as tag 3 states it.
        let config = &message["transactionConfig"];
        if let Some(units) = config["computeUnitLimit"].as_u64() {
            limit = Some(units);
            price = config["priorityFee"].as_u64().unwrap_or(0).saturating_mul(1_000_000) / units.max(1);
        }
        let limit = limit.unwrap_or((normal * 200_000).min(1_400_000)).clamp(1, 1_400_000);
        // Writable: the static keys the header marks writable, less any called as a program,
        // plus the lookup tables' writable addresses.
        let writable = statics
            .iter()
            .enumerate()
            .filter(|(i, _)| if *i < signed { *i < signed - readonly_signed } else { *i < statics.len() - readonly_unsigned })
            .filter(|(i, _)| !programs.contains(i))
            .map(|(_, k)| k.clone())
            .chain(loaded_writable)
            .collect();
        let signature = entry["transaction"]["signatures"][0].as_str().context("signature")?.to_owned();
        out.push(Candidate { id: signature, priority_fee_per_cu: price, compute_limit: limit, writable_accounts: writable });
    }
    Ok((out, votes))
}

/// Acrux slot after Acrux slot, each taking what the one before left, until everything is placed.
pub fn place(candidates: &[Candidate]) -> Vec<Selection> {
    let mut slots = vec![];
    let mut remaining = candidates.to_vec();
    while !remaining.is_empty() {
        let selection = select_with_fees(&remaining, TRACK_CAPACITY_CU, |_| true);
        let left = selection.rejected_ids.iter().collect::<std::collections::BTreeSet<_>>();
        remaining.retain(|c| left.contains(&c.id));
        slots.push(selection);
    }
    slots
}

fn short(signature: &str) -> String {
    format!("{}…", &signature[..signature.len().min(10)])
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Reads the block, places it and prints both orders: the leader's, and Acrux's merged order
/// with each transaction's Acrux slot, track and start.
pub async fn run(file: Option<&str>, rpc: Option<&str>, slot: Option<u64>, save: Option<&str>, rows: usize) -> Result<()> {
    let (block, number) = match (file, rpc, slot) {
        (Some(file), _, _) => {
            let block: Value = serde_json::from_slice(&std::fs::read(file).with_context(|| format!("Read {file}"))?)?;
            let number = block["slot"].as_u64().or(slot).unwrap_or(0);
            (block, number)
        }
        (None, Some(url), Some(slot)) => {
            let params = json!([slot, {"encoding": "json", "maxSupportedTransactionVersion": 1, "transactionDetails": "full", "rewards": false, "commitment": "finalized"}]);
            // The URL may carry a key: an error names the host only.
            let host = url.split("://").nth(1).unwrap_or(url).split(['/', '?']).next().unwrap_or("the RPC").to_owned();
            let mut block = crate::cli::rpc(url, "getBlock", params).await.map_err(|e| anyhow::anyhow!("getBlock {slot} from {host}: {}", e.to_string().replace(url, &host)))?;
            block["slot"] = json!(slot);
            (block, slot)
        }
        _ => anyhow::bail!("Give --block <file>, or --rpc <url> with --slot <n>"),
    };
    if let Some(save) = save {
        std::fs::write(save, serde_json::to_vec(&block)?).with_context(|| format!("Write {save}"))?;
    }
    let (leader, votes) = candidates(&block)?;
    let slots = place(&leader);
    let mut merged = vec![];
    for (index, selection) in slots.iter().enumerate() {
        let mut placed = selection.placements.clone();
        let order = leader.iter().filter(|c| selection.selected_ids.contains(&c.id)).cloned().collect::<Vec<_>>();
        let mut sorted = order.clone();
        sort_transactions(&mut sorted);
        placed.sort_by_key(|p| sorted.iter().position(|c| c.id == p.tx_id));
        for p in placed {
            let candidate = leader.iter().find(|c| c.id == p.tx_id).unwrap().clone();
            merged.push((candidate, index + 1, p));
        }
    }

    let requested = leader.iter().map(|c| c.compute_limit).sum::<u64>();
    println!("mainnet slot {}: {} transactions ({} votes left out), {} CU requested", group(number), leader.len(), votes, group(requested));
    println!();
    println!("{:>5}  {:<52}  {:<60}", "", "the leader's order", "Acrux's merged order (slot, track, start)");
    for i in 0..leader.len().min(rows) {
        let l = &leader[i];
        let (m, s, p) = &merged[i];
        println!(
            "{:>5}  {:<12} {:>14} µ/CU {:>9} CU  {:<12} {:>14} µ/CU {:>9} CU  s{} t{} @{}",
            i + 1, short(&l.id), group(l.priority_fee_per_cu), group(l.compute_limit), short(&m.id), group(m.priority_fee_per_cu), group(m.compute_limit), s, p.track, group(p.start_cu)
        );
    }
    if leader.len() > rows {
        println!("{:>5}  … {} more (--rows)", "", leader.len() - rows);
    }
    println!();
    for (index, selection) in slots.iter().enumerate() {
        let used = (1..=TRACKS).map(|t| selection.placements.iter().filter(|p| p.track == t).map(|p| p.length_cu).sum::<u64>()).map(|cu| format!("{:>9}", group(cu))).collect::<Vec<_>>();
        println!("Acrux slot {:>3}: {:>5} transactions, CU per track {}", index + 1, selection.selected_ids.len(), used.join(" "));
    }
    println!();
    println!("The block took {} Acrux slots of {} tracks × {} CU.", slots.len(), TRACKS, group(TRACK_CAPACITY_CU));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(signature: &str, keys: &[&str], header: [u64; 3], instructions: Value, loaded_writable: &[&str]) -> Value {
        json!({
            "transaction": {"signatures": [signature], "message": {"header": {"numRequiredSignatures": header[0], "numReadonlySignedAccounts": header[1], "numReadonlyUnsignedAccounts": header[2]}, "accountKeys": keys, "instructions": instructions}},
            "meta": {"loadedAddresses": {"writable": loaded_writable, "readonly": []}}
        })
    }

    #[test]
    fn budget_writable_keys_and_votes() {
        let limit = bs58::encode([2u8, 0x40, 0x0d, 0x03, 0x00]).into_string(); // 200,000
        let price = bs58::encode([3u8, 0xe8, 0x03, 0, 0, 0, 0, 0, 0]).into_string(); // 1,000
        let block = json!({"transactions": [
            tx("sigA", &["payer", "pool", COMPUTE_BUDGET, "amm"], [1, 0, 2], json!([{"programIdIndex": 2, "data": limit}, {"programIdIndex": 2, "data": price}, {"programIdIndex": 3, "data": ""}]), &["vault"]),
            tx("sigB", &["payer2", VOTE], [1, 0, 1], json!([{"programIdIndex": 1, "data": ""}]), &[]),
            tx("sigC", &["payer3", "other", "prog1", "prog2"], [1, 0, 2], json!([{"programIdIndex": 2, "data": ""}, {"programIdIndex": 3, "data": ""}]), &[]),
        ]});
        let mut block = block;
        let mut v1 = tx("sigD", &["payer4", "amm"], [1, 0, 1], json!([{"programIdIndex": 1, "data": ""}]), &[]);
        v1["transaction"]["message"]["transactionConfig"] = json!({"priorityFee": 74, "computeUnitLimit": 37});
        block["transactions"].as_array_mut().unwrap().push(v1);
        let (candidates, votes) = candidates(&block).unwrap();
        // 74 lamports over 37 CU: 2,000,000 micro-lamports a CU.
        assert_eq!((candidates[2].compute_limit, candidates[2].priority_fee_per_cu), (37, 2_000_000));
        assert_eq!(votes, 1);
        assert_eq!(candidates[0].compute_limit, 200_000);
        assert_eq!(candidates[0].priority_fee_per_cu, 1_000);
        assert_eq!(candidates[0].writable_accounts, vec!["payer", "pool", "vault"]);
        // No limit set: 200,000 for each of its two instructions.
        assert_eq!(candidates[1].compute_limit, 400_000);
        assert_eq!(candidates[1].writable_accounts, vec!["payer3", "other"]);
    }

    #[test]
    fn a_block_larger_than_a_slot_spills_into_the_next() {
        // Five 1.4M-CU transactions on one writable account run one after another: a track holds
        // one at a time and each Acrux slot holds one of them.
        let block = (0..5).map(|i| Candidate { id: format!("s{i}"), priority_fee_per_cu: i, compute_limit: 1_400_000, writable_accounts: vec!["pool".into()] }).collect::<Vec<_>>();
        let slots = place(&block);
        assert_eq!(slots.len(), 5);
        assert_eq!(slots[0].selected_ids, vec!["s4"]);
        // Independent accounts fill the four tracks side by side.
        let block = (0..8).map(|i| Candidate { id: format!("s{i}"), priority_fee_per_cu: 0, compute_limit: 1_000_000, writable_accounts: vec![format!("a{i}")] }).collect::<Vec<_>>();
        assert_eq!(place(&block).len(), 1);
    }
}
