//! A hub observer: bootstrap once, then authenticate and replay the actual QUIC wire.
use crate::{cli, consensus::{BlockRef, Certificate, CertificateKind, Votor}, crypto,
    types::{Attestation, CblockWitness, Commitment, Envelope, SignedVote, Wire}};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use bincode::Options;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use solana_sdk::signature::read_keypair_file;
use std::{collections::{BTreeMap, BTreeSet}, time::Duration};

const MAX_WIRE_BYTES: u64 = 1024 * 1024;
// Proposer payloads use 38,000-byte chunks and early blocks append one empty final slice.
const MAX_BLOCK_SLICES: u64 = MAX_WIRE_BYTES.div_ceil(38_000) + 1;
const MAX_CIPHER_BYTES: u64 = MAX_WIRE_BYTES + 16 * MAX_BLOCK_SLICES;
// Match the follower's two-minute data window. A stalled observer must bootstrap again.
const MAX_PENDING_SLOTS: usize = 2400;
const MAX_SEEN_ENVELOPES: usize = 100_000;
const MAX_PENDING_MATERIAL_BYTES: usize = 64 * 1024 * 1024;

type Accepted = (u64, Option<Vec<usize>>, Vec<Option<[u8; 32]>>);
type Preimage = (u64, Option<BlockRef>, Vec<Accepted>, String);

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(bincode::DefaultOptions::new().with_fixint_encoding().with_limit(MAX_WIRE_BYTES)
        .reject_trailing_bytes().deserialize(bytes)?)
}
fn hash32(text: &str) -> Result<[u8; 32]> {
    hex::decode(text)?.try_into().map_err(|_| anyhow::anyhow!("Expected a 32-byte hash"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubPeers { hub: String, address: String, certificate: String, peers: Vec<HubPeer> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HubPeer { id: usize, identity: String }

struct Slice {
    meta: crypto::ShredMeta,
    root: [u8; 32],
    shreds: BTreeMap<u8, crypto::Shred>,
    cipher: Option<Vec<u8>>,
    buffered_bytes: usize,
}
#[derive(Default)]
struct Slot {
    buffered_bytes: usize,
    slices: BTreeMap<(usize, u32), Slice>,
    attestations: BTreeMap<usize, (Attestation, Vec<u8>)>,
    commitments: BTreeMap<usize, Commitment>,
    equivocation: bool,
}
struct Witness { wire: CblockWitness, accepted: Vec<Accepted> }
#[derive(Clone)]
struct Finalized { block: BlockRef, kind: &'static str, share: f64, threshold: f64 }
struct Observer {
    replay: cli::Replay,
    anchor: BlockRef,
    keys: Vec<VerifyingKey>,
    stakes: Vec<u64>,
    votor: Votor,
    bridge: Option<crate::genesis::BridgeConfig>,
    slots: BTreeMap<u64, Slot>,
    witnesses: BTreeMap<BlockRef, Witness>,
    finalized: BTreeMap<u64, Finalized>,
    seen: BTreeMap<[u8; 32], u64>,
    checked: u64,
    buffered_bytes: usize,
}
impl Observer {
    fn proposer(&self, slot: u64, role: usize) -> Result<usize> {
        ensure!((1..=3).contains(&role), "Invalid proposer role");
        let ids = self.replay.proposers.as_ref().context("Missing proposer configuration")?.ids(slot / 16)?;
        ids.get(role - 1).copied().context("Missing drawn proposer")
    }
    fn verify_vote(&self, signed: &SignedVote) -> Result<()> {
        let key = self.keys.get(signed.vote.validator).context("Vote by an unknown validator")?;
        crypto::verify_bytes(key, &crate::types::vote_message(&signed.vote, self.bridge.as_ref())?, &signed.signature)
            .context("Invalid consensus vote signature")
    }
    fn shred(&mut self, shred: crypto::Shred) -> Result<()> {
        let slot = shred.meta.slot;
        if slot / 4 <= self.anchor.slot { return Ok(()) }
        ensure!((shred.meta.slice as u64) < MAX_BLOCK_SLICES, "Too many block slices");
        let id = self.proposer(slot, shred.meta.proposer)?;
        let state = self.slots.entry(slot).or_default();
        let key = (shred.meta.proposer, shred.meta.slice);
        if let Some(known) = state.slices.get(&key) {
            if known.meta != shred.meta || known.root != shred.root {
                // Only authenticated conflicting shreds are equivocation evidence.
                crypto::verify_shred(&shred, &self.keys[id])?;
                state.equivocation = true;
                return Ok(())
            }
            // Repeated relay copies and unused parity do not enter reconstruction twice.
            if known.cipher.is_some() || known.shreds.contains_key(&shred.index) { return Ok(()) }
        }
        let bytes = usize::try_from(bincode::serialized_size(&shred)?)?;
        ensure!(self.buffered_bytes.checked_add(bytes).is_some_and(|bytes| bytes <= MAX_PENDING_MATERIAL_BYTES),
            "Observer pending material exceeded 64 MiB; restart from a fresh snapshot");
        if !state.slices.contains_key(&key) {
            let encrypted_bytes = state.slices.iter().filter(|((proposer,_),_)| *proposer == shred.meta.proposer)
                .map(|(_,pile)| pile.meta.encrypted_len as u64).sum::<u64>();
            ensure!(encrypted_bytes + shred.meta.encrypted_len as u64 <= MAX_CIPHER_BYTES, "Block exceeds the payload bound");
        }
        crypto::verify_shred(&shred, &self.keys[id]).context("Invalid Rotor shred")?;
        let pile = state.slices.entry(key).or_insert_with(|| Slice {
            meta: shred.meta.clone(), root: shred.root, shreds: BTreeMap::new(), cipher: None, buffered_bytes: 0,
        });
        pile.shreds.insert(shred.index, shred);
        pile.buffered_bytes += bytes;
        state.buffered_bytes += bytes;
        self.buffered_bytes += bytes;
        if pile.shreds.len() == crypto::DATA_SHREDS {
            let cipher = crypto::reconstruct_verified_slice(&pile.shreds.values().cloned().collect::<Vec<_>>())?;
            state.buffered_bytes = state.buffered_bytes - pile.buffered_bytes + cipher.len();
            self.buffered_bytes = self.buffered_bytes - pile.buffered_bytes + cipher.len();
            pile.buffered_bytes = cipher.len();
            pile.cipher = Some(cipher);
            pile.shreds.clear();
        }
        Ok(())
    }
    fn attest(&mut self, attestation: Attestation, signature: Vec<u8>, from: Option<usize>) -> Result<()> {
        if attestation.slot / 4 <= self.anchor.slot { return Ok(()) }
        let id = self.proposer(attestation.slot, attestation.proposer)?;
        ensure!(from.is_none_or(|from| from == id), "Attestation sent by another proposer");
        ensure!(attestation.pi.len() == 3, "Invalid attestation vector");
        if self.slots.get(&attestation.slot).and_then(|slot| slot.attestations.get(&attestation.proposer))
            .is_some_and(|(known, _)| known.pi == attestation.pi && known.key == attestation.key) { return Ok(()) }
        crypto::verify_bytes(&self.keys[id], &bincode::serialize(&Wire::Attest(attestation.clone()))?, &signature)
            .context("Invalid attestation signature")?;
        let state = self.slots.entry(attestation.slot).or_default();
        if let Some((known, _)) = state.attestations.get(&attestation.proposer) {
            if known.pi != attestation.pi || known.key != attestation.key { state.equivocation = true; }
        } else { state.attestations.insert(attestation.proposer, (attestation, signature)); }
        Ok(())
    }
    fn commitment(&mut self, commitment: Commitment, signature: Option<&[u8]>, from: Option<usize>) -> Result<()> {
        if commitment.slot / 4 <= self.anchor.slot { return Ok(()) }
        let id = self.proposer(commitment.slot, commitment.proposer)?;
        ensure!(from.is_none_or(|from| from == id), "Commitment sent by another proposer");
        if let Some(signature) = signature {
            crypto::verify_bytes(&self.keys[id], &bincode::serialize(&Wire::Commit(commitment.clone()))?, signature)?;
        }
        let state = self.slots.entry(commitment.slot).or_default();
        if let Some(known) = state.commitments.get(&commitment.proposer) {
            if known.hash != commitment.hash || known.key_hash != commitment.key_hash { state.equivocation = true; }
        } else { state.commitments.insert(commitment.proposer, commitment); }
        Ok(())
    }
    fn certificate(&mut self, certificate: Certificate, proofs: Vec<SignedVote>) -> Result<()> {
        self.votor.validate_certificate(&certificate)?;
        ensure!(proofs.len() == certificate.votes.len(), "Certificate is missing its signed votes");
        for (vote, proof) in certificate.votes.iter().zip(&proofs) {
            ensure!(vote == &proof.vote, "Certificate proof names another vote");
            self.verify_vote(proof)?;
        }
        let (kind, threshold) = match certificate.kind {
            CertificateKind::FastFinalization => ("fast_finalization", 0.8),
            CertificateKind::Finalization => ("slow_finalization", 0.6),
            _ => return Ok(()),
        };
        let block = BlockRef { slot: certificate.slot, hash: certificate.hash.context("Finalization without a hash")? };
        if block.slot <= self.anchor.slot {
            if block.slot == self.anchor.slot { ensure!(block == self.anchor, "Conflicting finalized anchor"); }
            return Ok(())
        }
        let stake = certificate.votes.iter().map(|vote| self.stakes[vote.validator] as u128).sum::<u128>();
        let total = self.stakes.iter().map(|stake| *stake as u128).sum::<u128>();
        if let Some(known) = self.finalized.get(&block.slot) { ensure!(known.block == block, "Conflicting finalized cblocks"); }
        self.finalized.insert(block.slot, Finalized { block, kind, share: stake as f64 / total as f64, threshold });
        Ok(())
    }
    fn witness(&mut self, wire: CblockWitness, from: usize) -> Result<()> {
        ensure!(from == 0, "Cblock witness is not from the execution representative");
        if wire.block.slot <= self.anchor.slot { return Ok(()) }
        let (slot, parent, accepted, state): Preimage = decode(&wire.preimage)?;
        ensure!(slot == wire.block.slot && parent == wire.parent, "Cblock witness names another preimage");
        ensure!(parent.is_some_and(|parent| parent.slot < slot), "Cblock parent does not precede its child");
        ensure!(slot <= (u64::MAX - 3) / 4 && accepted.len() == 4 && wire.state_hashes.len() == 4, "Invalid cblock slot range");
        for (offset, (execution_slot, agreed, pi)) in accepted.iter().enumerate() {
            ensure!(*execution_slot == slot * 4 + offset as u64 && pi.len() == 3, "Invalid accepted-slot preimage");
            let roles = agreed.as_ref().context("Cblock contains a slot without agreement")?;
            ensure!(roles.len() >= 2 && roles.len() <= 3 && roles.windows(2).all(|pair| pair[0] < pair[1])
                && roles.iter().all(|role| (1..=3).contains(role) && pi[*role - 1].is_some()), "Invalid agreed proposer set");
            hash32(&wire.state_hashes[offset])?;
        }
        ensure!(wire.state_hashes.last() == Some(&state), "Cblock's end state differs from its preimage");
        let bridge_active = self.bridge.as_ref().is_some_and(|bridge| slot * 4 >= bridge.from_slot);
        ensure!(wire.outbox_root.is_some() == bridge_active, "Cblock has the wrong bridge commitment domain");
        let hash = if let Some(root) = wire.outbox_root { crypto::hash(&[wire.preimage.as_slice(), &root].concat()) }
            else { crypto::hash(&wire.preimage) };
        ensure!(hash == wire.block.hash, "Cblock preimage does not hash to its signed block");
        if let Some(known) = self.witnesses.get(&wire.block) {
            ensure!(known.wire.preimage == wire.preimage && known.wire.state_hashes == wire.state_hashes, "Conflicting cblock witnesses");
        } else { self.witnesses.insert(wire.block, Witness { wire, accepted }); }
        Ok(())
    }
    fn packet(&mut self, packet: crate::external::Packet, recipients: &BTreeSet<usize>) -> Result<()> {
        ensure!(recipients.contains(&packet.to), "Mirrored packet is addressed to another hub");
        let digest = crypto::hash(&packet.payload);
        if self.seen.contains_key(&digest) { return Ok(()) }
        let envelope: Envelope = decode(&packet.payload)?;
        ensure!(envelope.from == packet.from, "Mirrored envelope has another sender");
        let key = self.keys.get(envelope.from).context("Envelope from an unknown validator")?;
        crypto::verify_bytes(key, &bincode::serialize(&envelope.body)?, &envelope.signature).context("Invalid wire envelope signature")?;
        self.seen.insert(digest, packet.slot);
        match envelope.body {
            Wire::Shreds(shreds) | Wire::Relay(shreds) => for shred in shreds { self.shred(shred)?; },
            Wire::Commit(commitment) => self.commitment(commitment, None, Some(envelope.from))?,
            Wire::Attest(attestation) => self.attest(attestation, envelope.signature, Some(envelope.from))?,
            Wire::Vote(vote) => self.verify_vote(&vote)?,
            Wire::Certificate { certificate, proofs } => self.certificate(certificate, proofs)?,
            Wire::Handover { shreds, attestations, .. } => {
                for shred in shreds { self.shred(shred)?; }
                for (attestation, signature) in attestations { self.attest(attestation, signature, None)?; }
            }
            Wire::Equivocation { first, second } => {
                ensure!(first.0.slot == second.0.slot && first.0.proposer == second.0.proposer && first.0.hash != second.0.hash, "Invalid equivocation proof");
                self.commitment(first.0, Some(&first.1), None)?;
                self.commitment(second.0, Some(&second.1), None)?;
            }
            Wire::Cblock(witness) => self.witness(witness, envelope.from)?,
            // Observing an application's submission is not its admission or execution.
            Wire::Submit(_) | Wire::SubmitFragment(_) => {},
        }
        ensure!(self.slots.len() <= MAX_PENDING_SLOTS && self.witnesses.len() <= MAX_PENDING_SLOTS / 4
            && self.seen.len() <= MAX_SEEN_ENVELOPES, "Observer data exceeded its window; restart from a fresh snapshot");
        self.replay_ready()
    }
    fn slot_blocks(&self, accepted: &Accepted) -> Result<Option<Value>> {
        let (slot, agreed, pi) = accepted;
        let Some(state) = self.slots.get(slot) else { return Ok(None) };
        ensure!(!state.equivocation, "Finalized slot {slot} contains proposer equivocation");
        let mut attestations = vec![];
        let mut blocks = vec![];
        for role in agreed.as_ref().context("No agreed set")? {
            let Some((attestation, signature)) = state.attestations.get(role) else { return Ok(None) };
            ensure!(&attestation.pi == pi, "Slot {slot}'s attestation differs from the cblock preimage");
            if let Some(commitment) = state.commitments.get(role) {
                ensure!(Some(commitment.hash) == pi[*role - 1] && commitment.key_hash == crypto::hash(&attestation.key), "Slot {slot}'s signed commitment differs from its attestation");
            }
            let piles = state.slices.iter().filter(|((proposer, _), _)| proposer == role).map(|(_, pile)| pile).collect::<Vec<_>>();
            let Some(last) = piles.last() else { return Ok(None) };
            if !last.meta.final_slice || piles.iter().any(|pile| pile.cipher.is_none()) { return Ok(None) }
            ensure!(piles.iter().enumerate().all(|(i, pile)| pile.meta.slice == i as u32 && (pile.meta.final_slice == (i + 1 == piles.len()))), "Slot {slot} has a noncontiguous slice list");
            ensure!(piles.iter().map(|pile| pile.meta.encrypted_len as u64).sum::<u64>() <= MAX_CIPHER_BYTES, "Block exceeds the payload bound");
            attestations.push(json!({"proposer":role,"pi":pi.iter().map(|hash|hash.map(hex::encode)).collect::<Vec<_>>(),"key":B64.encode(attestation.key),"signature":bs58::encode(signature).into_string()}));
            blocks.push(json!({"proposer":role,"slices":piles.iter().map(|pile|json!({"meta":pile.meta,"root":hex::encode(pile.root),"cipher":B64.encode(pile.cipher.as_ref().unwrap())})).collect::<Vec<_>>()}));
        }
        Ok(Some(json!({"attestations":attestations,"blocks":blocks})))
    }
    fn outbox(&self, cblock: u64) -> Result<()> {
        let Some(root) = self.witnesses[&self.anchor].wire.outbox_root else { return Ok(()) };
        let bridge = self.bridge.as_ref().context("Bridge cblock without a bridge rule")?;
        let mut entries = vec![crate::bridge::OutboxEntry::ValidatorSet {
            epoch: cblock * 4 / bridge.epoch_slots + 1,
            validators: self.keys.iter().zip(&self.stakes).map(|(key,stake)|(key.to_bytes(),*stake)).collect(),
        }];
        entries.extend(self.replay.executor.withdrawals(cblock).into_iter().map(|(sequence,solana_mint,recipient,amount)| crate::bridge::OutboxEntry::Withdrawal {sequence,solana_mint,recipient,amount}));
        if let Some(burn) = self.replay.executor.fee_burn(cblock)? { entries.push(crate::bridge::OutboxEntry::FeeBurn {epoch:burn.epoch,amount:burn.amount}); }
        ensure!(crate::bridge::outbox_root(&entries) == root, "Cblock {cblock}'s re-executed outbox differs from its commitment");
        Ok(())
    }
    fn replay_ready(&mut self) -> Result<()> {
        loop {
            let mut ready = None;
            // Any finalized descendant authenticates the hash-linked ancestors down to the anchor.
            for finality in self.finalized.values() {
                let mut chain = vec![];
                let mut at = finality.block;
                let mut missing = false;
                while at != self.anchor {
                    ensure!(at.slot > self.anchor.slot, "Finalized cblock does not extend the execution anchor");
                    let Some(witness) = self.witnesses.get(&at) else { missing = true; break };
                    chain.push(at);
                    at = witness.wire.parent.context("Finalized chain lost its parent")?;
                }
                if missing { continue }
                let first = *chain.last().context("Empty finalized extension")?;
                let mut blocks = vec![];
                for accepted in &self.witnesses[&first].accepted {
                    let Some(block) = self.slot_blocks(accepted)? else { missing = true; break };
                    blocks.push(block);
                }
                if !missing { ready = Some((first, finality.clone(), blocks)); break }
            }
            let Some((block, finality, blocks)) = ready else { return Ok(()) };
            let witness = &self.witnesses[&block];
            for (offset, (accepted, blocks)) in witness.accepted.iter().zip(blocks).enumerate() {
                let slot = accepted.0;
                let proposers = self.replay.proposers.as_ref().context("Missing proposer configuration")?.window(slot / 16)?.proposers;
                let recipients = proposers.iter().enumerate().map(|(index,identity)|((index+1) as u8,identity.clone())).collect::<Vec<_>>();
                let view = json!({"slot":slot,"stateHash":witness.wire.state_hashes[offset],"agreedSet":accepted.1,
                    "window":{"proposers":proposers.iter().enumerate().map(|(index,identity)|json!({"index":index+1,"identity":identity})).collect::<Vec<_>>()},
                    "cblock":{"certificates":[{"kind":finality.kind,"stakeShare":finality.share,"threshold":finality.threshold,"consensusSlot":finality.block.slot}]}});
                cli::check_slot(&view, &blocks["attestations"]).with_context(||format!("slot {slot}"))?;
                let state = self.replay.apply(slot, &recipients, &blocks).with_context(||format!("re-executing slot {slot}"))?;
                ensure!(state == witness.wire.state_hashes[offset], "Slot {slot}: re-executed state differs from the certified cblock");
                self.checked += 1;
                println!("slot {slot} re-executed  agreed {}  state {} matches  checked {}",view["agreedSet"],&state[..12],self.checked);
            }
            self.anchor = block;
            self.outbox(block.slot)?;
            if let Some(proposers) = &mut self.replay.proposers {
                for (epoch,weights) in self.replay.executor.stake_schedule_snapshots(&self.replay.executor.checkpoint())? { proposers.learn(epoch,weights)?; }
            }
            self.buffered_bytes -= self.slots.iter().filter(|(slot,_)| **slot / 4 <= block.slot)
                .map(|(_,slot)|slot.buffered_bytes).sum::<usize>();
            self.slots.retain(|slot,_| *slot / 4 > block.slot);
            self.witnesses.retain(|known,_| known.slot > block.slot);
            self.finalized.retain(|slot,_| *slot > block.slot);
            self.seen.retain(|_,slot| *slot / 4 > block.slot);
        }
    }
}

pub async fn run(url: &str, hub: &str, identity: &str, genesis_path: &str) -> Result<()> {
    let genesis = crate::genesis::Genesis::load(std::path::Path::new(genesis_path))?;
    let programs = genesis.programs.iter().map(|(address,path)|crate::execution::GenesisProgram {address:address.clone(),path:path.clone()}).collect::<Vec<_>>();
    let snapshot = cli::rpc(url,"getExecutionSnapshot",json!({})).await?;
    let config: crate::protocol::ProposerConfig = serde_json::from_value(snapshot["proposerConfig"].clone()).context("Missing public proposer configuration")?;
    let stakes: Vec<u64> = serde_json::from_value(snapshot["consensusStakes"].clone()).context("Missing consensus stakes")?;
    ensure!(stakes.len() == config.identities.len() && !stakes.is_empty() && stakes.iter().all(|stake|*stake>0), "Invalid consensus stake set");
    stakes.iter().try_fold(0u128,|total,stake|total.checked_add(*stake as u128)).context("Consensus stake overflow")?;
    let keys = config.identities.iter().map(|identity| {
        let bytes: [u8;32] = bs58::decode(identity).into_vec()?.try_into().map_err(|_|anyhow::anyhow!("Invalid validator identity"))?;
        Ok(VerifyingKey::from_bytes(&bytes)?)
    }).collect::<Result<Vec<_>>>()?;
    ensure!(config.identities.iter().collect::<BTreeSet<_>>().len() == keys.len(), "Duplicate validator identities");
    let anchor_hash = hash32(snapshot["anchor"]["hash"].as_str().context("Missing snapshot anchor hash")?)?;
    let (replay,last,start_hash) = cli::load_follow_snapshot(snapshot,&genesis,&programs)?;
    let peers: HubPeers = serde_json::from_value(cli::rpc(url,"getHubPeers",json!({"hub":hub})).await?)?;
    ensure!(peers.hub == hub && peers.peers.len() == 4, "The endpoint returned another hub");
    let expected = config.hubs.iter().find(|candidate|candidate.code==hub).context("Unknown hub")?;
    let mut recipients = BTreeSet::new();
    for peer in &peers.peers {
        ensure!(config.identities.get(peer.id) == Some(&peer.identity) && expected.members.iter().any(|member|member.identity==peer.identity) && recipients.insert(peer.id), "Invalid logical hub peer");
    }
    ensure!(expected.members.len() == peers.peers.len(), "Incomplete logical hub peer set");
    let cert = B64.decode(&peers.certificate).context("Invalid QUIC certificate")?;
    let address = tokio::net::lookup_host(&peers.address).await?.next().context("The hub address did not resolve")?;
    let keypair = read_keypair_file(identity).map_err(|error|anyhow::anyhow!("Read observer identity: {error}"))?;
    let signer = SigningKey::from_bytes(&keypair.to_bytes()[..32].try_into()?);
    let from_slot = last.checked_mul(4).and_then(|slot|slot.checked_add(4)).context("Snapshot slot overflow")?;
    let mut client = crate::external::ExternalClient::connect(address,&cert,hub,&signer,from_slot).await?;
    let control = client.control.clone();
    let connection = client.connection.clone();
    let (sender,mut packets) = tokio::sync::mpsc::channel(256);
    // A dedicated reader owns frame decoding. Cancelling read_exact in select would lose framing.
    let reader = tokio::spawn(async move {
        loop {
            let packet = client.next_packet().await;
            let failed = packet.is_err();
            if sender.send(packet).await.is_err() || failed { break }
        }
    });
    println!("from cblock {last}: {} accounts loaded, state {} matches",replay.executor.account_count(),&start_hash[..12]);
    let mut observer = Observer {replay,anchor:BlockRef {slot:last,hash:anchor_hash},keys,stakes:stakes.clone(),votor:Votor::new(0,stakes)?,bridge:genesis.bridge.clone(),slots:BTreeMap::new(),witnesses:BTreeMap::new(),finalized:BTreeMap::new(),seen:BTreeMap::new(),checked:0,buffered_bytes:0};
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
    let outcome = async {
        loop {
            tokio::select! {
                packet = packets.recv() => observer.packet(packet.context("Hub wire stream ended")??,&recipients)?,
                _ = heartbeat.tick() => for peer in &peers.peers {
                    let rtt = control.ping(peer.id).await?;
                    println!("hub {hub} link {} RTT {:.3} ms",peer.identity,rtt.as_secs_f64()*1000.0);
                },
                result = tokio::signal::ctrl_c() => { result?; return Ok(()); },
            }
        }
    }.await;
    connection.close(0u32.into(),b"observer stopped");
    reader.abort();
    let _ = reader.await;
    outcome
}
