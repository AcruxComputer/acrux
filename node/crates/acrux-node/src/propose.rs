//! A hub peer: bootstrap once, replay the wire, and vote only while admitted.
use crate::{cli, consensus::{BlockRef, Certificate, CertificateKind, Votor}, crypto,
    types::{Attestation, CblockWitness, Commitment, Envelope, SignedVote, Wire}};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use bincode::Options;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use redb::{Database, Durability, ReadableTable, TableDefinition};
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

const AUTHORED_SLOTS: TableDefinition<&str, u64> = TableDefinition::new("authored_through");
const VOTE_JOURNALS: TableDefinition<&str, &[u8]> = TableDefinition::new("consensus_votes");
#[derive(Serialize, Deserialize)]
struct KeptVote { vote: crate::consensus::Vote, envelope: Vec<u8> }
#[derive(Serialize, Deserialize)]
struct VoteState { finalized: BlockRef, votes: Vec<KeptVote> }
struct VoteJournal { db: Database, chain: String, state: VoteState }
impl VoteJournal {
    fn open(identity: &str, chain: String, anchor: BlockRef) -> Result<Self> {
        // Database's exclusive file lock also prevents two processes signing with this identity.
        let db = Database::create(format!("{identity}.acrux-votes.redb"))?;
        let write = db.begin_write()?;
        let state = {
            let table = write.open_table(VOTE_JOURNALS)?;
            let value = table.get(chain.as_str())?;
            value.map(|value|decode(value.value())).transpose()?.unwrap_or(VoteState {finalized:anchor,votes:vec![]})
        };
        write.commit()?;
        ensure!(anchor.slot >= state.finalized.slot && (anchor.slot != state.finalized.slot || anchor.hash == state.finalized.hash),
            "Snapshot is older than or conflicts with this identity's finalized vote journal");
        let mut journal = Self {db,chain,state};
        journal.finalize(anchor)?;
        Ok(journal)
    }
    fn reserve_author(&mut self, slot:u64)->Result<bool> {
        let mut write=self.db.begin_write()?;
        write.set_durability(Durability::Immediate);
        {
            let mut table=write.open_table(AUTHORED_SLOTS)?;
            if table.get(self.chain.as_str())?.is_some_and(|old|old.value()>=slot) {return Ok(false)}
            table.insert(self.chain.as_str(),slot)?;
        }
        write.commit()?;
        Ok(true)
    }
    fn existing(&self, vote: &crate::consensus::Vote) -> Result<Option<Vec<u8>>> {
        use crate::consensus::VoteKind;
        ensure!(vote.slot > self.state.finalized.slot, "Refusing to sign a vote at or below the durable finalized anchor");
        for kept in self.state.votes.iter().filter(|kept|kept.vote.slot == vote.slot) {
            let old = &kept.vote;
            ensure!(old.validator == vote.validator, "This identity's epoch voter index changed after signing");
            let initial = matches!(old.kind,VoteKind::Notar|VoteKind::Skip) && matches!(vote.kind,VoteKind::Notar|VoteKind::Skip);
            let final_phase = old.kind == VoteKind::Finalize && vote.kind == VoteKind::Finalize;
            ensure!(!(initial || final_phase) || (old.kind == vote.kind && old.hash == vote.hash),
                "Refusing to equivocate against this identity's durable vote journal");
            if old == vote { return Ok(Some(kept.envelope.clone())) }
        }
        Ok(None)
    }
    fn record(&mut self, vote: crate::consensus::Vote, envelope: Vec<u8>) -> Result<()> {
        ensure!(self.existing(&vote)?.is_none(), "Vote was already durably signed");
        ensure!(envelope.len() <= 2048, "Signed vote envelope exceeds the member ingress limit");
        ensure!(self.state.votes.len() < MAX_PENDING_SLOTS, "Unfinalized vote journal exceeded its bound");
        self.state.votes.push(KeptVote {vote,envelope});
        self.save()
    }
    fn finalize(&mut self, anchor: BlockRef) -> Result<()> {
        ensure!(anchor.slot >= self.state.finalized.slot && (anchor.slot != self.state.finalized.slot || anchor.hash == self.state.finalized.hash), "Finalized vote journal moved backward or changed its hash");
        self.state.finalized = anchor;
        self.state.votes.retain(|kept|kept.vote.slot > anchor.slot);
        self.save()
    }
    fn save(&self) -> Result<()> {
        let bytes = bincode::serialize(&self.state)?;
        ensure!(bytes.len() as u64 <= MAX_WIRE_BYTES, "Unfinalized vote journal exceeded its byte bound");
        let mut write = self.db.begin_write()?;
        write.set_durability(Durability::Immediate);
        { write.open_table(VOTE_JOURNALS)?.insert(self.chain.as_str(),bytes.as_slice())?; }
        write.commit()?;
        Ok(())
    }
}

pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    Ok(bincode::DefaultOptions::new().with_fixint_encoding().with_limit(MAX_WIRE_BYTES)
        .reject_trailing_bytes().deserialize(bytes)?)
}
fn hash32(text: &str) -> Result<[u8; 32]> {
    hex::decode(text)?.try_into().map_err(|_| anyhow::anyhow!("Expected a 32-byte hash"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all="camelCase")]
struct HubPeers { hub:String,address:String,certificate:String,peers:Vec<crate::member_link::HubPeer>,relay_peers:Vec<crate::member_link::HubPeer> }

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
    votor: Votor,
    signer: SigningKey,
    journal: VoteJournal,
    identity: String,
    hub: String,
    checkpoints: BTreeMap<BlockRef, cli::ReplayCheckpoint>,
    outgoing: Vec<Vec<u8>>,
    epoch_updates: BTreeMap<u64, (crate::execution::AdmissionSnapshot, Vec<solana_sdk::pubkey::Pubkey>)>,
    reported_epoch: Option<u64>,
    control: Option<std::sync::Arc<crate::member_link::MemberLink>>,
    member: Option<crate::external_proposer::MemberProposer>,
    fast_lane: crate::external_proposer::FastLane,
    member_metrics: crate::external_proposer::MemberMetrics,
    certified: BTreeSet<BlockRef>,
    window_notices: BTreeMap<u64,(Option<BlockRef>,u64)>,
    bridge: Option<crate::genesis::BridgeConfig>,
    slots: BTreeMap<u64, Slot>,
    witnesses: BTreeMap<BlockRef, Witness>,
    finalized: BTreeMap<u64, Finalized>,
    seen: BTreeMap<[u8; 32], u64>,
    pending: BTreeMap<[u8; 32], (u64, std::time::Instant, usize, Envelope)>,
    checked: u64,
    buffered_bytes: usize,
}
impl Observer {
    fn proposer(&self, slot: u64, role: usize) -> Result<usize> {
        ensure!((1..=3).contains(&role), "Invalid proposer role");
        let ids = self.replay.proposers.as_ref().context("Missing proposer configuration")?.ids(slot / 16)?;
        ids.get(role - 1).copied().context("Missing drawn proposer")
    }
    fn vote_identity(&self, slot: u64, index: usize) -> Result<&VerifyingKey> {
        let proposers = self.replay.proposers.as_ref().context("Missing proposer configuration")?;
        let (members, _) = proposers.voting_set(slot)?;
        let identity = members.get(index).context("Vote by an unknown epoch member")?;
        let global = proposers.catalog.iter().position(|known| known == identity).context("Epoch member is absent from the validator catalog")?;
        self.keys.get(global).context("Epoch member key is not finalized yet")
    }
    fn verify_vote(&self, signed: &SignedVote) -> Result<()> {
        crypto::verify_bytes(self.vote_identity(signed.vote.slot, signed.vote.validator)?,
            &crate::types::vote_message(&signed.vote, self.bridge.as_ref())?, &signed.signature)
            .context("Invalid consensus vote signature")
    }
    fn actions(&mut self, actions: Vec<crate::consensus::Action>) -> Result<()> {
        for action in actions {
            match action {
                crate::consensus::Action::SendVote(vote) => {
                    ensure!(self.votor.own_at(vote.slot) == Some(vote.validator), "Consensus emitted a vote for another epoch member");
                    if let Some(envelope) = self.journal.existing(&vote)? {
                        self.outgoing.push(envelope);
                        continue;
                    }
                    let message = crate::types::vote_message(&vote, self.bridge.as_ref())?;
                    let signature = crypto::sign_bytes(&self.signer, &message);
                    let from = self.replay.proposers.as_ref().context("Missing proposer configuration")?.catalog.iter()
                        .position(|known| known == &self.identity).context("Own admitted identity is absent from the validator catalog")?;
                    let body = Wire::Vote(SignedVote {vote:vote.clone(),signature});
                    let signature = crypto::sign_bytes(&self.signer, &bincode::serialize(&body)?);
                    let envelope = bincode::serialize(&Envelope {from,body,signature})?;
                    self.journal.record(vote.clone(),envelope.clone())?;
                    self.outgoing.push(envelope);
                    println!("vote {:?} cblock {} epoch {}",vote.kind,vote.slot,self.epoch(vote.slot));
                }
                // A locally formed certificate may still include our unsent queued vote. Wait
                // for the node's signed certificate before advancing the durable final anchor.
                crate::consensus::Action::SendCertificate(_) => {},
                _ => {},
            }
        }
        Ok(())
    }
    fn epoch(&self, cblock: u64) -> u64 { cblock * 4 / self.bridge.as_ref().map_or(432_000, |bridge|bridge.epoch_slots) }
    fn status(&mut self, cblock: u64) {
        let epoch = self.epoch(cblock);
        if self.reported_epoch == Some(epoch) { return }
        self.reported_epoch = Some(epoch);
        match self.votor.own_at(cblock) {
            Some(index) => println!("epoch {epoch} member {} voting index {index}",self.identity),
            None => println!("epoch {epoch} observer {}",self.identity),
        }
    }
    fn learn_finalized(&mut self) -> Result<()> {
        self.replay.learn_finalized()?;
        let proposers = self.replay.proposers.as_ref().context("Missing proposer configuration")?;
        ensure!(proposers.catalog.len() >= self.keys.len(), "Finalized validator catalog shrank");
        for identity in &proposers.catalog[self.keys.len()..] {
            let bytes: [u8;32] = bs58::decode(identity).into_vec()?.try_into().map_err(|_|anyhow::anyhow!("Invalid validator identity"))?;
            self.keys.push(VerifyingKey::from_bytes(&bytes)?);
        }
        for snapshot in proposers.admissions.values() {
            let own = snapshot.members.iter().position(|member|member.identity.to_string()==self.identity && member.hub==self.hub);
            let from = snapshot.epoch.checked_mul(proposers.config.epoch_slots).context("Voting epoch overflow")? / 4;
            let stakes = snapshot.members.iter().map(|member|member.stake).collect::<Vec<_>>();
            // The bootstrap epoch inherits the legacy set. Its hashed marker can finalize
            // after its first wire votes; an identical inherited set needs no replacement.
            if self.votor.stakes_at(from) != stakes || self.votor.own_at(from) != own {
                self.votor.install_stakes(from,own,stakes)?;
            }
        }
        if let Some(server)=self.control.as_ref().and_then(|link|link.server.as_ref()) {
            server.set_catalog(self.keys.iter().enumerate().map(|(id,key)|(id,key.to_bytes())).collect())?;
        }
        if self.member.is_none() {
            if let (Some(control),Some(id)) = (&self.control,proposers.catalog.iter().position(|identity|identity==&self.identity)) {
                if id>=proposers.config.identities.len() {
                    self.member=Some(crate::external_proposer::MemberProposer::new(id,self.signer.clone(),self.identity.clone(),control.clone(),proposers.config.seed.parse()?));
                }
            }
        }
        if let Some(member)=&mut self.member {
            member.finalized(&self.replay.executor);
            self.member_metrics.set(member.metrics_engine());
        }
        let effective=self.bridge.as_ref().and_then(|bridge|bridge.admission_from_slot).is_some_and(|from|self.anchor.slot*4>=from)
            &&self.replay.executor.effective_memberships(self.epoch(self.anchor.slot))?.iter().any(|member|member.identity.to_string()==self.identity&&member.hub==self.hub);
        self.fast_lane.update(proposers.clone(),self.keys.clone(),self.member.as_ref().map(|member|member.inbox()),self.identity.clone(),self.hub.clone(),effective);
        for checkpoint in self.checkpoints.values_mut() { checkpoint.proposers = self.replay.proposers.clone(); }
        let ready = self.epoch_updates.iter().filter(|(epoch,_)|proposers.admissions.contains_key(epoch)).map(|(epoch,_)|*epoch).collect::<Vec<_>>();
        for epoch in ready {
            let (snapshot,catalog) = self.epoch_updates.remove(&epoch).unwrap();
            ensure!(proposers.admissions[&epoch] == snapshot, "Wire validator epoch differs from the replayed hashed snapshot");
            let catalog = catalog.iter().map(ToString::to_string).collect::<Vec<_>>();
            ensure!(proposers.catalog.starts_with(&catalog), "Wire validator catalog differs from the replayed hashed catalog");
        }
        Ok(())
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
        if certificate.kind!=CertificateKind::Skip {
            self.certified.insert(BlockRef{slot:certificate.slot,hash:certificate.hash.context("Certificate hash")?});
        }
        self.record_finality(&certificate)?;
        let actions = self.votor.receive(crate::consensus::Message::Certificate(certificate))?;
        self.actions(actions)
    }
    fn record_finality(&mut self, certificate: &Certificate) -> Result<()> {
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
        let weights = self.votor.stakes_at(certificate.slot);
        let stake = certificate.votes.iter().map(|vote|weights[vote.validator] as u128).sum::<u128>();
        let total = weights.iter().map(|stake|*stake as u128).sum::<u128>();
        if let Some(known) = self.finalized.get(&block.slot) { ensure!(known.block == block, "Conflicting finalized cblocks"); }
        self.finalized.insert(block.slot, Finalized {block,kind,share:stake as f64 / total as f64,threshold});
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
    fn recipient_ready(&self,to:usize)->Result<bool> {
        let proposers=self.replay.proposers.as_ref().context("Missing proposer configuration")?;
        let Some(identity)=proposers.catalog.get(to) else{return Ok(false)};
        let mirrored=proposers.config.hubs.iter().find(|hub|hub.code==self.hub).is_some_and(|hub|hub.members.iter().any(|member|&member.identity==identity));
        ensure!(identity==&self.identity||mirrored,"Mirrored packet is addressed to another hub or identity");
        Ok(true)
    }
    #[cfg(test)]
    fn packet(&mut self, packet: crate::external::Packet, recipients: &BTreeSet<usize>) -> Result<()> {
        self.packet_at(packet,recipients,std::time::Instant::now())
    }
    fn packet_at(&mut self, packet: crate::external::Packet, recipients: &BTreeSet<usize>, received_at:std::time::Instant) -> Result<()> {
        let target_ready=recipients.contains(&packet.to)||self.recipient_ready(packet.to)?;
        let digest = crypto::hash(&packet.payload);
        if self.seen.contains_key(&digest) { return Ok(()) }
        let envelope: Envelope = decode(&packet.payload)?;
        ensure!(envelope.from == packet.from, "Mirrored envelope has another sender");
        if !target_ready || !self.epoch_ready(&envelope.body)? {
            if !self.pending.contains_key(&digest) {
                ensure!(self.pending.len() + self.seen.len() < MAX_SEEN_ENVELOPES
                    && self.buffered_bytes.checked_add(packet.payload.len()).is_some_and(|bytes| bytes <= MAX_PENDING_MATERIAL_BYTES),
                    "Observer pending material exceeded its window; restart from a fresh snapshot");
                self.buffered_bytes += packet.payload.len();
                self.pending.insert(digest,(packet.slot,received_at,packet.to,envelope));
            }
        } else { self.envelope(envelope,digest,packet.slot,received_at,packet.to)?; }
        loop {
            self.start_windows()?;
            self.speculate()?;
            self.replay_ready()?;
            if !self.drain_pending()? { return Ok(()) }
        }
    }
    fn start_windows(&mut self)->Result<()> {
        let Some(proposers)=&self.replay.proposers else{return Ok(())};
        let Some(member)=&mut self.member else{return Ok(())};
        for (number,(parent,unix_us)) in &self.window_notices {
            if !parent.is_some_and(|parent|self.certified.contains(&parent)||parent==self.anchor) {continue}
            if self.votor.own_at(number*4).is_none() {continue}
            let window=proposers.window(*number)?;
            if window.hub_code!=self.hub||!window.proposers.contains(&self.identity){continue}
            let ids=proposers.ids(*number)?;
            let (identities,stakes)=proposers.voting_set(number*4)?;
            let view=crate::view::window_json(*number,&window,&identities,&stakes,None,&proposers.config.identities);
            member.start_window(*number,*parent,*unix_us,ids,view,proposers.catalog.clone())?;
        }
        Ok(())
    }
    fn own_exchange(&self,slot:u64)->Result<Option<crate::external_proposer::CommitInbox>> {
        let Some(member)=&self.member else{return Ok(None)};
        if self.votor.own_at(slot/4).is_none() {return Ok(None)}
        let proposers=self.replay.proposers.as_ref().context("Missing proposer configuration")?;
        let window=proposers.window(slot/16)?;
        if window.hub_code!=self.hub||!window.proposers.contains(&self.identity) {return Ok(None)}
        let inbox=member.inbox();
        inbox.set_window(slot/16,window.hub_code,proposers.catalog.clone(),proposers.ids(slot/16)?);
        Ok(Some(inbox))
    }
    async fn member_tick(&mut self)->Result<()> {
        if let Some(member)=&mut self.member {
            let journal=&mut self.journal;
            member.tick(&self.replay.executor,|slot|journal.reserve_author(slot)).await?;
        }
        Ok(())
    }
    fn epoch_ready(&self, wire: &Wire) -> Result<bool> {
        let proposers = self.replay.proposers.as_ref().context("Missing proposer configuration")?;
        let known = |cblock: u64| {
            let Some(from) = proposers.config.admission_from_slot else { return true };
            let epoch = cblock / (proposers.config.epoch_slots / 4);
            epoch <= from / proposers.config.epoch_slots || proposers.admissions.contains_key(&epoch)
        };
        Ok(match wire {
            Wire::Vote(vote) => known(vote.vote.slot),
            Wire::Certificate {certificate,..} => known(certificate.slot),
            Wire::Shreds(shreds) | Wire::Relay(shreds) => shreds.iter().all(|shred|known(shred.meta.slot / 4)),
            Wire::Commit(commitment) => known(commitment.slot / 4),
            Wire::CommitReceipt(receipt) => known(receipt.slot/4),
            Wire::Attest(attestation) => known(attestation.slot / 4),
            Wire::Handover {slot,shreds,attestations,..} => known(*slot / 4)
                && shreds.iter().all(|shred|known(shred.meta.slot / 4))
                && attestations.iter().all(|(attestation,_)|known(attestation.slot / 4)),
            Wire::Equivocation {first,second} => known(first.0.slot / 4) && known(second.0.slot / 4),
            Wire::Cblock(witness) => known(witness.block.slot),
            Wire::WindowStart {number,..} => known(number*4),
            Wire::ValidatorEpoch {..} | Wire::Submit(_) | Wire::SubmitFragment(_) | Wire::ExchangeMetrics {..} => true,
        })
    }
    fn drain_pending(&mut self) -> Result<bool> {
        let mut ready = vec![];
        for (digest,(_,_,to,envelope)) in &self.pending {
            if self.recipient_ready(*to)? && self.epoch_ready(&envelope.body)? { ready.push(*digest); }
        }
        for digest in &ready {
            let (slot,received_at,to,envelope) = self.pending.remove(digest).unwrap();
            self.buffered_bytes -= usize::try_from(bincode::serialized_size(&envelope)?)?;
            self.envelope(envelope,*digest,slot,received_at,to)?;
        }
        Ok(!ready.is_empty())
    }
    fn envelope(&mut self, envelope: Envelope, digest: [u8;32], slot: u64, received_at:std::time::Instant, to:usize) -> Result<()> {
        let key = self.keys.get(envelope.from).context("Envelope from an unknown validator")?;
        crypto::verify_bytes(key, &bincode::serialize(&envelope.body)?, &envelope.signature).context("Invalid wire envelope signature")?;
        self.seen.insert(digest, slot);
        match envelope.body {
            Wire::Shreds(shreds) | Wire::Relay(shreds) => for shred in shreds { self.shred(shred)?; },
            Wire::Commit(commitment) => {
                self.commitment(commitment.clone(), None, Some(envelope.from))?;
                if let Some(inbox)=self.own_exchange(commitment.slot)? {
                    let signature=envelope.signature;
                    tokio::spawn(async move{inbox.receive(envelope.from,commitment,signature,received_at).await;});
                }
            },
            Wire::CommitReceipt(receipt) => {
                ensure!((1..=3).contains(&receipt.proposer),"Invalid receipt proposer");
                if let Some(inbox)=self.own_exchange(receipt.slot)? {
                    let proposers=self.replay.proposers.as_ref().context("Missing proposer configuration")?;
                    let ids=proposers.ids(receipt.slot/16)?;
                    if to==ids[receipt.proposer-1] && proposers.catalog.get(ids[receipt.proposer-1])==Some(&self.identity) {
                        ensure!(ids.contains(&envelope.from)&&proposers.catalog.get(envelope.from)!=Some(&self.identity),"Receipt signer is not a drawn co-proposer");
                        inbox.receive_receipt(envelope.from,receipt,received_at);
                    }
                }
            },
            Wire::Attest(attestation) => self.attest(attestation, envelope.signature, Some(envelope.from))?,
            Wire::Vote(vote) => {
                self.verify_vote(&vote)?;
                ensure!(self.keys[envelope.from] == *self.vote_identity(vote.vote.slot,vote.vote.validator)?, "Vote envelope belongs to another epoch member");
                let actions = self.votor.receive(crate::consensus::Message::Vote(vote.vote))?;
                self.actions(actions)?;
            },
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
            Wire::ValidatorEpoch {snapshot,catalog} => {
                ensure!(envelope.from == 0 && snapshot.members.len() <= 32, "Invalid validator epoch publisher or size");
                ensure!(self.epoch_updates.len() < MAX_PENDING_SLOTS / 4 || self.epoch_updates.contains_key(&snapshot.epoch), "Too many pending validator epochs");
                if let Some((known,known_catalog)) = self.epoch_updates.get(&snapshot.epoch) { ensure!(known == &snapshot && known_catalog == &catalog, "Conflicting validator epoch messages"); }
                self.epoch_updates.insert(snapshot.epoch,(snapshot,catalog));
            },
            Wire::WindowStart {number,parent,started_at_unix_us} => {
                let proposers=self.replay.proposers.as_ref().context("Missing proposer configuration")?;
                let window=proposers.window(number)?;
                ensure!(envelope.from==proposers.window_representative(number)?,"Window notice is not from the fixed hub representative");
                if window.hub_code==self.hub && window.proposers.contains(&self.identity) && self.votor.own_at(number*4).is_some() {
                    if let Some(known)=self.window_notices.get(&number) {ensure!(*known==(parent,started_at_unix_us),"Conflicting window notice");}
                    self.window_notices.insert(number,(parent,started_at_unix_us));
                }
            },
            Wire::ExchangeMetrics {..} => {},
            Wire::Submit(transaction) => {
                let own=self.replay.proposers.as_ref().and_then(|proposers|proposers.catalog.iter().position(|identity|identity==&self.identity));
                if own==Some(to) {if let Some(member)=&mut self.member {member.transaction(&self.replay.executor,transaction)?;}}
            },
            Wire::SubmitFragment(fragment) => {
                let own=self.replay.proposers.as_ref().and_then(|proposers|proposers.catalog.iter().position(|identity|identity==&self.identity));
                if own==Some(to) {if let Some(member)=&mut self.member {member.fragment(fragment)?;}}
            },
        }
        ensure!(self.slots.len() <= MAX_PENDING_SLOTS && self.witnesses.len() <= MAX_PENDING_SLOTS / 4
            && self.seen.len() + self.pending.len() <= MAX_SEEN_ENVELOPES, "Observer data exceeded its window; restart from a fresh snapshot");
        Ok(())
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
    fn outbox(&self, block: BlockRef) -> Result<()> {
        let Some(root) = self.witnesses[&block].wire.outbox_root else { return Ok(()) };
        let bridge = self.bridge.as_ref().context("Bridge cblock without a bridge rule")?;
        let epoch = block.slot * 4 / bridge.epoch_slots + 1;
        let checkpoint = self.replay.executor.checkpoint();
        let snapshot = self.replay.executor.admission_snapshot(&checkpoint,epoch)?;
        let admission_active = bridge.admission_from_slot.is_some_and(|from|block.slot * 4 >= from);
        let mut entries = vec![];
        if let Some(snapshot) = snapshot {
            entries.push(crate::bridge::OutboxEntry::ValidatorSet {epoch,validators:snapshot.members.iter().map(|member|(member.identity.to_bytes(),member.stake)).collect()});
        } else if !admission_active {
            entries.push(crate::bridge::OutboxEntry::ValidatorSet {epoch,validators:self.keys.iter().take(12).map(|key|(key.to_bytes(),100_000_000)).collect()});
        }
        entries.extend(self.replay.executor.withdrawals(block.slot).into_iter().map(|(sequence,solana_mint,recipient,amount)| crate::bridge::OutboxEntry::Withdrawal {sequence,solana_mint,recipient,amount}));
        if let Some(burn) = self.replay.executor.fee_burn(block.slot)? { entries.push(crate::bridge::OutboxEntry::FeeBurn {epoch:burn.epoch,amount:burn.amount}); }
        ensure!(crate::bridge::outbox_root(&entries) == root, "Cblock {}'s re-executed outbox differs from its commitment",block.slot);
        Ok(())
    }
    fn view(&self, block: BlockRef, offset: usize, finality: Option<&Finalized>) -> Result<(Value, Vec<(u8,String)>)> {
        let witness = &self.witnesses[&block];
        let accepted = &witness.accepted[offset];
        let proposers = self.replay.proposers.as_ref().context("Missing proposer configuration")?.window(accepted.0 / 16)?.proposers;
        let recipients = proposers.iter().enumerate().map(|(index,identity)|((index+1) as u8,identity.clone())).collect::<Vec<_>>();
        let certificates = finality.map(|finality|vec![json!({"kind":finality.kind,"stakeShare":finality.share,"threshold":finality.threshold,"consensusSlot":finality.block.slot})]).unwrap_or_default();
        Ok((json!({"slot":accepted.0,"stateHash":witness.wire.state_hashes[offset],"agreedSet":accepted.1,
            "window":{"proposers":proposers.iter().enumerate().map(|(index,identity)|json!({"index":index+1,"identity":identity})).collect::<Vec<_>>()},
            "cblock":{"certificates":certificates}}),recipients))
    }
    fn speculate(&mut self) -> Result<()> {
        loop {
            let mut ready = None;
            for (block,witness) in self.witnesses.iter().filter(|(block,_)| !self.checkpoints.contains_key(block)) {
                let Some(parent) = witness.wire.parent else { continue };
                let Some(checkpoint) = self.checkpoints.get(&parent) else { continue };
                let blocks = witness.accepted.iter().map(|accepted|self.slot_blocks(accepted)).collect::<Result<Vec<_>>>()?;
                if blocks.iter().any(Option::is_none) { continue }
                ready = Some((*block,parent,checkpoint.clone(),blocks));
                break;
            }
            let Some((block,parent,checkpoint,blocks)) = ready else { return Ok(()) };
            self.replay.restore(checkpoint)?;
            self.status(block.slot);
            for (offset,blocks) in blocks.into_iter().enumerate() {
                let blocks = blocks.unwrap();
                let (view,recipients) = self.view(block,offset,None)?;
                cli::check_slot_agreement(&view,&blocks["attestations"])?;
                let slot = block.slot * 4 + offset as u64;
                let state = self.replay.apply(slot,&recipients,&blocks).with_context(||format!("re-executing slot {slot}"))?;
                ensure!(view["stateHash"].as_str() == Some(state.as_str()), "Slot {slot}: re-executed state differs from the signed cblock");
            }
            self.outbox(block)?;
            self.checkpoints.insert(block,self.replay.checkpoint());
            let actions = self.votor.local_block(crate::consensus::Block {id:block,parent:Some(parent)})?;
            self.actions(actions)?;
        }
    }
    fn replay_ready(&mut self) -> Result<()> {
        loop {
            let mut ready = None;
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
                let block = *chain.last().context("Empty finalized extension")?;
                if let Some(checkpoint) = self.checkpoints.get(&block) { ready = Some((block,finality.clone(),checkpoint.clone())); break }
            }
            let Some((block,finality,checkpoint)) = ready else { return Ok(()) };
            self.replay.restore(checkpoint)?;
            for offset in 0..4 {
                let accepted = &self.witnesses[&block].accepted[offset];
                let blocks = self.slot_blocks(accepted)?.context("Finalized block data disappeared")?;
                let (view,_) = self.view(block,offset,Some(&finality))?;
                cli::check_slot(&view,&blocks["attestations"])?;
                self.checked += 1;
                let slot = accepted.0;
                let state = &self.witnesses[&block].wire.state_hashes[offset];
                println!("slot {slot} re-executed  agreed {}  state {} matches  checked {}",view["agreedSet"],&state[..12],self.checked);
            }
            self.journal.finalize(block)?;
            self.anchor = block;
            self.learn_finalized()?;
            self.checkpoints.insert(block,self.replay.checkpoint());
            self.checkpoints.retain(|known,_|known.slot >= block.slot);
            self.buffered_bytes -= self.slots.iter().filter(|(slot,_)| **slot / 4 <= block.slot).map(|(_,slot)|slot.buffered_bytes).sum::<usize>();
            self.slots.retain(|slot,_| *slot / 4 > block.slot);
            self.witnesses.retain(|known,_| known.slot > block.slot);
            self.finalized.retain(|slot,_| *slot > block.slot);
            self.seen.retain(|_,slot| *slot / 4 > block.slot);
            self.votor.prune(block.slot.saturating_sub(8));
            self.certified.retain(|known|known.slot>=block.slot.saturating_sub(8));
            self.window_notices.retain(|number,_|*number>=block.slot/4);
            self.speculate()?;
        }
    }

}

/// Runs sessions until Ctrl-C. A session that lost its link after connecting starts again from a
/// fresh snapshot; the vote journal on disk keeps every vote it already cast.
pub async fn run(url: &str, hub: &str, identity: &str, genesis_path: &str, rpc_port:Option<u16>, metrics_port:Option<u16>, peer_port:Option<u16>, peer_address:Option<String>) -> Result<()> {
    ensure!(peer_port.is_some()||peer_address.is_none(),"--peer-address requires --peer-port");
    let mut wait=Duration::from_secs(1);
    // Once the first session has connected, a failed reconnect (the RPC or the link) is retried too.
    let mut connected=false;
    loop {
        let started=std::time::Instant::now();
        let error=match session(url,hub,identity,genesis_path,rpc_port,metrics_port,peer_port,peer_address.clone(),&mut connected).await {
            Ok(())=>return Ok(()),
            Err(error) if !connected=>return Err(error),
            Err(error)=>error,
        };
        if started.elapsed()>=Duration::from_secs(60) {wait=Duration::from_secs(1)}
        println!("link lost: {error:#}; reconnecting in {} s",wait.as_secs());
        tokio::select! {
            _=tokio::time::sleep(wait)=>{},
            result=tokio::signal::ctrl_c()=>{result?;return Ok(())},
        }
        wait=(wait*2).min(Duration::from_secs(30));
    }
}

async fn session(url: &str, hub: &str, identity: &str, genesis_path: &str, rpc_port:Option<u16>, metrics_port:Option<u16>, peer_port:Option<u16>, peer_address:Option<String>, connected:&mut bool) -> Result<()> {
    let genesis = crate::genesis::Genesis::load(std::path::Path::new(genesis_path))?;
    let programs = genesis.programs.iter().map(|(address,path)|crate::execution::GenesisProgram {address:address.clone(),path:path.clone()}).collect::<Vec<_>>();
    let snapshot = cli::rpc(url,"getExecutionSnapshot",json!({})).await?;
    let config: crate::protocol::ProposerConfig = serde_json::from_value(snapshot["proposerConfig"].clone()).context("Missing public proposer configuration")?;
    ensure!(config.hubs.iter().any(|known|known.code==hub)||peer_port.is_some(),"An outside-only hub requires --peer-port and its public --peer-address");
    let stakes: Vec<u64> = serde_json::from_value(snapshot["consensusStakes"].clone()).context("Missing consensus stakes")?;
    ensure!(!stakes.is_empty() && stakes.len() <= 32 && stakes.iter().all(|stake|*stake>0), "Invalid consensus stake set");
    stakes.iter().try_fold(0u128,|total,stake|total.checked_add(*stake as u128)).context("Consensus stake overflow")?;
    let keys = config.identities.iter().map(|identity| {
        let bytes: [u8;32] = bs58::decode(identity).into_vec()?.try_into().map_err(|_|anyhow::anyhow!("Invalid validator identity"))?;
        Ok(VerifyingKey::from_bytes(&bytes)?)
    }).collect::<Result<Vec<_>>>()?;
    ensure!(config.identities.iter().collect::<BTreeSet<_>>().len() == keys.len(), "Duplicate validator identities");
    let anchor_hash = hash32(snapshot["anchor"]["hash"].as_str().context("Missing snapshot anchor hash")?)?;
    let (replay,last,start_hash) = cli::load_follow_snapshot(snapshot,&genesis,&programs)?;
    ensure!(replay.proposers.as_ref().context("Missing proposer configuration")?.voting_set(last)?.1 == stakes, "Snapshot consensus stakes differ from the replayed epoch set");
    let mut peers: HubPeers = serde_json::from_value(cli::rpc(url,"getHubPeers",json!({"hub":hub,"direct":true})).await?)?;
    ensure!(peers.hub==hub,"The endpoint returned another hub");
    let catalog=&replay.proposers.as_ref().context("Missing proposer configuration")?.catalog;
    let mut known=BTreeSet::new();
    for peer in &peers.peers {
        if let Some(identity)=catalog.get(peer.id) {
            ensure!(identity==&peer.identity&&known.insert(peer.id),"Invalid logical hub peer");
        }
    }
    // Discovery can advance after the execution snapshot. Future identities become usable only
    // when finalized wire replay has learned their committed catalog and membership.
    peers.peers.retain(|peer|known.contains(&peer.id));
    let mut recipients=BTreeSet::new();
    ensure!(peers.relay_peers.len()==config.identities.len(),"Incomplete hosted relay set");
    for peer in &peers.relay_peers {
        ensure!(config.identities.get(peer.id)==Some(&peer.identity)&&recipients.insert(peer.id),"Invalid hosted relay peer");
    }
    let cert = B64.decode(&peers.certificate).context("Invalid QUIC certificate")?;
    let address = tokio::net::lookup_host(&peers.address).await?.next().context("The hub address did not resolve")?;
    let keypair = read_keypair_file(identity).map_err(|error|anyhow::anyhow!("Read observer identity: {error}"))?;
    let signer = SigningKey::from_bytes(&keypair.to_bytes()[..32].try_into()?);
    let anchor = BlockRef {slot:last,hash:anchor_hash};
    let chain = match &genesis.bridge { Some(bridge) => bridge.chain_tag(), None => crypto::hash(&bincode::serialize(&(&config.seed,&config.identities))?) };
    let journal = VoteJournal::open(identity,hex::encode(chain),anchor)?;
    let from_slot = last.checked_mul(4).and_then(|slot|slot.checked_add(4)).context("Snapshot slot overflow")?;
    let (peer_server,peer_ingress,advertised)=match peer_port {
        Some(port)=>{let (server,input,advertised)=crate::member_link::bind_peer(port,peer_address,hub).await?;(Some(server),Some(input),Some(advertised))},
        None=>(None,None,None),
    };
    let mut client = crate::external::ExternalClient::connect(address,&cert,hub,&signer,from_slot).await?;
    *connected=true;
    let control = std::sync::Arc::new(client.control.clone());
    let connection = client.connection.clone();
    println!("from cblock {last}: {} accounts loaded, state {} matches",replay.executor.account_count(),&start_hash[..12]);
    let identity = bs58::encode(signer.verifying_key().to_bytes()).into_string();
    let legacy_stakes = vec![100_000_000;config.identities.len()];
    let mut votor = Votor::new(0,legacy_stakes.clone())?;
    votor.install_stakes(0,None,legacy_stakes)?;
    let fast_lane=crate::external_proposer::FastLane::default();
    fast_lane.host_slot(last*4+3);
    let member_metrics=crate::external_proposer::MemberMetrics::default();
    let link=crate::member_link::MemberLink::new(control.clone(),peer_server);
    if let Some(input)=peer_ingress {link.receive(input,fast_lane.clone());}
    let mut observer = Observer {checkpoints:BTreeMap::from([(anchor,replay.checkpoint())]),replay,anchor,keys,votor,signer,journal,identity,hub:hub.to_owned(),outgoing:vec![],epoch_updates:BTreeMap::new(),reported_epoch:None,control:Some(link.clone()),member:None,fast_lane:fast_lane.clone(),member_metrics:member_metrics.clone(),certified:BTreeSet::from([anchor]),window_notices:BTreeMap::new(),bridge:genesis.bridge.clone(),slots:BTreeMap::new(),witnesses:BTreeMap::new(),finalized:BTreeMap::new(),seen:BTreeMap::new(),pending:BTreeMap::new(),checked:0,buffered_bytes:0};
    observer.learn_finalized()?;
    observer.votor.resume(anchor);
    observer.status(last);
    let (sender,mut packets) = tokio::sync::mpsc::channel(256);
    // A dedicated reader owns frame decoding. Cancelling read_exact in select would lose framing.
    let reader_fast=fast_lane.clone();
    let reader = tokio::spawn(async move {
        loop {
            let packet = client.next_packet().await;
            let received_at=std::time::Instant::now();
            let packet=match packet {Ok(packet)=>{
                reader_fast.host_slot(packet.slot);
                match reader_fast.receive(&packet,received_at).await{Ok(())=>Ok((received_at,packet)),Err(error)=>Err(error)}
            },Err(error)=>Err(error)};
            let failed=packet.is_err();
            if sender.send(packet).await.is_err() || failed {break}
        }
    });

    let (mut rpc_requests,rpc_task)=match rpc_port {Some(port)=>{let (receiver,task)=crate::external_proposer::bind_rpc(port).await?;(Some(receiver),Some(task))},None=>(None,None)};
    let metrics_task=match metrics_port {Some(port)=>Some(crate::external_proposer::bind_metrics(port,member_metrics.clone()).await?),None=>None};
    let mut member_tick=tokio::time::interval(Duration::from_millis(1));
    member_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut maintenance=tokio::spawn(crate::member_link::maintain(url.to_owned(),hub.to_owned(),observer.signer.clone(),link.clone(),fast_lane,member_metrics,advertised));
    let heartbeat_control=control.clone();let heartbeat_hub=hub.to_owned();
    let mut heartbeat=tokio::spawn(async move {
        let mut interval=tokio::time::interval(Duration::from_secs(10));interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            for peer in &peers.peers {
                let rtt=heartbeat_control.ping(peer.id).await?;
                println!("hub {} link {} RTT {:.3} ms",heartbeat_hub,peer.identity,rtt.as_secs_f64()*1000.0);
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        #[allow(unreachable_code)] Ok::<(),anyhow::Error>(())
    });
    let mut heartbeat_consumed=false;let mut maintenance_consumed=false;
    let outcome = async {
        loop {
            tokio::select! {
                packet = packets.recv() => {
                    let (received_at,packet)=packet.context("Hub wire stream ended")??;
                    observer.packet_at(packet,&recipients,received_at)?;
                    for envelope in observer.outgoing.drain(..) { control.send_envelope(&envelope).await?; }
                },
                _=member_tick.tick()=>observer.member_tick().await?,
                request=async{match &mut rpc_requests{Some(receiver)=>receiver.recv().await,None=>std::future::pending().await}}=>{
                    if let Some(request)=request {
                        let slot=observer.member.as_ref().and_then(|member|member.current_slot()).unwrap_or(0).max(observer.anchor.slot*4+3);
                        crate::external_proposer::rpc(request,observer.member.as_mut(),&mut observer.replay.executor,slot);
                    }
                },
                result=&mut heartbeat=>{heartbeat_consumed=true;return result.context("Host heartbeat task stopped")?;},
                result=&mut maintenance=>{maintenance_consumed=true;return result.context("Peer maintenance task stopped")?;},
                result = tokio::signal::ctrl_c() => { result?; return Ok(()); },
            }
        }
    }.await;
    connection.close(0u32.into(),b"observer stopped");
    if !heartbeat_consumed {heartbeat.abort();let _=heartbeat.await;}
    if !maintenance_consumed {maintenance.abort();let _=maintenance.await;}
    link.close().await;
    reader.abort();
    let _ = reader.await;
    if let Some(task)=rpc_task {task.abort();let _=task.await;}
    if let Some(task)=metrics_task {task.abort();let _=task.await;}
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{consensus::{CertificateKind,Vote,VoteKind}, execution::{AdmissionMember,AdmissionSnapshot},
        protocol::{ProposerConfig,ProposerSchedules}, schedule::{Hub,Member}};
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn admission_wire_votes_wait_for_the_finalized_cutoff_set() {
        let signers = (1..=12).map(|seed|SigningKey::from_bytes(&[seed;32])).collect::<Vec<_>>();
        let identities = signers.iter().map(|key|bs58::encode(key.verifying_key().to_bytes()).into_string()).collect::<Vec<_>>();
        let hubs = ["FRA","NYC","TYO"].into_iter().enumerate().map(|(index,code)|Hub {
            code:code.into(),region:code.into(),members:identities[index*4..index*4+4].iter()
                .map(|identity|Member {identity:identity.clone(),stake:100_000_000}).collect(),
        }).collect::<Vec<_>>();
        let config = ProposerConfig {seed:"9".into(),windows_per_epoch:96,identities:identities.clone(),hubs,
            epoch_slots:256,stake_from_slot:Some(256),genesis_stake_lamports:Some(1_000_000_000),admission_from_slot:Some(512)};
        let replay = cli::Replay::for_test(ProposerSchedules::new(config.clone()).unwrap()).unwrap();
        let signer = SigningKey::from_bytes(&[88;32]);
        let identity = bs58::encode(signer.verifying_key().to_bytes()).into_string();
        let anchor = BlockRef {slot:127,hash:[1;32]};
        let journal = VoteJournal {
            db:Database::builder().create_with_backend(redb::backends::InMemoryBackend::new()).unwrap(),
            chain:"admission-order-check".into(),state:VoteState {finalized:anchor,votes:vec![]},
        };
        let mut votor = Votor::new(0,vec![100_000_000;12]).unwrap();
        votor.install_stakes(0,None,vec![100_000_000;12]).unwrap();
        votor.resume(anchor);
        let mut observer = Observer {
            checkpoints:BTreeMap::from([(anchor,replay.checkpoint())]),replay,anchor,
            keys:signers.iter().map(SigningKey::verifying_key).collect(),votor,signer,journal,identity,
            hub:"FRA".into(),outgoing:vec![],epoch_updates:BTreeMap::new(),reported_epoch:None,control:None,member:None,fast_lane:crate::external_proposer::FastLane::default(),member_metrics:crate::external_proposer::MemberMetrics::default(),certified:BTreeSet::from([anchor]),window_notices:BTreeMap::new(),bridge:None,
            slots:BTreeMap::new(),witnesses:BTreeMap::new(),finalized:BTreeMap::new(),seen:BTreeMap::new(),
            pending:BTreeMap::new(),checked:0,buffered_bytes:0,
        };
        let packet = |from:usize,key:&SigningKey,slot:u64,body:Wire| {
            let signature = crypto::sign_bytes(key,&bincode::serialize(&body).unwrap());
            crate::external::Packet {from,to:0,slot:slot*4,payload:bincode::serialize(&Envelope {from,body,signature}).unwrap()}
        };
        let signed_vote = |key:&SigningKey,validator,slot,kind| {
            let vote = Vote {validator,slot,hash:Some([9;32]),kind};
            let signature = crypto::sign_bytes(key,&crate::types::vote_message(&vote,None).unwrap());
            SignedVote {vote,signature}
        };
        let recipients = BTreeSet::from([0]);
        // Reproduce the stand's first boundary: legacy votes run ahead of the newly
        // replayed bootstrap marker. Its identical inherited set must stay immutable.
        let vote = signed_vote(&signers[0],0,128,VoteKind::Notar);
        observer.packet(packet(0,&signers[0],128,Wire::Vote(vote)),&recipients).unwrap();
        let mut catalog = signers.iter().map(|key|Pubkey::new_from_array(key.verifying_key().to_bytes())).collect::<Vec<_>>();
        let bootstrap = AdmissionSnapshot {epoch:2,cutoff_slot:512,members:catalog.iter().enumerate().map(|(index,identity)|AdmissionMember {
            identity:*identity,hub:config.hubs[index/4].code.clone(),stake:100_000_000,admitted_at_epoch:0,
        }).collect()};
        observer.replay.proposers.as_mut().unwrap().learn_admission(bootstrap,catalog.clone()).unwrap();
        observer.learn_finalized().unwrap();
        assert_eq!(observer.votor.sizes()[1],1);

        // The next epoch's outside sender/catalog and weights are still unknown. Hold
        // its votes and a certificate without counting any against the bootstrap set.
        let votes = (0..8).map(|index|signed_vote(&signers[index],index,192,VoteKind::Finalize))
            .chain(std::iter::once(signed_vote(&observer.signer,12,192,VoteKind::Finalize))).collect::<Vec<_>>();
        for signed in &votes {
            let from = signed.vote.validator;
            let signer = if from == 12 {&observer.signer} else {&signers[from]};
            observer.packet(packet(from,signer,192,Wire::Vote(signed.clone())),&recipients).unwrap();
        }
        let certificate = Certificate {kind:CertificateKind::Finalization,slot:192,hash:Some([9;32]),votes:votes.iter().map(|vote|vote.vote.clone()).collect()};
        observer.packet(packet(0,&signers[0],192,Wire::Certificate {certificate,proofs:votes}),&recipients).unwrap();
        assert_eq!(observer.pending.len(),10);
        assert_eq!(observer.votor.sizes()[1],1);

        // Finalized replay reaches epoch 2's midpoint, before epoch 3 starts. The
        // exact hashed snapshot is the sole source of the newly installed voter set.
        catalog.push(Pubkey::new_from_array(observer.signer.verifying_key().to_bytes()));
        let snapshot = AdmissionSnapshot {epoch:3,cutoff_slot:640,members:catalog.iter().enumerate().map(|(index,identity)|AdmissionMember {
            identity:*identity,hub:if index==12 {"FRA".into()} else {config.hubs[index/4].code.clone()},
            stake:if index==12 {2_000_000_000} else {1_000_000_000},admitted_at_epoch:if index==12 {3} else {0},
        }).collect()};
        observer.replay.proposers.as_mut().unwrap().learn_admission(snapshot.clone(),catalog).unwrap();
        observer.learn_finalized().unwrap();
        assert_eq!(observer.votor.own_at(192),Some(12));
        assert_eq!(observer.votor.stakes_at(192),snapshot.members.iter().map(|member|member.stake).collect::<Vec<_>>());
        assert!(observer.drain_pending().unwrap());
        assert!(observer.pending.is_empty());
        assert_eq!(observer.buffered_bytes,0);
        assert_eq!(observer.votor.sizes()[1],10); // one old vote plus nine new identities
        assert!(observer.votor.sizes()[2] > 0);
        assert!((observer.finalized[&192].share - 10.0/14.0).abs() < f64::EPSILON);
    }
}
