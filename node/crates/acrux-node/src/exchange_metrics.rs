//! Bounded observations of actual Algorithm 1 commitments, separate from consensus timing.
use serde_json::{json, Value};
use serde::{Serialize,Deserialize};
use std::{collections::{BTreeMap,VecDeque},sync::{Arc,RwLock,mpsc::{sync_channel,Receiver,RecvTimeoutError,SyncSender}},time::{Duration,Instant,SystemTime,UNIX_EPOCH}};

const WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_SAMPLES: usize = 4096;

#[derive(Clone,Copy,PartialEq,Eq,PartialOrd,Ord)]
enum Kind { Applied, Real }
impl Kind {
    fn name(self)->&'static str {match self {Self::Applied=>"applied",Self::Real=>"real"}}
    fn pair(a:usize,b:usize)->Self {if a<12&&b<12 {Self::Applied} else {Self::Real}}
}
struct Sample {at:Instant,sequence:u64,value:u64}
#[derive(Default)]
struct Samples(VecDeque<Sample>);
impl Samples {
    fn prune(&mut self,now:Instant) {self.0.retain(|sample|now.saturating_duration_since(sample.at)<=WINDOW);}
    fn push(&mut self,at:Instant,value:u64,sequence:u64) {
        self.0.push_back(Sample{at,value,sequence});
        if self.0.len()>MAX_SAMPLES {self.0.pop_front();}
    }
    fn values(&self)->Vec<u64> {self.0.iter().map(|sample|sample.value).collect()}
    fn report(&self)->Vec<(u64,u64)> {
        self.0.iter().skip(self.0.len().saturating_sub(128)).map(|sample|(sample.sequence,sample.value)).collect()
    }
}
struct WindowMetadata {hub:String,catalog:Vec<String>,co_ids:Vec<usize>}
struct Pending {sent:Instant,proposer:usize,hub:String,members:[String;2],kind:Kind}
type PendingKey=(usize,usize,u64,[u8;32]);

/// The metrics worker copies labels and scalar samples for rendering, without copying
/// its outstanding receipts and window catalogs.
pub struct Snapshot {
    pairs:Vec<((String,[String;2],Kind),Vec<u64>)>,
    spreads:Vec<((String,Kind),Vec<u64>)>,
    total_spreads:BTreeMap<Kind,Vec<u64>>,
}
#[derive(Clone,Default,Serialize,Deserialize)]
pub(crate) struct ExchangeReport {
    pub session:u64,
    pub pairs:Vec<ReportedPair>,
    pub spreads:Vec<ReportedSpread>,
    pub arrivals:Vec<ReportedArrival>,
}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct ReportedArrival {
    pub slot:u64,pub hash:[u8;32],pub proposer:usize,pub receiver:usize,pub start_us:u64,pub arrived_us:u64,
    pub relative_to_start_us:i64,pub on_time:bool,
}
impl ReportedArrival {
    pub fn json(&self)->Value {json!({"proposer":self.proposer,"hash":hex::encode(self.hash),"arrival":{"receiver":self.receiver,"startUs":self.start_us,"arrivedUs":self.arrived_us,"relativeToStartUs":self.relative_to_start_us,"deadlineUs":20000,"onTime":self.on_time}})}
}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct ReportedPair {pub hub:String,pub members:[String;2],pub values:Vec<(u64,u64)>}
#[derive(Clone,Serialize,Deserialize)]
pub(crate) struct ReportedSpread {pub hub:String,pub values:Vec<(u64,u64)>}
fn stats(mut values:Vec<u64>)->Value {
    if values.is_empty(){return Value::Null}
    values.sort_unstable();
    let n=values.len();json!({"p50":values[(n-1)/2],"p99":values[(n*99).div_ceil(100)-1],"count":n})
}
impl Snapshot {
    pub fn json(self,measured_at:String)->Value {
        let mut hubs:BTreeMap<String,(Vec<Value>,Vec<u64>,Vec<u64>)>=BTreeMap::new();
        for ((hub,members,kind),values) in self.pairs {
            let sample=stats(values);
            hubs.entry(hub).or_default().0.push(json!({"members":members,"kind":kind.name(),"p50":sample["p50"],"p99":sample["p99"],"count":sample["count"]}));
        }
        for ((hub,kind),values) in self.spreads {
            let row=hubs.entry(hub).or_default();
            match kind {Kind::Applied=>row.1=values,Kind::Real=>row.2=values}
        }
        let hubs=hubs.into_iter().map(|(hub,(pairs,applied,real))| {
            json!({"hubCode":hub,"pairs":pairs,"commitSpreadUs":{"applied":stats(applied),"real":stats(real)}})
        }).collect::<Vec<_>>();
        let mut totals=self.total_spreads;
        json!({"measuredAt":measured_at,"measurement":"roundTrip","hubs":hubs,"commitSpreadByLinkUs":{
            "applied":stats(totals.remove(&Kind::Applied).unwrap_or_default()),
            "real":stats(totals.remove(&Kind::Real).unwrap_or_default()),
        }})
    }
}

#[derive(Default)]
pub struct ExchangeMetrics {
    sequence:u64,
    windows:BTreeMap<u64,WindowMetadata>,
    pending:BTreeMap<PendingKey,Pending>,
    pairs:BTreeMap<(String,[String;2],Kind),Samples>,
    spreads:BTreeMap<(String,Kind),Samples>,
    total_spreads:BTreeMap<Kind,Samples>,
    recorded_spreads:BTreeMap<(usize,u64,Kind),Instant>,
}
impl ExchangeMetrics {
    fn report(&self,session:u64)->ExchangeReport {
        ExchangeReport {session,arrivals:vec![],
            pairs:self.pairs.iter().filter(|((_,_,kind),_)|*kind==Kind::Real).map(|((hub,members,_),values)|ReportedPair{hub:hub.clone(),members:members.clone(),values:values.report()}).collect(),
            spreads:self.spreads.iter().filter(|((_,kind),_)|*kind==Kind::Real).map(|((hub,_),values)|ReportedSpread{hub:hub.clone(),values:values.report()}).collect(),
        }
    }
    pub fn set_window(&mut self,number:u64,hub:String,catalog:Vec<String>,co_ids:Vec<usize>) {
        self.windows.insert(number,WindowMetadata{hub,catalog,co_ids});
        let newest=*self.windows.keys().next_back().unwrap();
        self.windows.retain(|old,_|old.saturating_add(4)>=newest);
        let windows=&self.windows;
        self.pending.retain(|(_,_,slot,_),_|windows.contains_key(&(slot/16)));
        self.recorded_spreads.retain(|(_,slot,_),_|windows.contains_key(&(slot/16)));
    }
    /// Called at the same actual send Instant as Algorithm 1, before the transport send.
    pub fn sent(&mut self,owner:usize,slot:u64,hash:[u8;32],sent:Instant) {
        let Some(window)=self.windows.get(&(slot/16)) else{return};
        for peer in window.co_ids.iter().copied().filter(|peer|*peer!=owner) {
            let mut members=[window.catalog[owner].clone(),window.catalog[peer].clone()];members.sort();
            self.pending.insert((owner,peer,slot,hash),Pending{sent,proposer:window.co_ids.iter().position(|id|*id==owner).unwrap()+1,hub:window.hub.clone(),members,kind:Kind::pair(owner,peer)});
        }
    }
    /// The first exact signed receipt closes one pending exchange. Stale/duplicate receipts do not
    /// create rows or samples; the caller has authenticated its signer and scheduled route.
    pub fn received(&mut self,owner:usize,peer:usize,slot:u64,proposer:usize,hash:[u8;32],at:Instant) {
        let key=(owner,peer,slot,hash);
        let Some(pending)=self.pending.get(&key) else{return};
        if pending.proposer!=proposer || at<pending.sent {return}
        let pending=self.pending.remove(&key).unwrap();
        if at.saturating_duration_since(pending.sent)>WINDOW {return}
        let elapsed=at.saturating_duration_since(pending.sent).as_micros().min(u128::from(u64::MAX))as u64;
        self.sequence+=1;
        self.pairs.entry((pending.hub,pending.members,pending.kind)).or_default().push(at,elapsed,self.sequence);
    }
    /// One hosted observer per slot. Only complete groups become samples: a missing peer never
    /// becomes zero, and a commitment received after the attestation deadline retains its real time.
    pub fn spreads(&mut self,owner:usize,slot:u64,received:&BTreeMap<usize,(Instant,crate::types::Commitment)>,at:Instant) {
        let Some(window)=self.windows.get(&(slot/16)) else{return};
        let observer=window.co_ids.iter().copied().filter(|id|*id<12).min().or_else(||window.co_ids.iter().copied().min());
        if observer!=Some(owner) {return}
        let Some(own_role)=window.co_ids.iter().position(|id|*id==owner).map(|role|role+1) else{return};
        let Some((own_at,_))=received.get(&own_role) else{return};
        for kind in [Kind::Applied,Kind::Real] {
            if self.recorded_spreads.contains_key(&(owner,slot,kind)) {continue}
            let peers=window.co_ids.iter().copied().enumerate().filter(|(_,peer)|*peer!=owner&&Kind::pair(owner,*peer)==kind).collect::<Vec<_>>();
            if peers.is_empty(){continue}
            let mut times=vec![*own_at];
            for (role,_) in &peers {
                if let Some((time,_))=received.get(&(role+1)){times.push(*time);}
            }
            if times.len()!=peers.len()+1 {continue}
            let value=times.iter().max().unwrap().saturating_duration_since(*times.iter().min().unwrap()).as_micros().min(u128::from(u64::MAX))as u64;
            self.sequence+=1;
            self.spreads.entry((window.hub.clone(),kind)).or_default().push(at,value,self.sequence);
            self.total_spreads.entry(kind).or_default().push(at,value,self.sequence);
            self.recorded_spreads.insert((owner,slot,kind),at);
        }
    }
    fn prune(&mut self,now:Instant) {
        let windows=&self.windows;
        self.pending.retain(|(_,_,slot,_),pending|windows.contains_key(&(slot/16))&&now.saturating_duration_since(pending.sent)<=WINDOW);
        self.recorded_spreads.retain(|(_,slot,_),at|windows.contains_key(&(slot/16))&&now.saturating_duration_since(*at)<=WINDOW);
        self.pairs.retain(|_,samples|{samples.prune(now);!samples.0.is_empty()});
        self.spreads.retain(|_,samples|{samples.prune(now);!samples.0.is_empty()});
        self.total_spreads.retain(|_,samples|{samples.prune(now);!samples.0.is_empty()});
    }
    pub fn snapshot(&mut self,now:Instant)->Snapshot {
        self.prune(now);
        Snapshot {
            pairs:self.pairs.iter().map(|(key,samples)|(key.clone(),samples.values())).collect(),
            spreads:self.spreads.iter().map(|(key,samples)|(key.clone(),samples.values())).collect(),
            total_spreads:self.total_spreads.iter().map(|(kind,samples)|(*kind,samples.values())).collect(),
        }
    }

}


const EVENT_CAPACITY:usize=4096;
const PUBLISH_INTERVAL:Duration=Duration::from_secs(1);

enum Event {
    Window{number:u64,hub:String,catalog:Vec<String>,co_ids:Vec<usize>},
    Sent{owner:usize,slot:u64,hash:[u8;32],at:Instant},
    Receipt{owner:usize,peer:usize,slot:u64,proposer:usize,hash:[u8;32],at:Instant},
    Spreads{owner:usize,slot:u64,received:BTreeMap<usize,(Instant,crate::types::Commitment)>,at:Instant},
    Report{identity:String,report:ExchangeReport},
    Arrival(ReportedArrival),
}
struct Published {json:Value,report:ExchangeReport}

/// Protocol hooks never wait for aggregation or readers. Overload loses telemetry only.
/// The worker owns the receiver and published pointer, never this sender.
pub(crate) struct Recorder {
    sender:SyncSender<Event>,
    published:Arc<RwLock<Arc<Published>>>,
}
impl Recorder {
    pub fn arrival(&self,slot:u64,data:&Value) {
        let arrival=&data["arrival"];
        self.send(Event::Arrival(ReportedArrival{slot,hash:hex::decode(data["hash"].as_str().unwrap()).unwrap().try_into().unwrap(),proposer:data["proposer"].as_u64().unwrap()as usize,
            receiver:arrival["receiver"].as_u64().unwrap()as usize,start_us:arrival["startUs"].as_u64().unwrap(),
            arrived_us:arrival["arrivedUs"].as_u64().unwrap(),relative_to_start_us:arrival["relativeToStartUs"].as_i64().unwrap(),on_time:arrival["onTime"].as_bool().unwrap()}));
    }
    pub fn new()->Self {
        let (sender,receiver)=sync_channel(EVENT_CAPACITY);
        let published=Arc::new(RwLock::new(Arc::new(Published{json:empty_json(),report:ExchangeReport::default()})));
        let output=published.clone();
        std::thread::Builder::new().name("acrux-exchange-metrics".into()).spawn(move||collect(receiver,output)).expect("Cannot start exchange metrics worker");
        Self{sender,published}
    }
    fn send(&self,event:Event) {
        let _=self.sender.try_send(event);
    }
    pub fn set_window(&self,number:u64,hub:String,catalog:Vec<String>,co_ids:Vec<usize>) {
        self.send(Event::Window{number,hub,catalog,co_ids});
    }
    pub fn sent(&self,owner:usize,slot:u64,hash:[u8;32],at:Instant) {
        self.send(Event::Sent{owner,slot,hash,at});
    }
    pub fn received(&self,owner:usize,peer:usize,slot:u64,proposer:usize,hash:[u8;32],at:Instant) {
        self.send(Event::Receipt{owner,peer,slot,proposer,hash,at});
    }
    pub fn spreads(&self,owner:usize,slot:u64,received:BTreeMap<usize,(Instant,crate::types::Commitment)>,at:Instant) {
        self.send(Event::Spreads{owner,slot,received,at});
    }
    pub fn json(&self)->Value {
        let cached={self.published.read().unwrap().clone()};
        cached.json.clone()
    }
    pub fn report(&self)->ExchangeReport {
        let cached={self.published.read().unwrap().clone()};
        cached.report.clone()
    }
    pub fn include_report(&self,identity:String,report:ExchangeReport) {
        self.send(Event::Report{identity,report});
    }
}
fn collect(receiver:Receiver<Event>,published:Arc<RwLock<Arc<Published>>>) {
    let mut metrics=ExchangeMetrics::default();
    let session=SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()as u64;
    let mut reports:BTreeMap<String,(u64,BTreeMap<(String,[String;2]),u64>,BTreeMap<String,u64>,Instant)>=BTreeMap::new();
    let mut arrivals=BTreeMap::new();
    let mut last_publish=Instant::now();
    loop {
        let wait=PUBLISH_INTERVAL.saturating_sub(last_publish.elapsed());
        match receiver.recv_timeout(wait) {
            Ok(event)=>match event {
                Event::Window{number,hub,catalog,co_ids}=>metrics.set_window(number,hub,catalog,co_ids),
                Event::Sent{owner,slot,hash,at}=>metrics.sent(owner,slot,hash,at),
                Event::Receipt{owner,peer,slot,proposer,hash,at}=>metrics.received(owner,peer,slot,proposer,hash,at),
                Event::Spreads{owner,slot,received,at}=>metrics.spreads(owner,slot,&received,at),
                Event::Arrival(row)=>{
                    let slot=row.slot;
                    arrivals.insert((row.slot,row.proposer,row.receiver),row);
                    arrivals.retain(|(old,_,_),_|old.saturating_add(64)>=slot);
                },
                Event::Report{identity,report}=>{
                    let progress=reports.entry(identity).or_insert_with(||(report.session,BTreeMap::new(),BTreeMap::new(),Instant::now()));
                    if progress.0!=report.session {*progress=(report.session,BTreeMap::new(),BTreeMap::new(),Instant::now());}
                    let now=Instant::now();progress.3=now;
                    for row in report.pairs {
                        let last=progress.1.entry((row.hub.clone(),row.members.clone())).or_default();
                        for (number,value) in row.values {
                            if number>*last {metrics.sequence+=1;metrics.pairs.entry((row.hub.clone(),row.members.clone(),Kind::Real)).or_default().push(now,value,metrics.sequence);*last=number;}
                        }
                    }
                    for row in report.spreads {
                        let last=progress.2.entry(row.hub.clone()).or_default();
                        for (number,value) in row.values {
                            if number>*last {
                                metrics.sequence+=1;
                                metrics.spreads.entry((row.hub.clone(),Kind::Real)).or_default().push(now,value,metrics.sequence);
                                metrics.total_spreads.entry(Kind::Real).or_default().push(now,value,metrics.sequence);*last=number;
                            }
                        }
                    }
                },
            },
            Err(RecvTimeoutError::Timeout)=>{},
            Err(RecvTimeoutError::Disconnected)=>return,
        }
        // Check after every event too: a busy queue cannot postpone publication indefinitely.
        if last_publish.elapsed()>=PUBLISH_INTERVAL {
            reports.retain(|_,progress|progress.3.elapsed()<=WINDOW);
            let snapshot=metrics.snapshot(Instant::now());
            let mut report=metrics.report(session);
            report.arrivals=arrivals.values().cloned().collect();
            let rendered=Arc::new(Published{json:snapshot.json(measured_at()),report});
            let previous={let mut pointer=published.write().unwrap();std::mem::replace(&mut *pointer,rendered)};
            drop(previous);
            last_publish=Instant::now();
        }
    }
}

pub(crate) fn measured_at()->String {
    crate::protocol::utc(SystemTime::now().duration_since(UNIX_EPOCH).expect("System clock predates Unix epoch").as_millis()as u64)
}
pub fn empty_json()->Value {ExchangeMetrics::default().snapshot(Instant::now()).json(measured_at())}

/// These series use microseconds throughout; missing classes produce no zero-valued samples.
pub fn prometheus(snapshot:&Value)->String {
    let mut out=String::new();
    let Some(hubs)=snapshot["hubs"].as_array() else{return out};
    for hub in hubs {
        let hub_code=hub["hubCode"].as_str().unwrap();
        for pair in hub["pairs"].as_array().unwrap() {
            let members=pair["members"].as_array().unwrap();
            let labels=format!("hub=\"{}\",member_a=\"{}\",member_b=\"{}\",kind=\"{}\"",hub_code,members[0].as_str().unwrap(),members[1].as_str().unwrap(),pair["kind"].as_str().unwrap());
            for (field,quantile) in [("p50","0.5"),("p99","0.99")] {
                out.push_str(&format!("acrux_hash_exchange_rtt_microseconds{{{labels},quantile=\"{quantile}\"}} {}\n",pair[field].as_u64().unwrap()));
            }
            out.push_str(&format!("acrux_hash_exchange_samples{{{labels}}} {}\n",pair["count"].as_u64().unwrap()));
        }
        for kind in ["applied","real"] {
            let stats=&hub["commitSpreadUs"][kind];if stats.is_null(){continue}
            for (field,quantile) in [("p50","0.5"),("p99","0.99")] {
                out.push_str(&format!("acrux_commit_spread_by_link_microseconds{{hub=\"{hub_code}\",kind=\"{kind}\",quantile=\"{quantile}\"}} {}\n",stats[field].as_u64().unwrap()));
            }
            out.push_str(&format!("acrux_commit_spread_by_link_samples{{hub=\"{hub_code}\",kind=\"{kind}\"}} {}\n",stats["count"].as_u64().unwrap()));
        }
    }
    out
}
