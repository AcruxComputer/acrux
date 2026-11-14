//! Bounded, opt-in measurements for the local node runtime.
//!
//! These measurements use monotonic elapsed time and never alter protocol deadlines.
//! Histogram storage is capped per phase; totals and maxima cover the complete run.
use serde_json::{json,Value};
use std::{collections::BTreeMap,path::Path,sync::{Arc,Mutex},time::{Duration,Instant}};

const RECENT_SAMPLES:usize=4096;

#[derive(Default)]
struct Samples {
    count:u64,
    total_us:u128,
    maximum_us:u64,
    recent:Vec<u64>,
    cursor:usize,
}

impl Samples {
    fn record(&mut self,micros:u64){
        self.count+=1;
        self.total_us+=u128::from(micros);
        self.maximum_us=self.maximum_us.max(micros);
        if self.recent.len()<RECENT_SAMPLES {self.recent.push(micros);}else{self.recent[self.cursor]=micros;self.cursor=(self.cursor+1)%RECENT_SAMPLES;}
    }
    fn json(&self)->Value{
        let mut samples=self.recent.clone();samples.sort_unstable();
        if samples.is_empty(){return json!({"count":0,"sampleCount":0,"totalUs":"0","maximumUs":0});}
        json!({"count":self.count,"sampleCount":samples.len(),"totalUs":self.total_us.to_string(),"meanUs":(self.total_us/u128::from(self.count))as u64,"maximumUs":self.maximum_us,"p50Us":samples[samples.len()/2],"p99Us":samples[(samples.len()-1)*99/100]})
    }
}

#[derive(Default)]
pub struct Profiling { phases:Mutex<BTreeMap<(Option<usize>,&'static str),Samples>> }

impl Profiling {
    pub fn new()->Arc<Self>{Arc::new(Self::default())}
    pub fn record(&self,node:Option<usize>,phase:&'static str,duration:Duration){self.record_us(node,phase,duration.as_micros().min(u128::from(u64::MAX))as u64);}
    pub fn record_us(&self,node:Option<usize>,phase:&'static str,micros:u64){self.phases.lock().unwrap().entry((node,phase)).or_default().record(micros);}
    pub fn measure(self:&Arc<Self>,node:Option<usize>,phase:&'static str)->PhaseTimer{PhaseTimer{profile:self.clone(),node,phase,started:Instant::now()}}
    /// Record a queue's occupancy or job count without conflating it with a duration.
    pub fn gauge(&self,node:Option<usize>,name:&'static str,value:u64){self.record_us(node,name,value);}
    pub fn snapshot(&self)->Value {
        let phases=self.phases.lock().unwrap();
        let rows=phases.iter().map(|((node,phase),samples)|json!({"node":node,"phase":phase,"measurement":samples.json()})).collect::<Vec<_>>();
        json!({"clock":"monotonic","percentileWindow":"last4096","phases":rows})
    }
    pub fn write(&self,path:&Path)->std::io::Result<()> {
        if let Some(parent)=path.parent(){std::fs::create_dir_all(parent)?;}
        std::fs::write(path,serde_json::to_vec_pretty(&self.snapshot())?)
    }
}

/// Drop records the elapsed phase, including awaits when the scope intentionally spans them.
pub struct PhaseTimer {profile:Arc<Profiling>,node:Option<usize>,phase:&'static str,started:Instant}
impl Drop for PhaseTimer {fn drop(&mut self){self.profile.record(self.node,self.phase,self.started.elapsed());}}
