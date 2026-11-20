//! Algorithm 2, Agreed(s): unanimous attestation vectors from a strict proposer majority.
use crate::types::Attestation;
use std::{collections::BTreeMap, time::{Duration, Instant}};

/// The canonical block hashes chosen by the Fault Lab's local-agreement branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agreement {
    pub proposers: Vec<usize>,
    pub pi: Vec<Option<[u8; 32]>>,
}

/// Call after the synchronous attestation phase closes. A block needs the same hash
/// from two distinct roles; a null mark does not contradict a non-null hash.
/// Signatures and the availability of the chosen blocks are checked by the caller.
pub fn agreed_with_hashes(attestations: &[Attestation], equivocation: bool) -> Option<Agreement> {
    checked_agreement(attestations, equivocation).ok().flatten()
}

fn checked_agreement(attestations: &[Attestation], equivocation: bool) -> Result<Option<Agreement>, ()> {
    if equivocation {
        return Err(());
    }
    if attestations.is_empty() {
        return Ok(None);
    }
    let slot = attestations[0].slot;
    let mut observed: [Option<&Attestation>; 3] = [None; 3];
    for attestation in attestations {
        if attestation.slot != slot
            || !(1..=3).contains(&attestation.proposer)
            || attestation.pi.len() != 3
        {
            return Err(());
        }
        let previous = &mut observed[attestation.proposer - 1];
        if previous.is_some_and(|previous| {
            previous.pi != attestation.pi || previous.key != attestation.key
        }) {
            return Err(());
        }
        *previous = Some(attestation);
    }
    let mut pi = vec![None; 3];
    for (index, selected) in pi.iter_mut().enumerate() {
        let mut hash = None;
        let mut count = 0;
        for attestation in observed.iter().flatten() {
            if let Some(attested) = attestation.pi[index] {
                if hash.is_some_and(|hash| hash != attested) {
                    return Err(());
                }
                hash = Some(attested);
                count += 1;
            }
        }
        if count >= 2 {
            *selected = hash;
        }
    }
    let proposers = pi
        .iter()
        .enumerate()
        .filter_map(|(index, hash)| hash.map(|_| index + 1))
        .collect::<Vec<_>>();
    Ok((proposers.len() >= 2).then_some(Agreement { proposers, pi }))
}

/// The authenticated control-plane view of one destination's agreement round.
/// Recording a late row closes the preceding view before considering that row, so
/// a delayed executor cannot enlarge a decision that was already ready to freeze.
#[derive(Default)]
pub(crate) struct Round {
    attestations: BTreeMap<usize, (Attestation, Vec<u8>)>,
    started: Option<Instant>,
    decision: Option<Agreement>,
    equivocation: bool,
}

pub(crate) struct Snapshot {
    pub(crate) decision: Option<Agreement>,
    pub(crate) equivocation: bool,
}

impl Round {
    pub(crate) fn record(&mut self, attestation: Attestation, signature: Vec<u8>, at: Instant) {
        self.started = Some(self.started.map_or(at, |first| first.min(at)));
        self.advance(at);
        if let Some((previous, _)) = self.attestations.get(&attestation.proposer) {
            if previous.slot != attestation.slot
                || previous.pi != attestation.pi
                || previous.key != attestation.key
            {
                self.equivocation = true;
                self.decision = None;
            }
        } else {
            self.attestations.insert(attestation.proposer, (attestation, signature));
        }
        self.advance(at);
    }

    pub(crate) fn snapshot(&mut self, now: Instant) -> Snapshot {
        self.advance(now);
        Snapshot {
            decision: self.decision.clone(),
            equivocation: self.equivocation,
        }
    }

    fn advance(&mut self, now: Instant) {
        let rows = self.attestations.values().map(|(attestation, _)| attestation.clone()).collect::<Vec<_>>();
        let candidate = match checked_agreement(&rows, self.equivocation) {
            Ok(candidate) => candidate,
            Err(()) => {
                self.equivocation = true;
                self.decision = None;
                return;
            }
        };
        let complete = rows.len() == 3 || (rows.len() == 2 && rows[0].pi == rows[1].pi);
        let elapsed = self.started.is_some_and(|first| {
            now.saturating_duration_since(first) >= Duration::from_millis(500)
        });
        if self.decision.is_none() && (complete || elapsed) {
            self.decision = candidate;
        }
    }
}

pub fn agreed(attestations: &[Attestation], equivocation: bool) -> Option<Vec<usize>> {
    if equivocation || attestations.is_empty() {
        return None;
    }
    let first = &attestations[0];
    if first.pi.len() != 3 {
        return None;
    }
    let mut observed = [false; 3];
    for attestation in attestations {
        if attestation.slot != first.slot
            || !(1..=3).contains(&attestation.proposer)
            || attestation.pi.len() != 3
            || attestation.pi != first.pi
        {
            return None;
        }
        let index = attestation.proposer - 1;
        // Two distinct signed attestations by the same proposer are equivocation too.
        if observed[index]
            && attestations.iter().any(|other| {
                other.proposer == attestation.proposer
                    && (other.key != attestation.key || other.pi != attestation.pi)
            })
        {
            return None;
        }
        observed[index] = true;
    }
    let accepted = first
        .pi
        .iter()
        .enumerate()
        .filter_map(|(index, hash)| hash.map(|_| index + 1))
        .collect::<Vec<_>>();
    if accepted.len() <= 3 / 2 || accepted.iter().any(|proposer| !observed[*proposer - 1]) {
        return None;
    }
    Some(accepted)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn attestation(proposer: usize, pi: Vec<Option<[u8; 32]>>) -> Attestation {
        Attestation {
            slot: 42,
            proposer,
            pi,
            key: [proposer as u8; 32],
        }
    }
    #[test]
    fn agreed_requires_complete_unanimous_majority_without_equivocation() {
        let all = vec![Some([1; 32]), Some([2; 32]), Some([3; 32])];
        let two = vec![Some([1; 32]), Some([2; 32]), None];
        let one = vec![Some([1; 32]), None, None];
        let complete = (1..=3)
            .map(|p| attestation(p, all.clone()))
            .collect::<Vec<_>>();
        assert_eq!(agreed(&complete, false), Some(vec![1, 2, 3]));
        assert_eq!(
            agreed(
                &[attestation(1, two.clone()), attestation(2, two.clone())],
                false
            ),
            Some(vec![1, 2])
        );
        assert_eq!(agreed(&complete[..2], false), None); // An attested member is still missing.
        assert_eq!(agreed(&[attestation(1, one)], false), None); // No strict majority.
        assert_eq!(
            agreed(&[attestation(1, two), attestation(2, all.clone())], false),
            None
        ); // Disagreeing late mark.
        assert_eq!(agreed(&complete, true), None);
        assert_eq!(agreed(&[], false), None);
        assert_eq!(agreed(&[attestation(4, all)], false), None);
    }

    #[test]
    fn local_agreement_accepts_two_honest_quorums_with_skewed_nulls() {
        let pi = vec![Some([1; 32]), Some([2; 32]), None];
        let rows = vec![
            attestation(1, pi.clone()),
            attestation(2, pi.clone()),
            attestation(3, vec![None, None, Some([3; 32])]),
        ];
        assert_eq!(
            agreed_with_hashes(&rows, false),
            Some(Agreement { proposers: vec![1, 2], pi: pi.clone() })
        );
        assert_eq!(agreed(&rows, false), None, "today still requires identical vectors");
        assert_eq!(
            agreed_with_hashes(&[rows[0].clone(), rows[0].clone(), rows[2].clone()], false),
            None,
            "an identical duplicate cannot count as another signer"
        );
        let first = Instant::now();
        let mut round = Round::default();
        round.record(rows[0].clone(), vec![1], first);
        round.record(rows[2].clone(), vec![3], first + Duration::from_millis(10));
        assert!(round.snapshot(first + Duration::from_millis(499)).decision.is_none());
        assert!(round.snapshot(first + Duration::from_millis(500)).decision.is_none());
        round.record(rows[1].clone(), vec![2], first + Duration::from_millis(501));
        assert_eq!(
            round.snapshot(first + Duration::from_millis(502)).decision,
            Some(Agreement { proposers: vec![1, 2], pi: pi.clone() }),
            "a missing quorum does not permanently close the round"
        );
        let mut stable = Round::default();
        stable.record(rows[0].clone(), vec![1], first);
        stable.record(rows[1].clone(), vec![2], first + Duration::from_millis(1));
        assert_eq!(
            stable.snapshot(first + Duration::from_millis(1)).decision,
            Some(Agreement { proposers: vec![1, 2], pi }),
            "two identical rows do not wait for a crashed third role"
        );
    }

    #[test]
    fn local_agreement_includes_attacker_hash_attested_by_itself_and_one_honest_role() {
        let all = vec![Some([1; 32]), Some([2; 32]), Some([3; 32])];
        let rows = vec![
            attestation(1, all.clone()),
            attestation(2, vec![Some([1; 32]), Some([2; 32]), None]),
            attestation(3, vec![None, None, Some([3; 32])]),
        ];
        assert_eq!(
            agreed_with_hashes(&rows, false),
            Some(Agreement { proposers: vec![1, 2, 3], pi: all })
        );
        assert_eq!(agreed(&rows, false), None);
        let first = Instant::now();
        let mut delayed = Round::default();
        delayed.record(rows[0].clone(), vec![1], first);
        delayed.record(rows[1].clone(), vec![2], first + Duration::from_millis(10));
        delayed.record(rows[2].clone(), vec![3], first + Duration::from_millis(501));
        let snapshot = delayed.snapshot(first + Duration::from_millis(502));
        assert_eq!(
            snapshot.decision,
            Some(Agreement {
                proposers: vec![1, 2],
                pi: vec![Some([1; 32]), Some([2; 32]), None],
            }),
            "a delayed actor cannot expand the positive decision at the receipt deadline"
        );
        assert!(!snapshot.equivocation);
    }

    #[test]
    fn local_agreement_refuses_equivocation_in_hashes_or_duplicate_attestations() {
        let all = vec![Some([1; 32]), Some([2; 32]), Some([3; 32])];
        let rows = (1..=3).map(|role| attestation(role, all.clone())).collect::<Vec<_>>();
        assert_eq!(agreed_with_hashes(&rows, true), None);
        let mut conflicting_hash = rows.clone();
        conflicting_hash[2].pi[2] = Some([9; 32]);
        assert_eq!(
            agreed_with_hashes(&conflicting_hash, false),
            None,
            "a conflicting hash invalidates agreement even when two rows match"
        );
        let mut duplicate = rows.clone();
        let mut changed = rows[0].clone();
        changed.pi[2] = None;
        duplicate.push(changed);
        assert_eq!(agreed_with_hashes(&duplicate, false), None);
        let mut duplicate = rows.clone();
        let mut changed = rows[0].clone();
        changed.key = [9; 32];
        duplicate.push(changed);
        assert_eq!(agreed_with_hashes(&duplicate, false), None);
        let first = Instant::now();
        let mut late_hash = Round::default();
        late_hash.record(rows[0].clone(), vec![1], first);
        late_hash.record(rows[1].clone(), vec![2], first + Duration::from_millis(1));
        assert!(late_hash.snapshot(first + Duration::from_millis(1)).decision.is_some());
        late_hash.record(conflicting_hash[2].clone(), vec![3], first + Duration::from_millis(2));
        let rejected = late_hash.snapshot(first + Duration::from_millis(2));
        assert!(rejected.equivocation);
        assert!(rejected.decision.is_none(), "a late conflicting hash clears the frozen decision");
        let mut late_duplicate = Round::default();
        late_duplicate.record(rows[0].clone(), vec![1], first);
        late_duplicate.record(rows[1].clone(), vec![2], first + Duration::from_millis(1));
        late_duplicate.record(duplicate[3].clone(), vec![9], first + Duration::from_millis(2));
        let rejected = late_duplicate.snapshot(first + Duration::from_millis(2));
        assert!(rejected.equivocation);
        assert!(rejected.decision.is_none(), "a late conflicting duplicate clears the decision");
    }

    #[test]
    fn local_agreement_is_identical_for_every_validator_attestation_order() {
        let all = vec![Some([1; 32]), Some([2; 32]), Some([3; 32])];
        let rows = [
            attestation(1, all.clone()),
            attestation(2, vec![Some([1; 32]), Some([2; 32]), None]),
            attestation(3, vec![None, None, Some([3; 32])]),
        ];
        let expected = Some(Agreement { proposers: vec![1, 2, 3], pi: all });
        for order in [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
            let received = order.map(|index| rows[index].clone());
            assert_eq!(agreed_with_hashes(&received, false), expected);
            let first = Instant::now();
            let mut round = Round::default();
            for (index, row) in received.into_iter().enumerate() {
                round.record(row, vec![index as u8], first + Duration::from_millis(index as u64));
            }
            assert_eq!(round.snapshot(first + Duration::from_millis(3)).decision, expected);
        }
    }
}
