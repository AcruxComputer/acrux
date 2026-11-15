//! Incremental projection of observed protocol events into one slot snapshot.
//! No state, timing, keys, or certificates are inferred here.
use serde_json::{json, Value};

pub fn apply(view: &mut Value, kind: &str, data: &Value) {
    match kind {
        "slot.started" => {
            merge_slot(view, data);
        }
        "block.slice" => {
            if let Some(block) = proposer_block(view, &data["proposer"]) {
                let slice = &data["slice"];
                upsert(&mut block["slices"], slice, |old| {
                    old["index"] == slice["index"]
                });
                if let Some(slices) = block["slices"].as_array_mut() {
                    slices.sort_by_key(|item| item["index"].as_u64().unwrap_or(0));
                    let bytes: u64 = slices
                        .iter()
                        .filter_map(|item| item["bytes"].as_u64())
                        .sum();
                    block["bytes"] = json!(bytes);
                }
            }
        }
        "commitment.sent" => {
            if let Some(block) = proposer_block(view, &data["proposer"]) {
                let arrivals = block["commitment"]["arrivals"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                block["commitment"] = data.clone();
                for arrival in arrivals {
                    upsert(&mut block["commitment"]["arrivals"], &arrival, |old| {
                        old["receiver"] == arrival["receiver"]
                    });
                }
                if block["status"] == "streaming" {
                    block["status"] = json!("sealed");
                }
            }
        }
        "commitment.arrived" => {
            if let Some(block) = proposer_block(view, &data["proposer"]) {
                if block["commitment"].is_object() {
                    let arrival = &data["arrival"];
                    upsert(&mut block["commitment"]["arrivals"], arrival, |old| {
                        old["receiver"] == arrival["receiver"]
                    });
                    if let Some(arrivals) = block["commitment"]["arrivals"].as_array_mut() {
                        arrivals.sort_by_key(|item| item["receiver"].as_u64().unwrap_or(0));
                    }
                }
            }
        }
        "attestation.sent" => {
            record_attestation(view, data, true);
        }
        "attestation.arrived" => {
            let attestation = &data["attestation"];
            record_attestation(view, attestation, false);
            let arrival = json!({ "identity": data["validator"], "arrivedUs": data["arrivedUs"] });
            if let Some(item) = attestation_mut(view, &attestation["proposer"]) {
                upsert(&mut item["validatorArrivals"], &arrival, |old| {
                    old["identity"] == arrival["identity"]
                });
                if let Some(arrivals) = item["validatorArrivals"].as_array_mut() {
                    arrivals.sort_by(|a, b| a["identity"].as_str().cmp(&b["identity"].as_str()));
                }
            }
            sync_attestation_block(view, &attestation["proposer"]);
        }
        "slot.agreed" => {
            view["agreedSet"] = data["agreedSet"].clone();
        }
        "slot.skipped" => {
            view["status"] = json!("skipped");
            view["skip"] = data.clone();
            view["agreedSet"] = Value::Null;
            view["mergedOrder"] = json!([]);
            view["schedule"] = Value::Null;
            view["stateHash"] = Value::Null;
            view["xblockHash"] = Value::Null;
            view["executedUs"] = Value::Null;
            if let Some(blocks) = view["blocks"].as_array_mut() {
                for block in blocks {
                    if block["status"] != "missing" {
                        block["status"] = json!("dropped");
                    }
                }
            }
        }
        "block.decrypted" => {
            if let Some(block) = proposer_block(view, &data["proposer"]) {
                block["status"] = json!("accepted");
            }
        }
        "slot.merged" => {
            view["mergedOrder"] = data["transactions"].clone();
        }
        "slot.scheduled" => {
            view["schedule"] = data.clone();
        }
        "slot.executed" => {
            merge_slot(view, data);
        }
        "cblock.assembled" => {
            merge_cblock(&mut view["cblock"], data);
        }
        "cblock.vote" => {
            upsert(&mut view["cblock"]["votes"], data, |old| {
                old["validator"] == data["validator"]
                    && old["kind"] == data["kind"]
                    && old["hash"] == data["hash"]
            });
        }
        "cblock.certificate" => {
            upsert(&mut view["cblock"]["certificates"], data, |old| {
                old["kind"] == data["kind"] && old["hash"] == data["hash"]
            });
            match data["kind"].as_str() {
                Some("notarization" | "notar_fallback") => {
                    if view["cblock"]["status"] != "finalized"
                        && view["cblock"]["status"] != "skipped"
                    {
                        view["cblock"]["status"] = json!("notarized");
                    }
                }
                Some("skip") => {
                    view["cblock"]["status"] = json!("skipped");
                }
                _ => {}
            }
        }
        "cblock.finalized" => {
            merge_cblock(&mut view["cblock"], data);
            view["cblock"]["status"] = json!("finalized");
            if view["status"] == "executed" {
                view["status"] = json!("finalized");
            }
        }
        _ => {}
    }
}

fn proposer_block<'a>(view: &'a mut Value, proposer: &Value) -> Option<&'a mut Value> {
    view["blocks"]
        .as_array_mut()?
        .iter_mut()
        .find(|block| block["proposer"] == *proposer)
}
fn attestation_mut<'a>(view: &'a mut Value, proposer: &Value) -> Option<&'a mut Value> {
    view["attestations"]
        .as_array_mut()?
        .iter_mut()
        .find(|item| item["proposer"] == *proposer)
}
fn upsert(items: &mut Value, incoming: &Value, same: impl Fn(&Value) -> bool) {
    if !items.is_array() {
        *items = json!([]);
    }
    let items = items.as_array_mut().unwrap();
    if let Some(index) = items.iter().position(same) {
        items[index] = incoming.clone();
    } else {
        items.push(incoming.clone());
    }
}
fn record_attestation(view: &mut Value, incoming: &Value, includes_send: bool) {
    if let Some(existing) = attestation_mut(view, &incoming["proposer"]) {
        let arrivals = existing["validatorArrivals"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let sent = existing["sentUs"].clone();
        *existing = incoming.clone();
        if !includes_send && sent.as_u64().unwrap_or(0) != 0 {
            existing["sentUs"] = sent;
        }
        for arrival in arrivals {
            upsert(&mut existing["validatorArrivals"], &arrival, |old| {
                old["identity"] == arrival["identity"]
            });
        }
    } else {
        upsert(&mut view["attestations"], incoming, |old| {
            old["proposer"] == incoming["proposer"]
        });
    }
    if let Some(attestations) = view["attestations"].as_array_mut() {
        attestations.sort_by_key(|item| item["proposer"].as_u64().unwrap_or(0));
    }
    sync_attestation_block(view, &incoming["proposer"]);
}
fn sync_attestation_block(view: &mut Value, proposer: &Value) {
    let attestation = view["attestations"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["proposer"] == *proposer))
        .cloned();
    if let (Some(attestation), Some(block)) = (attestation, proposer_block(view, proposer)) {
        block["attestation"] = attestation;
    }
}
fn merge_cblock(current: &mut Value, incoming: &Value) {
    if !current.is_object() {
        *current = incoming.clone();
        return;
    }
    let votes = current["votes"].as_array().cloned().unwrap_or_default();
    let certificates = current["certificates"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let old_status = current["status"].clone();
    let finalized_us = current["finalizedUs"].clone();
    *current = incoming.clone();
    for vote in votes {
        upsert(&mut current["votes"], &vote, |old| {
            old["validator"] == vote["validator"]
                && old["kind"] == vote["kind"]
                && old["hash"] == vote["hash"]
        });
    }
    for certificate in certificates {
        upsert(&mut current["certificates"], &certificate, |old| {
            old["kind"] == certificate["kind"] && old["hash"] == certificate["hash"]
        });
    }
    if old_status == "finalized" {
        current["status"] = old_status;
        current["finalizedUs"] = finalized_us;
    } else if old_status == "notarized" && current["status"] == "pending" {
        current["status"] = old_status;
    }
}
fn merge_slot(current: &mut Value, incoming: &Value) {
    if !current.is_object() {
        *current = incoming.clone();
        return;
    }
    let old = current.clone();
    *current = incoming.clone();
    if let Some(blocks) = old["blocks"].as_array() {
        for block in blocks {
            if let Some(target) = proposer_block(current, &block["proposer"]) {
                if target["slices"].as_array().map_or(true, Vec::is_empty) {
                    target["slices"] = block["slices"].clone();
                    target["bytes"] = block["bytes"].clone();
                }
                if target["commitment"].is_null() {
                    target["commitment"] = block["commitment"].clone();
                }
                if target["attestation"].is_null() {
                    target["attestation"] = block["attestation"].clone();
                }
            }
        }
    }
    if current["attestations"]
        .as_array()
        .map_or(true, Vec::is_empty)
    {
        current["attestations"] = old["attestations"].clone();
    }
    let incoming_cblock = current["cblock"].clone();
    current["cblock"] = old["cblock"].clone();
    merge_cblock(&mut current["cblock"], &incoming_cblock);
    if old["status"] == "finalized" && current["status"] == "executed" {
        current["status"] = json!("finalized");
    }
}
