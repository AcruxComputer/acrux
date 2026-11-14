//! Algorithm 3, using an epoch-seeded PRNG and regional runs.
use anyhow::{bail, Result};
use rand::{seq::SliceRandom, Rng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const PROPOSERS: usize = 3;
pub const MINIMUM_MEMBERS: usize = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub identity: String,
    pub stake: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hub {
    pub code: String,
    pub region: String,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposerStake {
    pub identity: String,
    pub stake: u64,
    pub proposer_stake: u64,
    pub capped: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubStake {
    pub code: String,
    pub region: String,
    pub cap: u64,
    pub stake: u64,
    pub members: Vec<ProposerStake>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub number: u64,
    pub hub_code: String,
    pub region: String,
    pub proposers: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Share {
    pub hub_code: String,
    pub proposer_stake: u64,
    pub probability: f64,
    pub assigned_windows: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionalRun {
    pub region: String,
    pub first_window: u64,
    pub last_window: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub epoch: u64,
    pub seed: String,
    pub alpha: f64,
    pub shares: Vec<Share>,
    pub regional_runs: Vec<RegionalRun>,
    pub windows: Vec<Window>,
}

pub fn cap_stakes(hub: &Hub) -> Result<HubStake> {
    if hub.members.len() < PROPOSERS {
        bail!("A hub needs at least three distinct members to cap proposer stake");
    }
    let mut members = hub.members.clone();
    members.sort_by(|a, b| b.stake.cmp(&a.stake).then(a.identity.cmp(&b.identity)));
    if members
        .windows(2)
        .any(|pair| pair[0].identity == pair[1].identity)
    {
        bail!("Duplicate hub member");
    }
    let mut identities: Vec<_> = members.iter().map(|member| &member.identity).collect();
    identities.sort();
    if identities.windows(2).any(|pair| pair[0] == pair[1]) {
        bail!("Duplicate hub member");
    }
    let mut cap = 0;
    for j in 0..PROPOSERS {
        let remainder: u128 = members[j..]
            .iter()
            .map(|member| u128::from(member.stake))
            .sum();
        cap = u64::try_from(remainder / (PROPOSERS - j) as u128)?;
        if members[j].stake <= cap {
            break;
        }
    }
    let members: Vec<_> = members
        .into_iter()
        .map(|member| ProposerStake {
            identity: member.identity,
            stake: member.stake,
            proposer_stake: member.stake.min(cap),
            capped: member.stake > cap,
        })
        .collect();
    let stake = members
        .iter()
        .try_fold(0u64, |sum, member| sum.checked_add(member.proposer_stake))
        .ok_or_else(|| anyhow::anyhow!("Hub stake overflow"))?;
    Ok(HubStake {
        code: hub.code.clone(),
        region: hub.region.clone(),
        cap,
        stake,
        members,
    })
}

pub fn build_schedule(
    epoch: u64,
    seed: &str,
    hubs: &[Hub],
    total_validator_stake: u64,
    windows: u64,
) -> Result<Schedule> {
    let mut active = Vec::new();
    for hub in hubs {
        if hub.members.len() < MINIMUM_MEMBERS {
            continue;
        }
        let stake = cap_stakes(hub)?;
        if u128::from(stake.stake) * 100 >= u128::from(total_validator_stake) && stake.cap > 0 {
            active.push(stake);
        }
    }
    active.sort_by(|a, b| a.code.cmp(&b.code));
    if active.is_empty() {
        bail!("No hub satisfies four members and one percent proposer stake");
    }
    let mut digest = Sha256::new();
    digest.update(b"acrux-algorithm-3-v1");
    digest.update(epoch.to_le_bytes());
    digest.update(seed.as_bytes());
    let mut rng = ChaCha20Rng::from_seed(digest.finalize().into());
    let total: u128 = active.iter().map(|hub| u128::from(hub.stake)).sum();
    let mut shares: Vec<_> = active
        .iter()
        .map(|hub| Share {
            hub_code: hub.code.clone(),
            proposer_stake: hub.stake,
            probability: 0.5 * hub.stake as f64 / total as f64 + 0.5 / active.len() as f64,
            assigned_windows: 0,
        })
        .collect();
    let mut draws = Vec::new();
    for _ in 0..windows {
        let index = if rng.gen_bool(0.5) {
            let value = rng.gen_range(0..total);
            let mut accumulated = 0u128;
            active
                .iter()
                .position(|hub| {
                    accumulated += u128::from(hub.stake);
                    value < accumulated
                })
                .unwrap()
        } else {
            rng.gen_range(0..active.len())
        };
        shares[index].assigned_windows += 1;
        draws.push(index);
    }
    // A region stays contiguous, preserving the seeded draw order inside it.
    draws.sort_by(|a, b| active[*a].region.cmp(&active[*b].region));
    let mut output = Vec::new();
    for (number, index) in draws.into_iter().enumerate() {
        let hub = &active[index];
        let mut members = hub.members.clone();
        members.shuffle(&mut rng);
        // Integer systematic sampling over exactly the total capped stake. Scaling
        // interval endpoints by k avoids losing the fractional remainder of c.
        let mut point = rng.gen_range(0..hub.stake as u128);
        let mut cumulative = 0u128;
        let mut chosen = Vec::new();
        for member in members {
            cumulative += u128::from(member.proposer_stake) * PROPOSERS as u128;
            if point < cumulative && chosen.len() < PROPOSERS {
                chosen.push(member.identity);
                point += u128::from(hub.stake);
            }
        }
        if chosen.len() != PROPOSERS {
            bail!("Systematic proposer draw did not produce three members");
        }
        chosen.shuffle(&mut rng);
        output.push(Window {
            number: number as u64,
            hub_code: hub.code.clone(),
            region: hub.region.clone(),
            proposers: chosen,
        });
    }
    let mut runs: Vec<RegionalRun> = Vec::new();
    for window in &output {
        if let Some(run) = runs.last_mut().filter(|run| run.region == window.region) {
            run.last_window = window.number;
        } else {
            runs.push(RegionalRun {
                region: window.region.clone(),
                first_window: window.number,
                last_window: window.number,
            });
        }
    }
    Ok(Schedule {
        epoch,
        seed: seed.into(),
        alpha: 0.5,
        shares,
        regional_runs: runs,
        windows: output,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleFixture {
    pub epoch: u64,
    pub seed: String,
    pub total_validator_stake: u64,
    pub window_count: u64,
    pub hubs: Vec<Hub>,
    pub expected: Schedule,
}

/// A deterministic vector shared by the API's preview and the node schedule.
pub fn write_schedule_fixture(path: &std::path::Path) -> Result<()> {
    let hubs = [
        ("FRA", "Europe"),
        ("NYC", "North America"),
        ("TYO", "Asia Pacific"),
    ]
    .iter()
    .map(|(code, region)| Hub {
        code: (*code).into(),
        region: (*region).into(),
        members: [6_000_000, 2_000_000, 1_200_000, 800_000]
            .iter()
            .enumerate()
            .map(|(index, stake)| Member {
                identity: format!("{code}-validator-{}", index + 1),
                stake: *stake,
            })
            .collect(),
    })
    .collect::<Vec<_>>();
    let expected = build_schedule(43, "acrux-schedule-v1", &hubs, 30_000_000, 48)?;
    let fixture = ScheduleFixture {
        epoch: 43,
        seed: "acrux-schedule-v1".into(),
        total_validator_stake: 30_000_000,
        window_count: 48,
        hubs,
        expected,
    };
    std::fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(&fixture)?),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caps_and_epoch_draw_match_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/schedule/paper-caps.json"))
                .unwrap();
        let hub: Hub = serde_json::from_value(fixture["hub"].clone()).unwrap();
        let capped = cap_stakes(&hub).unwrap();
        assert_eq!(capped.cap, fixture["expectedCap"].as_u64().unwrap());
        assert_eq!(capped.stake, fixture["expectedHubStake"].as_u64().unwrap());
        let first = build_schedule(43, "schedule", &[hub.clone()], 10_000_000, 64).unwrap();
        let second = build_schedule(43, "schedule", &[hub], 10_000_000, 64).unwrap();
        assert_eq!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        assert!(first.windows.iter().all(|window| {
            let unique: std::collections::BTreeSet<_> = window.proposers.iter().collect();
            unique.len() == 3
        }));
        assert!(first
            .windows
            .iter()
            .all(|window| window.proposers.contains(&"a".to_owned())
                && window.proposers.contains(&"b".to_owned())));
    }
}
