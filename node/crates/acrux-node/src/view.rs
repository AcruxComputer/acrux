//! Browser contract views of locally observed protocol objects.
use crate::{consensus, schedule};
use serde_json::{json, Value};

/// Proposer indices in the browser contract are local roles 1..=3, not validator ids.
/// `crashed` uses that same local role; identities and stake use the epoch snapshot.
pub fn window_json(
    number: u64,
    window: &schedule::Window,
    identities: &[String],
    stakes: &[u64],
    crashed: Option<usize>,
    genesis_identities: &[String],
) -> Value {
    let proposers: Vec<Value> = window.proposers.iter().enumerate().map(|(index, identity)| {
        let validator = identities.iter().position(|candidate| candidate == identity)
            .expect("the schedule must contain only configured validators");
        json!({ "index": index + 1, "identity": identity, "stake": stakes[validator].to_string(),
            "status": if crashed == Some(index + 1) { "crashed" } else { "online" },
            "validatorKind": if genesis_identities.contains(identity) {"genesis"} else {"external"} })
    }).collect();
    json!({ "number": number, "hubCode": window.hub_code, "firstSlot": number * 16,
        "lastSlot": number * 16 + 15, "consensusSlots": [number * 4, number * 4 + 1, number * 4 + 2, number * 4 + 3],
        "proposers": proposers })
}

/// Deadlines are relative to window ParentReady. All other slot view timings are
/// relative to this slot's anchor; vote/certificate timings use their cblock anchor.
pub fn pending_slot(slot: u64, window: Value, wall_time: String) -> Value {
    let consensus_slot = slot / 4;
    let blocks: Vec<Value> = window["proposers"]
        .as_array()
        .expect("window has proposers")
        .iter()
        .map(|proposer| {
            json!({ "proposer": proposer["index"], "identity": proposer["identity"], "validatorKind":proposer["validatorKind"],
            "status": if proposer["status"] == "crashed" { "missing" } else { "streaming" },
            "bytes": 0, "slices": [], "commitment": null, "attestation": null, "parent": null })
        })
        .collect();
    json!({ "slot": slot, "wallTime": wall_time, "window": window, "consensusSlot": consensus_slot,
        "status": "pending", "blocks": blocks, "attestations": [], "agreedSet": null,
        "skip": null, "mergedOrder": [], "schedule": null, "stateHash": null,
        "xblockHash": null, "cblock": cblock_json(consensus_slot, None, "", "", "pending"),
        "executedUs": null, "attestationDeadlineUs": 570000,
        "consensusTimeoutUs": 750000 + (consensus_slot % 4 + 1) * 200000 + 50000 })
}

pub fn vote_json(
    vote: &consensus::Vote,
    identities: &[String],
    stakes: &[u64],
    at_us: u64,
) -> Value {
    let total: u128 = stakes.iter().map(|stake| *stake as u128).sum();
    let kind = match vote.kind {
        consensus::VoteKind::Notar => "notar",
        consensus::VoteKind::NotarFallback => "notar_fallback",
        consensus::VoteKind::Skip => "skip",
        consensus::VoteKind::SkipFallback => "skip_fallback",
        consensus::VoteKind::Finalize => "finalize",
    };
    json!({ "consensusSlot": vote.slot, "validator": identities[vote.validator], "kind": kind,
        "stakeShare": stakes[vote.validator] as f64 / total as f64,
        "hash": vote.hash.map(hex_hash), "atUs": at_us })
}

pub fn certificate_json(
    certificate: &consensus::Certificate,
    identities: &[String],
    stakes: &[u64],
    at_us: u64,
    first_skip: Option<u64>,
) -> Value {
    let total: u128 = stakes.iter().map(|stake| *stake as u128).sum();
    let mut signers = std::collections::BTreeSet::new();
    for vote in &certificate.votes {
        signers.insert(vote.validator);
    }
    let signed_stake: u128 = signers
        .iter()
        .map(|validator| stakes[*validator] as u128)
        .sum();
    let kind = match certificate.kind {
        consensus::CertificateKind::Notarization => "notarization",
        consensus::CertificateKind::NotarFallback => "notar_fallback",
        consensus::CertificateKind::FastFinalization => "fast_finalization",
        consensus::CertificateKind::Finalization => "slow_finalization",
        consensus::CertificateKind::Skip => "skip",
    };
    let addresses: Vec<&str> = signers
        .iter()
        .map(|validator| identities[*validator].as_str())
        .collect();
    json!({ "consensusSlot": certificate.slot, "kind": kind, "stakeShare": signed_stake as f64 / total as f64,
        "threshold": if certificate.kind == consensus::CertificateKind::FastFinalization { 0.8 } else { 0.6 },
        "hash": certificate.hash.map(hex_hash), "signers": addresses, "atUs": at_us, "linkedConsensusSlot": if certificate.kind == consensus::CertificateKind::Skip {first_skip.filter(|first|*first<certificate.slot)}else{None} })
}

pub fn cblock_json(
    consensus_slot: u64,
    parent: Option<consensus::BlockRef>,
    hash: &str,
    state_hash: &str,
    status: &str,
) -> Value {
    let parent =
        parent.map(|value| json!({ "consensusSlot": value.slot, "hash": hex_hash(value.hash) }));
    json!({ "consensusSlot": consensus_slot,
        "slots": [consensus_slot * 4, consensus_slot * 4 + 1, consensus_slot * 4 + 2, consensus_slot * 4 + 3],
        "parent": parent, "hash": if hash.is_empty() { None } else { Some(hash) },
        "stateHash": if state_hash.is_empty() { None } else { Some(state_hash) },
        "votes": [], "certificates": [], "status": status, "finalizedUs": null })
}

fn hex_hash(bytes: consensus::Hash) -> String {
    const ALPHABET: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in bytes {
        output.push(ALPHABET[(byte >> 4) as usize] as char);
        output.push(ALPHABET[(byte & 15) as usize] as char);
    }
    output
}
