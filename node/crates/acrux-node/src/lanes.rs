//! Fault Lab lanes: independent content decisions, followed by the existing SVM merge.
//! The caller authenticates lane-tagged votes and certificate proofs before delivery.
use crate::consensus::{
    Action, Block, BlockRef, Certificate, CertificateKind, ConsensusError, Hash, Message, Votor,
    CONSENSUS_SLOTS_PER_WINDOW,
};
use std::{collections::BTreeMap, time::{Duration, Instant}};

type Result<T> = std::result::Result<T, ConsensusError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Inputs {
    pub(crate) hashes: Vec<Option<Hash>>,
}

/// A destination's authenticated attestation view, before its SVM actor queue.
#[derive(Default)]
pub(crate) struct Round {
    rows: BTreeMap<usize, crate::types::Attestation>,
    started: Option<Instant>,
    inputs: Option<Inputs>,
    invalid: [bool; 3],
}

impl Round {
    pub(crate) fn record(&mut self, attestation: crate::types::Attestation, at: Instant) {
        self.started = Some(self.started.map_or(at, |first| first.min(at)));
        // Freeze the preceding view at its actual receipt deadline even when
        // the actor has not yet asked for a snapshot.
        self.advance(at);
        if let Some(previous) = self.rows.get(&attestation.proposer) {
            if previous.key != attestation.key {
                self.invalid[attestation.proposer - 1] = true;
            }
            for column in 0..3 {
                if previous.pi[column] != attestation.pi[column] {
                    self.invalid[column] = true;
                }
            }
        } else {
            for column in 0..3 {
                if let Some(hash) = attestation.pi[column] {
                    if self.rows.values().any(|previous| {
                        previous.pi[column].is_some_and(|previous| previous != hash)
                    }) {
                        self.invalid[column] = true;
                    }
                }
            }
            self.rows.insert(attestation.proposer, attestation);
        }
        self.advance(at);
    }

    pub(crate) fn snapshot(&mut self, now: Instant) -> Option<Inputs> {
        self.advance(now);
        self.inputs.clone()
    }

    fn advance(&mut self, now: Instant) {
        if self.inputs.is_none() {
            let elapsed = self.started.is_some_and(|first| {
                now.saturating_duration_since(first) >= Duration::from_millis(500)
            });
            if self.rows.len() != 3 && !elapsed {
                return;
            }
            let hashes = (0..3).map(|column| {
                if self.rows.len() < 2 || !self.rows.contains_key(&(column + 1)) {
                    return None;
                }
                let hash = self.rows.values().next()?.pi[column]?;
                self.rows.values().all(|row| row.pi[column] == Some(hash)).then_some(hash)
            }).collect();
            self.inputs = Some(Inputs { hashes });
        }
        if let Some(inputs) = &mut self.inputs {
            for (column, invalid) in self.invalid.iter().enumerate() {
                if *invalid {
                    inputs.hashes[column] = None;
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AcceptedLanes {
    pub(crate) proposers: Vec<usize>,
    /// Role order, with a null hash for each skip-certified lane.
    pub(crate) hashes: Vec<Option<Hash>>,
}

pub(crate) struct Lanes {
    lanes: Vec<Lane>,
}

struct Lane {
    votor: Votor,
    blocks: BTreeMap<BlockRef, Block>,
    decisions: BTreeMap<u64, Option<Hash>>,
    parents: BTreeMap<u64, Option<BlockRef>>,
}

impl Lanes {
    pub(crate) fn new(own: usize, stakes: Vec<u64>) -> Result<Self> {
        let mut lanes = Vec::with_capacity(3);
        for _ in 0..3 {
            let mut votor = Votor::new(own, stakes.clone())?;
            // The execution Votor owns Network's clock. These notifications only
            // initialize the independent content chains' parent bookkeeping.
            let actions = votor.genesis();
            let mut lane = Lane {
                votor,
                blocks: BTreeMap::new(),
                decisions: BTreeMap::new(),
                parents: BTreeMap::new(),
            };
            lane.observe(&actions)?;
            lanes.push(lane);
        }
        Ok(Self { lanes })
    }

    pub(crate) fn local_block(&mut self, lane: usize, block: Block) -> Result<Vec<(usize, Action)>> {
        let state = self.lane_mut(lane)?;
        let actions = state.votor.local_block(block.clone())?;
        state.blocks.insert(block.id, block);
        state.observe(&actions)?;
        Ok(actions.into_iter().map(|action| (lane, action)).collect())
    }

    pub(crate) fn timeout(&mut self, lane: usize, cblock: u64) -> Result<Vec<(usize, Action)>> {
        let state = self.lane_mut(lane)?;
        let actions = state.votor.timeout(cblock)?;
        state.observe(&actions)?;
        Ok(actions.into_iter().map(|action| (lane, action)).collect())
    }

    pub(crate) fn receive(&mut self, lane: usize, message: Message) -> Result<Vec<(usize, Action)>> {
        let received_skip = match &message {
            Message::Certificate(certificate) if certificate.kind == CertificateKind::Skip => {
                Some(certificate.slot)
            }
            _ => None,
        };
        let state = self.lane_mut(lane)?;
        let actions = state.votor.receive(message)?;
        // A received certificate need not be emitted again as SendCertificate.
        // Votor has validated every vote before this decision is recorded.
        if let Some(slot) = received_skip {
            state.decide(slot, None)?;
        }
        state.observe(&actions)?;
        Ok(actions.into_iter().map(|action| (lane, action)).collect())
    }

    pub(crate) fn validate_certificate(&self, lane: usize, certificate: &Certificate) -> Result<()> {
        self.lane(lane)?.votor.validate_certificate(certificate)
    }

    /// Outer null means pending; inner null means the lane is skip-certified.
    pub(crate) fn decision(&self, lane: usize, cblock: u64) -> Option<Option<Hash>> {
        self.lane(lane).ok()?.decisions.get(&cblock).copied()
    }

    /// No execution may start until every lane has reached its own decision.
    pub(crate) fn decisions(&self, cblock: u64) -> Option<AcceptedLanes> {
        let hashes = self.lanes.iter().map(|lane| lane.decisions.get(&cblock).copied())
            .collect::<Option<Vec<_>>>()?;
        let proposers = hashes.iter().enumerate()
            .filter_map(|(index, hash)| hash.map(|_| index + 1)).collect();
        Some(AcceptedLanes { proposers, hashes })
    }

    /// Inside a window, the caller first distinguishes a pending predecessor from
    /// a skip using decision(). A skipped lane stays skipped until the next window.
    pub(crate) fn parent(&self, lane: usize, cblock: u64) -> Option<BlockRef> {
        let state = self.lane(lane).ok()?;
        if cblock % CONSENSUS_SLOTS_PER_WINDOW == 0 {
            state.parents.get(&cblock).copied().flatten()
        } else {
            state.decisions.get(&(cblock - 1)).copied().flatten()
                .map(|hash| BlockRef { slot: cblock - 1, hash })
        }
    }

    pub(crate) fn prune(&mut self, below: u64) {
        for lane in &mut self.lanes {
            // A wholly skipped lane still needs its skip chain until its first
            // accepted block; Votor cannot discard that genesis ancestry yet.
            let below = lane.votor.finalized().map_or(0, |last| below.min(last.slot));
            lane.votor.prune(below);
            lane.blocks.retain(|id, _| id.slot >= below);
            lane.decisions = lane.decisions.split_off(&below);
            lane.parents = lane.parents.split_off(&(below / CONSENSUS_SLOTS_PER_WINDOW * CONSENSUS_SLOTS_PER_WINDOW));
        }
    }

    fn lane(&self, lane: usize) -> Result<&Lane> {
        lane.checked_sub(1).and_then(|index| self.lanes.get(index))
            .ok_or_else(|| ConsensusError("Unknown proposer lane".into()))
    }

    fn lane_mut(&mut self, lane: usize) -> Result<&mut Lane> {
        lane.checked_sub(1).and_then(|index| self.lanes.get_mut(index))
            .ok_or_else(|| ConsensusError("Unknown proposer lane".into()))
    }
}

impl Lane {
    fn observe(&mut self, actions: &[Action]) -> Result<()> {
        for action in actions {
            match action {
                Action::SendCertificate(certificate) if certificate.kind == CertificateKind::Skip => {
                    self.decide(certificate.slot, None)?;
                }
                Action::Finalized(block) => self.finalize(*block)?,
                Action::ParentReady { window_first_slot, parent } => {
                    self.parents.insert(*window_first_slot, *parent);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn decide(&mut self, cblock: u64, hash: Option<Hash>) -> Result<()> {
        match self.decisions.get(&cblock) {
            Some(previous) if *previous != hash => {
                Err(ConsensusError("Conflicting decisions in one proposer lane".into()))
            }
            Some(_) => Ok(()),
            None => {
                self.decisions.insert(cblock, hash);
                Ok(())
            }
        }
    }

    fn finalize(&mut self, mut block: BlockRef) -> Result<()> {
        loop {
            if block.slot < self.votor.pruned_below() {
                return Ok(());
            }
            let recorded = self.decisions.contains_key(&block.slot);
            self.decide(block.slot, Some(block.hash))?;
            if recorded {
                return Ok(());
            }
            let known = self.blocks.get(&block).ok_or_else(|| {
                ConsensusError("Finalized lane block was not locally validated".into())
            })?;
            match known.parent {
                Some(parent) => block = parent,
                None => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ExecutionItem, SvmExecutor};
    use crate::consensus::{Vote, VoteKind};
    use solana_sdk::{
        compute_budget::ComputeBudgetInstruction, hash::Hash as SolanaHash,
        pubkey::Pubkey, signature::{Keypair, Signer}, system_instruction, transaction::Transaction,
    };
    use std::str::FromStr;

    #[test]
    fn skipped_lane_waits_at_barrier_then_merges_the_two_accepted_sources() {
        let first = Instant::now();
        let row = |proposer, pi| crate::types::Attestation {
            slot: 0, proposer, pi, key: [proposer as u8; 32],
        };
        let all = vec![Some([1; 32]), Some([2; 32]), Some([3; 32])];
        let two = vec![Some([1; 32]), Some([2; 32]), None];
        let expected = Some(Inputs { hashes: two.clone() });
        let mut split = Round::default();
        split.record(row(1, all.clone()), first);
        split.record(row(2, two.clone()), first + Duration::from_millis(10));
        assert!(split.snapshot(first + Duration::from_millis(20)).is_none());
        split.record(row(3, all.clone()), first + Duration::from_millis(30));
        assert_eq!(split.snapshot(first + Duration::from_millis(30)), expected,
            "a mixed-null column skips only the split proposer's lane");
        let mut crash = Round::default();
        crash.record(row(1, two.clone()), first);
        crash.record(row(2, two.clone()), first + Duration::from_millis(10));
        assert!(crash.snapshot(first + Duration::from_millis(499)).is_none());
        assert_eq!(crash.snapshot(first + Duration::from_millis(500)), expected,
            "two signed rows settle a crashed owner's lane at the receipt bound");
        crash.record(row(3, vec![None, None, Some([3; 32])]), first + Duration::from_millis(501));
        assert_eq!(crash.snapshot(first + Duration::from_millis(502)), expected,
            "a late null row does not change the frozen view or revive a skipped lane");
        let mut conflicting = row(1, two.clone());
        conflicting.key = [9; 32];
        crash.record(conflicting, first + Duration::from_millis(503));
        assert_eq!(crash.snapshot(first + Duration::from_millis(503)),
            Some(Inputs { hashes: vec![None, Some([2; 32]), None] }),
            "a changed owner key invalidates only that owner's column");
        let mut delayed = Round::default();
        delayed.record(row(1, all.clone()), first);
        delayed.record(row(2, two.clone()), first + Duration::from_millis(10));
        delayed.record(row(3, all), first + Duration::from_millis(501));
        assert_eq!(delayed.snapshot(first + Duration::from_millis(502)), expected,
            "a late actor cannot expand the control-plane decision");
        let mut changed = row(3, two);
        changed.pi[1] = Some([9; 32]);
        delayed.record(changed, first + Duration::from_millis(503));
        assert_eq!(delayed.snapshot(first + Duration::from_millis(503)),
            Some(Inputs { hashes: vec![Some([1; 32]), None, None] }),
            "a duplicate hash conflict invalidates only its changed column");

        let mut lanes = Lanes::new(0, vec![1; 12]).unwrap();
        let blocks = [
            Block { id: BlockRef { slot: 0, hash: [1; 32] }, parent: None },
            Block { id: BlockRef { slot: 0, hash: [2; 32] }, parent: None },
        ];
        for (index, block) in blocks.iter().enumerate() {
            let lane = index + 1;
            let actions = lanes.local_block(lane, block.clone()).unwrap();
            assert!(actions.iter().any(|(role, action)| *role == lane
                && matches!(action, Action::SendVote(vote) if vote.kind == VoteKind::Notar)));
            for validator in 1..=7 {
                lanes.receive(lane, Message::Vote(Vote {
                    validator, slot: 0, hash: Some(block.id.hash), kind: VoteKind::Notar,
                })).unwrap();
            }
            assert!(lanes.decision(lane, 0).is_none(), "a notar certificate alone is not finalization");
            for validator in 8..=9 {
                lanes.receive(lane, Message::Vote(Vote {
                    validator, slot: 0, hash: Some(block.id.hash), kind: VoteKind::Notar,
                })).unwrap();
            }
            assert_eq!(lanes.decision(lane, 0), Some(Some(block.id.hash)));
        }
        assert!(lanes.decisions(0).is_none(), "two accepted lanes cannot bypass the undecided third");
        lanes.timeout(3, 0).unwrap();
        for validator in 1..=7 {
            lanes.receive(3, Message::Vote(Vote {
                validator, slot: 0, hash: None, kind: VoteKind::Skip,
            })).unwrap();
        }
        let accepted = lanes.decisions(0).unwrap();
        assert_eq!(accepted.proposers, vec![1, 2]);
        assert_eq!(accepted.hashes, vec![Some([1; 32]), Some([2; 32]), None]);
        assert_eq!(lanes.parent(1, 1), Some(blocks[0].id));
        assert_eq!(lanes.decision(3, 0), Some(None));

        let payer = Keypair::new();
        let receiver = Pubkey::new_unique();
        let recipients = [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()];
        let balances = std::iter::once((payer.pubkey().to_string(), 100_000_000))
            .chain(std::iter::once((receiver.to_string(), 2_000_000)))
            .chain(recipients.iter().map(|key| (key.to_string(), 2_000_000)));
        let mut executor = SvmExecutor::new(balances, &[]).unwrap();
        executor.set_proposers(&recipients.iter().enumerate()
            .map(|(index, key)| ((index + 1) as u8, key.to_string())).collect::<Vec<_>>()).unwrap();
        let transaction = |amount| {
            let transaction = Transaction::new_signed_with_payer(&[
                ComputeBudgetInstruction::set_compute_unit_limit(20_000),
                system_instruction::transfer(&payer.pubkey(), &receiver, amount),
            ], Some(&payer.pubkey()), &[&payer], SolanaHash::from_str(&executor.latest_blockhash()).unwrap());
            executor.validate_transaction(&bincode::serialize(&transaction).unwrap(), 0).unwrap()
        };
        let shared = transaction(1_000_000);
        let skipped = transaction(9_000_000);
        let skipped_id = skipped.header.id.clone();
        let mut payloads = BTreeMap::from([
            (1, vec![shared.clone()]), (2, vec![shared]), (3, vec![skipped]),
        ]);
        let mut items = Vec::new();
        for role in accepted.proposers {
            for transaction in payloads.remove(&role).unwrap() {
                executor.charge_inclusion(&payer.pubkey().to_string(), role as u8, 0, 1).unwrap();
                items.push(ExecutionItem { transaction, source_proposers: vec![role as u8] });
            }
        }
        let merged = executor.execute_slot(0, items).unwrap();
        assert_eq!(merged.results.len(), 1, "the two honest copies execute exactly once");
        assert!(merged.results[0].success, "{:?}", merged.results[0]);
        assert!(!executor.is_executed(&skipped_id));
        assert_eq!(executor.balance(&receiver.to_string()).unwrap(), 3_000_000);
        assert_eq!(executor.balance(&payer.pubkey().to_string()).unwrap(), 98_993_000);
        assert_eq!(executor.balance(&recipients[0].to_string()).unwrap(), 2_002_250);
        assert_eq!(executor.balance(&recipients[1].to_string()).unwrap(), 2_002_250);
        assert_eq!(executor.balance(&recipients[2].to_string()).unwrap(), 2_000_000);
    }
}
