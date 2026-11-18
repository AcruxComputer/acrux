//! The `acrux` commands that talk to a running net or to the Hub Registry: keygen, send, node
//! follow, and the hub instructions.
use crate::{agreement, crypto, threshold::seal_for_receivers, types::Attestation};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, write_keypair_file, Keypair, Signer},
    system_instruction, system_program,
    transaction::Transaction,
};
use std::{str::FromStr, time::Duration};

pub async fn rpc(url: &str, method: &str, params: Value) -> Result<Value> {
    let body: Value = reqwest::Client::new()
        .post(url)
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .with_context(|| format!("{url} is not reachable"))?
        .json()
        .await?;
    if let Some(error) = body.get("error") {
        bail!("{method}: {}", error["message"].as_str().unwrap_or(&error.to_string()));
    }
    Ok(body["result"].clone())
}

pub fn keygen(out: &str) -> Result<()> {
    let keypair = Keypair::new();
    write_keypair_file(&keypair, out).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{}", keypair.pubkey());
    Ok(())
}

pub fn identities(out: &str) -> Result<()> {
    std::fs::create_dir_all(out)?;
    for i in 0..12 {
        let secret = crate::identity_key(i);
        let keypair = Keypair::from_bytes(&secret.to_keypair_bytes()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let path = format!("{out}/validator-{i}.json");
        write_keypair_file(&keypair, &path).map_err(|e| anyhow::anyhow!("{e}"))?;
        println!("{} {} {path}", ["FRA", "NYC", "TYO"][i / 4], keypair.pubkey());
    }
    Ok(())
}

fn keypair(path: &str) -> Result<Keypair> {
    read_keypair_file(path).map_err(|e| anyhow::anyhow!("Read keypair {path}: {e}"))
}

/// A transfer on Acrux Net, plain (t = 1) or split into n fragments of which t rebuild it,
/// followed until it is finalized.
pub async fn send(url: &str, keypair_path: &str, to: &str, lamports: u64, n: u8, t: u8, priority: u64) -> Result<()> {
    let payer = keypair(keypair_path)?;
    let latest = rpc(url, "getLatestBlockhash", json!({})).await?;
    let blockhash: Hash = latest["blockhash"].as_str().context("blockhash")?.parse()?;
    let target = latest["startedSlot"].as_u64().context("startedSlot")? + 4;
    let tx = Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(20_000),
            ComputeBudgetInstruction::set_compute_unit_price(priority),
            system_instruction::transfer(&payer.pubkey(), &Pubkey::from_str(to)?, lamports),
        ],
        Some(&payer.pubkey()),
        &[&payer],
        blockhash,
    );
    let bytes = bincode::serialize(&tx)?;
    let header = json!({
        "id": hex::encode(Sha256::digest(tx.message.serialize())),
        "feePayer": payer.pubkey().to_string(),
        "priorityFeePerCu": priority.to_string(),
        "computeLimit": 20_000,
        "writableAccounts": tx.message.account_keys.iter().enumerate().filter(|(i, _)| tx.message.is_maybe_writable(*i, None)).map(|(_, k)| k.to_string()).collect::<Vec<_>>(),
        "targetSlot": target,
    });
    let receivers: Vec<u8> = (1..=n).collect();
    let id = header["id"].as_str().unwrap().to_owned();
    if t <= 1 {
        rpc(url, "submitTransaction", json!({"header": header, "transaction": B64.encode(&bytes), "receivers": receivers})).await?;
    } else {
        let signer = SigningKey::from_bytes(&payer.to_bytes()[..32].try_into()?);
        // All at once, as the SDK sends them: one after another, a far client's fragments land in
        // slots further apart than the fragment expiry and never reach t together.
        let fragments = seal_for_receivers(&bytes, serde_json::from_value(header)?, &signer, n, t, &receivers)?
            .into_iter()
            .map(|fragment| async move { rpc(url, "submitFragment", serde_json::to_value(fragment)?).await })
            .collect::<Vec<_>>();
        futures_util::future::try_join_all(fragments).await?;
    }
    println!("sent {id} for slot {target}{}", if t > 1 { format!(", {t} of {n} fragments") } else { String::new() });
    let mut last = String::new();
    loop {
        let state = rpc(url, "getTransaction", json!({"id": id})).await.ok().and_then(|tx| tx["state"].as_str().map(str::to_owned)).unwrap_or_else(|| "unknown".into());
        if state != last {
            println!("  {state}");
            last = state.clone();
        }
        if matches!(state.as_str(), "finalized" | "failed" | "expired") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Follows a net the way a validator would, from its latest finalized cblock: loads that cblock's
/// executed state and checks its lattice hash against the net's, then for every later finalized
/// cblock receives each slot's encrypted blocks, checks every attestation's signature and Agreed(s),
/// decrypts the agreed blocks with the released keys, checks each against its proposer's attested
/// commitment, merges and re-executes them on its own SVM (Algorithm 4, the fees, the threshold
/// fragments), and compares the state hash with the net's and with its finalization certificate.
pub async fn follow(url: &str, genesis: &str) -> Result<()> {
    let snapshot = rpc(url, "getExecutionSnapshot", json!({})).await?;
    let genesis = crate::genesis::Genesis::load(std::path::Path::new(genesis))?;
    let programs = genesis
        .programs
        .iter()
        .map(|(address, path)| crate::execution::GenesisProgram { address: address.clone(), path: path.clone() })
        .collect::<Vec<_>>();
    let mut executor = crate::execution::SvmExecutor::new(Vec::<(String, u64)>::new(), &programs)?;
    executor.set_placements(genesis.placements.clone());
    if let Some(bridge) = &genesis.bridge {
        executor.set_bridge(bridge.program);
        executor.set_bridge_minimums(bridge.from_slot, bridge.min_withdrawal.iter().map(|(mint, amount)| crate::execution::MinimumWithdrawal { mint: *mint, amount: *amount }).collect());
    }
    let state: crate::execution::ExecutorSnapshot = serde_json::from_value(snapshot["executor"].clone())?;
    let start_hash = state.state_hash.clone();
    executor.load(state)?;
    let mut last = snapshot["consensusSlot"].as_u64().context("consensusSlot")?;
    let net_hash = rpc(url, "getSlot", json!({"slot": last * 4 + 3})).await?["stateHash"].as_str().map(str::to_owned);
    ensure!(executor.state_hash() == start_hash && net_hash.as_deref() == Some(start_hash.as_str()), "the starting state does not hash to cblock {last}'s state hash");
    println!("from cblock {last}: {} accounts loaded, state {} matches", executor.account_count(), &start_hash[..12]);
    let mut replay = Replay { executor, threshold: serde_json::from_value(snapshot["threshold"].clone())?, window: None };
    let mut checked = 0u64;
    loop {
        let slots = rpc(url, "getRecentSlots", json!({"limit": 64})).await?;
        let top = slots.as_array().context("slots")?.iter()
            .filter(|s| s["slot"].as_u64().is_some_and(|n| n % 4 == 3) && s["cblock"]["status"] == "finalized")
            .filter_map(|s| s["cblock"]["consensusSlot"].as_u64())
            .filter(|c| *c > last)
            .max();
        if let Some(top) = top {
            // Every slot since the last cblock run, its view and blocks, in pages of 64: a follower
            // far from the gateway pays a round trip per page, not two per slot.
            let mut fetched = std::collections::BTreeMap::new();
            let mut from = last * 4 + 4;
            while from < top * 4 + 4 {
                let count = (top * 4 + 4 - from).min(64);
                let page = rpc(url, "getSlotRange", json!({"from": from, "count": count})).await?;
                let page = page.as_array().context("slot range")?;
                ensure!(!page.is_empty(), "slot {from} is not held by the gateway");
                for entry in page {
                    fetched.insert(entry["slot"].as_u64().context("slot")?, entry.clone());
                }
                from += page.len() as u64;
            }
            // A finalized cblock finalizes its ancestors: execute the chain back to the last one run.
            let mut chain = vec![top];
            loop {
                let first = chain.last().unwrap() * 4;
                let cblock = &fetched.get(&first).with_context(|| format!("slot {first} was not fetched"))?["view"]["cblock"];
                let parent = cblock["parent"]["consensusSlot"].as_u64().context("cblock without a parent")?;
                if parent == last {
                    break;
                }
                ensure!(parent > last, "cblock {top} does not extend cblock {last}");
                chain.push(parent);
            }
            for c in chain.into_iter().rev() {
                for s in c * 4..c * 4 + 4 {
                    let entry = fetched.get(&s).with_context(|| format!("slot {s} was not fetched"))?;
                    let view = &entry["view"];
                    check_slot(view).with_context(|| format!("slot {s}"))?;
                    let blocks = &entry["blocks"];
                    ensure!(!blocks.is_null(), "slot {s}: the gateway no longer holds its blocks");
                    let recipients = view["window"]["proposers"].as_array().context("proposers")?.iter()
                        .map(|p| (p["index"].as_u64().unwrap() as u8, p["identity"].as_str().unwrap().to_owned())).collect::<Vec<_>>();
                    let state = replay.apply(s, &recipients, blocks).with_context(|| format!("re-executing slot {s}"))?;
                    ensure!(view["stateHash"].as_str() == Some(state.as_str()), "slot {s}: re-executed state {} differs from the net's {}", &state[..12], view["stateHash"]);
                    checked += 1;
                    println!("slot {s} re-executed  agreed {}  state {} matches  checked {checked}", view["agreedSet"], &state[..12]);
                }
                last = c;
            }
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

struct Replay {
    executor: crate::execution::SvmExecutor,
    threshold: crate::threshold::ThresholdCache,
    window: Option<u64>,
}

impl Replay {
    /// One slot, the way Algorithm 2's TryExecute runs it on a validator.
    fn apply(&mut self, s: u64, recipients: &[(u8, String)], blocks: &Value) -> Result<String> {
        use crate::execution::ExecutionItem;
        use crate::threshold::AcceptOutcome;
        let w = s / 16;
        if self.window != Some(w) {
            self.executor.set_proposers(recipients)?;
            self.window = Some(w);
        }
        let attestations = blocks["attestations"].as_array().context("attestations")?;
        let pi = attestations.first().context("no attestation")?["pi"].as_array().context("pi")?.iter()
            .map(|h| h.as_str().map(|h| Ok::<[u8; 32], anyhow::Error>(hex::decode(h)?.try_into().map_err(|_| anyhow::anyhow!("hash"))?)).transpose())
            .collect::<Result<Vec<_>>>()?;
        let mut payloads = vec![];
        let mut fragments = vec![];
        let mut credits: Vec<crate::execution::BridgeCredit> = vec![];
        for block in blocks["blocks"].as_array().context("blocks")? {
            let q = block["proposer"].as_u64().context("proposer")? as usize;
            let key: [u8; 32] = B64.decode(attestations.iter().find(|a| a["proposer"] == q).context("no attestation from an agreed proposer")?["key"].as_str().context("key")?)?
                .try_into().map_err(|_| anyhow::anyhow!("key length"))?;
            let mut slices = block["slices"].as_array().context("slices")?.iter()
                .map(|x| Ok((serde_json::from_value::<crate::crypto::ShredMeta>(x["meta"].clone())?, B64.decode(x["cipher"].as_str().context("cipher")?)?, hex::decode(x["root"].as_str().context("root")?)?)))
                .collect::<Result<Vec<_>>>()?;
            slices.sort_by_key(|(meta, _, _)| meta.slice);
            let mut plaintext = vec![];
            let mut roots = vec![];
            for (meta, cipher, root) in &slices {
                ensure!(crypto::hash(&key) == meta.key_hash, "released key does not match the block's key commitment");
                plaintext.extend(crypto::decrypt_slice(&key, s, q, meta.slice, cipher)?);
                roots.push(<[u8; 32]>::try_from(root.as_slice())?);
            }
            let last = slices.last().context("block without slices")?;
            ensure!(last.0.final_slice, "block without its final slice");
            let parent = last.0.parent.map(|(slot, hash)| crate::consensus::BlockRef { slot, hash });
            let commitment = crypto::hash(&bincode::serialize(&crate::types::BlockCommitment { slot: s, proposer: q, key_hash: last.0.key_hash, parent, roots })?);
            ensure!(pi.get(q - 1).copied().flatten() == Some(commitment), "block {q} does not match its attested commitment");
            let payload = crate::types::BlockPayload::decode(&plaintext)?;
            // Bridge credits ride in certified blocks; the follower applies them as the net did.
            credits.extend(payload.credits);
            let mut txs = vec![];
            for t in payload.transactions {
                let valid = self.executor.check_transaction(&t.bytes, t.header.target_slot)?;
                ensure!(valid.header == t.header && valid.header.target_slot <= s, "signed header or target mismatch");
                txs.push(valid);
            }
            for f in payload.fragments {
                let f = f.revalidate()?;
                ensure!(f.receiver as usize == q, "fragment carried by the wrong proposer");
                fragments.push((q as u8, f));
            }
            payloads.push((q as u8, txs));
        }
        let mut inclusions = payloads.iter().flat_map(|(q, txs)| txs.iter().map(move |t| (t.header.fee_payer.clone(), *q, 1u64))).collect::<Vec<_>>();
        inclusions.extend(fragments.iter().map(|(q, f)| (f.header.fee_payer.clone(), *q, 1)));
        let mut admitted = self.executor.admit_inclusions(&inclusions, w)?.into_iter();
        let mut items = vec![];
        for (q, txs) in payloads {
            for tx in txs {
                if !admitted.next().unwrap() {
                    continue;
                }
                self.executor.charge_inclusion(&tx.header.fee_payer, q, w, 1)?;
                items.push(ExecutionItem { transaction: tx, source_proposers: vec![q] });
            }
        }
        self.threshold.expire(s);
        for (q, fragment) in fragments {
            if !admitted.next().unwrap() {
                continue;
            }
            self.executor.charge_inclusion(&fragment.header.fee_payer, q, w, 1)?;
            let id = fragment.header.id.clone();
            match self.threshold.accept_at(s, 0, q, fragment) {
                Err(_) => self.threshold.reject(&id),
                Ok(AcceptOutcome::Decoded { header, bytes, inclusions }) => match crate::threshold::validate_decoded(&self.executor, &header, &bytes) {
                    Ok(tx) => {
                        let mut sources = inclusions.iter().map(|i| i.proposer).collect::<Vec<_>>();
                        sources.sort_unstable();
                        sources.dedup();
                        items.push(ExecutionItem { transaction: tx, source_proposers: sources });
                    }
                    Err(_) => self.threshold.reject(&header.id),
                },
                Ok(_) => {}
            }
        }
        credits.sort_by_key(|c| c.sequence);
        credits.dedup_by_key(|c| c.sequence);
        self.executor.set_credits(credits);
        Ok(self.executor.execute_slot(s, items)?.state_hash)
    }
}

fn check_slot(slot: &Value) -> Result<()> {
    let proposers = slot["window"]["proposers"].as_array().context("proposers")?;
    let mut attestations = vec![];
    for a in slot["attestations"].as_array().context("attestations")? {
        let index = a["proposer"].as_u64().context("proposer")? as usize;
        let identity = proposers.iter().find(|p| p["index"] == index).context("attestation by a non-proposer")?["identity"].as_str().unwrap();
        let attestation = Attestation {
            slot: slot["slot"].as_u64().unwrap(),
            proposer: index,
            pi: a["pi"].as_array().context("pi")?.iter().map(|h| h.as_str().map(|h| hex::decode(h).map(|b| b.try_into().unwrap())).transpose()).collect::<Result<_, _>>()?,
            key: B64.decode(a["key"].as_str().context("key")?)?.try_into().map_err(|_| anyhow::anyhow!("key length"))?,
        };
        let key = ed25519_dalek::VerifyingKey::from_bytes(&bs58::decode(identity).into_vec()?.try_into().map_err(|_| anyhow::anyhow!("identity"))?)?;
        let signature = bs58::decode(a["signature"].as_str().context("signature")?).into_vec()?;
        crypto::verify_bytes(&key, &bincode::serialize(&crate::types::Wire::Attest(attestation.clone()))?, &signature).context("attestation signature")?;
        attestations.push(attestation);
    }
    let agreed = agreement::agreed(&attestations, false).context("the attestations do not agree")?;
    let claimed: Vec<usize> = serde_json::from_value(slot["agreedSet"].clone())?;
    ensure!(agreed == claimed, "agreed set {claimed:?} differs from the attestations' {agreed:?}");
    if slot["slot"].as_u64().unwrap() % 4 == 3 {
        let certificates = slot["cblock"]["certificates"].as_array().context("certificates")?;
        ensure!(
            certificates.iter().any(|c| matches!(c["kind"].as_str(), Some("fast_finalization" | "slow_finalization")) && c["stakeShare"].as_f64() >= c["threshold"].as_f64()),
            "no finalization certificate"
        );
    }
    Ok(())
}

/// Anchor's instruction discriminator.
fn discriminator(name: &str) -> Vec<u8> {
    Sha256::digest(format!("global:{name}"))[..8].to_vec()
}

fn pda(program: &Pubkey, seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, program).0
}

/// WGS84 geodetic to ECEF, whole metres at sea level.
fn ecef(latitude: f64, longitude: f64) -> [i32; 3] {
    let (a, e2) = (6_378_137.0f64, 6.69437999014e-3f64);
    let (phi, lambda) = (latitude.to_radians(), longitude.to_radians());
    let n = a / (1.0 - e2 * phi.sin().powi(2)).sqrt();
    [(n * phi.cos() * lambda.cos()).round() as i32, (n * phi.cos() * lambda.sin()).round() as i32, (n * (1.0 - e2) * phi.sin()).round() as i32]
}

pub enum HubAction {
    Register { code: String, latitude: f64, longitude: f64 },
    Join { code: String },
    Leave { code: String },
    Move { from: String, to: String },
    Release { code: String },
}

/// One Hub Registry instruction, signed with the identity keypair (and paid by it).
pub async fn hub(rpc_url: &str, program: &str, keypair_path: &str, action: HubAction) -> Result<()> {
    let program = Pubkey::from_str(program)?;
    let signer = keypair(keypair_path)?;
    let me = signer.pubkey();
    let registry = pda(&program, &[b"registry"]);
    let events = pda(&program, &[b"__event_authority"]);
    let hub_of = |code: &str| pda(&program, &[b"hub", code.as_bytes()]);
    let membership = pda(&program, &[b"member", me.as_ref()]);
    let tail = [AccountMeta::new_readonly(events, false), AccountMeta::new_readonly(program, false)];
    let (name, data, accounts) = match &action {
        HubAction::Register { code, latitude, longitude } => {
            let mut data = (code.len() as u32).to_le_bytes().to_vec();
            data.extend_from_slice(code.as_bytes());
            for v in ecef(*latitude, *longitude) {
                data.extend_from_slice(&v.to_le_bytes());
            }
            ("register_hub", data, vec![AccountMeta::new(me, true), AccountMeta::new(registry, false), AccountMeta::new(hub_of(code), false), AccountMeta::new(pda(&program, &[b"fee_vault"]), false), AccountMeta::new_readonly(system_program::id(), false)])
        }
        HubAction::Join { code } => ("join_hub", vec![], vec![AccountMeta::new(me, true), AccountMeta::new_readonly(me, true), AccountMeta::new_readonly(registry, false), AccountMeta::new(hub_of(code), false), AccountMeta::new(membership, false), AccountMeta::new_readonly(system_program::id(), false)]),
        HubAction::Leave { code } => ("leave_hub", vec![], vec![AccountMeta::new_readonly(me, true), AccountMeta::new_readonly(registry, false), AccountMeta::new(hub_of(code), false), AccountMeta::new(membership, false)]),
        HubAction::Move { from, to } => ("move_hub", vec![], vec![AccountMeta::new_readonly(me, true), AccountMeta::new_readonly(registry, false), AccountMeta::new(hub_of(from), false), AccountMeta::new(hub_of(to), false), AccountMeta::new(membership, false)]),
        HubAction::Release { code } => {
            let hub = rpc(rpc_url, "getAccountInfo", json!([hub_of(code).to_string(), {"encoding": "base64", "commitment": "confirmed"}])).await?;
            let data = B64.decode(hub["value"]["data"][0].as_str().context("This hub does not exist")?)?;
            // Hub: discriminator 8, id 8, code 8, location 12, then the registrant.
            let registrant = Pubkey::try_from(&data[36..68]).map_err(|_| anyhow::anyhow!("hub layout"))?;
            ("release_hub", vec![], vec![AccountMeta::new_readonly(registry, false), AccountMeta::new(hub_of(code), false), AccountMeta::new(registrant, false)])
        }
    };
    let instruction = Instruction { program_id: program, accounts: accounts.into_iter().chain(tail).collect(), data: [discriminator(name), data].concat() };
    let blockhash: Hash = rpc(rpc_url, "getLatestBlockhash", json!([{"commitment": "confirmed"}])).await?["value"]["blockhash"].as_str().context("blockhash")?.parse()?;
    let tx = Transaction::new_signed_with_payer(&[instruction], Some(&me), &[&signer], blockhash);
    let signature = rpc(rpc_url, "sendTransaction", json!([B64.encode(bincode::serialize(&tx)?), {"encoding": "base64", "preflightCommitment": "confirmed"}])).await?;
    let signature = signature.as_str().context("signature")?.to_owned();
    for _ in 0..60 {
        let status = rpc(rpc_url, "getSignatureStatuses", json!([[signature]])).await?;
        let entry = &status["value"][0];
        if !entry.is_null() && matches!(entry["confirmationStatus"].as_str(), Some("confirmed" | "finalized")) {
            ensure!(entry["err"].is_null(), "{name} failed: {}", entry["err"]);
            println!("{name} confirmed: {signature}");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("{name} was not confirmed within 30 s: {signature}")
}

/// The cut-over from a net that keeps no state of its own (0.3.7 and earlier): takes its latest
/// finalized cblock's state over RPC, every account executables included, checks it against the
/// net's state hash for that cblock, and writes it as the saved state `acrux net up` continues
/// from. The new build's rules apply from the next cblock on: an account left with no lamports no
/// longer exists. The built-in traffic's keys were the old run's, so new ones are funded as at genesis.
/// The executables at a net's latest finalized state that `getExecutionSnapshot` leaves out (all
/// of them before 0.4.1, the genesis loaders' since): every loader's, from `getProgramAccounts`. The RPC reports no rent epoch, so each takes this
/// build's own copy's where the program is the same, else 0 or u64::MAX, whichever makes the
/// snapshot's state hash.
async fn executables(
    url: &str,
    state: &crate::execution::ExecutorSnapshot,
    programs: &[crate::execution::GenesisProgram],
    cblock: u64,
) -> Result<Vec<(String, solana_sdk::account::Account)>> {
    use crate::execution::{lattice_hash, SvmExecutor};
    use solana_sdk::account::Account;
    let local = SvmExecutor::new(Vec::<(String, u64)>::new(), programs)?;
    let mut executables = vec![];
    let mut unknown = vec![];
    for loader in ["NativeLoader1111111111111111111111111111111", "BPFLoader1111111111111111111111111111111111", "BPFLoader2111111111111111111111111111111111", "BPFLoaderUpgradeab1e11111111111111111111111"] {
        let listed = rpc(url, "getProgramAccounts", json!([loader, {"encoding": "base64"}])).await?;
        for entry in listed.as_array().context("program accounts")? {
            let account = &entry["account"];
            if account["executable"] != true {
                continue;
            }
            let key = entry["pubkey"].as_str().context("pubkey")?.to_owned();
            if state.accounts.iter().any(|(k, _)| *k == key) {
                continue;
            }
            let mut fetched = Account {
                lamports: account["lamports"].as_u64().context("lamports")?,
                data: B64.decode(account["data"][0].as_str().context("data")?)?,
                owner: Pubkey::from_str(account["owner"].as_str().context("owner")?)?,
                executable: true,
                rent_epoch: 0,
            };
            match local.account(&key)? {
                Some(own) if own.lamports == fetched.lamports && own.owner == fetched.owner && own.data == fetched.data => fetched.rent_epoch = own.rent_epoch,
                _ => unknown.push(executables.len()),
            }
            executables.push((key, fetched));
        }
    }
    ensure!(unknown.len() <= 16, "{} programs of unknown rent epoch", unknown.len());
    let matched = (0u32..1 << unknown.len()).find_map(|mask| {
        let mut candidate = executables.clone();
        for (bit, i) in unknown.iter().enumerate() {
            candidate[*i].1.rent_epoch = if mask >> bit & 1 == 1 { u64::MAX } else { 0 };
        }
        let all = state.accounts.iter().chain(candidate.iter());
        let hash = lattice_hash(all.map(|(k, a)| (Pubkey::from_str(k).unwrap(), a)));
        (hash == state.state_hash).then_some(candidate)
    });
    matched.with_context(|| format!("the accounts taken do not hash to cblock {cblock}'s state hash {}", state.state_hash))
}

pub async fn import(url: &str, store: &str, genesis: &str) -> Result<()> {
    use crate::execution::{ExecutorSnapshot, SvmExecutor};
    let path = std::path::Path::new(store).join("state.redb");
    ensure!(!path.exists(), "{} already holds a saved state", path.display());
    let snapshot = rpc(url, "getExecutionSnapshot", json!({})).await?;
    let cblock = snapshot["consensusSlot"].as_u64().context("consensusSlot")?;
    let mut state: ExecutorSnapshot = serde_json::from_value(snapshot["executor"].clone())?;
    let threshold: crate::threshold::ThresholdCache = serde_json::from_value(snapshot["threshold"].clone())?;
    let view = rpc(url, "getSlot", json!({"slot": cblock * 4 + 3})).await?;
    let net_hash = view["stateHash"].as_str().context("the net holds no state hash for that cblock")?.to_owned();
    let block_hash = view["cblock"]["hash"].as_str().context("the net holds no hash for that cblock")?.to_owned();
    let genesis_hash = rpc(url, "getGenesisHash", json!([])).await?.as_str().context("genesis hash")?.to_owned();
    let counters = rpc(url, "getCounters", json!({})).await?;
    let programs = crate::genesis::Genesis::load(std::path::Path::new(genesis))?
        .programs
        .into_iter()
        .map(|(address, path)| crate::execution::GenesisProgram { address, path })
        .collect::<Vec<_>>();
    let executables = executables(url, &state, &programs, cblock).await?;
    ensure!(net_hash == state.state_hash, "the snapshot's state hash {} is not the net's {net_hash} for cblock {cblock}", state.state_hash);
    println!("cblock {cblock} (slot {}): {} accounts and {} programs, state {} matches the net's", cblock * 4 + 3, state.accounts.len(), executables.len(), &net_hash[..12]);
    let empty = state.accounts.iter().filter(|(_, a)| a.lamports == 0).count();
    state.accounts.extend(executables);
    let mut executor = SvmExecutor::from_state(&programs, state)?;
    let senders = (0..24).map(|_| Keypair::new()).collect::<Vec<_>>();
    for sender in &senders {
        executor.fund(&sender.pubkey().to_string(), 100_000_000_000)?;
    }
    let checkpoint = executor.checkpoint();
    let meta = crate::state::Meta {
        format: crate::state::FORMAT,
        cblock,
        hash: block_hash,
        executor: executor.snapshot_meta(&checkpoint),
        threshold,
        senders: senders.iter().map(|k| k.to_base58_string()).collect(),
        finalized_cblocks: counters["finalizedCblocks"].as_u64().unwrap_or(0),
        skips: counters["skips"].as_u64().unwrap_or(0),
        genesis_hash,
        saved_at_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64,
    };
    crate::state::StateStore::open(&path)?.write(&checkpoint, None, &meta)?;
    println!("{empty} accounts without lamports dropped; the traffic's 24 keys funded; saved to {} at state {}", path.display(), &executor.state_hash()[..12]);
    println!("`acrux net up --store {store}` continues the chain from cblock {}", cblock + 1);
    Ok(())
}
