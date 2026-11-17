//! Admission rules shared by the plain and fragment submission paths.
use crate::execution::TxHeader;
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// How long after its target slot has started a send is still admitted: as long as a fragment can
/// be handed on to a later window (64 windows).
pub const LATE_SLOTS: u64 = 1_024;

/// A submission targets a slot up to 64 ahead. One whose target has already started (a client far
/// from the gateway, a wallet that took its time to sign) is still admitted, and goes to the
/// proposers now due in the same roles. Returns the window whose proposers receive it.
pub fn validate_target(latest_started: u64, target: u64) -> Result<u64> {
    ensure!(target + LATE_SLOTS > latest_started, "TRANSACTION_EXPIRED");
    ensure!(target <= latest_started + 64, "INVALID_INPUT");
    Ok(target.max(latest_started + 1) / 16)
}

/// Returns true on first admission, false for an identical retry.
/// Use the transaction ID as a plain key and `id:index` as a fragment key.
/// Call after input validation, before dispatch; an exact retry needs no second dispatch.
pub fn remember(
    fingerprints: &mut BTreeMap<String, [u8; 32]>,
    key: &str,
    payload: &Value,
) -> Result<bool> {
    let fingerprint: [u8; 32] = Sha256::digest(serde_json::to_vec(payload)?).into();
    if let Some(previous) = fingerprints.get(key) {
        ensure!(*previous == fingerprint, "IDEMPOTENCY_CONFLICT");
        return Ok(false);
    }
    fingerprints.insert(key.to_owned(), fingerprint);
    Ok(true)
}

/// The send path publishes this view before any proposer includes the transaction.
pub fn transaction_sent(header: &TxHeader, kind: &str, n: u8, t: u8, receivers: &[Value]) -> Value {
    json!({
        "header": header,
        "state": "sent",
        "kind": kind,
        "n": n,
        "t": t,
        "receivers": receivers,
        "fragments": [],
        "includedCount": 0,
        "readableSlot": null,
        "executedSlot": null,
        "mergedPosition": null,
        "result": null,
        "logs": [],
        "failure": null,
        "expiresAfterSlot": null,
        "finalization": null
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_bounds_are_relative_to_the_latest_started_slot() {
        assert_eq!(validate_target(100, 101).unwrap(), 6);
        assert_eq!(validate_target(100, 164).unwrap(), 10);
        // A target that started 300 ms before the send arrived goes to the window now due.
        assert_eq!(validate_target(100, 94).unwrap(), 6);
        assert_eq!(validate_target(2_000, 977).unwrap(), 125);
        assert_eq!(
            validate_target(2_000, 976).unwrap_err().to_string(),
            "TRANSACTION_EXPIRED"
        );
        assert_eq!(
            validate_target(100, 165).unwrap_err().to_string(),
            "INVALID_INPUT"
        );
    }

    #[test]
    fn identical_retries_are_idempotent_and_changes_conflict() {
        let mut fingerprints = BTreeMap::new();
        let original =
            json!({"header":{"id":"tx","targetSlot":10},"receivers":[1],"transaction":"AQ=="});
        assert!(remember(&mut fingerprints, "tx", &original).unwrap());
        assert!(!remember(&mut fingerprints, "tx", &original).unwrap());
        for changed in [
            json!({"header":{"id":"tx","targetSlot":11},"receivers":[1],"transaction":"AQ=="}),
            json!({"header":{"id":"tx","targetSlot":10},"receivers":[2],"transaction":"AQ=="}),
            json!({"header":{"id":"tx","targetSlot":10},"receivers":[1],"transaction":"Ag=="}),
        ] {
            assert_eq!(
                remember(&mut fingerprints, "tx", &changed)
                    .unwrap_err()
                    .to_string(),
                "IDEMPOTENCY_CONFLICT"
            );
        }
        let fragment = json!({"header":{"id":"other"},"index":0,"receiver":1,"fragment":"AQ==","keyShare":"Ag=="});
        assert!(remember(&mut fingerprints, "other:0", &fragment).unwrap());
        assert!(!remember(&mut fingerprints, "other:0", &fragment).unwrap());
        let mut changed_fragment = fragment;
        changed_fragment["keyShare"] = json!("Aw==");
        assert!(remember(&mut fingerprints, "other:0", &changed_fragment).is_err());
    }
}
