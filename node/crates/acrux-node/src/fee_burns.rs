//! Finalized burn observations. These tables are a read model, not executor state: they commit
//! with the durable checkpoint and never change a state hash or the data a follower replays.
use crate::execution::FeeBurnSnapshot;
use anyhow::{ensure, Context, Result};
use redb::{Database, ReadableTable, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

const EPOCHS: TableDefinition<u64, &[u8]> = TableDefinition::new("fee_burn_epochs");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("fee_burn_observations");

#[derive(Clone)]
pub struct Config {
    pub program: String,
    pub mint: String,
    pub native_from_slot: u64,
    pub epoch_slots: u64,
    pub scale: u64,
    pub fee_burn_from_slot: Option<u64>,
}

#[derive(Clone)]
pub struct SlotBurn {
    pub slot: u64,
    pub burned_lamports: u64,
    pub at_ms: u64,
}

#[derive(Clone)]
pub struct Observation {
    pub cblock: u64,
    pub slots: Vec<SlotBurn>,
    pub complete: bool,
    pub counter: Option<FeeBurnSnapshot>,
}

#[derive(Clone)]
pub struct Batch {
    pub config: Config,
    /// The first slot whose finalized execution this run can observe, including known skips.
    pub from_slot: u64,
    pub observations: Vec<Observation>,
}

#[derive(Default, Serialize, Deserialize)]
struct Epoch {
    base_fee_burned_lamports: Option<u64>,
    fee_burn_amount_base_units: Option<u64>,
    remainder_lamports: Option<u64>,
    started_at: Option<u64>,
    ended_at: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct Progress {
    from_slot: u64,
    through: Option<u64>,
    counter: Option<FeeBurnSnapshot>,
}

pub fn create_tables(write: &WriteTransaction) -> Result<()> {
    write.open_table(EPOCHS)?;
    write.open_table(META)?;
    Ok(())
}

/// Called inside the state checkpoint's transaction. Every queued observation is retained even
/// when the state writer passes over intermediate account checkpoints for the newest one.
pub fn write(write: &WriteTransaction, batches: &[Batch]) -> Result<()> {
    if batches.is_empty() { return Ok(()) }
    let mut epochs = write.open_table(EPOCHS)?;
    let mut metadata = write.open_table(META)?;
    let mut progress: Progress = metadata.get("progress")?.map(|v| bincode::deserialize(v.value())).transpose()?
        .unwrap_or(Progress { from_slot:batches[0].from_slot,through:None,counter:None });
    for batch in batches {
        let config = &batch.config;
        for observation in &batch.observations {
            if progress.through.is_some_and(|through| observation.cblock <= through) { continue }
            let epoch = observation.cblock * 4 / config.epoch_slots;
            if observation.cblock * 4 + 3 < config.native_from_slot {
                progress.through = Some(observation.cblock);
                continue;
            }
            let first = (epoch * config.epoch_slots).max(config.native_from_slot);
            let last = (epoch + 1).checked_mul(config.epoch_slots).context("Burn epoch overflow")? - 1;
            let mut row: Epoch = epochs.get(epoch)?.map(|v| bincode::deserialize(v.value())).transpose()?
                .unwrap_or(Epoch { base_fee_burned_lamports:(first >= progress.from_slot).then_some(0),..Epoch::default() });
            if !observation.complete { row.base_fee_burned_lamports = None; }
            for slot in observation.slots.iter().filter(|slot| slot.slot >= config.native_from_slot) {
                if let Some(total) = &mut row.base_fee_burned_lamports {
                    *total = total.checked_add(slot.burned_lamports).context("Epoch burn observation overflow")?;
                }
                if slot.slot == first { row.started_at = Some(slot.at_ms); }
                // Epoch UTC intervals are half-open: include the final slot's 50 ms.
                if slot.slot == last { row.ended_at = Some(slot.at_ms.checked_add(50).context("Epoch end time overflow")?); }
            }
            if let Some(counter) = &observation.counter {
                // An open epoch's exact sum is already in hashed state, including fees before
                // this read model was installed. At closure keep the observed sum: the counter
                // has reset it, and the rounded amount alone cannot recover the incoming dust.
                if counter.epoch == epoch && counter.last_closed_epoch != epoch {
                    row.base_fee_burned_lamports = Some(counter.epoch_burned_lamports);
                }
                if observation.cblock * 4 + 3 == last && counter.last_closed_epoch == epoch {
                    row.fee_burn_amount_base_units = Some(counter.last_closed_amount);
                    row.remainder_lamports = Some(counter.carried_lamports);
                }
                progress.counter = Some(counter.clone());
            }
            epochs.insert(epoch,bincode::serialize(&row)?.as_slice())?;
            progress.through = Some(observation.cblock);
        }
    }
    metadata.insert("progress",bincode::serialize(&progress)?.as_slice())?;
    Ok(())
}

#[derive(Clone)]
pub struct FeeBurnStore {
    pub db: Arc<Database>,
    pub config: Config,
}

impl FeeBurnStore {
    /// An upgraded process may first see an already durable checkpoint. Keep its authoritative
    /// counter without pretending its old slot observations or UTC boundaries are available.
    pub fn observe_saved(&self, cblock: u64, counter: Option<FeeBurnSnapshot>) -> Result<()> {
        let write = self.db.begin_write()?;
        self::write(&write,&[Batch { config:self.config.clone(),from_slot:cblock * 4 + 4,
            observations:vec![Observation { cblock,slots:vec![],complete:false,counter }] }])?;
        write.commit()?;
        Ok(())
    }

    /// `head` is the durably published cblock. Archived epoch-end proofs can fill rounded
    /// amounts, but cannot establish raw sums, dust, or UTC boundaries missing from the index.
    pub fn page(&self, head: Option<u64>, cursor: Option<u64>, limit: u64, archived_amount: impl Fn(u64) -> Result<Option<u64>>) -> Result<Value> {
        ensure!((1..=100).contains(&limit), "Burn page limit must be 1 to 100");
        let config = &self.config;
        let read = self.db.begin_read()?;
        let epochs = read.open_table(EPOCHS)?;
        let progress: Option<Progress> = read.open_table(META)?.get("progress")?.map(|v| bincode::deserialize(v.value())).transpose()?;
        let head = head.filter(|cblock| cblock * 4 + 3 >= config.native_from_slot);
        let first_epoch = config.native_from_slot / config.epoch_slots;
        let newest = head.map(|cblock| (cblock * 4 + 3) / config.epoch_slots);
        let last = newest.and_then(|newest| match cursor { Some(0) => None,Some(cursor) => Some(newest.min(cursor-1)),None => Some(newest) });
        let mut items = vec![];
        if let Some(last) = last.filter(|last| *last >= first_epoch) {
            for epoch in (first_epoch..=last).rev().take(limit as usize) {
                let first = (epoch * config.epoch_slots).max(config.native_from_slot);
                let end = (epoch+1).checked_mul(config.epoch_slots).context("Burn epoch overflow")? - 1;
                let closed = head.is_some_and(|cblock| cblock * 4 + 3 >= end);
                let stored = epochs.get(epoch)?.map(|v| bincode::deserialize::<Epoch>(v.value())).transpose()?;
                let missing = stored.is_none();
                let mut row = stored.unwrap_or_default();
                let fully_observed = progress.as_ref().is_some_and(|p| first >= p.from_slot && p.through.is_some_and(|c| c * 4 + 3 >= end));
                if missing && fully_observed { row.base_fee_burned_lamports = Some(0); }
                let burn_active = config.fee_burn_from_slot.is_some_and(|from| end >= from);
                if !burn_active {
                    row.fee_burn_amount_base_units = Some(0);
                    row.remainder_lamports = Some(0);
                } else if closed && row.fee_burn_amount_base_units.is_none() {
                    row.fee_burn_amount_base_units = archived_amount(epoch)?;
                }
                items.push(json!({"epoch":epoch,"firstSlot":first,"lastSlot":end,"startedAt":row.started_at,
                    "endedAt":if closed { row.ended_at } else { None },"closed":closed,
                    "baseFeeBurnedLamports":row.base_fee_burned_lamports.map(|n|n.to_string()),
                    "feeBurnAmountBaseUnits":row.fee_burn_amount_base_units.map(|n|n.to_string()),
                    "remainderLamports":row.remainder_lamports.map(|n|n.to_string())}));
            }
        }
        let next = items.last().and_then(|row|row["epoch"].as_u64()).filter(|epoch|*epoch>first_epoch).map(|epoch|epoch.to_string());
        let allocated = progress.and_then(|p|p.counter).map(|counter| -> Result<u64> {
            Ok(counter.raw_burned_lamports.checked_sub(counter.epoch_burned_lamports).and_then(|n|n.checked_sub(counter.carried_lamports)).context("Invalid committed fee burn observation")? / config.scale)
        }).transpose()?.unwrap_or(0);
        Ok(json!({"program":config.program,"mint":config.mint,"nativeFromSlot":config.native_from_slot,
            "epochSlots":config.epoch_slots,"items":items,"nextCursor":next,"feeBurnedBaseUnits":allocated.to_string()}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{BridgeCredit,ExecutionItem,FeeBurnConfig,NativeCutover,SvmExecutor};
    use solana_sdk::{hash::Hash,pubkey::Pubkey,signature::{Keypair,Signer},system_instruction,transaction::Transaction};
    use std::str::FromStr;

    #[test]
    fn fee_burn_pages_preserve_hashed_epoch_observations_across_restart() {
        let owner = Keypair::new();
        let recipient = Pubkey::new_unique();
        let proposer = Pubkey::new_unique();
        let bridge = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let config = Config { program:bridge.to_string(),mint:mint.to_string(),native_from_slot:4,
            epoch_slots:8,scale:4000,fee_burn_from_slot:Some(8) };
        let mut executor = SvmExecutor::new([(owner.pubkey().to_string(),100_000_000)],&[]).unwrap();
        executor.set_bridge(bridge);
        executor.set_native_cutover(NativeCutover { slot:4,solana_mint:mint,lamports_per_base_unit:4000 });
        executor.set_fee_burn_config(FeeBurnConfig { from_slot:8,epoch_slots:8 });
        executor.set_proposers(&[(1,proposer.to_string())]).unwrap();
        executor.set_credits(vec![
            BridgeCredit { sequence:0,solana_mint:mint.to_string(),decimals:6,recipient:owner.pubkey().to_string(),amount:10_000 },
            BridgeCredit { sequence:1,solana_mint:mint.to_string(),decimals:6,recipient:recipient.to_string(),amount:1_000 },
            BridgeCredit { sequence:2,solana_mint:mint.to_string(),decimals:6,recipient:proposer.to_string(),amount:1_000 },
        ]);
        executor.execute_slot(3,vec![]).unwrap();
        let mut observations = vec![];
        let mut slots = vec![];
        for slot in 4..=27 {
            let items = if [5,9,13,17,25].contains(&slot) {
                let tx = Transaction::new_signed_with_payer(&[system_instruction::transfer(&owner.pubkey(),&recipient,slot)],
                    Some(&owner.pubkey()),&[&owner],Hash::from_str(&executor.latest_blockhash()).unwrap());
                vec![ExecutionItem { transaction:executor.validate_transaction(&bincode::serialize(&tx).unwrap(),slot).unwrap(),source_proposers:vec![1] }]
            } else { vec![] };
            let executed = executor.execute_slot(slot,items).unwrap();
            assert!(executed.results.iter().all(|result|result.success));
            slots.push(SlotBurn { slot,burned_lamports:executed.results.iter().map(|result|result.base_fee/2).sum(),at_ms:1_700_000_000_000+slot*50 });
            if slot%4 == 3 {
                let checkpoint = executor.checkpoint();
                let hash = checkpoint.hash();
                let counter = executor.fee_burn_snapshot_at(&checkpoint).unwrap();
                assert_eq!(checkpoint.hash(),hash,"reading the hashed counter cannot mutate it");
                observations.push(Observation { cblock:slot/4,slots:std::mem::take(&mut slots),complete:true,counter });
            }
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target").join(format!("fee-burn-pages-{}.redb",Pubkey::new_unique()));
        {
            let db = Arc::new(Database::create(&path).unwrap());
            let write = db.begin_write().unwrap(); create_tables(&write).unwrap();
            self::write(&write,&[Batch { config:config.clone(),from_slot:0,observations }]).unwrap();
            write.commit().unwrap();
            let store = FeeBurnStore { db,config:config.clone() };
            assert!(store.page(Some(0),None,2,|_|Ok(None)).unwrap()["items"].as_array().unwrap().is_empty());
            let first = store.page(Some(6),None,2,|_|Ok(None)).unwrap();
            assert_eq!(first["items"][0]["epoch"],3);
            assert_eq!(first["items"][0]["baseFeeBurnedLamports"],"2500");
            assert_eq!(first["items"][0]["closed"],false);
            assert!(first["items"][0]["endedAt"].is_null());
            assert_eq!(first["items"][1]["baseFeeBurnedLamports"],"2500");
            assert_eq!(first["items"][1]["feeBurnAmountBaseUnits"],"0");
            assert_eq!(first["items"][1]["remainderLamports"],"3500");
            assert_eq!(first["nextCursor"],"2");
            assert_eq!(first["feeBurnedBaseUnits"],"1");
        }
        {
            let store = FeeBurnStore { db:Arc::new(Database::open(&path).unwrap()),config };
            let next = store.page(Some(6),Some(2),2,|_|Ok(None)).unwrap();
            assert_eq!(next["items"][0]["epoch"],1);
            assert_eq!(next["items"][0]["baseFeeBurnedLamports"],"5000");
            assert_eq!(next["items"][0]["feeBurnAmountBaseUnits"],"1");
            assert_eq!(next["items"][0]["remainderLamports"],"1000");
            assert_eq!(next["items"][1]["epoch"],0);
            assert_eq!(next["items"][1]["firstSlot"],4);
            assert_eq!(next["items"][1]["baseFeeBurnedLamports"],"2500","native fees before F are observed from canonical charged results");
            assert_eq!(next["items"][1]["feeBurnAmountBaseUnits"],"0");
            assert_eq!(next["items"][1]["startedAt"],1_700_000_000_200u64);
            assert!(next["nextCursor"].is_null());
        }
        std::fs::remove_file(path).unwrap();
    }
}
