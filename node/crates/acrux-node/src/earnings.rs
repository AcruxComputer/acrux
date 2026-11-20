//! Finalized fee earnings, committed with balances but outside the execution state hash.
use crate::execution::{ExecutorCheckpoint, ValidatorEarningsEpoch};
use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition, WriteTransaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};

const EPOCHS: TableDefinition<u64, &[u8]> = TableDefinition::new("validator_earnings");

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct Member {
    pub identity:String,
    pub hub:String,
    pub validator_kind:String,
}

#[derive(Serialize, Deserialize)]
struct StoredEpoch {
    tally:ValidatorEarningsEpoch,
    members:Vec<Member>,
}

pub fn create_table(write:&WriteTransaction)->Result<()> {write.open_table(EPOCHS)?;Ok(())}

pub fn write(write:&WriteTransaction,checkpoint:&ExecutorCheckpoint,earlier:Option<&ExecutorCheckpoint>,members:&BTreeMap<u64,Vec<Member>>)->Result<()> {
    let mut table=write.open_table(EPOCHS)?;
    for (epoch,tally) in checkpoint.validator_earnings() {
        if earlier.and_then(|checkpoint|checkpoint.validator_earnings().get(epoch)).is_some_and(|before|Arc::ptr_eq(before,tally)) {continue}
        let row=StoredEpoch{tally:tally.as_ref().clone(),members:members.get(epoch).context("Finalized earnings epoch is missing its roster")?.clone()};
        table.insert(*epoch,bincode::serialize(&row)?.as_slice())?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct EarningsStore {
    pub db:Arc<Database>,
    pub epoch_slots:u64,
}
impl EarningsStore {
    pub fn load(&self)->Result<Vec<ValidatorEarningsEpoch>> {
        self.db.begin_read()?.open_table(EPOCHS)?.iter()?.map(|entry| {
            let (_,value)=entry?;
            Ok(bincode::deserialize::<StoredEpoch>(value.value())?.tally)
        }).collect()
    }
    pub fn epochs(&self)->Result<Value> {
        let epochs=self.db.begin_read()?.open_table(EPOCHS)?.iter()?.rev().map(|entry|Ok(entry?.0.value())).collect::<Result<Vec<_>>>()?;
        Ok(json!({"epochSlots":self.epoch_slots,"epochs":epochs}))
    }
    pub fn epoch(&self,epoch:u64)->Result<Option<Value>> {
        let read=self.db.begin_read()?;
        let table=read.open_table(EPOCHS)?;
        let Some(value)=table.get(epoch)? else{return Ok(None)};
        let stored:StoredEpoch=bincode::deserialize(value.value())?;
        let rows=stored.members.iter().map(|member| {
            let paid=stored.tally.rows.iter().find(|row|row.identity==member.identity);
            json!({"identity":member.identity,"hub":member.hub,"validatorKind":member.validator_kind,
                "base":paid.map_or(0,|row|row.base).to_string(),"priority":paid.map_or(0,|row|row.priority).to_string(),
                "inclusion":paid.map_or(0,|row|row.inclusion).to_string()})
        }).collect::<Vec<_>>();
        Ok(Some(json!({"epoch":stored.tally.epoch,"countedFromSlot":stored.tally.counted_from_slot,"rows":rows})))
    }
}
