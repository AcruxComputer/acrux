//! The transactions people send, kept for days after the gateway's own records age out: each by
//! its Solana signature with its execution record and its Acrux record, found by signature, by
//! Acrux id and by every address it names. Validator 0 writes each as its cblock finalizes; the
//! built-in traffic and the faucet's credits are not kept. The oldest go once they pass the kept
//! days or the kept count, whichever comes first.
use crate::solana::Executed;
use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solana_sdk::transaction::VersionedTransaction;
use std::path::Path;

const TRANSACTIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("transactions");
const IDS: TableDefinition<&str, &str> = TableDefinition::new("ids");
/// `address/slot-from-newest/signature`: an address's signatures, newest first.
const BY_ADDRESS: TableDefinition<&str, ()> = TableDefinition::new("by_address");
/// `slot/signature`, oldest first, for aging out.
const BY_AGE: TableDefinition<&str, ()> = TableDefinition::new("by_age");
const COUNT: TableDefinition<&str, u64> = TableDefinition::new("count");
/// Outside use by the hour (Unix hours): transactions, and `hour/fee payer` for the distinct payers.
const HOURS: TableDefinition<u64, u64> = TableDefinition::new("hours");
const PAYERS: TableDefinition<&str, ()> = TableDefinition::new("payers");

#[derive(Clone, Serialize, Deserialize)]
pub struct Kept {
    pub executed: Executed,
    /// The Acrux transaction record as the gateway held it, JSON.
    pub record: Option<String>,
}

impl Kept {
    /// The Acrux record, finalized: a kept transaction is one whose cblock finalized.
    pub fn record(&self) -> Option<Value> {
        let mut record: Value = serde_json::from_str(self.record.as_deref()?).ok()?;
        record["state"] = "finalized".into();
        Some(record)
    }
}

pub struct History {
    db: Database,
    keep_slots: u64,
    keep_max: u64,
}

fn address_key(address: &str, slot: u64, signature: &str) -> String {
    format!("{address}/{:016x}/{signature}", u64::MAX - slot)
}

fn payer_key(hour: u64, payer: &str) -> String {
    format!("{hour:016x}/{payer}")
}

fn addresses(executed: &Executed) -> Vec<String> {
    bincode::deserialize::<VersionedTransaction>(&executed.bytes)
        .map(|tx| tx.message.static_account_keys().iter().map(|k| k.to_string()).collect())
        .unwrap_or_default()
}

impl History {
    pub fn open(path: &Path, keep_days: u64, keep_max: u64) -> Result<Self> {
        std::fs::create_dir_all(path.parent().context("history path")?)?;
        let db = Database::create(path)?;
        let w = db.begin_write()?;
        {
            w.open_table(TRANSACTIONS)?;
            w.open_table(IDS)?;
            w.open_table(BY_ADDRESS)?;
            w.open_table(BY_AGE)?;
            w.open_table(COUNT)?;
            w.open_table(HOURS)?;
            w.open_table(PAYERS)?;
        }
        w.commit()?;
        Ok(Self { db, keep_slots: keep_days * 86_400 * 20, keep_max })
    }

    /// Writes finalized transactions in one transaction, then ages out what has passed the limits.
    pub fn write(&self, kept: Vec<(String, Kept)>) -> Result<()> {
        let Some(newest) = kept.iter().map(|(_, k)| k.executed.slot).max() else {
            return Ok(());
        };
        let w = self.db.begin_write()?;
        {
            let mut transactions = w.open_table(TRANSACTIONS)?;
            let mut ids = w.open_table(IDS)?;
            let mut by_address = w.open_table(BY_ADDRESS)?;
            let mut by_age = w.open_table(BY_AGE)?;
            let mut count = w.open_table(COUNT)?;
            let mut hours = w.open_table(HOURS)?;
            let mut payers = w.open_table(PAYERS)?;
            let mut held = count.get("transactions")?.map_or(0, |v| v.value());
            for (signature, entry) in kept {
                let slot = entry.executed.slot;
                if transactions.insert(signature.as_str(), bincode::serialize(&entry)?.as_slice())?.is_none() {
                    held += 1;
                    let hour = entry.executed.time.max(0) as u64 / 3600;
                    let used = hours.get(hour)?.map_or(0, |v| v.value());
                    hours.insert(hour, used + 1)?;
                    if let Some(payer) = addresses(&entry.executed).first() {
                        payers.insert(payer_key(hour, payer).as_str(), ())?;
                    }
                }
                ids.insert(entry.executed.id.as_str(), signature.as_str())?;
                for address in addresses(&entry.executed) {
                    by_address.insert(address_key(&address, slot, &signature).as_str(), ())?;
                }
                by_age.insert(format!("{slot:016x}/{signature}").as_str(), ())?;
            }
            // At most a few thousand out per write, so a write stays short; the rest go next time.
            let oldest_kept = newest.saturating_sub(self.keep_slots);
            let mut expired = vec![];
            for entry in by_age.iter()?.take(4_096) {
                let key = entry?.0.value().to_owned();
                let slot = u64::from_str_radix(&key[..16], 16)?;
                if slot >= oldest_kept && held - expired.len() as u64 <= self.keep_max {
                    break;
                }
                expired.push(key);
            }
            for key in &expired {
                by_age.remove(key.as_str())?;
                let signature = &key[17..];
                let Some(entry) = transactions.remove(signature)? else { continue };
                let entry: Kept = bincode::deserialize(entry.value())?;
                ids.remove(entry.executed.id.as_str())?;
                for address in addresses(&entry.executed) {
                    by_address.remove(address_key(&address, entry.executed.slot, signature).as_str())?;
                }
            }
            count.insert("transactions", held - expired.len() as u64)?;
            // Hours are counted for as long as transactions are kept.
            let first_hour = (crate::solana::now().max(0) as u64 / 3600).saturating_sub(self.keep_slots / 72_000);
            let old = hours.range(..first_hour)?.take(64).map(|e| e.map(|e| e.0.value())).collect::<Result<Vec<_>, _>>()?;
            for hour in old {
                hours.remove(hour)?;
                let end = payer_key(hour + 1, "");
                let gone = payers.range(payer_key(hour, "").as_str()..end.as_str())?.map(|e| e.map(|e| e.0.value().to_owned())).collect::<Result<Vec<_>, _>>()?;
                for key in gone {
                    payers.remove(key.as_str())?;
                }
            }
        }
        w.commit()?;
        Ok(())
    }

    pub fn get(&self, signature: &str) -> Result<Option<Kept>> {
        let r = self.db.begin_read()?;
        Ok(match r.open_table(TRANSACTIONS)?.get(signature)? {
            Some(entry) => Some(bincode::deserialize(entry.value())?),
            None => None,
        })
    }

    pub fn signature_of(&self, id: &str) -> Result<Option<String>> {
        let r = self.db.begin_read()?;
        Ok(r.open_table(IDS)?.get(id)?.map(|s| s.value().to_owned()))
    }

    /// An address's kept signatures, newest first: those older than slot `older_than`, skipping
    /// `skip`, up to `limit`, stopping at `until`.
    pub fn signatures_for(&self, address: &str, older_than: Option<u64>, skip: &dyn Fn(&str) -> bool, until: Option<&str>, limit: usize) -> Result<Vec<(String, Kept)>> {
        let r = self.db.begin_read()?;
        let by_address = r.open_table(BY_ADDRESS)?;
        let transactions = r.open_table(TRANSACTIONS)?;
        let from = match older_than {
            Some(0) => return Ok(vec![]),
            Some(slot) => format!("{address}/{:016x}/", u64::MAX - (slot - 1)),
            None => format!("{address}/"),
        };
        let end = format!("{address}0");
        let mut out = vec![];
        for entry in by_address.range(from.as_str()..end.as_str())? {
            let key = entry?.0.value().to_owned();
            let signature = &key[address.len() + 18..];
            if Some(signature) == until || out.len() == limit {
                break;
            }
            if skip(signature) {
                continue;
            }
            if let Some(entry) = transactions.get(signature)? {
                out.push((signature.to_owned(), bincode::deserialize(entry.value())?));
            }
        }
        Ok(out)
    }

    /// Outside use in each of the last `hours` Unix hours, newest first: transactions sent and their
    /// distinct fee payers.
    pub fn use_by_hour(&self, hours: u64) -> Result<Vec<(u64, u64, u64)>> {
        let r = self.db.begin_read()?;
        let counts = r.open_table(HOURS)?;
        let payers = r.open_table(PAYERS)?;
        let now = crate::solana::now().max(0) as u64 / 3600;
        (0..hours.min(now + 1))
            .map(|back| {
                let hour = now - back;
                let sent = counts.get(hour)?.map_or(0, |v| v.value());
                let distinct = if sent == 0 { 0 } else { payers.range(payer_key(hour, "").as_str()..payer_key(hour + 1, "").as_str())?.count() as u64 };
                Ok((hour, sent, distinct))
            })
            .collect()
    }

    pub fn count(&self) -> Result<u64> {
        let r = self.db.begin_read()?;
        Ok(r.open_table(COUNT)?.get("transactions")?.map_or(0, |v| v.value()))
    }
}

/// The writer thread: one write per finalized cblock that has kept transactions.
pub fn spawn_writer(history: std::sync::Arc<History>) -> Result<std::sync::mpsc::Sender<Vec<(String, Kept)>>> {
    let (sender, receiver) = std::sync::mpsc::channel::<Vec<(String, Kept)>>();
    std::thread::Builder::new().name("acrux-history".into()).spawn(move || {
        while let Ok(mut batch) = receiver.recv() {
            while let Ok(more) = receiver.try_recv() {
                batch.extend(more);
            }
            if let Err(error) = history.write(batch) {
                tracing::error!(%error, "keeping transactions failed");
            }
        }
    })?;
    Ok(sender)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{hash::Hash, signature::{Keypair, Signer}, system_instruction, transaction::Transaction};

    fn kept(payer: &Keypair, to: &solana_sdk::pubkey::Pubkey, slot: u64, lamports: u64) -> (String, Kept) {
        let tx = Transaction::new_signed_with_payer(&[system_instruction::transfer(&payer.pubkey(), to, lamports)], Some(&payer.pubkey()), &[payer], Hash::default());
        let executed = Executed { id: format!("{slot:064x}"), slot, time: crate::solana::now(), bytes: bincode::serialize(&tx).unwrap(), fee: 5000, err: None, logs: vec![], units: 150, pre_balances: vec![], post_balances: vec![], pre_tokens: vec![], post_tokens: vec![] };
        (tx.signatures[0].to_string(), Kept { executed, record: None })
    }

    #[test]
    fn kept_transactions_are_found_by_signature_id_and_address_and_bounded() {
        let dir = std::env::temp_dir().join(format!("acrux-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let history = History::open(&dir.join("history.redb"), 7, 3).unwrap();
        let (a, b) = (Keypair::new(), Keypair::new());
        let to = solana_sdk::pubkey::Pubkey::new_unique();
        let first = kept(&a, &to, 10, 1);
        let second = kept(&b, &to, 20, 2);
        history.write(vec![first.clone(), second.clone()]).unwrap();
        assert_eq!(history.get(&first.0).unwrap().unwrap().executed.slot, 10);
        assert_eq!(history.signature_of(&first.1.executed.id).unwrap().as_deref(), Some(first.0.as_str()));
        let newest_first = history.signatures_for(&to.to_string(), None, &|_| false, None, 10).unwrap();
        assert_eq!(newest_first.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(), [second.0.as_str(), first.0.as_str()]);
        assert_eq!(history.signatures_for(&to.to_string(), Some(20), &|_| false, None, 10).unwrap().len(), 1);
        assert_eq!(history.use_by_hour(1).unwrap()[0].1..=history.use_by_hour(1).unwrap()[0].2, 2..=2);
        // Past the kept count, the oldest go first.
        history.write(vec![kept(&a, &to, 30, 3), kept(&a, &to, 40, 4)]).unwrap();
        assert_eq!(history.count().unwrap(), 3);
        assert!(history.get(&first.0).unwrap().is_none());
        assert!(history.signature_of(&first.1.executed.id).unwrap().is_none());
        assert_eq!(history.signatures_for(&to.to_string(), None, &|_| false, None, 10).unwrap().len(), 3);
        drop(history);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
