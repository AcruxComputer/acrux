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
    // Reuse the HTTP/TLS connection. Reconnecting for every page costs extra round trips on a
    // remote link even after the explorer views have been removed from the follower payload.
    let client = reqwest::Client::new();
    let genesis = crate::genesis::Genesis::load(std::path::Path::new(genesis))?;
    let programs = genesis.programs.iter().map(|(address, path)| crate::execution::GenesisProgram {
        address: address.clone(), path: path.clone(),
    }).collect::<Vec<_>>();
    let snapshot = follower_rpc(&client, url, "getExecutionSnapshot", json!({})).await?;
    let (mut replay, mut last, start_hash) = load_follow_snapshot(snapshot, &genesis, &programs)?;
    println!("from cblock {last}: {} accounts loaded, state {} matches", replay.executor.account_count(), &start_hash[..12]);
    let mut head = follower_rpc(&client, url, "getFinalizedCblock", json!({})).await?;
    let mut checked = 0u64;
    loop {
        let finalized = head.get("consensusSlot").context("finalized consensusSlot")?;
        let top = if finalized.is_null() { None } else { Some(finalized.as_u64().context("finalized consensusSlot")?) };
        let Some(top) = top.filter(|top| *top > last) else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            head = follower_rpc(&client, url, "getFinalizedCblock", json!({})).await?;
            continue;
        };
        let mut fetched = std::collections::BTreeMap::new();
        let mut chain = vec![];
        let mut at = top;
        let mut missing = None;
        // Walk the accepted chain backward in pages, rather than transferring every skipped
        // window between the snapshot and the tip. Each page includes the latest finalized head,
        // which avoids another head poll while catching up.
        loop {
            let first = at * 4;
            if !fetched.contains_key(&first) {
                let through = first + 4;
                let from = through.saturating_sub(256).max(last * 4 + 4);
                let count = through - from;
                let mut page = follower_rpc(&client, url, "getFollowerPage", json!({"from": from, "count": count})).await?;
                let slots = page["slots"].as_array_mut().context("follower slots")?;
                if slots.is_empty() { missing = Some(first); break }
                ensure!(slots.len() <= count as usize, "follower page exceeds its requested range");
                for (index, entry) in std::mem::take(slots).into_iter().enumerate() {
                    let slot = entry["slot"].as_u64().context("follower slot")?;
                    ensure!(slot == from + index as u64, "follower page is not contiguous");
                    fetched.insert(slot, entry);
                }
                let newest = page["finalized"]["consensusSlot"].as_u64().context("follower finalized consensusSlot")?;
                ensure!(newest >= top, "the finalized follower head moved backward");
                head = page["finalized"].clone();
            }
            let Some(entry) = fetched.get(&first).filter(|entry| !entry["view"].is_null()) else {
                missing = Some(first);
                break;
            };
            let cblock = &entry["view"]["cblock"];
            ensure!(cblock["consensusSlot"].as_u64() == Some(at), "slot {first} names another cblock");
            let parent = cblock["parent"]["consensusSlot"].as_u64().context("cblock without a parent")?;
            ensure!(parent < at && parent >= last, "cblock {top} does not extend cblock {last}");
            chain.push(at);
            if parent == last { break }
            at = parent;
        }
        // Null data in a skipped/noncanonical part of a page is harmless. Require blocks only
        // for the ancestors that the finalized tip actually commits.
        if missing.is_none() {
            missing = chain.iter().flat_map(|c| c * 4..c * 4 + 4).find(|slot| {
                fetched.get(slot).is_none_or(|entry| entry["view"].is_null() || entry["blocks"].is_null())
            });
        }
        if let Some(slot) = missing {
            let snapshot = follower_rpc(&client, url, "getExecutionSnapshot", json!({})).await?;
            let loaded = load_follow_snapshot(snapshot, &genesis, &programs)?;
            ensure!(loaded.1 >= last, "the fresh snapshot is behind the already checked chain");
            (replay, last) = (loaded.0, loaded.1);
            println!("re-anchored at cblock {last}: history unavailable at slot {slot}, {} accounts loaded, state {} matches", replay.executor.account_count(), &loaded.2[..12]);
            head = follower_rpc(&client, url, "getFinalizedCblock", json!({})).await?;
            continue;
        }
        for c in chain.into_iter().rev() {
            let lane_cblock = if replay.lanes_active(c * 4) {
                let raw = &fetched[&(c * 4 + 3)]["blocks"]["laneCblock"];
                Some(replay.check_lane_cblock(raw)?)
            } else { None };
            for s in c * 4..c * 4 + 4 {
                let entry = fetched.get(&s).with_context(|| format!("slot {s} was not fetched"))?;
                let view = &entry["view"];
                let blocks = &entry["blocks"];
                if let Some(proposers) = &replay.proposers {
                    let expected = proposers.window(s / 16)?;
                    let actual = view["window"]["proposers"].as_array().context("proposers")?.iter().map(|proposer| proposer["identity"].as_str().context("proposer identity")).collect::<Result<Vec<_>>>()?;
                    ensure!(expected.proposers.iter().map(String::as_str).eq(actual), "slot {s}: proposers differ from the finalized stake schedule");
                }
                if let Some(parsed) = &lane_cblock {
                    let accepted = &parsed.accepted[(s % 4) as usize];
                    ensure!(accepted.0 == s && serde_json::to_value(&accepted.1)? == view["agreedSet"], "slot {s}: lane accepted set differs from its cblock");
                    let pi: Vec<Option<[u8;32]>> = read_hash_vector(&blocks["pi"])?;
                    ensure!(pi == accepted.2, "slot {s}: lane commitments differ from its cblock");
                    if s%4==3 {ensure!(view["stateHash"].as_str()==Some(parsed.state.as_str()), "slot {s}: execution state differs from its certified preimage");}
                }
                check_slot(view, &blocks["attestations"], lane_cblock.as_ref().map(|_| &blocks["pi"])).with_context(|| format!("slot {s}"))?;
                let recipients = view["window"]["proposers"].as_array().context("proposers")?.iter()
                    .map(|p| Ok((p["index"].as_u64().context("proposer index")? as u8, p["identity"].as_str().context("proposer identity")?.to_owned())))
                    .collect::<Result<Vec<_>>>()?;
                let state = replay.apply(s, &recipients, blocks).with_context(|| format!("re-executing slot {s}"))?;
                ensure!(view["stateHash"].as_str() == Some(state.as_str()), "slot {s}: re-executed state {} differs from the net's {}", &state[..12], view["stateHash"]);
                if s % 4 == 3 {
                    replay.learn_finalized()?;
                }
                checked += 1;
                println!("slot {s} re-executed  agreed {}  state {} matches  checked {checked}", view["agreedSet"], &state[..12]);
            }
            last = c;
        }
    }
}

/// A follower runs for as long as its operator leaves it: a call that times out, loses its body or
/// is rate limited is sent again after a pause that grows to 5 s, with one line per outage. If the
/// outage outlasts the gateway's block window, the next page shows the gap and the follower
/// re-anchors. An answer the net gives as an error is returned as one.
async fn follower_rpc(client: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let mut pause = Duration::from_millis(250);
    let mut failing = false;
    loop {
        let attempt = async {
            let response = client.post(url).json(&request).timeout(Duration::from_secs(10)).send().await?;
            anyhow::Ok(response.json::<Value>().await?)
        };
        let failure = match attempt.await {
            Ok(mut body) => match body.get("error") {
                Some(error) if error["data"]["error"]["code"] != "RATE_LIMITED" => bail!("{method}: {}", error["message"].as_str().unwrap_or(&error.to_string())),
                Some(_) => anyhow::anyhow!("rate limited"),
                None => {
                    if failing { println!("{method}: reachable again"); }
                    return Ok(body.get_mut("result").context("follower RPC result")?.take());
                }
            },
            Err(error) => error,
        };
        if !failing { println!("{method}: {failure:#}; retrying"); failing = true; }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_secs(5));
    }
}

fn validate_proposer_config(config: &crate::protocol::ProposerConfig, genesis: &crate::genesis::Genesis) -> Result<()> {
    let seed = config.seed.parse::<u64>().context("Invalid public schedule seed")?;
    ensure!(seed.to_string() == config.seed, "The public schedule seed must be canonical decimal");
    ensure!(config.windows_per_epoch == crate::types::Configuration::default().windows_per_epoch as u64, "The public schedule window count differs from this product's configuration");
    ensure!(config.identities.len() == 12 && config.identities.iter().collect::<std::collections::BTreeSet<_>>().len() == 12, "Invalid public validator identities");
    for identity in &config.identities { Pubkey::from_str(identity).context("Invalid public validator identity")?; }
    ensure!(config.hubs.iter().all(|hub| hub.members.iter().all(|member| member.stake == 100_000_000)), "The public legacy schedule template has different genesis stakes");
    // The packaged genesis carries the local development registry, not the host's, so the snapshot's
    // identities and hubs are taken as served; the state hashes the follower checks cover them.
    let stake_rule = genesis.bridge.as_ref().map(|bridge| bridge.stake_activation()).transpose()?.flatten();
    ensure!(config.stake_from_slot == stake_rule.map(|rule| rule.0) && config.genesis_stake_lamports == stake_rule.map(|rule| rule.1), "Snapshot stake activation differs from the local rule");
    ensure!(config.epoch_slots == genesis.bridge.as_ref().map_or(432_000, |bridge| bridge.epoch_slots), "Snapshot epoch length differs from the local rule");
    ensure!(config.admission_from_slot == genesis.bridge.as_ref().and_then(|bridge|bridge.admission_from_slot), "Snapshot admission activation differs from the local rule");
    Ok(())
}

pub(crate) fn load_follow_snapshot(mut snapshot: Value, genesis: &crate::genesis::Genesis, programs: &[crate::execution::GenesisProgram]) -> Result<(Replay, u64, String)> {
    let last = snapshot["consensusSlot"].as_u64().context("snapshot consensusSlot")?;
    let lanes_from = genesis.bridge.as_ref().and_then(|bridge|bridge.lanes_from_slot);
    match snapshot.get("lanesFromSlot") {
        Some(value) => ensure!((value.is_null() && lanes_from.is_none()) || value.as_u64().is_some_and(|from|Some(from)==lanes_from), "Snapshot lanes activation differs from the local rule"),
        None => {
            let last_slot=last.checked_mul(4).and_then(|slot|slot.checked_add(3)).context("Snapshot slot overflow")?;
            ensure!(lanes_from.is_none_or(|from|last_slot<from), "Snapshot is missing its active lanes rule");
        }
    }
    ensure!(snapshot["anchor"]["consensusSlot"].as_u64() == Some(last), "snapshot anchor names another cblock");
    let state: crate::execution::ExecutorSnapshot = serde_json::from_value(snapshot["executor"].take())?;
    let start_hash = state.state_hash.clone();
    ensure!(snapshot["anchor"]["stateHash"].as_str() == Some(start_hash.as_str()), "the starting state does not hash to cblock {last}'s state hash");
    let config = snapshot.get("proposerConfig").filter(|value| !value.is_null()).cloned().map(serde_json::from_value::<crate::protocol::ProposerConfig>).transpose()?;
    let stake_rule = genesis.bridge.as_ref().map(|bridge| bridge.stake_activation()).transpose()?.flatten();
    ensure!(stake_rule.is_none() || config.is_some(), "The snapshot is missing its public proposer configuration");
    if let Some(config) = &config { validate_proposer_config(config, genesis)?; }
    let mut executor = crate::execution::SvmExecutor::new(Vec::<(String, u64)>::new(), programs)?;
    executor.set_placements(genesis.placements.clone());
    if let (Some(bridge), Some(config)) = (&genesis.bridge, &config) {
        let identities = config.identities.iter().map(|identity| {
            let hub = config.hubs.iter().find(|hub| hub.members.iter().any(|member| &member.identity == identity)).context("Validator has no configured hub")?;
            Ok((Pubkey::from_str(identity)?, hub.code.clone()))
        }).collect::<Result<Vec<_>>>()?;
        if let Some(admission) = bridge.admission_config(&identities)? { executor.set_admission_config(admission); }
    }
    if let Some(bridge) = &genesis.bridge {
        executor.set_bridge(bridge.program);
        executor.set_bridge_minimums(bridge.from_slot, bridge.min_withdrawal.iter().map(|(mint, amount)| crate::execution::MinimumWithdrawal { mint: *mint, amount: *amount }).collect());
        if let Some(native) = bridge.native_cutover()? { executor.set_native_cutover(native); } if let Some(slot) = bridge.native_mint_from_slot { executor.set_native_mint_from(slot); }
        if let Some(fee_burn) = bridge.fee_burn_activation()? { executor.set_fee_burn_config(fee_burn); }
        if let Some((from_slot, genesis_stake_lamports)) = stake_rule {
            let identities = config.as_ref().context("Public proposer configuration is missing")?.identities.iter().map(|identity| identity.parse()).collect::<std::result::Result<Vec<_>, _>>()?;
            executor.set_stake_config(crate::execution::StakeConfig { from_slot, epoch_slots: bridge.epoch_slots, genesis_stake_lamports, identities });
        }
    }
    executor.load(state)?;
    ensure!(executor.state_hash() == start_hash, "the loaded state does not hash to cblock {last}'s state hash");
    let mut proposers = config.map(crate::protocol::ProposerSchedules::new).transpose()?;
    if let Some(proposers) = &mut proposers {
        for (epoch, weights) in executor.stake_schedule_snapshots(&executor.checkpoint())? { proposers.learn(epoch, weights)?; }
    }
    let mut replay = Replay { executor, threshold: serde_json::from_value(snapshot["threshold"].take())?, window: None, proposers, bridge:genesis.bridge.clone() };
    replay.learn_finalized()?;
    Ok((replay, last, start_hash))
}

pub(crate) struct Replay {
    pub(crate) executor: crate::execution::SvmExecutor,
    threshold: crate::threshold::ThresholdCache,
    window: Option<u64>,
    pub(crate) proposers: Option<crate::protocol::ProposerSchedules>,
    bridge: Option<crate::genesis::BridgeConfig>,
}

#[derive(Clone)]
pub(crate) struct ReplayCheckpoint {
    executor: crate::execution::ExecutorCheckpoint,
    threshold: crate::threshold::ThresholdCache,
    window: Option<u64>,
    pub(crate) proposers: Option<crate::protocol::ProposerSchedules>,
}
impl Replay {
    #[cfg(test)]
    pub(crate) fn for_test(proposers: crate::protocol::ProposerSchedules) -> Result<Self> {
        Ok(Self {
            executor: crate::execution::SvmExecutor::new(Vec::<(String,u64)>::new(), &[])?,
            threshold: crate::threshold::ThresholdCache::new(2400)?,
            window: None,
            proposers: Some(proposers), bridge:None,
        })
    }
    pub(crate) fn lanes_active(&self, slot: u64) -> bool {
        self.bridge.as_ref().and_then(|bridge|bridge.lanes_from_slot).is_some_and(|from|slot >= from)
    }
    pub(crate) fn check_lane_cblock(&self, raw: &Value) -> Result<crate::cblock_format::Parsed> {
        let preimage = B64.decode(raw["preimage"].as_str().context("Missing lane cblock preimage")?)?;
        let parsed = crate::cblock_format::decode(&preimage,true)?;
        ensure!(self.lanes_active(parsed.cblock * 4), "Lane proof before activation");
        let hash = read_hash(raw["hash"].as_str().context("Missing lane cblock hash")?)?;
        let root = raw["outboxRoot"].as_str().map(read_hash).transpose()?;
        ensure!(root.is_some() == self.bridge.as_ref().is_some_and(|bridge|parsed.cblock*4 >= bridge.from_slot), "Wrong lane bridge commitment domain");
        let actual = root.map_or_else(||crypto::hash(&preimage), |root|crypto::hash(&[preimage.as_slice(),&root].concat()));
        ensure!(actual == hash, "Lane cblock preimage hash mismatch");
        let (members,stakes) = self.proposers.as_ref().context("Missing finalized proposer configuration")?.voting_set(parsed.cblock)?;
        let verifier = crate::consensus::Votor::new(0,stakes)?;
        let check = |value:&Value, lane:Option<usize>| -> Result<crate::consensus::Certificate> {
            let certificate: crate::consensus::Certificate = serde_json::from_value(value["certificate"].clone())?;
            let proofs: Vec<crate::types::SignedVote> = serde_json::from_value(value["proofs"].clone())?;
            ensure!(certificate.slot == parsed.cblock && proofs.len() == certificate.votes.len(), "Lane certificate names another cblock or omits signatures");
            verifier.validate_certificate(&certificate)?;
            for (vote,proof) in certificate.votes.iter().zip(proofs) {
                ensure!(vote == &proof.vote, "Certificate signature belongs to another vote");
                let identity = members.get(vote.validator).context("Unknown epoch vote signer")?;
                let key = ed25519_dalek::VerifyingKey::from_bytes(&bs58::decode(identity).into_vec()?.try_into().map_err(|_|anyhow::anyhow!("Invalid vote signer"))?)?;
                let message = match lane {Some(lane)=>crate::types::lane_vote_message(lane,vote,self.bridge.as_ref())?,None=>crate::types::vote_message(vote,self.bridge.as_ref())?};
                crypto::verify_bytes(&key,&message,&proof.signature)?;
            }
            Ok(certificate)
        };
        let execution = check(&raw["execution"],None)?;
        ensure!(matches!(execution.kind,crate::consensus::CertificateKind::FastFinalization|crate::consensus::CertificateKind::Finalization) && execution.hash == Some(hash), "Missing execution finality over the lane cblock");
        let hashes = parsed.lane_hashes.as_ref().context("Lane hashes missing from cblock")?;
        let lanes = raw["lanes"].as_array().context("Lane certificate proofs")?;
        ensure!(lanes.len() == 3, "A cblock must decide all three lanes");
        for (index,value) in lanes.iter().enumerate() {
            let lane = index+1;
            ensure!(value["lane"].as_u64() == Some(lane as u64), "Lane proofs must be in proposer order");
            let certificate = check(value,Some(lane))?;
            ensure!(certificate.hash == hashes[index], "Lane certificate hash differs from the execution commitment");
            if let Some(hash) = hashes[index] {
                ensure!(certificate.kind != crate::consensus::CertificateKind::Skip, "Accepted lane has a skip certificate");
                let block: crate::consensus::Block = serde_json::from_value(value["block"].clone())?;
                ensure!(if parsed.cblock%4==0 {block.parent==parsed.parent}else{block.parent.is_some_and(|parent|parent.slot.checked_add(1)==Some(parsed.cblock))}, "Lane block parent differs from its window chain");
                let commitments = value["commitments"].as_array().context("Lane slot commitments")?.iter().map(|hash|read_hash(hash.as_str().context("Lane commitment hash")?)).collect::<Result<Vec<_>>>()?;
                let commitments: [crate::consensus::Hash;4] = commitments.try_into().map_err(|_|anyhow::anyhow!("A lane needs four slot commitments"))?;
                ensure!(block.id.slot == parsed.cblock && block.id.hash == hash && crate::cblock_format::lane_hash(lane,parsed.cblock,block.parent,&commitments)? == hash, "Lane block preimage mismatch");
                for (offset,commitment) in commitments.iter().enumerate() {ensure!(parsed.accepted[offset].2[index] == Some(*commitment), "Lane slot commitment differs from accepted preimage");}
            } else {
                ensure!(certificate.kind == crate::consensus::CertificateKind::Skip, "Omitted lane lacks its skip certificate");
            }
        }
        Ok(parsed)
    }
    pub(crate) fn checkpoint(&self) -> ReplayCheckpoint {
        ReplayCheckpoint {executor:self.executor.checkpoint(),threshold:self.threshold.clone(),window:self.window,proposers:self.proposers.clone()}
    }
    pub(crate) fn restore(&mut self, checkpoint: ReplayCheckpoint) -> Result<()> {
        self.executor.restore(checkpoint.executor)?;
        self.threshold = checkpoint.threshold;
        self.window = checkpoint.window;
        self.proposers = checkpoint.proposers;
        Ok(())
    }
    pub(crate) fn learn_finalized(&mut self) -> Result<()> {
        if let Some(proposers) = &mut self.proposers {
            let checkpoint = self.executor.checkpoint();
            for (epoch, weights) in self.executor.stake_schedule_snapshots(&checkpoint)? { proposers.learn(epoch, weights)?; }
            let catalog = self.executor.validator_catalog(&checkpoint)?;
            for snapshot in self.executor.admission_snapshots(&checkpoint)? {
                proposers.learn_hubs(snapshot.epoch,self.executor.admission_hubs(&checkpoint,snapshot.epoch)?)?;
                proposers.learn_admission(snapshot, catalog.clone())?;
            }
        }
        Ok(())
    }
    /// One slot, the way Algorithm 2's TryExecute runs it on a validator.
    pub(crate) fn apply(&mut self, s: u64, recipients: &[(u8, String)], blocks: &Value) -> Result<String> {
        use crate::execution::ExecutionItem;
        use crate::threshold::AcceptOutcome;
        let w = s / 16;
        if self.window != Some(w) {
            self.executor.set_proposers(recipients)?;
            self.window = Some(w);
        }
        let attestations = blocks["attestations"].as_array().context("attestations")?;
        let canonical = if self.lanes_active(s) { &blocks["pi"] } else { &attestations.first().context("no attestation")?["pi"] };
        let pi = canonical.as_array().context("pi")?.iter()
            .map(|h| h.as_str().map(|h| Ok::<[u8; 32], anyhow::Error>(hex::decode(h)?.try_into().map_err(|_| anyhow::anyhow!("hash"))?)).transpose())
            .collect::<Result<Vec<_>>>()?;
        if self.lanes_active(s) {
            let roles=blocks["blocks"].as_array().context("Lane blocks")?.iter().map(|block|block["proposer"].as_u64().context("Lane block proposer")).collect::<Result<Vec<_>>>()?;
            let expected=pi.iter().enumerate().filter_map(|(index,hash)|hash.map(|_|index as u64+1)).collect::<Vec<_>>();
            ensure!(roles==expected,"Lane block data differs from the certified accepted set");
        }
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
        self.executor.activate_native_cutover(s)?;
        self.executor.prepare_stake_epoch(s)?;
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

pub(crate) fn check_slot(slot: &Value, signed_attestations: &Value, canonical_pi: Option<&Value>) -> Result<()> {
    check_slot_agreement(slot, signed_attestations,canonical_pi)?;
    if slot["slot"].as_u64().unwrap() % 4 == 3 {
        let certificates = slot["cblock"]["certificates"].as_array().context("certificates")?;
        ensure!(certificates.iter().any(|c| matches!(c["kind"].as_str(), Some("fast_finalization" | "slow_finalization")) && c["stakeShare"].as_f64() >= c["threshold"].as_f64()), "no finalization certificate");
    }
    Ok(())
}
pub(crate) fn check_slot_agreement(slot: &Value, signed_attestations: &Value, canonical_pi: Option<&Value>) -> Result<()> {
    let proposers = slot["window"]["proposers"].as_array().context("proposers")?;
    let mut attestations = vec![];
    for a in signed_attestations.as_array().context("attestations")? {
        let index = a["proposer"].as_u64().context("proposer")? as usize;
        let identity = proposers.iter().find(|p| p["index"] == index).context("attestation by a non-proposer")?["identity"].as_str().unwrap();
        let attestation = Attestation {
            slot: slot["slot"].as_u64().unwrap(),
            proposer: index,
            pi: read_hash_vector(&a["pi"])?,
            key: B64.decode(a["key"].as_str().context("key")?)?.try_into().map_err(|_| anyhow::anyhow!("key length"))?,
        };
        let key = ed25519_dalek::VerifyingKey::from_bytes(&bs58::decode(identity).into_vec()?.try_into().map_err(|_| anyhow::anyhow!("identity"))?)?;
        let signature = bs58::decode(a["signature"].as_str().context("signature")?).into_vec()?;
        crypto::verify_bytes(&key, &bincode::serialize(&crate::types::Wire::Attest(attestation.clone()))?, &signature).context("attestation signature")?;
        attestations.push(attestation);
    }
    let agreed = if let Some(pi) = canonical_pi {
        let pi = read_hash_vector(pi)?;
        let accepted = pi.iter().enumerate().filter_map(|(index,hash)|hash.map(|_|index+1)).collect::<Vec<_>>();
        ensure!(!accepted.is_empty(), "Execution cblock has no accepted lanes");
        for role in &accepted {
            let owner = attestations.iter().find(|attestation|attestation.proposer == *role).context("Accepted lane owner has no signed key release")?;
            ensure!(owner.pi[*role-1] == pi[*role-1], "Accepted lane differs from its owner's signed commitment");
        }
        accepted
    } else {agreement::agreed(&attestations, false).context("the attestations do not agree")?};
    let claimed: Vec<usize> = serde_json::from_value(slot["agreedSet"].clone())?;
    ensure!(agreed == claimed, "agreed set {claimed:?} differs from the attestations' {agreed:?}");
    Ok(())
}

fn read_hash(text: &str) -> Result<[u8;32]> { hex::decode(text)?.try_into().map_err(|_|anyhow::anyhow!("Invalid hash length")) }
fn read_hash_vector(value:&Value)->Result<Vec<Option<[u8;32]>>> {
    let values = value.as_array().context("Commitment vector")?;
    ensure!(values.len() == 3, "Expected three commitment columns");
    values.iter().map(|hash|if hash.is_null(){Ok(None)}else{Ok(Some(read_hash(hash.as_str().context("Commitment hash")?)?))}).collect()
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
    let genesis = crate::genesis::Genesis::load(std::path::Path::new(genesis))?;
    let programs = genesis.programs.iter().map(|(address, path)| crate::execution::GenesisProgram { address: address.clone(), path: path.clone() }).collect::<Vec<_>>();
    let executables = executables(url, &state, &programs, cblock).await?;
    ensure!(net_hash == state.state_hash, "the snapshot's state hash {} is not the net's {net_hash} for cblock {cblock}", state.state_hash);
    println!("cblock {cblock} (slot {}): {} accounts and {} programs, state {} matches the net's", cblock * 4 + 3, state.accounts.len(), executables.len(), &net_hash[..12]);
    let empty = state.accounts.iter().filter(|(_, a)| a.lamports == 0).count();
    state.accounts.extend(executables);
    let mut executor = SvmExecutor::from_state(&programs, state)?;
    executor.set_placements(genesis.placements.clone());
    if let Some(bridge) = &genesis.bridge {
        executor.set_bridge(bridge.program);
        executor.set_bridge_minimums(bridge.from_slot, bridge.min_withdrawal.iter().map(|(mint, amount)| crate::execution::MinimumWithdrawal { mint: *mint, amount: *amount }).collect());
        if let Some(native) = bridge.native_cutover()? { executor.set_native_cutover(native); } if let Some(slot) = bridge.native_mint_from_slot { executor.set_native_mint_from(slot); }
        if let Some(fee_burn) = bridge.fee_burn_activation()? { executor.set_fee_burn_config(fee_burn); }
        if let Some((from_slot, genesis_stake_lamports)) = bridge.stake_activation()? {
            let config: crate::protocol::ProposerConfig = serde_json::from_value(snapshot.get("proposerConfig").context("Public proposer configuration is missing")?.clone())?;
            validate_proposer_config(&config, &genesis)?;
            let identities = config.identities.iter().map(|identity| identity.parse()).collect::<std::result::Result<Vec<_>, _>>()?;
            executor.set_stake_config(crate::execution::StakeConfig { from_slot, epoch_slots: bridge.epoch_slots, genesis_stake_lamports, identities });
        }
    }
    let senders = (0..24).map(|_| Keypair::new()).collect::<Vec<_>>();
    let native = executor.native_supply()?.is_some();
    if !native {
        for sender in &senders { executor.fund(&sender.pubkey().to_string(), 100_000_000_000)?; }
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
    println!("{empty} accounts without lamports dropped; traffic funding {}; saved to {} at state {}", if native { "retired" } else { "prepared" }, path.display(), &executor.state_hash()[..12]);
    println!("`acrux net up --store {store}` continues the chain from cblock {}", cblock + 1);
    Ok(())
}
