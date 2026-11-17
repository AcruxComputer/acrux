//! The chain's state on disk: every account, the executed transaction ids and the rest of the
//! executor's state at the latest finalized cblock, with what the net needs to go on from there.
//! Validator 0 writes it as the chain finalizes, only what changed, in one transaction each time,
//! so a restart or a crash continues from the last finalized cblock instead of from genesis.
use crate::execution::{ExecutorCheckpoint, ExecutorSnapshot};
use anyhow::{ensure, Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;

const ACCOUNTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accounts");
const EXECUTED: TableDefinition<&str, u64> = TableDefinition::new("executed");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// The layout of this file. A later version reads every earlier one.
pub const FORMAT: u64 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub format: u64,
    /// The finalized cblock the state is at, and its hash in hex.
    pub cblock: u64,
    pub hash: String,
    /// The executor's state but its accounts and executed ids, which have tables of their own.
    pub executor: ExecutorSnapshot,
    pub threshold: crate::threshold::ThresholdCache,
    /// The built-in traffic's sending keys, base58: their balances are part of the state.
    pub senders: Vec<String>,
    pub finalized_cblocks: u64,
    pub skips: u64,
    /// The chain's genesis hash, which wallets know it by: the first run's, kept across runs.
    pub genesis_hash: String,
    pub saved_at_ms: u64,
}

pub struct Saved {
    pub meta: Meta,
    /// The whole executor state, accounts and executed ids included.
    pub state: ExecutorSnapshot,
}

pub struct StateStore(Database);

impl StateStore {
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path.parent().context("state path")?)?;
        let db = Database::create(path)?;
        let w = db.begin_write()?;
        {
            w.open_table(ACCOUNTS)?;
            w.open_table(EXECUTED)?;
            w.open_table(META)?;
        }
        w.commit()?;
        Ok(Self(db))
    }

    pub fn load(&self) -> Result<Option<Saved>> {
        let r = self.0.begin_read()?;
        let Some(meta) = r.open_table(META)?.get("state")? else {
            return Ok(None);
        };
        let meta: Meta = serde_json::from_slice(meta.value())?;
        ensure!(meta.format <= FORMAT, "the saved state is format {}, newer than this build's {FORMAT}", meta.format);
        let mut state = meta.executor.clone();
        for entry in r.open_table(ACCOUNTS)?.iter()? {
            let (key, value) = entry?;
            let key: [u8; 32] = key.value().try_into().context("account key")?;
            state.accounts.push((bs58::encode(key).into_string(), bincode::deserialize(value.value())?));
        }
        for entry in r.open_table(EXECUTED)?.iter()? {
            let (id, slot) = entry?;
            state.executed.push((id.value().to_owned(), slot.value()));
        }
        Ok(Some(Saved { meta, state }))
    }

    /// Writes `checkpoint` over `earlier`, the state this store holds, in one transaction.
    pub fn write(&self, checkpoint: &ExecutorCheckpoint, earlier: Option<&ExecutorCheckpoint>, meta: &Meta) -> Result<()> {
        let (accounts, gone) = checkpoint.account_changes(earlier);
        let (executed, expired) = checkpoint.executed_changes(earlier);
        let w = self.0.begin_write()?;
        {
            let mut table = w.open_table(ACCOUNTS)?;
            for key in gone {
                table.remove(key.as_ref())?;
            }
            for (key, account) in accounts {
                table.insert(key.as_ref(), bincode::serialize(account.as_ref())?.as_slice())?;
            }
            let mut table = w.open_table(EXECUTED)?;
            for id in expired {
                table.remove(id.as_str())?;
            }
            for (id, slot) in executed {
                table.insert(id.as_str(), slot)?;
            }
            w.open_table(META)?.insert("state", serde_json::to_vec(meta)?.as_slice())?;
        }
        w.commit()?;
        Ok(())
    }
}

pub struct Pending {
    pub checkpoint: ExecutorCheckpoint,
    pub meta: Meta,
}

/// The writer thread: takes the newest finalized state queued and writes what changed since the
/// one before; states queued while it writes are passed over for the newest.
pub fn spawn_writer(store: StateStore, mut written: Option<ExecutorCheckpoint>) -> Result<std::sync::mpsc::Sender<Pending>> {
    let (sender, receiver) = std::sync::mpsc::channel::<Pending>();
    std::thread::Builder::new().name("acrux-state".into()).spawn(move || {
        while let Ok(mut pending) = receiver.recv() {
            while let Ok(newer) = receiver.try_recv() {
                pending = newer;
            }
            match store.write(&pending.checkpoint, written.as_ref(), &pending.meta) {
                Ok(()) => written = Some(pending.checkpoint),
                Err(error) => tracing::error!(%error, cblock = pending.meta.cblock, "writing the chain state failed"),
            }
        }
    })?;
    Ok(sender)
}
