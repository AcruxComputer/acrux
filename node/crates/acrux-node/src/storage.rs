use anyhow::Result;
use redb::{Database, TableDefinition};
use serde_json::Value;
use std::{path::Path, sync::Arc};
use tokio::sync::mpsc;
const RECORDS: TableDefinition<&str, &[u8]> = TableDefinition::new("records");
#[derive(Clone)]
pub struct Store(pub Arc<Database>);
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        let db = Database::builder()
            .set_cache_size(128 * 1024 * 1024)
            .create(path)?;
        let w = db.begin_write()?;
        {
            w.open_table(RECORDS)?;
        }
        w.commit()?;
        Ok(Self(Arc::new(db)))
    }
    pub fn put(&self, key: &str, value: &Value) -> Result<()> {
        self.put_many(&[(key.to_owned(),value.clone())])
    }
    /// Commit one bounded batch atomically, keeping the last update for a repeated record key.
    pub fn put_many(&self, records:&[(String,Value)]) -> Result<()> {
        if records.is_empty(){return Ok(());}
        let mut encoded=std::collections::BTreeMap::new();
        for(key,value)in records{encoded.insert(key.as_str(),serde_json::to_vec(value)?);}
        let w = self.0.begin_write()?;
        {
            let mut table=w.open_table(RECORDS)?;
            for(key,data)in encoded{table.insert(key,data.as_slice())?;}
        }
        w.commit()?;
        Ok(())
    }
    /// Runs on a blocking worker. Drain only records already queued; never sleep to form a batch.
    /// Records older than `retain` slots behind the newest slot record are deleted.
    pub fn write_batches(&self,mut receiver:mpsc::Receiver<(String,Value)>,retain:u64)->Result<()> {
        let mut batch=Vec::with_capacity(256);
        let mut newest=0u64;
        let mut written:std::collections::VecDeque<(u64,String)>=Default::default();
        let mut latest:std::collections::HashMap<String,u64>=Default::default();
        while let Some(record)=receiver.blocking_recv(){
            batch.push(record);
            while batch.len()<256{match receiver.try_recv(){Ok(record)=>batch.push(record),Err(_)=>break}}
            for(key,_)in &batch{
                if let Some(slot)=key.strip_prefix("slot/").and_then(|s|s.parse::<u64>().ok()){newest=newest.max(slot);}
                latest.insert(key.clone(),newest);
                written.push_back((newest,key.clone()));
            }
            let mut expired=vec![];
            while written.front().map_or(false,|(slot,_)|slot+retain<newest){
                let(slot,key)=written.pop_front().unwrap();
                if latest.get(&key)==Some(&slot){latest.remove(&key);expired.push(key);}
            }
            self.put_many(&batch)?;
            self.delete_many(&expired)?;
            batch.clear();
        }
        Ok(())
    }
    fn delete_many(&self,keys:&[String])->Result<()>{
        if keys.is_empty(){return Ok(());}
        let w=self.0.begin_write()?;
        {
            let mut table=w.open_table(RECORDS)?;
            for key in keys{table.remove(key.as_str())?;}
        }
        w.commit()?;
        Ok(())
    }
    pub fn get(&self, key: &str) -> Result<Option<Value>> {
        let r = self.0.begin_read()?;
        let t = r.open_table(RECORDS)?;
        Ok(t.get(key)?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?)
    }
}
