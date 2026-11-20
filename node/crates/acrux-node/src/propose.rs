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
use std::{collections::{BTreeMap, BTreeSet, VecDeque}, time::Duration};

const MAX_WIRE_BYTES: u64 = 1024 * 1024;
// Proposer payloads use 38,000-byte chunks and early blocks append one empty final slice.
const MAX_BLOCK_SLICES: u64 = MAX_WIRE_BYTES.div_ceil(38_000) + 1;
const MAX_CIPHER_BYTES: u64 = MAX_WIRE_BYTES + 16 * MAX_BLOCK_SLICES;
// Match the follower's two-minute data window. A stalled observer must bootstrap again.
const MAX_PENDING_SLOTS: usize = 2400;
const MAX_SEEN_ENVELOPES: usize = 100_000;
const MAX_PENDING_MATERIAL_BYTES: usize = 64 * 1024 * 1024;

type Accepted = crate::cblock_format::Accepted;

const AUTHORED_SLOTS: TableDefinition<&str, u64> = TableDefinition::new("authored_through");
const VOTE_JOURNALS: TableDefinition<&str, &[u8]> = TableDefinition::new("consensus_votes");
const LANE_VOTE_JOURNALS: TableDefinition<&str, &[u8]> = TableDefinition::new("lane_consensus_votes");
#[derive(Serialize, Deserialize)]
struct KeptVote { vote: crate::consensus::Vote, envelope: Vec<u8> }
#[derive(Serialize, Deserialize)]
struct VoteState { finalized: BlockRef, votes: Vec<KeptVote> }
#[derive(Serialize, Deserialize)]
struct KeptLaneVote { lane: usize, vote: crate::consensus::Vote, envelope: Vec<u8> }
struct VoteJournal { db: Database, chain: String, state: VoteState, lane_votes: Vec<KeptLaneVote> }
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
        let lane_votes = {
            let table = write.open_table(LANE_VOTE_JOURNALS)?;
            let value = table.get(chain.as_str())?;
            value.map(|value|decode(value.value())).transpose()?.unwrap_or_default()
        };
        write.commit()?;
        ensure!(anchor.slot >= state.finalized.slot && (anchor.slot != state.finalized.slot || anchor.hash == state.finalized.hash),
            "Snapshot is older than or conflicts with this identity's finalized vote journal");
        let mut journal = Self {db,chain,state,lane_votes};
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
    fn existing_lane(&self, lane: usize, vote: &crate::consensus::Vote) -> Result<Option<Vec<u8>>> {
        use crate::consensus::VoteKind;
        ensure!((1..=3).contains(&lane) && vote.slot > self.state.finalized.slot, "Refusing a lane vote at or below the durable finalized anchor");
        for kept in self.lane_votes.iter().filter(|kept|kept.lane == lane && kept.vote.slot == vote.slot) {
            let old = &kept.vote;
            ensure!(old.validator == vote.validator, "This identity's epoch lane index changed after signing");
            let initial = matches!(old.kind,VoteKind::Notar|VoteKind::Skip) && matches!(vote.kind,VoteKind::Notar|VoteKind::Skip);
            let final_phase = old.kind == VoteKind::Finalize && vote.kind == VoteKind::Finalize;
            ensure!(!(initial || final_phase) || (old.kind == vote.kind && old.hash == vote.hash), "Refusing to equivocate against this identity's durable lane vote journal");
            if old == vote { return Ok(Some(kept.envelope.clone())) }
        }
        Ok(None)
    }
    fn record_lane(&mut self, lane: usize, vote: crate::consensus::Vote, envelope: Vec<u8>) -> Result<()> {
        ensure!(self.existing_lane(lane,&vote)?.is_none(), "Lane vote was already durably signed");
        ensure!(envelope.len() <= 2048 && self.lane_votes.len() < MAX_PENDING_SLOTS * 3, "Unfinalized lane vote journal exceeded its bound");
        self.lane_votes.push(KeptLaneVote {lane,vote,envelope});
        self.save()
    }
    fn finalize(&mut self, anchor: BlockRef) -> Result<()> {
        ensure!(anchor.slot >= self.state.finalized.slot && (anchor.slot != self.state.finalized.slot || anchor.hash == self.state.finalized.hash), "Finalized vote journal moved backward or changed its hash");
        self.state.finalized = anchor;
        self.state.votes.retain(|kept|kept.vote.slot > anchor.slot);
        self.lane_votes.retain(|kept|kept.vote.slot > anchor.slot);
        self.save()
    }
    fn save(&self) -> Result<()> {
        let bytes = bincode::serialize(&self.state)?;
        ensure!(bytes.len() as u64 <= MAX_WIRE_BYTES, "Unfinalized vote journal exceeded its byte bound");
        let lane_bytes = bincode::serialize(&self.lane_votes)?;
        ensure!(lane_bytes.len() as u64 <= MAX_WIRE_BYTES, "Unfinalized lane vote journal exceeded its byte bound");
        let mut write = self.db.begin_write()?;
        write.set_durability(Durability::Immediate);
        { write.open_table(VOTE_JOURNALS)?.insert(self.chain.as_str(),bytes.as_slice())?; }
        { write.open_table(LANE_VOTE_JOURNALS)?.insert(self.chain.as_str(),lane_bytes.as_slice())?; }
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
    lane_equivocation: BTreeSet<usize>,
}
struct Witness { wire: CblockWitness, accepted: Vec<Accepted>, lane_hashes:Option<Vec<Option<[u8;32]>>> }
#[derive(Clone)]
struct Finalized { block: BlockRef, kind: &'static str, share: f64, threshold: f64 }
struct Observer {
    replay: cli::Replay,
    anchor: BlockRef,
    keys: Vec<VerifyingKey>,
    votor: Votor,
    lanes: crate::lanes::Lanes,
    lane_floor: u64,
    lane_windows: BTreeMap<u64,std::time::Instant>,
    lane_rounds: BTreeMap<u64,crate::lanes::Round>,
    lane_proposed: BTreeMap<(usize,u64),crate::consensus::Block>,
    lane_voted: BTreeSet<(usize,u64)>,
    lane_certificates: BTreeMap<(usize,u64,CertificateKind,Option<[u8;32]>),(Certificate,Vec<SignedVote>)>,
    signer: SigningKey,
    journal: VoteJournal,
    identity: String,
    hub: String,
    checkpoints: BTreeMap<BlockRef, cli::ReplayCheckpoint>,
    execution_registered: BTreeSet<BlockRef>,
    outgoing: Vec<Vec<u8>>,
    epoch_updates: BTreeMap<u64, (crate::execution::AdmissionSnapshot, Vec<solana_sdk::pubkey::Pubkey>)>,
    reported_epoch: Option<u64>,
    control: Option<std::sync::Arc<crate::member_link::MemberLink>>,
    member: Option<crate::external_proposer::MemberProposer>,
    fast_lane: crate::external_proposer::FastLane,
    member_metrics: crate::external_proposer::MemberMetrics,
    certified: BTreeSet<BlockRef>,
    window_notices: BTreeMap<u64,(Option<BlockRef>,u64)>,
    abandoned_windows: BTreeSet<u64>,
    execution_parents: BTreeMap<u64,Option<BlockRef>>,
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
        let mut actions=VecDeque::from(actions);
        while let Some(action)=actions.pop_front() {
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
                crate::consensus::Action::ParentReady {window_first_slot,parent}=>{
                    let window=window_first_slot/4;
                    if let Some(known)=self.execution_parents.insert(window,parent) {ensure!(known==parent,"Execution consensus changed its selected window parent");}
                    if self.lanes_active(window_first_slot) && self.window_notices.get(&window).is_some_and(|(announced,_)|*announced!=parent) {
                        actions.extend(self.abandon_window(window)?);
                    }
                }
                // A locally formed certificate may still include our unsent queued vote. Wait
                // for the node's signed certificate before advancing the durable final anchor.
                crate::consensus::Action::SendCertificate(_) => {},
                _ => {},
            }
        }
        Ok(())
    }
    fn abandon_window(&mut self,window:u64)->Result<Vec<crate::consensus::Action>> {
        let mut actions=vec![];
        if self.abandoned_windows.insert(window) {
            // Never replace signed proposals: the ordinary Skip chain repairs a
            // provisional parent or lane selection that did not become canonical.
            for cblock in window*4..window*4+4 {
                if cblock>self.anchor.slot {actions.extend(self.votor.timeout(cblock)?);}
            }
        }
        Ok(actions)
    }
    fn lanes_active(&self, cblock:u64)->bool { cblock.checked_mul(4).is_some_and(|slot|self.replay.lanes_active(slot)) }
    fn lane_actions(&mut self, actions:Vec<(usize,crate::consensus::Action)>)->Result<()> {
        for (lane,action) in actions {
            if let crate::consensus::Action::SendVote(vote)=action {
                ensure!(vote.slot >= self.lane_floor && self.lanes.own_at(vote.slot)==Some(vote.validator), "Lane vote is outside this member's resumed window");
                if matches!(vote.kind,crate::consensus::VoteKind::Notar|crate::consensus::VoteKind::Skip) {self.lane_voted.insert((lane,vote.slot));}
                if let Some(envelope)=self.journal.existing_lane(lane,&vote)? {self.outgoing.push(envelope);continue}
                let signature=crypto::sign_bytes(&self.signer,&crate::types::lane_vote_message(lane,&vote,self.bridge.as_ref())?);
                let body=Wire::LaneVote {lane,signed:SignedVote {vote:vote.clone(),signature}};
                let from=self.replay.proposers.as_ref().context("Missing proposer configuration")?.catalog.iter().position(|identity|identity==&self.identity).context("Admitted member absent from catalog")?;
                let signature=crypto::sign_bytes(&self.signer,&bincode::serialize(&body)?);
                let envelope=bincode::serialize(&Envelope {from,body,signature})?;
                self.journal.record_lane(lane,vote.clone(),envelope.clone())?;
                self.outgoing.push(envelope);
                println!("lane vote {:?} proposer {lane} cblock {} epoch {}",vote.kind,vote.slot,self.epoch(vote.slot));
            }
        }
        Ok(())
    }
    fn verify_lane_vote(&self,lane:usize,signed:&SignedVote)->Result<()> {
        ensure!((1..=3).contains(&lane) && self.lanes_active(signed.vote.slot), "Lane vote before activation or invalid proposer");
        crypto::verify_bytes(self.vote_identity(signed.vote.slot,signed.vote.validator)?,&crate::types::lane_vote_message(lane,&signed.vote,self.bridge.as_ref())?,&signed.signature)
    }
    fn sealed_lane_hash(&self,slot:u64,lane:usize)->Result<Option<[u8;32]>> {
        let Some(state)=self.slots.get(&slot) else{return Ok(None)};
        let Some((owner,_))=state.attestations.get(&lane) else{return Ok(None)};
        let piles=state.slices.iter().filter(|((role,_),_)|*role==lane).map(|(_,pile)|pile).collect::<Vec<_>>();
        let Some(last)=piles.last() else{return Ok(None)};
        if !last.meta.final_slice || piles.iter().any(|pile|pile.cipher.is_none()) {return Ok(None)}
        ensure!(piles.iter().enumerate().all(|(index,pile)|pile.meta.slice==index as u32 && pile.meta.final_slice==(index+1==piles.len())), "Noncontiguous lane slices");
        let parent=last.meta.parent.map(|(slot,hash)|BlockRef {slot,hash});
        let expected=if slot%16==0 {self.window_notices.get(&(slot/16)).context("Missing lane window anchor")?.0}else{None};
        ensure!(parent==expected,"Lane block has a stale execution parent");
        let mut roots=vec![];
        for pile in &piles {
            ensure!(crypto::hash(&owner.key)==pile.meta.key_hash,"Released lane key mismatch");
            crypto::decrypt_slice(&owner.key,slot,lane,pile.meta.slice,pile.cipher.as_ref().unwrap())?;
            roots.push(pile.root);
        }
        Ok(Some(crypto::hash(&bincode::serialize(&crate::types::BlockCommitment {slot,proposer:lane,key_hash:last.meta.key_hash,parent,roots})?)))
    }
    fn try_content(&mut self)->Result<()> {
        let windows=self.lane_windows.iter().map(|(window,start)|(*window,*start)).collect::<Vec<_>>();
        for (window,start) in windows {
            if self.abandoned_windows.contains(&window) {continue}
            for cblock in window*4..window*4+4 {for lane in 1..=3 {
            if self.lanes.decision(lane,cblock).is_some() || self.lane_proposed.contains_key(&(lane,cblock)) || self.lane_voted.contains(&(lane,cblock)) {continue}
            let mut skip=false;
            if cblock%4!=0 {match self.lanes.decision(lane,cblock-1).or_else(||self.lanes.readiness(lane,cblock-1)) {None=>continue,Some(None)=>skip=true,Some(Some(_))=>{}}}
            if !skip && (cblock*4..cblock*4+4).any(|slot|self.lane_rounds.get(&slot).is_some_and(|round|round.missing(lane))) {skip=true}
            let mut hashes=vec![];
            let mut pending=false;
            if !skip {for slot in cblock*4..cblock*4+4 {
                if self.slots.get(&slot).is_some_and(|state|state.lane_equivocation.contains(&lane)) {skip=true;break}
                let Some(inputs)=self.lane_rounds.get_mut(&slot).and_then(|round|round.snapshot(std::time::Instant::now())) else{pending=true;break};
                let Some(expected)=inputs.hashes[lane-1] else{skip=true;break};
                match self.sealed_lane_hash(slot,lane) {Ok(Some(actual)) if actual==expected=>hashes.push(actual),Ok(None)=>{pending=true;break},_=>{skip=true;break}}
            }}
            let expired=std::time::Instant::now().saturating_duration_since(start).as_micros()>=u128::from(1_000_000+(cblock%4)*200_000);
            if pending && !expired {continue}
            let actions=if skip || (pending&&expired) {self.lanes.timeout(lane,cblock)?} else {
                let parent=self.lanes.parent(lane,cblock);
                let hashes:[crate::consensus::Hash;4]=hashes.try_into().map_err(|_|anyhow::anyhow!("Incomplete lane cblock"))?;
                let hash=crate::cblock_format::lane_hash(lane,cblock,parent,&hashes)?;
                let block=crate::consensus::Block {id:BlockRef {slot:cblock,hash},parent};
                self.lane_proposed.insert((lane,cblock),block.clone());
                self.lanes.local_block(lane,block)?
            };
            self.lane_actions(actions)?;
        }}}
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
                self.votor.install_stakes(from,own,stakes.clone())?;
                self.lanes.install_stakes(from,own,stakes)?;
            }
            if self.lanes.stakes_at(from) != self.votor.stakes_at(from) || self.lanes.own_at(from) != own {self.lanes.install_stakes(from,own,self.votor.stakes_at(from).to_vec())?;}
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
                state.lane_equivocation.insert(shred.meta.proposer);
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
    fn attest(&mut self, attestation: Attestation, signature: Vec<u8>, from: Option<usize>, received_at:std::time::Instant) -> Result<()> {
        if attestation.slot / 4 <= self.anchor.slot { return Ok(()) }
        let id = self.proposer(attestation.slot, attestation.proposer)?;
        ensure!(from.is_none_or(|from| from == id), "Attestation sent by another proposer");
        ensure!(attestation.pi.len() == 3, "Invalid attestation vector");
        if self.slots.get(&attestation.slot).and_then(|slot| slot.attestations.get(&attestation.proposer))
            .is_some_and(|(known, _)| known.pi == attestation.pi && known.key == attestation.key) { return Ok(()) }
        crypto::verify_bytes(&self.keys[id], &bincode::serialize(&Wire::Attest(attestation.clone()))?, &signature)
            .context("Invalid attestation signature")?;
        if self.lanes_active(attestation.slot/4) {self.lane_rounds.entry(attestation.slot).or_default().record(attestation.clone(),received_at);}
        let state = self.slots.entry(attestation.slot).or_default();
        if let Some((known, _)) = state.attestations.get(&attestation.proposer) {
            if known.pi != attestation.pi || known.key != attestation.key {
                state.equivocation = true;
                if known.key!=attestation.key {state.lane_equivocation.insert(attestation.proposer);}
                for (column,(before,after)) in known.pi.iter().zip(&attestation.pi).enumerate() {if before!=after {state.lane_equivocation.insert(column+1);}}
            }
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
            if known.hash != commitment.hash || known.key_hash != commitment.key_hash { state.equivocation = true; state.lane_equivocation.insert(commitment.proposer); }
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
        let parsed=crate::cblock_format::decode(&wire.preimage,self.lanes_active(wire.block.slot))?;
        let (slot,parent,accepted,state,lane_hashes)=(parsed.cblock,parsed.parent,parsed.accepted,parsed.state,parsed.lane_hashes);
        ensure!(slot == wire.block.slot && parent == wire.parent, "Cblock witness names another preimage");
        ensure!(parent.is_some_and(|parent| parent.slot < slot), "Cblock parent does not precede its child");
        ensure!(slot <= (u64::MAX - 3) / 4 && accepted.len() == 4 && wire.state_hashes.len() == 4, "Invalid cblock slot range");
        for (offset, (execution_slot, agreed, pi)) in accepted.iter().enumerate() {
            ensure!(*execution_slot == slot * 4 + offset as u64 && pi.len() == 3, "Invalid accepted-slot preimage");
            let roles = agreed.as_ref().context("Cblock contains a slot without agreement")?;
            ensure!(roles.len() >= (if lane_hashes.is_some(){1}else{2}) && roles.len() <= 3 && roles.windows(2).all(|pair| pair[0] < pair[1])
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
        } else { self.witnesses.insert(wire.block, Witness { wire, accepted, lane_hashes }); }
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
            self.try_content()?;
            self.speculate()?;
            self.register_ready()?;
            self.replay_ready()?;
            if !self.drain_pending()? { return Ok(()) }
        }
    }
    fn parent_ready(&self,parent:BlockRef)->bool {
        if self.certified.contains(&parent)||parent==self.anchor {return true}
        // Content certificates may start the clock before execution notarization.
        // The actual parent still needs its locally reproduced state and outbox.
        let base_certified=self.window_notices.get(&(parent.slot/4)).is_some_and(|(base,_)|base.is_none_or(|base|self.certified.contains(&base)||base==self.anchor));
        parent.slot%4==3 && self.lanes_active(parent.slot) && base_certified && self.checkpoints.contains_key(&parent)
            && self.witnesses.get(&parent).and_then(|witness|witness.lane_hashes.as_ref())
                .is_some_and(|hashes|self.lanes.decisions(parent.slot).or_else(||self.lanes.ready(parent.slot)).is_some_and(|ready|ready.hashes==*hashes))
    }
    fn start_windows(&mut self)->Result<()> {
        let notices=self.window_notices.iter().filter(|(number, (parent,_))|!self.abandoned_windows.contains(*number)&&parent.is_some_and(|parent|self.parent_ready(parent)))
            .map(|(number,(parent,unix_us))|(*number,*parent,*unix_us)).collect::<Vec<_>>();
        for (number,parent,unix_us) in &notices {
            if !self.lane_windows.contains_key(number) && number*4 >= self.lane_floor && self.lanes_active(number*4) {
                let start=crate::external_proposer::host_instant(*unix_us)?;
                self.lanes.ensure_window(number*4,*parent)?;
                self.lane_windows.insert(*number,start);
            }
        }
        let Some(proposers)=&self.replay.proposers else{return Ok(())};
        let Some(member)=&mut self.member else{return Ok(())};
        for (number,parent,unix_us) in notices {
            if self.votor.own_at(number*4).is_none() {continue}
            let window=proposers.window(number)?;
            if window.hub_code!=self.hub||!window.proposers.contains(&self.identity){continue}
            let ids=proposers.ids(number)?;
            let (identities,stakes)=proposers.voting_set(number*4)?;
            let view=crate::view::window_json(number,&window,&identities,&stakes,None,&proposers.config.identities);
            member.start_window(number,parent,unix_us,ids,view,proposers.catalog.clone())?;
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
        self.start_windows()?;
        self.try_content()?;
        self.speculate()?;
        self.register_ready()?;
        self.replay_ready()?;
        if let Some(member)=&mut self.member {
            if member.current_slot().is_some_and(|slot|self.abandoned_windows.contains(&(slot/16))) {return Ok(())}
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
            Wire::LaneVote{signed,..}=>{ensure!(self.lanes_active(signed.vote.slot),"Lane frame before activation");known(signed.vote.slot)&&(signed.vote.slot<self.lane_floor||self.lane_windows.contains_key(&(signed.vote.slot/4)))},
            Wire::LaneCertificate{certificate,..}=>{ensure!(self.lanes_active(certificate.slot),"Lane frame before activation");known(certificate.slot)&&(certificate.slot<self.lane_floor||self.lane_windows.contains_key(&(certificate.slot/4)))},
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
            Wire::LaneVote{lane,signed}=>{
                self.verify_lane_vote(lane,&signed)?;
                ensure!(self.keys[envelope.from]==*self.vote_identity(signed.vote.slot,signed.vote.validator)?,"Lane envelope belongs to another epoch member");
                if self.lane_windows.contains_key(&(signed.vote.slot/4)) {
                    let actions=self.lanes.receive(lane,crate::consensus::Message::Vote(signed.vote))?;
                    self.lane_actions(actions)?;
                }
            },
            Wire::LaneCertificate{lane,certificate,proofs}=>{
                ensure!((1..=3).contains(&lane)&&self.lanes_active(certificate.slot),"Lane certificate before activation");
                self.votor.validate_certificate(&certificate)?;
                ensure!(proofs.len()==certificate.votes.len(),"Lane certificate omitted signatures");
                for (vote,proof) in certificate.votes.iter().zip(&proofs) {ensure!(vote==&proof.vote,"Lane certificate signature belongs to another vote");self.verify_lane_vote(lane,proof)?;}
                self.lane_certificates.insert((lane,certificate.slot,certificate.kind,certificate.hash),(certificate.clone(),proofs));
                if self.lane_windows.contains_key(&(certificate.slot/4)) {
                    let actions=self.lanes.receive(lane,crate::consensus::Message::Certificate(certificate))?;
                    self.lane_actions(actions)?;
                }
            },
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
            Wire::Attest(attestation) => self.attest(attestation, envelope.signature, Some(envelope.from),received_at)?,
            Wire::Vote(vote) => {
                self.verify_vote(&vote)?;
                ensure!(self.keys[envelope.from] == *self.vote_identity(vote.vote.slot,vote.vote.validator)?, "Vote envelope belongs to another epoch member");
                let actions = self.votor.receive(crate::consensus::Message::Vote(vote.vote))?;
                self.actions(actions)?;
            },
            Wire::Certificate { certificate, proofs } => self.certificate(certificate, proofs)?,
            Wire::Handover { shreds, attestations, .. } => {
                for shred in shreds { self.shred(shred)?; }
                for (attestation, signature) in attestations { self.attest(attestation, signature, None,received_at)?; }
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
                proposers.window(number)?;
                ensure!(envelope.from==proposers.window_representative(number)?,"Window notice is not from the fixed hub representative");
                if let Some(known)=self.window_notices.get(&number) {ensure!(*known==(parent,started_at_unix_us),"Conflicting window notice");}
                    self.window_notices.insert(number,(parent,started_at_unix_us));
                // A certificate may precede its notice on the pull stream. Reapply
                // that already authenticated local notification before any startup.
                if let Some(parent)=self.execution_parents.get(&number).copied() {
                    self.actions(vec![crate::consensus::Action::ParentReady {window_first_slot:number*4,parent}])?;
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
        let lane_mode=self.lanes_active(slot/4);
        ensure!(if lane_mode {agreed.as_ref().is_some_and(|roles|roles.iter().all(|role|!state.lane_equivocation.contains(role)))}else{!state.equivocation}, "Finalized slot {slot} contains proposer equivocation");
        let mut attestations = vec![];
        let mut blocks = vec![];
        for role in agreed.as_ref().context("No agreed set")? {
            let Some((attestation, signature)) = state.attestations.get(role) else { return Ok(None) };
            if !lane_mode {ensure!(&attestation.pi == pi, "Slot {slot}'s attestation differs from the cblock preimage");}
            ensure!(attestation.pi[*role-1]==pi[*role-1],"Accepted lane commitment differs from its owner attestation");
            if let Some(commitment) = state.commitments.get(role) {
                ensure!(Some(commitment.hash) == pi[*role - 1] && commitment.key_hash == crypto::hash(&attestation.key), "Slot {slot}'s signed commitment differs from its attestation");
            }
            let piles = state.slices.iter().filter(|((proposer, _), _)| proposer == role).map(|(_, pile)| pile).collect::<Vec<_>>();
            let Some(last) = piles.last() else { return Ok(None) };
            if !last.meta.final_slice || piles.iter().any(|pile| pile.cipher.is_none()) { return Ok(None) }
            ensure!(piles.iter().enumerate().all(|(i, pile)| pile.meta.slice == i as u32 && (pile.meta.final_slice == (i + 1 == piles.len()))), "Slot {slot} has a noncontiguous slice list");
            ensure!(piles.iter().map(|pile| pile.meta.encrypted_len as u64).sum::<u64>() <= MAX_CIPHER_BYTES, "Block exceeds the payload bound");
            attestations.push(json!({"proposer":role,"pi":attestation.pi.iter().map(|hash|hash.map(hex::encode)).collect::<Vec<_>>(),"key":B64.encode(attestation.key),"signature":bs58::encode(signature).into_string()}));
            blocks.push(json!({"proposer":role,"slices":piles.iter().map(|pile|json!({"meta":pile.meta,"root":hex::encode(pile.root),"cipher":B64.encode(pile.cipher.as_ref().unwrap())})).collect::<Vec<_>>()}));
        }
        Ok(Some(json!({"attestations":attestations,"blocks":blocks,"pi":pi.iter().map(|hash|hash.map(hex::encode)).collect::<Vec<_>>()})))
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
                if self.abandoned_windows.contains(&(block.slot/4)) {continue}
                if let Some(hashes)=&witness.lane_hashes {
                    if block.slot>=self.lane_floor {
                        if self.lanes.decisions(block.slot).or_else(||self.lanes.ready(block.slot)).is_none_or(|decision|decision.hashes!=*hashes) {continue}
                    } else {
                        // A reconnect inside a window does not vote content from an incomplete
                        // history. Its already-finalized execution certificate authenticates the
                        // barrier, while signed lane certificates authenticate each content hash.
                        if !self.finalized.contains_key(&block.slot) {continue}
                        let complete=hashes.iter().enumerate().all(|(index,hash)|self.lane_certificates.keys().any(|(lane,slot,kind,known)|*lane==index+1&&*slot==block.slot&&known==hash&&if hash.is_none(){*kind==CertificateKind::Skip}else{*kind!=CertificateKind::Skip}));
                        if !complete {continue}
                    }
                }
                let Some(checkpoint) = self.checkpoints.get(&parent) else { continue };
                let blocks = witness.accepted.iter().map(|accepted|self.slot_blocks(accepted)).collect::<Result<Vec<_>>>()?;
                if blocks.iter().any(Option::is_none) { continue }
                ready = Some((*block,checkpoint.clone(),blocks));
                break;
            }
            let Some((block,checkpoint,blocks)) = ready else { return Ok(()) };
            self.replay.restore(checkpoint)?;
            self.status(block.slot);
            for (offset,blocks) in blocks.into_iter().enumerate() {
                let blocks = blocks.unwrap();
                let (view,recipients) = self.view(block,offset,None)?;
                cli::check_slot_agreement(&view,&blocks["attestations"],self.lanes_active(block.slot).then_some(&blocks["pi"]))?;
                let slot = block.slot * 4 + offset as u64;
                let state = self.replay.apply(slot,&recipients,&blocks).with_context(||format!("re-executing slot {slot}"))?;
                ensure!(view["stateHash"].as_str() == Some(state.as_str()), "Slot {slot}: re-executed state differs from the signed cblock");
            }
            self.outbox(block)?;
            self.checkpoints.insert(block,self.replay.checkpoint());
            if !self.lanes_active(block.slot) {
                let parent=self.witnesses[&block].wire.parent;
                let actions=self.votor.local_block(crate::consensus::Block {id:block,parent})?;
                self.execution_registered.insert(block);
                self.actions(actions)?;
            }
            // This checkpoint can start the next window, but execution consensus
            // still waits for actual final content decisions, exactly as in R23.
        }
    }
    fn discard_speculation(&mut self,block:BlockRef)->Result<()> {
        let parent=self.witnesses[&block].wire.parent.context("Speculative cblock lost its parent")?;
        let restore=self.checkpoints.get(&parent).context("Speculative parent checkpoint disappeared")?.clone();
        let mut discarded=BTreeSet::from([block]);
        loop {
            let descendants=self.witnesses.iter().filter(|(candidate,witness)|self.checkpoints.contains_key(candidate)&&!discarded.contains(candidate)&&witness.wire.parent.is_some_and(|parent|discarded.contains(&parent)))
                .map(|(candidate,_)|*candidate).collect::<Vec<_>>();
            if descendants.is_empty() {break}
            discarded.extend(descendants);
        }
        for candidate in &discarded {
            ensure!(!self.journal.state.votes.iter().any(|kept|kept.vote.slot==candidate.slot&&kept.vote.hash==Some(candidate.hash)&&matches!(kept.vote.kind,crate::consensus::VoteKind::Notar|crate::consensus::VoteKind::Finalize)), "A finalized lane repair conflicts with an execution vote already signed");
        }
        self.checkpoints.retain(|candidate,_|!discarded.contains(candidate));
        self.execution_registered.retain(|candidate|!discarded.contains(candidate));
        self.replay.restore(restore)?;
        // Keep the current window and every signed vote intact. Only children
        // already announced on a discarded execution parent must be skipped.
        let children=self.window_notices.iter().filter(|(_, (parent,_))|parent.is_some_and(|parent|discarded.contains(&parent)))
            .map(|(window,_)|*window).collect::<Vec<_>>();
        for window in children {
            let actions=self.abandon_window(window)?;
            self.actions(actions)?;
        }
        Ok(())
    }
    fn register_ready(&mut self)->Result<()> {
        let candidates=self.witnesses.iter().filter(|(block,_)|self.checkpoints.contains_key(block)&&!self.execution_registered.contains(block))
            .map(|(block,witness)|(*block,witness.wire.parent,witness.lane_hashes.clone())).collect::<Vec<_>>();
        for (block,parent,hashes) in candidates {
            if !self.checkpoints.contains_key(&block) {continue}
            if self.abandoned_windows.contains(&(block.slot/4)) {continue}
            if let Some(hashes)=hashes {
                if block.slot>=self.lane_floor {
                    let Some(decisions)=self.lanes.decisions(block.slot) else{continue};
                    if decisions.hashes!=hashes {
                        self.discard_speculation(block)?;
                        continue;
                    }
                }
            }
            let actions=self.votor.local_block(crate::consensus::Block {id:block,parent})?;
            self.execution_registered.insert(block);
            self.actions(actions)?;
        }
        Ok(())
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
                if let Some(checkpoint) = self.checkpoints.get(&block).filter(|_|self.execution_registered.contains(&block)) { ready = Some((block,finality.clone(),checkpoint.clone())); break }
            }
            let Some((block,finality,checkpoint)) = ready else { return Ok(()) };
            self.replay.restore(checkpoint)?;
            for offset in 0..4 {
                let accepted = &self.witnesses[&block].accepted[offset];
                let blocks = self.slot_blocks(accepted)?.context("Finalized block data disappeared")?;
                let (view,_) = self.view(block,offset,Some(&finality))?;
                cli::check_slot(&view,&blocks["attestations"],self.lanes_active(block.slot).then_some(&blocks["pi"]))?;
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
            self.execution_registered.retain(|known|known.slot >= block.slot);
            self.buffered_bytes -= self.slots.iter().filter(|(slot,_)| **slot / 4 <= block.slot).map(|(_,slot)|slot.buffered_bytes).sum::<usize>();
            self.slots.retain(|slot,_| *slot / 4 > block.slot);
            self.witnesses.retain(|known,_| known.slot > block.slot);
            self.finalized.retain(|slot,_| *slot > block.slot);
            self.seen.retain(|_,slot| *slot / 4 > block.slot);
            self.votor.prune(block.slot.saturating_sub(8));
            self.lanes.prune(block.slot.saturating_sub(8));
            self.lane_rounds.retain(|slot,_|*slot/4>block.slot);
            self.lane_proposed.retain(|(_,slot),_|*slot>block.slot);
            self.lane_voted.retain(|(_,slot)|*slot>block.slot);
            self.lane_certificates.retain(|(_,slot,_,_),_|*slot>block.slot);
            self.lane_windows.retain(|window,_|*window>=block.slot/4);
            self.certified.retain(|known|known.slot>=block.slot.saturating_sub(8));
            self.window_notices.retain(|number,_|*number>=block.slot/4);
            self.abandoned_windows.retain(|window|*window>=block.slot/4);
            self.execution_parents.retain(|window,_|*window>=block.slot/4);
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
    let mut observer = Observer {checkpoints:BTreeMap::from([(anchor,replay.checkpoint())]),execution_registered:BTreeSet::from([anchor]),replay,anchor,keys,votor,lanes:crate::lanes::Lanes::new(None,vec![100_000_000;12])?,lane_floor:(last/4+1)*4,lane_windows:BTreeMap::new(),lane_rounds:BTreeMap::new(),lane_proposed:BTreeMap::new(),lane_voted:BTreeSet::new(),lane_certificates:BTreeMap::new(),signer,journal,identity,hub:hub.to_owned(),outgoing:vec![],epoch_updates:BTreeMap::new(),reported_epoch:None,control:Some(link.clone()),member:None,fast_lane:fast_lane.clone(),member_metrics:member_metrics.clone(),certified:BTreeSet::from([anchor]),window_notices:BTreeMap::new(),abandoned_windows:BTreeSet::new(),execution_parents:BTreeMap::new(),bridge:genesis.bridge.clone(),slots:BTreeMap::new(),witnesses:BTreeMap::new(),finalized:BTreeMap::new(),seen:BTreeMap::new(),pending:BTreeMap::new(),checked:0,buffered_bytes:0};
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
                _=member_tick.tick()=>{observer.member_tick().await?;for envelope in observer.outgoing.drain(..){control.send_envelope(&envelope).await?;}},
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
            chain:"admission-order-check".into(),state:VoteState {finalized:anchor,votes:vec![]},lane_votes:vec![],
        };
        let mut votor = Votor::new(0,vec![100_000_000;12]).unwrap();
        votor.install_stakes(0,None,vec![100_000_000;12]).unwrap();
        votor.resume(anchor);
        let mut observer = Observer {
            checkpoints:BTreeMap::from([(anchor,replay.checkpoint())]),execution_registered:BTreeSet::from([anchor]),replay,anchor,
            keys:signers.iter().map(SigningKey::verifying_key).collect(),votor,lanes:crate::lanes::Lanes::new(None,vec![100_000_000;12]).unwrap(),lane_floor:128,lane_windows:BTreeMap::new(),lane_rounds:BTreeMap::new(),lane_proposed:BTreeMap::new(),lane_voted:BTreeSet::new(),lane_certificates:BTreeMap::new(),signer,journal,identity,
            hub:"FRA".into(),outgoing:vec![],epoch_updates:BTreeMap::new(),reported_epoch:None,control:None,member:None,fast_lane:crate::external_proposer::FastLane::default(),member_metrics:crate::external_proposer::MemberMetrics::default(),certified:BTreeSet::from([anchor]),window_notices:BTreeMap::new(),abandoned_windows:BTreeSet::new(),execution_parents:BTreeMap::new(),bridge:None,
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
