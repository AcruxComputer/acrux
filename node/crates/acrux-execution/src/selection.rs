//! New Constellation Algorithm 4: deterministic virtual execution tracks.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const TRACKS: u8 = 4;
pub const TRACK_CAPACITY_CU: u64 = 2_000_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub id: String,
    pub priority_fee_per_cu: u64,
    pub compute_limit: u64,
    pub writable_accounts: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Placement {
    pub track: u8,
    pub start_cu: u64,
    pub length_cu: u64,
    pub tx_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Gap {
    pub track: u8,
    pub start_cu: u64,
    pub length_cu: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Selection {
    pub algorithm: u8,
    pub tracks: u8,
    pub capacity_cu: u64,
    pub placements: Vec<Placement>,
    pub gaps: Vec<Gap>,
    pub selected_ids: Vec<String>,
    pub rejected_ids: Vec<String>,
}

#[derive(Clone)]
struct Space {
    start: u64,
    end: u64,
    track: u8,
}

pub fn sort_transactions(txs: &mut [Candidate]) {
    txs.sort_by(|a, b| {
        b.priority_fee_per_cu
            .cmp(&a.priority_fee_per_cu)
            .then(a.compute_limit.cmp(&b.compute_limit))
            .then(a.id.cmp(&b.id))
    });
}

/// The fee callback is Algorithm 4 line 9. The caller owns account debits.
/// Failed charges are excluded before trying spaces; each accepted block copy's
/// inclusion fee is charged independently, including an unselected transaction.
pub fn select_with_fees(
    txs: &[Candidate],
    capacity: u64,
    mut charge_priority: impl FnMut(&Candidate) -> bool,
) -> Selection {
    let mut ordered = txs.to_vec();
    sort_transactions(&mut ordered);
    let mut spaces: Vec<Space> = (1..=TRACKS)
        .map(|track| Space {
            start: 0,
            end: capacity,
            track,
        })
        .collect();
    let mut scheduled: Vec<(Placement, BTreeSet<&str>)> = Vec::new();
    let mut selected_ids = Vec::new();
    let mut rejected_ids = Vec::new();
    for tx in &ordered {
        if !charge_priority(tx) {
            rejected_ids.push(tx.id.clone());
            continue;
        }
        let accounts: BTreeSet<&str> = tx.writable_accounts.iter().map(String::as_str).collect();
        // Evaluate each free space's first valid position, then choose the
        // earliest position and lowest track as the prose and Figure 5 specify.
        // This disambiguates line 13's `min spaces`: free-space start alone
        // can choose a higher track even when the lower track fits at that time.
        let mut candidates = Vec::new();
        let mut ends = vec![0];
        ends.extend(scheduled.iter().map(|(p, _)| p.start_cu + p.length_cu));
        ends.sort_unstable();
        ends.dedup();
        for (space_index, space) in spaces.iter().enumerate() {
            for end in &ends {
                let pos = space.start.max(*end);
                if pos > space.end || tx.compute_limit > space.end - pos {
                    continue;
                }
                let finish = pos + tx.compute_limit;
                if scheduled.iter().any(|(p, busy)| {
                    p.start_cu < finish
                        && p.start_cu + p.length_cu > pos
                        && !accounts.is_disjoint(busy)
                }) {
                    continue;
                }
                candidates.push((pos, space.track, space_index));
                break;
            }
        }
        candidates.sort_unstable();
        if let Some((pos, _, space_index)) = candidates.first().copied() {
            let space = spaces.remove(space_index);
            let finish = pos + tx.compute_limit;
            scheduled.push((
                Placement {
                    track: space.track,
                    start_cu: pos,
                    length_cu: tx.compute_limit,
                    tx_id: tx.id.clone(),
                },
                accounts,
            ));
            if pos > space.start {
                spaces.push(Space {
                    start: space.start,
                    end: pos,
                    track: space.track,
                });
            }
            if finish < space.end {
                spaces.push(Space {
                    start: finish,
                    end: space.end,
                    track: space.track,
                });
            }
            selected_ids.push(tx.id.clone());
        } else {
            rejected_ids.push(tx.id.clone());
        }
    }
    spaces.sort_by_key(|s| (s.track, s.start));
    Selection {
        algorithm: 4,
        tracks: TRACKS,
        capacity_cu: capacity,
        placements: scheduled.into_iter().map(|(p, _)| p).collect(),
        gaps: spaces
            .into_iter()
            .map(|s| Gap {
                track: s.track,
                start_cu: s.start,
                length_cu: s.end - s.start,
            })
            .collect(),
        selected_ids,
        rejected_ids,
    }
}

pub fn select_transactions(txs: &[Candidate], capacity: u64) -> Selection {
    select_with_fees(txs, capacity, |_| true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn figure_five_contention_and_gap_filling() {
        // Figure 5 supplies account conflicts and order, but no numeric CU scale.
        let accounts = [
            "AB", "A", "CD", "E", "F", "BE", "CF", "D", "BD", "G", "E", "AD", "F",
        ];
        let lengths = [4, 3, 6, 5, 3, 3, 4, 3, 5, 4, 2, 4, 4];
        let txs: Vec<_> = accounts
            .iter()
            .enumerate()
            .map(|(i, a)| Candidate {
                id: format!("t{}", i + 1),
                priority_fee_per_cu: (13 - i) as u64,
                compute_limit: lengths[i],
                writable_accounts: a.chars().map(|c| c.to_string()).collect(),
            })
            .collect();
        let result = select_transactions(&txs, 14);
        assert_eq!(result.selected_ids.len(), 12);
        assert_eq!(result.rejected_ids, ["t12"]);
        assert_eq!(result.placements[1].track, 1);
        assert_eq!(result.placements[1].start_cu, 4);
        assert_eq!(result.placements[2].track, 2);
        for (i, a) in result.placements.iter().enumerate() {
            assert!(a.start_cu + a.length_cu <= 14);
            for b in result.placements.iter().skip(i + 1) {
                let overlap =
                    a.start_cu < b.start_cu + b.length_cu && b.start_cu < a.start_cu + a.length_cu;
                let at = txs.iter().find(|t| t.id == a.tx_id).unwrap();
                let bt = txs.iter().find(|t| t.id == b.tx_id).unwrap();
                assert!(
                    !overlap
                        || (a.track != b.track
                            && !at
                                .writable_accounts
                                .iter()
                                .any(|key| bt.writable_accounts.contains(key)))
                );
            }
        }
        assert!(result.gaps.iter().any(|gap| gap.start_cu > 0));
    }

    #[test]
    fn tie_breaks_and_failed_priority_charge_are_deterministic() {
        let tx = |id: &str, cu| Candidate {
            id: id.into(),
            priority_fee_per_cu: 1,
            compute_limit: cu,
            writable_accounts: vec![],
        };
        let result = select_with_fees(&[tx("b", 4), tx("a", 4), tx("c", 2)], 4, |t| t.id != "a");
        assert_eq!(result.selected_ids, ["c", "b"]);
        assert_eq!(result.rejected_ids, ["a"]);
        assert_eq!(result.placements[0].track, 1);
        assert_eq!(result.placements[1].track, 2);
    }
}
