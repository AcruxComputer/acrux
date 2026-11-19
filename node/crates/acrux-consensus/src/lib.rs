//! Per-validator Votor state machine for NewCon consensus blocks.
//!
//! This module never manufactures stake or signatures. `Message` is the logical payload
//! inside the transport's signed envelope. Before calling `receive`, the caller must
//! verify the sender of a vote, and every signed vote embedded in a certificate. Keep
//! those original signatures when broadcasting `SendCertificate` actions.
//!
//! Initial notar/skip votes are exclusive. Notar-fallback is emitted once per
//! target after 40% initial notar stake, or after 20% initial notar stake and 60%
//! combined initial notar/skip stake. Skip-fallback needs 40% initial stake after
//! subtracting the largest notar target. Fallback targets are independent.
//! Certificates count distinct stake: 60% notar, notar plus notar-fallback, skip
//! plus skip-fallback, or final votes; fast finalization needs 80% initial notar.
//! Weak certificates can coexist. Only incompatible finalization is an error.
//!
//! BadWindow is interpreted per consensus slot: an own skip or fallback prevents
//! an own final vote for that slot. A final vote closes local fallback voting for
//! that slot, so eligibility cannot be invalidated by a later own fallback.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Bound::{Excluded, Unbounded};

pub type Hash = [u8; 32];
pub const CONSENSUS_SLOTS_PER_WINDOW: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BlockRef {
    pub slot: u64,
    pub hash: Hash,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    pub id: BlockRef,
    pub parent: Option<BlockRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoteKind {
    Notar,
    NotarFallback,
    Skip,
    SkipFallback,
    Finalize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vote {
    pub validator: usize,
    pub slot: u64,
    pub hash: Option<Hash>,
    pub kind: VoteKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificateKind {
    Notarization,
    NotarFallback,
    FastFinalization,
    Finalization,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    pub kind: CertificateKind,
    pub slot: u64,
    pub hash: Option<Hash>,
    pub votes: Vec<Vote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Message {
    Vote(Vote),
    Certificate(Certificate),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SendVote(Vote),
    SendCertificate(Certificate),
    Finalized(BlockRef),
    ParentReady {
        window_first_slot: u64,
        parent: Option<BlockRef>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsensusError(pub String);
impl fmt::Display for ConsensusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConsensusError {}
type Result<T> = std::result::Result<T, ConsensusError>;
type VoteKey = (u64, VoteKind, Option<Hash>);
type CertificateKey = (u64, CertificateKind, Option<Hash>);

#[derive(Clone, PartialEq, Eq)]
struct StakeSet { own: Option<usize>, stakes: Vec<u64>, total: u128 }

pub struct Votor {
    stake_sets: BTreeMap<u64, StakeSet>,
    blocks: BTreeMap<BlockRef, Block>,
    votes: BTreeMap<VoteKey, BTreeMap<usize, Vote>>,
    certificates: BTreeMap<CertificateKey, Certificate>,
    initial: BTreeMap<u64, Vote>,
    fallback: BTreeMap<(u64, Option<Hash>), Vote>,
    final_votes: BTreeMap<u64, Vote>,
    finalized: Option<BlockRef>,
    finalized_announced: BTreeSet<BlockRef>,
    pending_finalizations: BTreeSet<CertificateKey>,
    next_window: u64,
    started: bool,
    pruned_below: u64,
    /// The first window's first slot after a resume: the slots between the finalized cblock and it
    /// were never produced and count as skipped.
    floor: u64,
}

impl Votor {
    pub fn pruned_below(&self) -> u64 {
        self.pruned_below
    }

    /// Drops state for consensus slots below `below`, which lies under the finalized slot.
    pub fn prune(&mut self, below: u64) {
        let finalized = self.finalized;
        self.blocks.retain(|id, _| id.slot >= below || Some(*id) == finalized);
        self.votes = self.votes.split_off(&(below, VoteKind::Notar, None));
        self.certificates = self.certificates.split_off(&(below, CertificateKind::Notarization, None));
        self.initial = self.initial.split_off(&below);
        self.fallback = self.fallback.split_off(&(below, None));
        self.final_votes = self.final_votes.split_off(&below);
        self.finalized_announced.retain(|id| id.slot >= below);
        self.pending_finalizations.retain(|key| key.0 >= below);
        self.pruned_below = below;
    }

    /// Entry counts of the per-slot maps, for the run report's memory view.
    pub fn sizes(&self) -> [usize; 6] {
        [self.blocks.len(), self.votes.values().map(|v| v.len()).sum(), self.certificates.len(), self.initial.len(), self.fallback.len(), self.final_votes.len()]
    }
    pub fn new(own: usize, stakes: Vec<u64>) -> Result<Self> {
        if own >= stakes.len() || stakes.is_empty() || stakes.iter().any(|s| *s == 0) {
            return Err(ConsensusError(
                "validator set must contain positive stake for this validator".into(),
            ));
        }
        let total_stake = stakes.iter().map(|s| *s as u128).sum();
        Ok(Self {
            stake_sets: BTreeMap::from([(0, StakeSet { own: Some(own), stakes, total: total_stake })]),
            blocks: BTreeMap::new(),
            votes: BTreeMap::new(),
            certificates: BTreeMap::new(),
            initial: BTreeMap::new(),
            fallback: BTreeMap::new(),
            final_votes: BTreeMap::new(),
            finalized: None,
            finalized_announced: BTreeSet::new(),
            pending_finalizations: BTreeSet::new(),
            next_window: 0,
            started: false,
            pruned_below: 0,
            floor: 0,
        })
    }

    /// Installs the compact voter indices and weights from this consensus slot onward.
    /// Older slots keep their original set even after later epochs have been installed.
    pub fn install_stakes(&mut self, from: u64, own: Option<usize>, stakes: Vec<u64>) -> Result<()> {
        if stakes.is_empty() || stakes.iter().any(|stake| *stake == 0) || own.is_some_and(|own| own >= stakes.len()) {
            return Err(ConsensusError("validator set must contain positive stake and a valid own index".into()));
        }
        let total = stakes.iter().map(|stake| *stake as u128).sum();
        let set = StakeSet { own, stakes, total };
        if self.stake_sets.get(&from) == Some(&set) { return Ok(()) }
        if self.stake_sets.contains_key(&from) && !(from == 0 && !self.started && self.votes.is_empty()) {
            return Err(ConsensusError("an installed epoch validator set is immutable".into()));
        }
        if self.votes.keys().any(|(slot, _, _)| *slot >= from) {
            return Err(ConsensusError("cannot replace the validator set of an observed vote".into()));
        }
        self.stake_sets.insert(from, set);
        Ok(())
    }
    fn stake_set(&self, slot: u64) -> &StakeSet {
        self.stake_sets.range(..=slot).next_back().expect("genesis stake set").1
    }
    pub fn own_at(&self, slot: u64) -> Option<usize> { self.stake_set(slot).own }
    pub fn stakes_at(&self, slot: u64) -> &[u64] { &self.stake_set(slot).stakes }

    /// Call once at startup. Genesis is the sole parent that needs no certificate.
    pub fn genesis(&mut self) -> Vec<Action> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        self.next_window = CONSENSUS_SLOTS_PER_WINDOW;
        vec![Action::ParentReady {
            window_first_slot: 0,
            parent: None,
        }]
    }

    /// Call once at startup instead of `genesis`, to continue a chain from its finalized cblock:
    /// the next window builds on it.
    pub fn resume(&mut self, finalized: BlockRef) -> Vec<Action> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let first = (finalized.slot / CONSENSUS_SLOTS_PER_WINDOW + 1) * CONSENSUS_SLOTS_PER_WINDOW;
        self.blocks.insert(finalized, Block { id: finalized, parent: None });
        self.finalized = Some(finalized);
        self.floor = first;
        self.pruned_below = finalized.slot;
        self.next_window = first + CONSENSUS_SLOTS_PER_WINDOW;
        vec![Action::ParentReady {
            window_first_slot: first,
            parent: Some(finalized),
        }]
    }

    pub fn finalized(&self) -> Option<BlockRef> {
        self.finalized
    }

    /// Only call after NewCon validation and deterministic execution produced this cblock.
    pub fn local_block(&mut self, block: Block) -> Result<Vec<Action>> {
        self.check_block_shape(&block)?;
        if let Some(existing) = self.blocks.get(&block.id) {
            if existing != &block {
                return Err(ConsensusError(
                    "one block hash names different parents".into(),
                ));
            }
        } else {
            self.blocks.insert(block.id, block);
        }
        self.drive()
    }

    /// The NewCon validator supplies T(i) relative to its own ParentReady.
    pub fn timeout(&mut self, slot: u64) -> Result<Vec<Action>> {
        let mut actions = Vec::new();
        if let Some(own) = self.own_at(slot).filter(|_| !self.initial.contains_key(&slot)) {
            let vote = Vote {
                validator: own,
                slot,
                hash: None,
                kind: VoteKind::Skip,
            };
            self.initial.insert(slot, vote.clone());
            self.insert_vote(vote.clone())?;
            actions.push(Action::SendVote(vote));
        }
        actions.extend(self.drive()?);
        Ok(actions)
    }

    pub fn receive(&mut self, message: Message) -> Result<Vec<Action>> {
        let slot = match &message {
            Message::Vote(vote) => vote.slot,
            Message::Certificate(certificate) => certificate.slot,
        };
        if slot < self.pruned_below {
            return Ok(vec![]); // Settled long ago; nothing it says can change.
        }
        match message {
            Message::Vote(vote) => {
                self.insert_vote(vote)?;
            }
            Message::Certificate(certificate) => {
                self.validate_certificate(&certificate)?;
                // Check every item before storing any of this certificate's votes.
                for vote in &certificate.votes {
                    self.check_vote_against_known(vote)?;
                }
                for vote in &certificate.votes {
                    self.insert_vote(vote.clone())?;
                }
                self.install_certificate(certificate)?;
            }
        }
        self.drive()
    }

    /// Quorums count distinct identities at their configured stake; payload counts are irrelevant.
    pub fn validate_certificate(&self, certificate: &Certificate) -> Result<()> {
        if (certificate.kind == CertificateKind::Skip) != certificate.hash.is_none() {
            return Err(ConsensusError(
                "certificate hash does not match its kind".into(),
            ));
        }
        let mut signers = BTreeSet::new();
        let mut stake = 0u128;
        for vote in &certificate.votes {
            self.check_vote(vote)?;
            if vote.slot != certificate.slot
                || vote.hash != certificate.hash
                || !Self::certificate_accepts(certificate.kind, vote.kind)
            {
                return Err(ConsensusError(
                    "certificate contains a vote for another target or kind".into(),
                ));
            }
            if !signers.insert(vote.validator) {
                return Err(ConsensusError("duplicate validator in certificate".into()));
            }
            stake += self.stakes_at(certificate.slot)[vote.validator] as u128;
        }
        let percent = if certificate.kind == CertificateKind::FastFinalization {
            80
        } else {
            60
        };
        if !self.threshold(certificate.slot, stake, percent) {
            return Err(ConsensusError("certificate has insufficient stake".into()));
        }
        Ok(())
    }

    fn threshold(&self, slot: u64, stake: u128, percent: u128) -> bool {
        stake * 100 >= self.stake_set(slot).total * percent
    }
    fn certificate_accepts(certificate: CertificateKind, vote: VoteKind) -> bool {
        match certificate {
            CertificateKind::Notarization | CertificateKind::FastFinalization => {
                vote == VoteKind::Notar
            }
            CertificateKind::NotarFallback => {
                matches!(vote, VoteKind::Notar | VoteKind::NotarFallback)
            }
            CertificateKind::Finalization => vote == VoteKind::Finalize,
            CertificateKind::Skip => matches!(vote, VoteKind::Skip | VoteKind::SkipFallback),
        }
    }
    fn check_vote(&self, vote: &Vote) -> Result<()> {
        if vote.validator >= self.stakes_at(vote.slot).len() {
            return Err(ConsensusError("unknown validator".into()));
        }
        if matches!(vote.kind, VoteKind::Skip | VoteKind::SkipFallback) != vote.hash.is_none() {
            return Err(ConsensusError("vote hash does not match its kind".into()));
        }
        Ok(())
    }
    fn check_vote_against_known(&self, vote: &Vote) -> Result<()> {
        self.check_vote(vote)?;
        for ((slot, kind, hash), votes) in self.votes.range(
            (vote.slot, VoteKind::Notar, None)..=(vote.slot, VoteKind::Finalize, Some([255; 32])),
        ) {
            if *slot != vote.slot || !votes.contains_key(&vote.validator) {
                continue;
            }
            let initial_conflict = matches!(*kind, VoteKind::Notar | VoteKind::Skip)
                && matches!(vote.kind, VoteKind::Notar | VoteKind::Skip);
            let final_conflict = *kind == VoteKind::Finalize && vote.kind == VoteKind::Finalize;
            if (initial_conflict || final_conflict) && (*hash != vote.hash || *kind != vote.kind) {
                return Err(ConsensusError(
                    "validator equivocated in one voting phase".into(),
                ));
            }
        }
        Ok(())
    }
    fn insert_vote(&mut self, vote: Vote) -> Result<bool> {
        self.check_vote_against_known(&vote)?;
        let votes = self
            .votes
            .entry((vote.slot, vote.kind, vote.hash))
            .or_default();
        if votes.contains_key(&vote.validator) {
            return Ok(false);
        }
        votes.insert(vote.validator, vote);
        Ok(true)
    }
    fn check_block_shape(&self, block: &Block) -> Result<()> {
        match block.parent {
            Some(parent) if parent.slot >= block.id.slot => {
                return Err(ConsensusError("block parent must precede it".into()))
            }
            Some(parent)
                if block.id.slot % CONSENSUS_SLOTS_PER_WINDOW != 0
                    && parent.slot + 1 != block.id.slot =>
            {
                return Err(ConsensusError(
                    "inside a window, the parent must be the preceding cblock".into(),
                ))
            }
            None if block.id.slot % CONSENSUS_SLOTS_PER_WINDOW != 0 => {
                return Err(ConsensusError(
                    "inside a window, a cblock must have a parent".into(),
                ))
            }
            _ => {}
        }
        Ok(())
    }
    fn certified(&self, id: BlockRef) -> bool {
        self.certificates
            .range(
                (id.slot, CertificateKind::Notarization, None)
                    ..=(id.slot, CertificateKind::Skip, Some([255; 32])),
            )
            .any(|((_, kind, hash), _)| *hash == Some(id.hash) && *kind != CertificateKind::Skip)
    }
    fn skipped(&self, slot: u64) -> bool {
        (slot < self.floor && self.finalized.is_some_and(|f| slot > f.slot))
            || self.certificates
            .contains_key(&(slot, CertificateKind::Skip, None))
    }
    fn extends(&self, mut id: BlockRef, ancestor: BlockRef) -> bool {
        loop {
            if id == ancestor {
                return true;
            }
            if id.slot <= ancestor.slot {
                return false;
            }
            match self.blocks.get(&id).and_then(|b| b.parent) {
                Some(parent) => id = parent,
                None => return false,
            }
        }
    }
    fn compatible_with_finalized(&self, id: BlockRef) -> bool {
        self.finalized.map(|f| self.extends(id, f)).unwrap_or(true)
    }
    fn safe_parent(&self, block: &Block) -> bool {
        if !self.started
            || self.finalized.map_or(false, |f| block.id.slot <= f.slot)
            || !self.compatible_with_finalized(block.id)
        {
            return false;
        }
        let first = block.id.slot % CONSENSUS_SLOTS_PER_WINDOW == 0;
        match block.parent {
            None => {
                self.finalized.is_none() && first && (0..block.id.slot).all(|s| self.skipped(s))
            }
            Some(parent) => {
                if !self.blocks.contains_key(&parent) {
                    return false;
                }
                if first {
                    (self.certified(parent) || self.finalized == Some(parent))
                        && ((parent.slot + 1)..block.id.slot).all(|s| self.skipped(s))
                } else {
                    self.certified(parent)
                        || self
                            .initial
                            .get(&parent.slot)
                            .map(|v| v.kind == VoteKind::Notar && v.hash == Some(parent.hash))
                            .unwrap_or(false)
                }
            }
        }
    }
    fn collect_votes(&self, slot: u64, hash: Option<Hash>, kind: CertificateKind) -> Vec<Vote> {
        let mut signers = BTreeMap::new();
        for ((s, vote_kind, h), votes) in self
            .votes
            .range((slot, VoteKind::Notar, None)..=(slot, VoteKind::Finalize, Some([255; 32])))
        {
            if *s == slot && *h == hash && Self::certificate_accepts(kind, *vote_kind) {
                for (validator, vote) in votes {
                    signers.entry(*validator).or_insert_with(|| vote.clone());
                }
            }
        }
        signers.into_values().collect()
    }
    fn stake_of(&self, votes: &[Vote]) -> u128 {
        votes.iter().map(|v| self.stakes_at(v.slot)[v.validator] as u128).sum()
    }
    fn initial_stakes(&self, slot: u64) -> (u128, BTreeMap<Hash, u128>) {
        let skip = self
            .votes
            .get(&(slot, VoteKind::Skip, None))
            .map(|votes| votes.keys().map(|v| self.stakes_at(slot)[*v] as u128).sum())
            .unwrap_or(0);
        let notar = self
            .votes
            .range((slot, VoteKind::Notar, None)..=(slot, VoteKind::Notar, Some([255; 32])))
            .filter_map(|((_, _, hash), votes)| {
                hash.map(|hash| (hash, votes.keys().map(|v| self.stakes_at(slot)[*v] as u128).sum()))
            })
            .collect();
        (skip, notar)
    }
    fn bad_slot(&self, slot: u64) -> bool {
        self.initial
            .get(&slot)
            .is_some_and(|vote| vote.kind == VoteKind::Skip)
            || self
                .fallback
                .range((slot, None)..=(slot, Some([255; 32])))
                .next()
                .is_some()
    }
    fn install_certificate(&mut self, certificate: Certificate) -> Result<bool> {
        let finalizes = |kind| {
            matches!(
                kind,
                CertificateKind::FastFinalization | CertificateKind::Finalization
            )
        };
        if self
            .certificates
            .range(
                (certificate.slot, CertificateKind::Notarization, None)
                    ..=(certificate.slot, CertificateKind::Skip, Some([255; 32])),
            )
            .any(|((_, kind, hash), _)| {
                finalizes(*kind) && finalizes(certificate.kind) && *hash != certificate.hash
            })
        {
            return Err(ConsensusError(
                "conflicting finalization certificates: validator-set assumptions violated".into(),
            ));
        }
        let key = (certificate.slot, certificate.kind, certificate.hash);
        if self.certificates.contains_key(&key) {
            return Ok(false);
        }
        if finalizes(certificate.kind) {
            self.pending_finalizations.insert(key);
        }
        self.certificates.insert(key, certificate);
        Ok(true)
    }
    fn form_certificates(
        &mut self,
        slots: &BTreeSet<u64>,
        actions: &mut Vec<Action>,
    ) -> Result<bool> {
        let mut changed = false;
        for slot in slots {
            let hashes: BTreeSet<_> = self
                .votes
                .range(
                    (*slot, VoteKind::Notar, None)..=(*slot, VoteKind::Finalize, Some([255; 32])),
                )
                .map(|((_, _, hash), _)| *hash)
                .collect();
            for hash in hashes {
                for kind in [
                    CertificateKind::Notarization,
                    CertificateKind::NotarFallback,
                    CertificateKind::FastFinalization,
                    CertificateKind::Finalization,
                    CertificateKind::Skip,
                ] {
                    if (kind == CertificateKind::Skip) != hash.is_none()
                        || self.certificates.contains_key(&(*slot, kind, hash))
                    {
                        continue;
                    }
                    let votes = self.collect_votes(*slot, hash, kind);
                    let percent = if kind == CertificateKind::FastFinalization {
                        80
                    } else {
                        60
                    };
                    if self.threshold(*slot, self.stake_of(&votes), percent) {
                        let certificate = Certificate {
                            kind,
                            slot: *slot,
                            hash,
                            votes,
                        };
                        self.install_certificate(certificate.clone())?;
                        actions.push(Action::SendCertificate(certificate));
                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
    }
    fn process_certificates(&mut self, actions: &mut Vec<Action>) -> Result<bool> {
        let mut changed = false;
        let live_bound = self.finalized.map_or(Unbounded, |f| {
            Excluded((f.slot, CertificateKind::Skip, Some([255; 32])))
        });
        let mut keys = self
            .certificates
            .range((live_bound, Unbounded))
            .map(|(key, _)| *key)
            .collect::<BTreeSet<_>>();
        keys.extend(self.pending_finalizations.iter().copied());
        let certificates: Vec<_> = keys
            .iter()
            .map(|key| &self.certificates[key])
            .cloned()
            .collect();
        for certificate in certificates {
            let Some(hash) = certificate.hash else {
                continue;
            };
            let id = BlockRef {
                slot: certificate.slot,
                hash,
            };
            let Some(block) = self.blocks.get(&id) else {
                continue;
            };
            if certificate.kind == CertificateKind::Notarization
                && !self.final_votes.contains_key(&id.slot)
                && self.initial.contains_key(&id.slot)
                && !self.bad_slot(id.slot)
                && self.safe_parent(block)
            {
                if let Some(own) = self.own_at(id.slot) {
                let vote = Vote {
                    validator: own,
                    slot: id.slot,
                    hash: Some(id.hash),
                    kind: VoteKind::Finalize,
                };
                self.final_votes.insert(id.slot, vote.clone());
                self.insert_vote(vote.clone())?;
                actions.push(Action::SendVote(vote));
                changed = true;
                }
            }
            if matches!(
                certificate.kind,
                CertificateKind::FastFinalization | CertificateKind::Finalization
            ) && !self.finalized_announced.contains(&id)
                && self.chain_known(id)
            {
                if self.finalized.is_some_and(|f| self.extends(f, id)) {
                    self.finalized_announced.insert(id);
                    self.pending_finalizations
                        .retain(|(slot, _, hash)| *slot != id.slot || *hash != Some(id.hash));
                } else if self.compatible_with_finalized(id) {
                    self.finalized = Some(id);
                    self.finalized_announced.insert(id);
                    self.pending_finalizations
                        .retain(|(slot, _, hash)| *slot != id.slot || *hash != Some(id.hash));
                    actions.push(Action::Finalized(id));
                    changed = true;
                } else {
                    return Err(ConsensusError(
                        "finalization conflicts with the finalized chain".into(),
                    ));
                }
            }
        }
        Ok(changed)
    }
    fn drive(&mut self) -> Result<Vec<Action>> {
        let mut actions = Vec::new();
        loop {
            let mut changed = false;
            let blocks: Vec<_> = self
                .blocks
                .range((
                    self.finalized.map_or(Unbounded, |f| {
                        Excluded(BlockRef {
                            slot: f.slot,
                            hash: [255; 32],
                        })
                    }),
                    Unbounded,
                ))
                .map(|(_, block)| block)
                .cloned()
                .collect();
            for block in &blocks {
                let Some(own) = self.own_at(block.id.slot) else { continue };
                if !self.initial.contains_key(&block.id.slot)
                    && !self.skipped(block.id.slot)
                    && self.safe_parent(block)
                {
                    let vote = Vote {
                        validator: own,
                        slot: block.id.slot,
                        hash: Some(block.id.hash),
                        kind: VoteKind::Notar,
                    };
                    self.initial.insert(vote.slot, vote.clone());
                    self.insert_vote(vote.clone())?;
                    actions.push(Action::SendVote(vote));
                    changed = true;
                }
            }
            let slots: BTreeSet<_> = self
                .votes
                .range((
                    self.finalized.map_or(Unbounded, |f| {
                        Excluded((f.slot, VoteKind::Finalize, Some([255; 32])))
                    }),
                    Unbounded,
                ))
                .map(|((slot, _, _), _)| *slot)
                .collect();

            // An available NC can trigger a final vote before optional fallback.
            changed |= self.form_certificates(&slots, &mut actions)?;
            changed |= self.process_certificates(&mut actions)?;
            for block in &blocks {
                let slot = block.id.slot;
                let Some(own) = self.own_at(slot) else { continue };
                let (skip, notar) = self.initial_stakes(slot);
                let target = notar.get(&block.id.hash).copied().unwrap_or(0);
                let safe_notar_fallback = self.threshold(slot, target, 40)
                    || (self.threshold(slot, skip + target, 60) && self.threshold(slot, target, 20));
                if self.initial.contains_key(&slot)
                    && !self.final_votes.contains_key(&slot)
                    && !self.fallback.contains_key(&(slot, Some(block.id.hash)))
                    && self.chain_known(block.id)
                    && self.compatible_with_finalized(block.id)
                    && safe_notar_fallback
                {
                    let vote = Vote {
                        validator: own,
                        slot,
                        hash: Some(block.id.hash),
                        kind: VoteKind::NotarFallback,
                    };
                    self.fallback.insert((slot, vote.hash), vote.clone());
                    self.insert_vote(vote.clone())?;
                    actions.push(Action::SendVote(vote));
                    changed = true;
                }
            }
            for slot in &slots {
                let Some(own) = self.own_at(*slot) else { continue };
                let (skip, notar) = self.initial_stakes(*slot);
                let total_notar: u128 = notar.values().sum();
                let largest_notar = notar.values().copied().max().unwrap_or(0);
                let safe_skip_fallback = self.threshold(*slot, skip + total_notar - largest_notar, 40);
                if self.initial.contains_key(slot)
                    && !self.final_votes.contains_key(slot)
                    && !self.fallback.contains_key(&(*slot, None))
                    && safe_skip_fallback
                {
                    let vote = Vote {
                        validator: own,
                        slot: *slot,
                        hash: None,
                        kind: VoteKind::SkipFallback,
                    };
                    self.fallback.insert((*slot, None), vote.clone());
                    self.insert_vote(vote.clone())?;
                    actions.push(Action::SendVote(vote));
                    changed = true;
                }
            }
            changed |= self.form_certificates(&slots, &mut actions)?;
            changed |= self.process_certificates(&mut actions)?;
            while let Some(parent) = self.ready_parent(self.next_window) {
                actions.push(Action::ParentReady {
                    window_first_slot: self.next_window,
                    parent,
                });
                self.next_window += CONSENSUS_SLOTS_PER_WINDOW;
                changed = true;
            }
            if !changed {
                break;
            }
        }
        Ok(actions)
    }
    fn chain_known(&self, mut id: BlockRef) -> bool {
        loop {
            if self.finalized == Some(id) {
                return true;
            }
            let Some(block) = self.blocks.get(&id) else {
                return false;
            };
            match block.parent {
                Some(parent) => id = parent,
                None => return true,
            }
        }
    }
    fn ready_parent(&self, boundary: u64) -> Option<Option<BlockRef>> {
        if !self.started || boundary == 0 {
            return None;
        }
        for ((slot, kind, hash), _) in self
            .certificates
            .range(..(boundary, CertificateKind::Notarization, None))
            .rev()
        {
            if *slot >= boundary || *kind == CertificateKind::Skip {
                continue;
            }
            if let Some(hash) = hash {
                let parent = BlockRef {
                    slot: *slot,
                    hash: *hash,
                };
                if self.finalized.map_or(true,|f|parent.slot>f.slot || parent==f)
                    && ((parent.slot + 1)..boundary).all(|s| self.skipped(s))
                {
                    return Some(Some(parent));
                }
            }
        }
        // The finalized cblock is always a parent, its certificate pruned or never held (a resume).
        match self.finalized {
            None if (0..boundary).all(|s| self.skipped(s)) => Some(None),
            Some(f) if f.slot < boundary && ((f.slot + 1)..boundary).all(|s| self.skipped(s)) => Some(Some(f)),
            _ => None,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn id(slot: u64, byte: u8) -> BlockRef {
        BlockRef {
            slot,
            hash: [byte; 32],
        }
    }
    fn vote(validator: usize, block: BlockRef, kind: VoteKind) -> Vote {
        Vote {
            validator,
            slot: block.slot,
            hash: Some(block.hash),
            kind,
        }
    }
    fn machine() -> Votor {
        let mut v = Votor::new(0, vec![1; 5]).unwrap();
        v.genesis();
        v
    }

    #[test]
    fn duplicate_signer_does_not_make_a_certificate() {
        let v = machine();
        let a = id(0, 1);
        let repeated = vote(1, a, VoteKind::Notar);
        let cert = Certificate {
            kind: CertificateKind::Notarization,
            slot: a.slot,
            hash: Some(a.hash),
            votes: vec![repeated; 3],
        };
        assert!(v.validate_certificate(&cert).is_err());
    }

    #[test]
    fn timeout_prevents_later_notarization() {
        let mut v = machine();
        v.timeout(0).unwrap();
        let actions = v
            .local_block(Block {
                id: id(0, 1),
                parent: None,
            })
            .unwrap();
        assert!(!actions.iter().any(|a| matches!(
            a,
            Action::SendVote(Vote {
                kind: VoteKind::Notar | VoteKind::NotarFallback | VoteKind::Finalize,
                ..
            })
        )));
    }

    #[test]
    fn fallbacks_are_per_target_and_preserve_the_one_initial_vote() {
        let mut v = machine();
        let a = id(0, 1);
        let b = id(0, 2);
        v.local_block(Block {
            id: a,
            parent: None,
        })
        .unwrap();
        v.local_block(Block {
            id: b,
            parent: None,
        })
        .unwrap();
        v.receive(Message::Vote(vote(1, b, VoteKind::Notar)))
            .unwrap();
        assert!(!v.fallback.contains_key(&(0, Some(b.hash))));
        v.receive(Message::Vote(vote(2, b, VoteKind::Notar)))
            .unwrap();
        assert!(v.fallback.contains_key(&(0, Some(b.hash))));
        v.receive(Message::Vote(vote(3, a, VoteKind::Notar)))
            .unwrap();
        assert!(v.fallback.contains_key(&(0, Some(a.hash))));
        assert!(v.fallback.contains_key(&(0, None)));
        assert_eq!(v.initial[&0].hash, Some(a.hash));
        assert!(v
            .receive(Message::Vote(vote(0, b, VoteKind::Notar)))
            .is_err());

        let mut combined = machine();
        combined
            .local_block(Block {
                id: a,
                parent: None,
            })
            .unwrap();
        combined
            .local_block(Block {
                id: b,
                parent: None,
            })
            .unwrap();
        combined
            .receive(Message::Vote(vote(1, b, VoteKind::Notar)))
            .unwrap();
        for validator in 2..4 {
            combined
                .receive(Message::Vote(Vote {
                    validator,
                    slot: 0,
                    hash: None,
                    kind: VoteKind::Skip,
                }))
                .unwrap();
        }
        assert!(combined.fallback.contains_key(&(0, Some(b.hash))));
    }

    #[test]
    fn a_resumed_chain_builds_and_finalizes_on_its_finalized_cblock() {
        let mut v = Votor::new(0, vec![1; 5]).unwrap();
        let f = id(41, 7);
        assert_eq!(v.resume(f), vec![Action::ParentReady { window_first_slot: 44, parent: Some(f) }]);
        let a = id(44, 1);
        let actions = v.local_block(Block { id: a, parent: Some(f) }).unwrap();
        assert!(actions.contains(&Action::SendVote(vote(0, a, VoteKind::Notar))));
        for validator in 1..4 {
            v.receive(Message::Vote(vote(validator, a, VoteKind::Notar))).unwrap();
        }
        assert_eq!(v.finalized(), Some(a));
        // A window skipped whole after the resume still gets the last finalized cblock as parent.
        for slot in 45..52 {
            for validator in 0..3 {
                v.receive(Message::Vote(Vote { validator, slot, hash: None, kind: VoteKind::SkipFallback })).unwrap();
            }
        }
        assert!(v.ready_parent(52) == Some(Some(a)));
    }

    #[test]
    fn fast_finalization_requires_eighty_percent() {
        let mut v = machine();
        let a = id(0, 1);
        v.local_block(Block {
            id: a,
            parent: None,
        })
        .unwrap();
        v.receive(Message::Vote(vote(1, a, VoteKind::Notar)))
            .unwrap();
        let at_sixty = v
            .receive(Message::Vote(vote(2, a, VoteKind::Notar)))
            .unwrap();
        assert!(!at_sixty.iter().any(|a| matches!(a, Action::Finalized(_))));
        let at_eighty = v
            .receive(Message::Vote(vote(3, a, VoteKind::Notar)))
            .unwrap();
        assert!(at_eighty
            .iter()
            .any(|action| matches!(action, Action::Finalized(b) if *b == a)));
    }

    #[test]
    fn next_window_uses_latest_certified_parent_after_skips() {
        let mut v = machine();
        for slot in 0..4 {
            for validator in 0..3 {
                let actions = v
                    .receive(Message::Vote(Vote {
                        validator,
                        slot,
                        hash: None,
                        kind: VoteKind::SkipFallback,
                    }))
                    .unwrap();
                if slot == 3 && validator == 2 {
                    assert!(actions.iter().any(|a| matches!(
                        a,
                        Action::ParentReady {
                            window_first_slot: 4,
                            parent: None
                        }
                    )));
                }
            }
        }
    }

    #[test]
    fn split_initial_votes_progress_without_second_initial_vote() {
        let target = id(0, 7);
        let mut machines = (0..12)
            .map(|own| {
                let mut machine = Votor::new(own, vec![1; 12]).unwrap();
                machine.genesis();
                machine
            })
            .collect::<Vec<_>>();
        let mut messages = std::collections::VecDeque::new();
        let mut initial_kinds = BTreeMap::new();
        for (own, machine) in machines.iter_mut().enumerate() {
            let actions = if own < 6 {
                machine
                    .local_block(Block {
                        id: target,
                        parent: None,
                    })
                    .unwrap()
            } else {
                let actions = machine.timeout(0).unwrap();
                machine
                    .local_block(Block {
                        id: target,
                        parent: None,
                    })
                    .unwrap();
                actions
            };
            for action in actions {
                if let Action::SendVote(vote) = action {
                    initial_kinds.insert(vote.validator, vote.kind);
                    messages.push_back(Message::Vote(vote));
                }
            }
        }
        assert_eq!(
            initial_kinds
                .values()
                .filter(|kind| **kind == VoteKind::Notar)
                .count(),
            6
        );
        assert_eq!(
            initial_kinds
                .values()
                .filter(|kind| **kind == VoteKind::Skip)
                .count(),
            6
        );
        let mut delivered = 0;
        while let Some(message) = messages.pop_front() {
            delivered += 1;
            assert!(delivered < 10_000, "Consensus messages must settle");
            for machine in &mut machines {
                for action in machine.receive(message.clone()).unwrap() {
                    match action {
                        Action::SendVote(vote) => {
                            if matches!(vote.kind, VoteKind::Notar | VoteKind::Skip) {
                                assert_eq!(initial_kinds[&vote.validator], vote.kind);
                            }
                            messages.push_back(Message::Vote(vote));
                        }
                        Action::SendCertificate(certificate) => {
                            messages.push_back(Message::Certificate(certificate))
                        }
                        _ => {}
                    }
                }
            }
        }
        // Fallback enables progress; it does not make a fallback voter eligible to finalize.
        assert!(machines.iter().all(|machine| machine.finalized().is_none()));
        assert!(machines
            .iter()
            .all(|machine| machine.final_votes.is_empty()));
        assert!(machines
            .iter()
            .all(|machine| machine.certificates.contains_key(&(
                0,
                CertificateKind::NotarFallback,
                Some(target.hash)
            ))));
    }

    #[test]
    fn forty_notar_sixty_skip_allow_overlapping_weak_certificates() {
        let mut v = machine();
        let block = id(0, 9);
        v.local_block(Block {
            id: block,
            parent: None,
        })
        .unwrap();
        v.receive(Message::Vote(vote(1, block, VoteKind::Notar)))
            .unwrap();
        for validator in 2..5 {
            v.receive(Message::Vote(Vote {
                validator,
                slot: 0,
                hash: None,
                kind: VoteKind::Skip,
            }))
            .unwrap();
        }
        for validator in 2..5 {
            v.receive(Message::Vote(vote(
                validator,
                block,
                VoteKind::NotarFallback,
            )))
            .unwrap();
        }
        assert!(v
            .certificates
            .contains_key(&(0, CertificateKind::Skip, None)));
        assert!(v.certificates.contains_key(&(
            0,
            CertificateKind::NotarFallback,
            Some(block.hash)
        )));
        assert!(v.final_votes.is_empty());
        assert_eq!(v.finalized(), None);
    }

    #[test]
    fn nc_final_vote_requires_no_own_skip_or_fallback_and_locks_fallback() {
        let target = id(0, 4);
        let nc = Certificate {
            kind: CertificateKind::Notarization,
            slot: 0,
            hash: Some(target.hash),
            votes: (1..7)
                .map(|validator| vote(validator, target, VoteKind::Notar))
                .collect(),
        };
        let mut eligible = Votor::new(0, vec![1; 10]).unwrap();
        eligible.genesis();
        eligible
            .local_block(Block {
                id: target,
                parent: None,
            })
            .unwrap();
        let actions = eligible.receive(Message::Certificate(nc.clone())).unwrap();
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SendVote(Vote {
                kind: VoteKind::Finalize,
                ..
            })
        )));
        assert!(eligible.fallback.is_empty());
        assert_eq!(eligible.finalized(), None);
        let final_votes = (0..6)
            .map(|validator| vote(validator, target, VoteKind::Finalize))
            .collect();
        eligible
            .receive(Message::Certificate(Certificate {
                kind: CertificateKind::Finalization,
                slot: 0,
                hash: Some(target.hash),
                votes: final_votes,
            }))
            .unwrap();
        assert_eq!(eligible.finalized(), Some(target));

        let mut skipped = Votor::new(0, vec![1; 10]).unwrap();
        skipped.genesis();
        skipped.timeout(0).unwrap();
        skipped
            .local_block(Block {
                id: target,
                parent: None,
            })
            .unwrap();
        skipped.receive(Message::Certificate(nc)).unwrap();
        assert!(skipped.final_votes.is_empty());
        assert!(skipped.fallback.contains_key(&(0, Some(target.hash))));
    }
}
