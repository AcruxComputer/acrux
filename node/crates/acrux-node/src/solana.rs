//! Solana's view of executed transactions, kept by validator 0 for the gateway: each transaction by
//! its signature, and the signatures that touched each address, newest last, for as long as the
//! store keeps slots.
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde_json::{json, Value};
use solana_sdk::transaction::VersionedTransaction;
use std::collections::{BTreeMap, VecDeque};

/// How a failed transaction's error reads in Solana's shape: the execution's own message is in its
/// logs and on `/tx/[id]`.
pub static FAILED: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| json!({"InstructionError": [0, {"Custom": 0}]}));

/// Signatures kept per address; Solana's own `getSignaturesForAddress` pages at 1,000.
const PER_ADDRESS: usize = 1_000;

pub struct Executed {
    pub id: String,
    pub slot: u64,
    pub bytes: Vec<u8>,
    pub fee: u64,
    pub err: Option<String>,
    pub logs: Vec<String>,
    pub units: u64,
}

#[derive(Default)]
pub struct SolanaIndex {
    transactions: BTreeMap<String, Executed>,
    by_slot: VecDeque<(u64, String)>,
    by_address: BTreeMap<String, VecDeque<(u64, String)>>,
}

/// Seconds the chain's clock gives a slot (`set_slot_clock` in acrux-execution).
pub fn block_time(slot: u64) -> i64 {
    1_794_787_200 + (slot / 20) as i64
}

impl SolanaIndex {
    pub fn record(&mut self, signature: String, executed: Executed) {
        let Ok(tx) = bincode::deserialize::<VersionedTransaction>(&executed.bytes) else { return };
        for key in tx.message.static_account_keys() {
            let list = self.by_address.entry(key.to_string()).or_default();
            list.push_back((executed.slot, signature.clone()));
            if list.len() > PER_ADDRESS {
                list.pop_front();
            }
        }
        self.by_slot.push_back((executed.slot, signature.clone()));
        self.transactions.insert(signature, executed);
    }

    /// Drops what is older than the store keeps.
    pub fn prune(&mut self, below: u64) {
        while self.by_slot.front().is_some_and(|(slot, _)| *slot < below) {
            let (_, signature) = self.by_slot.pop_front().unwrap();
            self.transactions.remove(&signature);
        }
        self.by_address.retain(|_, list| {
            while list.front().is_some_and(|(slot, _)| *slot < below) {
                list.pop_front();
            }
            !list.is_empty()
        });
    }

    pub fn get(&self, signature: &str) -> Option<&Executed> {
        self.transactions.get(signature)
    }

    /// Newest first, as Solana answers: up to `limit`, starting after `before` and stopping at
    /// `until` when given.
    pub fn signatures_for(&self, address: &str, limit: usize, before: Option<&str>, until: Option<&str>) -> Vec<(&str, &Executed)> {
        let Some(list) = self.by_address.get(address) else { return vec![] };
        let mut started = before.is_none();
        let mut out = vec![];
        for (_, signature) in list.iter().rev() {
            if !started {
                started = Some(signature.as_str()) == before;
                continue;
            }
            if Some(signature.as_str()) == until || out.len() == limit {
                break;
            }
            if let Some(executed) = self.transactions.get(signature) {
                out.push((signature.as_str(), executed));
            }
        }
        out
    }
}

/// `getTransaction`'s answer for one executed transaction, in the encoding asked for.
pub fn transaction_json(executed: &Executed, encoding: &str) -> Value {
    let tx: VersionedTransaction = bincode::deserialize(&executed.bytes).expect("executed bytes decode");
    let version = match &tx.message {
        solana_sdk::message::VersionedMessage::Legacy(_) => json!("legacy"),
        solana_sdk::message::VersionedMessage::V0(_) => json!(0),
    };
    let transaction = match encoding {
        "base64" => json!([B64.encode(&executed.bytes), "base64"]),
        "base58" => json!([bs58::encode(&executed.bytes).into_string(), "base58"]),
        _ => {
            let header = tx.message.header();
            json!({
                "signatures": tx.signatures.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                "message": {
                    "accountKeys": tx.message.static_account_keys().iter().map(|k| k.to_string()).collect::<Vec<_>>(),
                    "header": {
                        "numRequiredSignatures": header.num_required_signatures,
                        "numReadonlySignedAccounts": header.num_readonly_signed_accounts,
                        "numReadonlyUnsignedAccounts": header.num_readonly_unsigned_accounts,
                    },
                    "recentBlockhash": tx.message.recent_blockhash().to_string(),
                    "instructions": tx.message.instructions().iter().map(|ix| json!({
                        "programIdIndex": ix.program_id_index,
                        "accounts": ix.accounts,
                        "data": bs58::encode(&ix.data).into_string(),
                        "stackHeight": null,
                    })).collect::<Vec<_>>(),
                },
            })
        }
    };
    json!({
        "slot": executed.slot,
        "blockTime": block_time(executed.slot),
        "version": version,
        "transaction": transaction,
        "meta": {
            "err": executed.err.as_ref().map(|_| FAILED.clone()),
            "status": match &executed.err { None => json!({"Ok": null}), Some(_) => json!({"Err": *FAILED}) },
            "fee": executed.fee,
            "preBalances": [],
            "postBalances": [],
            "innerInstructions": [],
            "logMessages": executed.logs,
            "preTokenBalances": [],
            "postTokenBalances": [],
            "rewards": [],
            "loadedAddresses": {"writable": [], "readonly": []},
            "computeUnitsConsumed": executed.units,
        },
    })
}
