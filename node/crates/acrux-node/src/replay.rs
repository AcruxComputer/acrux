//! `acrux replay`: one recorded Solana mainnet block run through Acrux's merge and selection
//! (Algorithm 4, four tracks), printed beside the order its leader gave it. The block comes from a
//! file holding a `getBlock` result or from a Solana RPC (versions legacy, 0 and 1); Acrux slot after Acrux slot takes what
//! the one before could not place, until the whole block is placed.
use crate::selection::{select_with_fees, sort_transactions, Candidate, Selection, TRACKS, TRACK_CAPACITY_CU};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

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

const WSOL: &str = "So11111111111111111111111111111111111111112";

/// What one transaction moved: each owner's net change per asset across the token accounts it
/// owns (the signer's lamports counted as SOL), and every token account whose balance changed,
/// with its owner. Wrapped SOL counts as SOL.
#[derive(Clone, Debug)]
pub struct Flow {
    pub index: usize,
    pub signature: String,
    pub signer: String,
    /// Raw units per asset ("SOL" for lamports and wrapped SOL), per owner.
    pub holdings: BTreeMap<String, BTreeMap<String, i128>>,
    pub decimals: BTreeMap<String, u8>,
    /// Changed token accounts and their owners.
    pub changed: BTreeMap<String, String>,
}

impl Flow {
    fn delta(&self, owner: &str, asset: &str) -> i128 {
        self.holdings.get(owner).and_then(|h| h.get(asset)).copied().unwrap_or(0)
    }
    /// The changed accounts that belong to neither of these owners: the pools'.
    fn pool(&self, others: &[&str]) -> BTreeSet<String> {
        self.changed.iter().filter(|(_, owner)| !others.contains(&owner.as_str())).map(|(account, _)| account.clone()).collect()
    }
}

/// Every successful non-vote transaction's flow, in the leader's order.
pub fn flows(block: &Value) -> Vec<Flow> {
    let mut out = vec![];
    for (index, entry) in block["transactions"].as_array().into_iter().flatten().enumerate() {
        let meta = &entry["meta"];
        if !meta["err"].is_null() {
            continue;
        }
        let message = &entry["transaction"]["message"];
        let mut keys = message["accountKeys"].as_array().cloned().unwrap_or_default();
        keys.extend(meta["loadedAddresses"]["writable"].as_array().cloned().unwrap_or_default());
        keys.extend(meta["loadedAddresses"]["readonly"].as_array().cloned().unwrap_or_default());
        let keys = keys.iter().map(|k| k.as_str().unwrap_or_default().to_owned()).collect::<Vec<_>>();
        if keys.iter().any(|k| k == VOTE) {
            continue;
        }
        let Some(signer) = keys.first().cloned() else { continue };
        let asset = |mint: &str| if mint == WSOL { "SOL".to_owned() } else { mint.to_owned() };
        let mut holdings = BTreeMap::<String, BTreeMap<String, i128>>::new();
        let mut decimals = BTreeMap::from([("SOL".to_owned(), 9u8)]);
        let mut changed = BTreeMap::new();
        // Each token account's mint, owner, and balance before and after, by its index.
        let mut balances = BTreeMap::<u64, (String, String, i128, i128)>::new();
        for (after, list) in [(false, &meta["preTokenBalances"]), (true, &meta["postTokenBalances"])] {
            for b in list.as_array().into_iter().flatten() {
                let Some(i) = b["accountIndex"].as_u64() else { continue };
                let raw = b["uiTokenAmount"]["amount"].as_str().and_then(|a| a.parse::<i128>().ok()).unwrap_or(0);
                let mint = b["mint"].as_str().unwrap_or_default().to_owned();
                decimals.insert(asset(&mint), b["uiTokenAmount"]["decimals"].as_u64().unwrap_or(0) as u8);
                let e = balances.entry(i).or_insert((mint, b["owner"].as_str().unwrap_or_default().to_owned(), 0, 0));
                if after { e.3 = raw } else { e.2 = raw }
            }
        }
        for (i, (mint, owner, before, after)) in balances {
            if after == before {
                continue;
            }
            *holdings.entry(owner.clone()).or_default().entry(asset(&mint)).or_default() += after - before;
            if let Some(account) = keys.get(i as usize) {
                changed.insert(account.clone(), owner);
            }
        }
        // The signer's lamports with the fee given back: a swap paid or paid out in native SOL.
        let lamports = meta["postBalances"][0].as_i64().unwrap_or(0) as i128 - meta["preBalances"][0].as_i64().unwrap_or(0) as i128 + meta["fee"].as_i64().unwrap_or(0) as i128;
        *holdings.entry(signer.clone()).or_default().entry("SOL".into()).or_default() += lamports;
        for h in holdings.values_mut() {
            h.retain(|_, d| *d != 0);
        }
        holdings.retain(|_, h| !h.is_empty());
        out.push(Flow { index, signature: entry["transaction"]["signatures"][0].as_str().unwrap_or_default().to_owned(), signer, holdings, decimals, changed });
    }
    out
}

/// Three transactions on one pool in one block: a front leg in which a trader buys what the
/// victim buys, the victim, and a back leg in which the same trader sells it back. The trader is
/// the owner of the accounts that hold the front leg's tokens, whoever signs; the victim signs its own.
#[derive(Clone, Debug)]
pub struct Sandwich {
    pub front: Flow,
    pub victim: Flow,
    pub back: Flow,
    pub trader: String,
    pub victim_owner: String,
    /// The asset the victim bought and the one it paid with.
    pub bought: String,
    pub paid: String,
    pub vaults: Vec<String>,
}

impl Sandwich {
    /// What the two legs left the trader in the asset the victim paid with, raw units.
    pub fn take(&self) -> i128 {
        self.front.delta(&self.trader, &self.paid) + self.back.delta(&self.trader, &self.paid)
    }
}

pub fn sandwiches(flows: &[Flow]) -> Vec<Sandwich> {
    let mut found: Vec<Sandwich> = vec![];
    for (f, front) in flows.iter().enumerate() {
        for (trader, held) in &front.holdings {
            let bought = held.iter().filter(|(_, d)| **d > 0).map(|(a, _)| a.clone()).collect::<Vec<_>>();
            let paid = held.iter().filter(|(_, d)| **d < 0).map(|(a, _)| a.clone()).collect::<Vec<_>>();
            for back in &flows[f + 1..] {
                for x in &bought {
                    for y in &paid {
                        // The back leg sells what the front leg bought (the same amount, within 2%),
                        // for more of what it paid with than the front leg spent.
                        let (got, sold) = (front.delta(trader, x), -back.delta(trader, x));
                        if !(sold * 100 >= got * 98 && sold * 100 <= got * 102 && back.delta(trader, y) > 0 && front.delta(trader, y) + back.delta(trader, y) > 0) {
                            continue;
                        }
                        let pool = front.pool(&[trader]).intersection(&back.pool(&[trader])).cloned().collect::<BTreeSet<_>>();
                        if pool.is_empty() {
                            continue;
                        }
                        // A pool's own authority also gains and loses both assets; it is told
                        // apart by the victim's trade moving its accounts, which a trader's it never does.
                        for victim in flows.iter().filter(|v| v.index > front.index && v.index < back.index && !v.changed.values().any(|o| o == trader)) {
                            for (owner, _) in victim.holdings.iter().filter(|(o, _)| **o == victim.signer && *o != trader) {
                                if !(victim.delta(owner, x) > 0 && victim.delta(owner, y) < 0) {
                                    continue;
                                }
                                let shared = victim.pool(&[owner, trader]).intersection(&pool).cloned().collect::<Vec<_>>();
                                if shared.is_empty() || found.iter().any(|s| s.victim.signature == victim.signature) {
                                    continue;
                                }
                                found.push(Sandwich { front: front.clone(), victim: victim.clone(), back: back.clone(), trader: trader.clone(), victim_owner: owner.clone(), bought: x.clone(), paid: y.clone(), vaults: shared });
                            }
                        }
                    }
                }
            }
        }
    }
    found
}

fn amount(raw: i128, decimals: u8) -> String {
    let scale = 10i128.pow(decimals as u32);
    let fraction = format!("{:0width$}", raw.abs() % scale, width = decimals as usize);
    let fraction = fraction.trim_end_matches('0');
    format!("{}{}{}{}", if raw < 0 { "-" } else { "" }, group((raw.abs() / scale) as u64), if fraction.is_empty() { "" } else { "." }, fraction)
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
    let found = sandwiches(&flows(&block));
    println!();
    println!("Sandwiches: {}", found.len());
    for s in &found {
        let name = |asset: &str| if asset == "SOL" { "SOL".to_owned() } else { short(asset) };
        let decimals = s.victim.decimals.get(&s.paid).copied().unwrap_or(0);
        println!("  victim {} at {} bought {} with {}", s.victim.signature, s.victim.index + 1, name(&s.bought), name(&s.paid));
        println!("    front {} at {}, back {} at {}, trader {}", s.front.signature, s.front.index + 1, s.back.signature, s.back.index + 1, s.trader);
        println!("    pool vaults {}; the two legs left the trader {} {}", s.vaults.iter().map(|v| short(v)).collect::<Vec<_>>().join(", "), amount(s.take(), decimals), name(&s.paid));
    }
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

    /// A swap by `owner` against the pool vaults named, which belong to "pool".
    fn swap(index: usize, owner: &str, deltas: &[(&str, i128)], vaults: &[&str]) -> Flow {
        let mut changed = vaults.iter().map(|v| (v.to_string(), "pool".to_owned())).collect::<BTreeMap<_, _>>();
        changed.insert(format!("{owner}-account"), owner.into());
        Flow {
            index,
            signature: format!("sig{index}"),
            signer: owner.into(),
            holdings: BTreeMap::from([(owner.to_owned(), deltas.iter().map(|(a, d)| (a.to_string(), *d)).collect())]),
            decimals: BTreeMap::from([("SOL".to_owned(), 9)]),
            changed,
        }
    }

    #[test]
    fn a_sandwich_is_found_and_a_plain_round_trip_is_not() {
        let flows = vec![
            swap(0, "bot", &[("SOL", -1_000_000_000), ("MEME", 5_000)], &["vaultSOL", "vaultMEME"]),
            swap(1, "alice", &[("SOL", -2_000_000_000), ("MEME", 9_000)], &["vaultSOL", "vaultMEME"]),
            swap(2, "carol", &[("SOL", -1_000), ("OTHER", 7)], &["otherVault"]),
            swap(3, "bot", &[("SOL", 1_050_000_000), ("MEME", -5_000)], &["vaultSOL", "vaultMEME"]),
            // A buy and a sell by one signer with no one between them on that pool: no sandwich.
            swap(4, "dave", &[("SOL", -10), ("X", 1)], &["vaultX"]),
            swap(5, "dave", &[("SOL", 9), ("X", -1)], &["vaultX"]),
        ];
        let found = sandwiches(&flows);
        assert_eq!(found.len(), 1);
        let s = &found[0];
        assert_eq!((s.front.index, s.victim.index, s.back.index), (0, 1, 3));
        assert_eq!((s.bought.as_str(), s.paid.as_str()), ("MEME", "SOL"));
        assert_eq!(s.take(), 50_000_000);
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
