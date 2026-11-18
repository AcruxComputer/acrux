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

/// Pump.fun's TradeEvent, as the program logs it after every trade on a bonding curve.
const TRADE_EVENT: [u8; 8] = [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee];

/// One Pump.fun trade as its event states it: the SOL that moved to or from the curve, the tokens,
/// and the curve's virtual reserves after it.
#[derive(Clone, Copy, Debug)]
struct PumpTrade {
    sol: i128,
    tokens: i128,
    virtual_sol: i128,
    virtual_tokens: i128,
}

fn pump_trade(entry: &Value, mint: &str) -> Option<PumpTrade> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let mint = bs58::decode(mint).into_vec().ok()?;
    entry["meta"]["logMessages"].as_array()?.iter().filter_map(|l| l.as_str()?.strip_prefix("Program data: ")).find_map(|data| {
        let d = STANDARD.decode(data).ok()?;
        // discriminator, mint, sol, tokens, is_buy, user, timestamp, virtual sol, virtual tokens
        if d.len() < 113 || d[..8] != TRADE_EVENT || d[8..40] != mint[..] {
            return None;
        }
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap()) as i128;
        Some(PumpTrade { sol: u64_at(40), tokens: u64_at(48), virtual_sol: u64_at(97), virtual_tokens: u64_at(105) })
    })
}

/// A token account's mint and balance before and after a transaction, by address.
fn token_account(entry: &Value, account: &str) -> Option<(String, i128, i128)> {
    let meta = &entry["meta"];
    let mut keys = entry["transaction"]["message"]["accountKeys"].as_array().cloned().unwrap_or_default();
    keys.extend(meta["loadedAddresses"]["writable"].as_array().cloned().unwrap_or_default());
    keys.extend(meta["loadedAddresses"]["readonly"].as_array().cloned().unwrap_or_default());
    let index = keys.iter().position(|k| k.as_str() == Some(account))? as u64;
    let find = |list: &Value| list.as_array()?.iter().find(|b| b["accountIndex"].as_u64() == Some(index)).cloned();
    let (pre, post) = (find(&meta["preTokenBalances"]), find(&meta["postTokenBalances"]));
    let mint = pre.as_ref().or(post.as_ref())?["mint"].as_str()?.to_owned();
    let raw = |b: Option<Value>| b.and_then(|b| b["uiTokenAmount"]["amount"].as_str()?.parse::<i128>().ok()).unwrap_or(0);
    Some((if mint == WSOL { "SOL".into() } else { mint }, raw(pre), raw(post)))
}

/// One trade against a pool, as its two vaults recorded it: their balances before and the pool's change.
#[derive(Clone, Copy, Debug)]
struct PoolSwap {
    x0: f64,
    y0: f64,
    dx: f64,
    dy: f64,
}

impl PoolSwap {
    /// The trade's input into the pool, its output out of it, and whether it bought X.
    fn trade(&self) -> Option<(f64, f64, bool)> {
        match (self.dx < 0.0, self.dy < 0.0) {
            (true, false) if self.dy > 0.0 => Some((self.dy, -self.dx, true)),
            (false, true) if self.dx > 0.0 => Some((self.dx, -self.dy, false)),
            _ => None,
        }
    }
}

/// A pool on the constant-product rule over its vaults' balances, with a fee off the input, or
/// off the Y side whichever way the trade goes (a pool that collects its fee in the quote asset).
#[derive(Clone, Copy, Debug)]
struct PoolModel {
    fee: f64,
    fee_on_y: bool,
}

impl PoolModel {
    fn output(&self, s: &PoolSwap, input: f64, buys_x: bool) -> f64 {
        let e = input * (1.0 - self.fee);
        match (buys_x, self.fee_on_y) {
            (true, _) => s.x0 * e / (s.y0 + e),
            (false, true) => (1.0 - self.fee) * s.y0 * input / (s.x0 + input),
            (false, false) => s.y0 * e / (s.x0 + e),
        }
    }
    /// The worst relative miss over the trades.
    fn miss(&self, swaps: &[PoolSwap]) -> f64 {
        swaps.iter().filter_map(|s| s.trade().map(|(i, o, x)| ((self.output(s, i, x) / o) - 1.0).abs())).fold(0.0, f64::max)
    }
}

/// How closely the model must reproduce every trade the block made against the pool to be trusted.
const POOL_FIT: f64 = 0.002;

/// Fits the pool to every trade the block made against it (two at least) on the constant-product
/// rule, the fee being the median each trade implies, in either placement. A pool it does not
/// reproduce within `POOL_FIT` on every trade (concentrated liquidity, bins, a fee that moves) is
/// not priced.
fn fit_pool(swaps: &[PoolSwap]) -> Option<(&'static str, PoolModel)> {
    if swaps.len() < 2 {
        return None;
    }
    let model = |fee_on_y: bool| {
        let mut fees = swaps
            .iter()
            .filter_map(|s| {
                let (input, out, buys_x) = s.trade()?;
                match (buys_x, fee_on_y) {
                    (false, true) => Some(1.0 - out * (s.x0 + input) / (s.y0 * input)),
                    _ => {
                        let (to, from) = if buys_x { (s.x0, s.y0) } else { (s.y0, s.x0) };
                        (to > out).then(|| 1.0 - out * from / (to - out) / input)
                    }
                }
            })
            .collect::<Vec<_>>();
        fees.sort_by(f64::total_cmp);
        fees.get(fees.len() / 2).copied().filter(|f| (0.0..0.2).contains(f)).map(|fee| PoolModel { fee, fee_on_y })
    };
    [false, true].into_iter().filter_map(model).map(|m| (m.miss(swaps), m)).filter(|(miss, _)| *miss < POOL_FIT).min_by(|x, y| x.0.total_cmp(&y.0)).map(|(_, m)| ("constant-product", m))
}

/// The payloads (after the 8-byte discriminator) of every Anchor event with this discriminator in
/// a transaction: logged (`Program data:`) or emitted through a self-invocation (`emit_cpi!`).
fn anchor_events(entry: &Value, discriminator: [u8; 8]) -> Vec<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    const CPI_EVENT: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
    let logged = entry["meta"]["logMessages"].as_array().into_iter().flatten().filter_map(|l| STANDARD.decode(l.as_str()?.strip_prefix("Program data: ")?).ok());
    let inner = entry["meta"]["innerInstructions"].as_array().into_iter().flatten().flat_map(|i| i["instructions"].as_array().into_iter().flatten()).filter_map(|ix| {
        let data = bs58::decode(ix["data"].as_str()?).into_vec().ok()?;
        (data.len() > 16 && data[..8] == CPI_EVENT).then(|| data[8..].to_vec())
    });
    logged.chain(inner).filter(|d| d.len() > 8 && d[..8] == discriminator).map(|d| d[8..].to_vec()).collect()
}

/// One trade on a concentrated-liquidity pool as its program's own event states it: the pool, which
/// way it went (token A in), what went in after the fee, what came out before any fee taken off
/// the output, the square-root price after it (token B per token A), and the pool's liquidity when
/// the event states it.
#[derive(Clone, Copy, Debug)]
struct RangeTrade {
    pool: [u8; 32],
    a_to_b: bool,
    input: f64,
    output: f64,
    sqrt_after: f64,
    liquidity: Option<f64>,
}

impl RangeTrade {
    /// The pool's liquidity over the range the trade stayed in, and the virtual reserves of the
    /// asset that went in and the one that came out, before the trade.
    fn reserves_before(&self) -> Option<(f64, f64, f64)> {
        let s = self.sqrt_after;
        let (i, o) = (self.input, self.output);
        // Virtual reserves are A = L/s and B = L·s; the trade moved them along A·B = L².
        let l = self.liquidity.unwrap_or(if self.a_to_b { i * o / (o / s - i * s) } else { i * o / (o * s - i / s) });
        if !(l.is_finite() && l > 0.0) {
            return None;
        }
        let (a, b) = (l / s, l * s);
        let (in_after, out_after) = if self.a_to_b { (a, b) } else { (b, a) };
        Some((l, in_after - i, out_after + o))
    }
}

fn u64_at(d: &[u8], o: usize) -> Option<f64> {
    Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?) as f64)
}
fn u128_at(d: &[u8], o: usize) -> Option<f64> {
    Some(u128::from_le_bytes(d.get(o..o + 16)?.try_into().ok()?) as f64)
}
fn pubkey_at(d: &[u8], o: usize) -> Option<[u8; 32]> {
    d.get(o..o + 32)?.try_into().ok()
}
const Q64: f64 = 18_446_744_073_709_551_616.0;

/// Every concentrated-liquidity trade in a transaction, from the events the programs emit (layouts
/// from each program's IDL): Meteora DAMM v2 `EvtSwap2`, Orca Whirlpool `Traded`, Raydium CLMM
/// `SwapEvent`. Meteora DLMM states only its bins, which do not tell the trade without the front
/// leg, so it is not read.
fn range_trades(entry: &Value) -> Vec<RangeTrade> {
    let mut out = vec![];
    for d in anchor_events(entry, [189, 66, 51, 168, 38, 80, 117, 153]) {
        // pool, trade_direction, collect_fee_mode, has_referral, params (u64, u64, u8), then the result:
        // fee-included input, fee-excluded input, amount left, output, next sqrt price, four fees.
        let (Some(pool), Some(&direction)) = (pubkey_at(&d, 0), d.get(32)) else { continue };
        let (Some(included), Some(excluded), Some(output), Some(sqrt)) = (u64_at(&d, 52), u64_at(&d, 60), u64_at(&d, 76), u128_at(&d, 84)) else { continue };
        let fees = [100, 108, 116, 124].iter().filter_map(|&o| u64_at(&d, o)).sum::<f64>();
        // A fee not taken off the input was taken off the output.
        let output = if included == excluded { output + fees } else { output };
        out.push(RangeTrade { pool, a_to_b: direction == 0, input: excluded, output, sqrt_after: sqrt / Q64, liquidity: None });
    }
    for d in anchor_events(entry, [225, 202, 73, 175, 147, 43, 160, 150]) {
        // whirlpool, a_to_b, pre and post sqrt price, input, output, transfer fees, lp and protocol fee.
        let (Some(pool), Some(&a_to_b)) = (pubkey_at(&d, 0), d.get(32)) else { continue };
        let (Some(sqrt), Some(input), Some(output), Some(lp), Some(protocol)) = (u128_at(&d, 49), u64_at(&d, 65), u64_at(&d, 73), u64_at(&d, 97), u64_at(&d, 105)) else { continue };
        out.push(RangeTrade { pool, a_to_b: a_to_b == 1, input: input - lp - protocol, output, sqrt_after: sqrt / Q64, liquidity: None });
    }
    for d in anchor_events(entry, [64, 198, 205, 232, 38, 8, 113, 226]) {
        // pool, sender, two token accounts, amount_0, transfer_fee_0, amount_1, transfer_fee_1,
        // zero_for_one, sqrt price, liquidity, tick. The input after the fee follows from the rest.
        let (Some(pool), Some(&zero_for_one)) = (pubkey_at(&d, 0), d.get(160)) else { continue };
        let (Some(amount_0), Some(amount_1), Some(sqrt), Some(liquidity)) = (u64_at(&d, 128), u64_at(&d, 144), u128_at(&d, 161), u128_at(&d, 177)) else { continue };
        let a_to_b = zero_for_one == 1;
        let (s, output) = (sqrt / Q64, if a_to_b { amount_1 } else { amount_0 });
        let l = liquidity;
        let input = if a_to_b { l / s - l * l / (l * s + output) } else { l * s - l * l / (l / s + output) };
        out.push(RangeTrade { pool, a_to_b, input, output, sqrt_after: s, liquidity: Some(liquidity) });
    }
    out
}

/// The victim's trade repriced on a concentrated-liquidity pool: its virtual reserves before it, the
/// front leg's trade taken back out of them, the victim's input run against what is left.
fn reprice_range(front: &Value, victim: &Value) -> Option<(f64, f64)> {
    let fronts = range_trades(front);
    let v = range_trades(victim).into_iter().find(|v| fronts.iter().any(|f| f.pool == v.pool && f.a_to_b == v.a_to_b))?;
    let f = fronts.into_iter().find(|f| f.pool == v.pool && f.a_to_b == v.a_to_b)?;
    let (liquidity, paid_reserve, bought_reserve) = v.reserves_before()?;
    // Both trades must read the same liquidity: one range, the events read right.
    let (front_liquidity, _, _) = f.reserves_before()?;
    if (front_liquidity / liquidity - 1.0).abs() > 0.01 {
        return None;
    }
    let (paid, bought) = (paid_reserve - f.input, bought_reserve + f.output);
    (paid > 0.0).then(|| (v.output, bought * v.input / (paid + v.input)))
}

/// A sandwich repriced as it would land on Acrux.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repricing {
    /// "pump-curve", "concentrated" (from the pool program's own swap events) or "constant-product".
    pub pool: &'static str,
    /// What the victim got in the asset it bought, and what it would have got with no front leg.
    pub victim_got: String,
    pub victim_would_get: String,
    /// The difference, valued in the asset the victim paid with at the victim's own price.
    pub kept_paid: String,
    /// Sent plain (t = 1), a proposer front-runs only by outbidding the victim's priority fee per CU
    /// over the front leg's compute limit: that cost in lamports, and the trader's take after it.
    pub outbid_lamports: String,
    pub take_after_outbid: String,
}

/// The victim's output with the front leg taken out of the pool, from the pool's state recorded in
/// the block: a Pump.fun curve's virtual reserves from its trade events; a concentrated-liquidity
/// pool's liquidity and price from its swap events; else the pool vaults' balances on the
/// constant-product rule, where it reproduces every trade the block made there.
pub fn reprice(block: &Value, sandwich: &Sandwich, candidates: &[Candidate]) -> Option<Repricing> {
    let txs = block["transactions"].as_array()?;
    let (front, victim) = (&txs[sandwich.front.index], &txs[sandwich.victim.index]);
    let (pool, got, would_get) = if let (Some(f), Some(v)) = (pump_trade(front, &sandwich.bought), pump_trade(victim, &sandwich.bought)) {
        // The victim's reserves before its trade, then without the front leg's buy.
        let (sol, tokens) = (v.virtual_sol - v.sol - f.sol, v.virtual_tokens + v.tokens + f.tokens);
        ("pump-curve", v.tokens, tokens * v.sol / (sol + v.sol))
    } else if let Some((got, would_get)) = reprice_range(front, victim) {
        ("concentrated", got as i128, would_get as i128)
    } else {
        // The pool's vault of each asset among those all three trades moved, and every trade the
        // block made against that pair.
        let vault = |asset: &str| sandwich.vaults.iter().find(|a| token_account(victim, a).is_some_and(|(m, _, _)| m == asset));
        let (x_vault, y_vault) = (vault(&sandwich.bought)?, vault(&sandwich.paid)?);
        let swaps = txs
            .iter()
            .enumerate()
            .filter(|(_, t)| t["meta"]["err"].is_null())
            .filter_map(|(i, t)| {
                let (_, x0, x1) = token_account(t, x_vault)?;
                let (_, y0, y1) = token_account(t, y_vault)?;
                let swap = PoolSwap { x0: x0 as f64, y0: y0 as f64, dx: (x1 - x0) as f64, dy: (y1 - y0) as f64 };
                swap.trade().map(|_| (i, swap))
            })
            .collect::<Vec<_>>();
        let (kind, model) = fit_pool(&swaps.iter().map(|(_, s)| *s).collect::<Vec<_>>())?;
        let at = |index: usize| swaps.iter().find(|(i, _)| *i == index).map(|(_, s)| *s);
        let (v, f) = (at(sandwich.victim.index)?, at(sandwich.front.index)?);
        let (input, out, buys_x) = v.trade()?;
        if !buys_x {
            return None;
        }
        // The victim's trade against the pool as it stood without the front leg.
        let clean = PoolSwap { x0: v.x0 - f.dx, y0: v.y0 - f.dy, dx: 0.0, dy: 0.0 };
        (kind, out as i128, model.output(&clean, input, true) as i128)
    };
    let paid = -sandwich.victim.delta(&sandwich.victim_owner, &sandwich.paid);
    let kept = if got > 0 { (would_get - got) * paid / got } else { 0 };
    let by_id = |id: &str| candidates.iter().find(|c| c.id == id);
    let outbid = by_id(&sandwich.front.signature).zip(by_id(&sandwich.victim.signature)).map(|(f, v)| ((v.priority_fee_per_cu + 1) as i128 * f.compute_limit as i128) / 1_000_000).unwrap_or(0);
    Some(Repricing {
        pool,
        victim_got: got.to_string(),
        victim_would_get: would_get.to_string(),
        kept_paid: kept.to_string(),
        outbid_lamports: outbid.to_string(),
        take_after_outbid: (sandwich.take() - if sandwich.paid == "SOL" { outbid } else { 0 }).to_string(),
    })
}

fn amount(raw: i128, decimals: u8) -> String {
    let scale = 10i128.pow(decimals as u32);
    let fraction = format!("{:0width$}", raw.abs() % scale, width = decimals as usize);
    let fraction = fraction.trim_end_matches('0');
    format!("{}{}{}{}", if raw < 0 { "-" } else { "" }, group((raw.abs() / scale) as u64), if fraction.is_empty() { "" } else { "." }, fraction)
}

/// Reads the block, places it and prints both orders: the leader's, and Acrux's merged order
/// with each transaction's Acrux slot, track and start.
pub async fn run(file: Option<&str>, rpc: Option<&str>, slot: Option<u64>, save: Option<&str>, rows: usize, json: Option<&str>) -> Result<()> {
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
        match reprice(&block, s, &leader) {
            Some(r) => {
                let bought = s.victim.decimals.get(&s.bought).copied().unwrap_or(0);
                let n = |v: &str| v.parse::<i128>().unwrap_or(0);
                println!(
                    "    on Acrux, sent 2-of-3: no front leg; the victim gets {} instead of {} {}, keeping {} {} ({})",
                    amount(n(&r.victim_would_get), bought), amount(n(&r.victim_got), bought), name(&s.bought), amount(n(&r.kept_paid), decimals), name(&s.paid), r.pool
                );
                println!("    sent plain (t = 1): front-running costs {} lamports in priority over the victim's; the trader keeps {} {}", group(n(&r.outbid_lamports) as u64), amount(n(&r.take_after_outbid), decimals), name(&s.paid));
            }
            None => println!("    not priced: no event states the pool's state (a Meteora DLMM bin pool, say) and the constant-product rule does not reproduce its trades in this block"),
        }
    }
    if let Some(path) = json {
        let record = json!({
            "slot": number,
            "transactions": leader.len(),
            "votesLeftOut": votes,
            "cuRequested": requested,
            "acruxSlots": slots.iter().map(|s| json!({"transactions": s.selected_ids.len(), "cuPerTrack": (1..=TRACKS).map(|t| s.placements.iter().filter(|p| p.track == t).map(|p| p.length_cu).sum::<u64>()).collect::<Vec<_>>()})).collect::<Vec<_>>(),
            "leaderOrder": leader.iter().map(|c| json!({"signature": c.id, "priorityFeePerCu": c.priority_fee_per_cu, "computeLimit": c.compute_limit})).collect::<Vec<_>>(),
            "mergedOrder": merged.iter().map(|(c, slot, p)| json!({"signature": c.id, "priorityFeePerCu": c.priority_fee_per_cu, "acruxSlot": slot, "track": p.track, "startCu": p.start_cu, "lengthCu": p.length_cu})).collect::<Vec<_>>(),
            "sandwiches": found.iter().map(|s| json!({
                "victim": s.victim.signature, "victimPosition": s.victim.index + 1,
                "front": s.front.signature, "frontPosition": s.front.index + 1,
                "back": s.back.signature, "backPosition": s.back.index + 1,
                "trader": s.trader, "bought": s.bought, "paid": s.paid, "vaults": s.vaults,
                "take": s.take().to_string(), "repricing": reprice(&block, s, &leader),
            })).collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_vec_pretty(&record)?).with_context(|| format!("Write {path}"))?;
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
    fn a_constant_product_pool_fits_and_a_bent_one_does_not() {
        // Three trades on a 0.25% pool, each from the reserves the one before left.
        let (mut x, mut y) = (1_000_000_000.0f64, 50_000_000_000.0f64);
        let mut swaps = vec![];
        for (input, buys_x) in [(1_000_000_000.0, true), (5_000_000.0, false), (2_000_000_000.0, true)] {
            let e: f64 = input * 0.9975;
            let (dx, dy) = if buys_x { (-(x * e / (y + e)).floor(), input) } else { (input, -(y * e / (x + e)).floor()) };
            swaps.push(PoolSwap { x0: x, y0: y, dx, dy });
            (x, y) = (x + dx, y + dy);
        }
        let (kind, model) = fit_pool(&swaps).unwrap();
        assert_eq!(kind, "constant-product");
        assert!((model.fee - 0.0025).abs() < 1e-6);
        // A trade paid out 5% more than the rule allows: the pool is not priced.
        swaps[1].dy *= 1.05;
        assert!(fit_pool(&swaps).is_none());
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
