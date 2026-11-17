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
    /// Unix seconds when validator 0 executed the slot: Solana's `blockTime`.
    pub time: i64,
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

/// Unix seconds now.
pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
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
        "blockTime": executed.time,
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

pub const TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

fn pubkey(bytes: &[u8]) -> String {
    bs58::encode(bytes).into_string()
}
fn option_key(bytes: &[u8]) -> Value {
    if bytes[..4] == [1, 0, 0, 0] { json!(pubkey(&bytes[4..36])) } else { Value::Null }
}
fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// A token amount as Solana's `jsonParsed` gives it.
pub fn token_amount(amount: u64, decimals: u8) -> Value {
    let ui = amount as f64 / 10f64.powi(decimals as i32);
    let whole = amount / 10u64.pow(decimals as u32);
    let fraction = amount % 10u64.pow(decimals as u32);
    let ui_string = if decimals == 0 || fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{:0width$}", fraction, width = decimals as usize).trim_end_matches('0').to_owned()
    };
    json!({"amount": amount.to_string(), "decimals": decimals, "uiAmount": ui, "uiAmountString": ui_string})
}

/// SPL Token and Token-2022 accounts and mints in Solana's `jsonParsed` shape, read from their base
/// layout (Token-2022 extensions are not listed). `decimals` reads a mint's decimals.
pub fn parse_token(owner: &str, data: &[u8], decimals: impl Fn(&str) -> Option<u8>) -> Option<Value> {
    let program = match owner {
        TOKEN => "spl-token",
        TOKEN_2022 => "spl-token-2022",
        _ => return None,
    };
    let parsed = if data.len() >= 165 && (owner == TOKEN_2022 || data.len() == 165) && data[108] != 0 {
        let mint = pubkey(&data[0..32]);
        let d = decimals(&mint)?;
        let native = data[109..113] == [1, 0, 0, 0];
        let mut info = json!({
            "mint": mint,
            "owner": pubkey(&data[32..64]),
            "tokenAmount": token_amount(u64_at(data, 64), d),
            "state": if data[108] == 2 { "frozen" } else { "initialized" },
            "isNative": native,
        });
        if let Value::String(delegate) = option_key(&data[72..108]) {
            info["delegate"] = json!(delegate);
            info["delegatedAmount"] = token_amount(u64_at(data, 121), d);
        }
        if native {
            info["rentExemptReserve"] = token_amount(u64_at(data, 113), d);
        }
        if let Value::String(close) = option_key(&data[129..165]) {
            info["closeAuthority"] = json!(close);
        }
        json!({"type": "account", "info": info})
    } else if data.len() >= 82 && data[45] == 1 {
        json!({"type": "mint", "info": {
            "mintAuthority": option_key(&data[0..36]),
            "supply": u64_at(data, 36).to_string(),
            "decimals": data[44],
            "isInitialized": true,
            "freezeAuthority": option_key(&data[46..82]),
        }})
    } else {
        return None;
    };
    Some(json!({"program": program, "parsed": parsed, "space": data.len()}))
}

/// One account in the encoding asked for: `base64` (the default), `base58`, or `jsonParsed`, which
/// parses token accounts and mints and gives every other account as base64, as Solana does.
pub fn account_json(account: &solana_sdk::account::Account, encoding: &str, decimals: impl Fn(&str) -> Option<u8>) -> Value {
    let owner = account.owner.to_string();
    let data = match encoding {
        "base58" => json!([bs58::encode(&account.data).into_string(), "base58"]),
        "jsonParsed" => parse_token(&owner, &account.data, decimals).unwrap_or_else(|| json!([B64.encode(&account.data), "base64"])),
        _ => json!([B64.encode(&account.data), "base64"]),
    };
    json!({"lamports": account.lamports, "owner": owner, "data": data, "executable": account.executable, "rentEpoch": u64::MAX, "space": account.data.len()})
}
