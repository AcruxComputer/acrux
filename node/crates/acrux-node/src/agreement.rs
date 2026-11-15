//! Algorithm 2, Agreed(s): unanimous attestation vectors from a strict proposer majority.
use crate::types::Attestation;

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
}
