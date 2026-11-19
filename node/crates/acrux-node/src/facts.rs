//! Rolling measurements of the running net, served by the gateway's `getFacts`.
use crate::transport::{great_circle_km, Peer};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

/// Add measured exchange reads without changing the existing facts series.
pub fn include_hash_exchange(facts:&mut Value,mut measurements:Value) {
    facts["commitSpreadByLinkUs"]=measurements["commitSpreadByLinkUs"].take();
    measurements.as_object_mut().unwrap().remove("commitSpreadByLinkUs");
    facts["hashExchange"]=measurements;
}

/// About ten minutes of 50 ms slots.
const KEEP: usize = 12_000;
/// Handovers are one per window; keep the last thousand per link.
const KEEP_HANDOVERS: usize = 1_000;

#[derive(Default)]
pub struct Facts {
    within: VecDeque<i64>,
    handover: BTreeMap<String, VecDeque<i64>>,
    spread: VecDeque<i64>,
    attestation: VecDeque<i64>,
    finalization: VecDeque<i64>,
    floors: BTreeMap<String, u64>,
}

fn push<T>(ring: &mut VecDeque<T>, keep: usize, value: T) {
    ring.push_back(value);
    while ring.len() > keep {
        ring.pop_front();
    }
}

fn percentiles(ring: &VecDeque<i64>) -> (i64, i64) {
    if ring.is_empty() {
        return (0, 0);
    }
    let mut values = ring.iter().copied().collect::<Vec<_>>();
    values.sort_unstable();
    // Nearest rank: the smallest value at or above the given share of samples.
    let n = values.len();
    (values[(n - 1) / 2], values[(n * 99).div_ceil(100) - 1])
}

impl Facts {
    pub fn new(peers: &[Peer]) -> Self {
        Self { floors: rotor_floors(peers), ..Self::default() }
    }
    pub fn within_window(&mut self, gap: u64) {
        push(&mut self.within, KEEP, gap as i64);
    }
    pub fn handover(&mut self, link: String, gap: u64) {
        push(self.handover.entry(link).or_default(), KEEP_HANDOVERS, gap as i64);
    }
    pub fn commit_spread(&mut self, spread: u64) {
        push(&mut self.spread, KEEP, spread as i64);
    }
    pub fn attestation(&mut self, relative_to_block: i64) {
        push(&mut self.attestation, KEEP * 12, relative_to_block);
    }
    pub fn finalization(&mut self, after_cblock: u64) {
        push(&mut self.finalization, KEEP / 4, after_cblock as i64);
    }
    pub fn json(&self, measured_at: String) -> Value {
        let pair = |(p50, p99): (i64, i64)| json!({"p50": p50, "p99": p99});
        let handover = self
            .handover
            .iter()
            .map(|(link, ring)| {
                let (p50, p99) = percentiles(ring);
                (link.clone(), json!({"p50": p50, "p99": p99, "floorUs": self.floors.get(link).copied().unwrap_or(0), "count": ring.len()}))
            })
            .collect::<serde_json::Map<_, _>>();
        json!({
            "source": "measured",
            "measuredAt": measured_at,
            "sampleSize": self.within.len(),
            "slotPacingUs": pair(percentiles(&self.within)),
            "commitSpreadUs": pair(percentiles(&self.spread)),
            "attestationRelativeToBlockUs": pair(percentiles(&self.attestation)),
            "cblockFinalizationUs": pair(percentiles(&self.finalization)),
            "handoverUs": handover,
        })
    }
}

/// Mean one-way delay of the transport's link model, in microseconds.
fn link_us(a: &Peer, b: &Peer) -> u64 {
    if a.hub == b.hub {
        return 2_500; // 1 to 4 ms inside a hub
    }
    let km = great_circle_km(a.latitude, a.longitude, b.latitude, b.longitude);
    (km / 199_861.638 * 1_000_000.0).round() as u64 + 8_000 + 1_000
}

fn second_smallest(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[1.min(values.len() - 1)]
}

/// The earliest a window can follow the previous hub's window: ParentReady at the next hub needs
/// a 60% notarization of the last cblock, which is the votes of two hubs; a hub votes once it has
/// the attestations and 32 of 64 shreds, which Rotor relays through validators in every hub.
/// The first block then completes one slot (50 ms) after ParentReady.
pub fn rotor_floors(peers: &[Peer]) -> BTreeMap<String, u64> {
    let mut hubs: Vec<&Peer> = vec![];
    for peer in peers {
        if !hubs.iter().any(|h| h.hub == peer.hub) {
            hubs.push(peer);
        }
    }
    let mut floors = BTreeMap::new();
    for a in &hubs {
        for b in &hubs {
            let executed = hubs
                .iter()
                .map(|x| {
                    let attested = 5_000 + link_us(a, x);
                    let shreds = second_smallest(hubs.iter().map(|r| link_us(a, r) + link_us(r, x)).collect());
                    attested.max(shreds)
                })
                .collect::<Vec<_>>();
            let ready = second_smallest(hubs.iter().zip(&executed).map(|(x, at)| at + link_us(x, b)).collect());
            floors.insert(format!("{}-{}", a.hub, b.hub), 50_000 + ready);
        }
    }
    floors
}
