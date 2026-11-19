//! The outside validator's adapter to the same packing, sealing and hash exchange as local nodes.
use crate::{consensus::BlockRef, crypto, exchange::ExchangeEngine, execution::{SvmExecutor,ValidatedTx},
    external::{ExternalControl,Packet}, gateway::{GatewayState,SharedGateway}, profiling::Profiling,
    protocol::ProposerSchedules, proposer::{self,BuiltBlock,ProposalLink}, threshold::VerifiedFragment,
    types::{BlockPayload,Commitment,CommitReceipt,Envelope,Wire}};
use anyhow::{ensure,Context,Result};
use ed25519_dalek::{SigningKey,VerifyingKey};
use serde_json::{json,Value};
use base64::{engine::general_purpose::STANDARD as B64,Engine};
use std::{collections::BTreeMap,sync::{Arc,RwLock},time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
use tokio::{sync::mpsc,task::JoinHandle};

/// Host and member clocks must be synchronized. A notice's wire delay never shifts its deadline.
pub(crate) fn host_instant(unix_us:u64)->Result<Instant> {
    let wall = SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros();
    let now = Instant::now();
    if u128::from(unix_us) <= wall {
        now.checked_sub(Duration::from_micros(u64::try_from(wall-u128::from(unix_us))?)).context("Window start predates the monotonic clock")
    } else { now.checked_add(Duration::from_micros(u64::try_from(u128::from(unix_us)-wall)?)).context("Window start exceeds the monotonic clock") }
}
#[derive(Clone)]
pub(crate) struct CommitInbox {id:usize,exchange:Arc<ExchangeEngine>,starts:Arc<RwLock<BTreeMap<u64,Instant>>>}
impl CommitInbox {
    pub(crate) fn set_window(&self,number:u64,hub:String,catalog:Vec<String>,co_ids:Vec<usize>) {
        self.exchange.set_window(number,hub,catalog,co_ids);
    }
    pub(crate) fn receive_receipt(&self,from:usize,receipt:CommitReceipt,received_at:Instant) {
        self.exchange.receive_receipt(self.id,from,receipt,received_at);
    }
    pub(crate) async fn receive(&self,from:usize,commitment:Commitment,signature:Vec<u8>,received_at:Instant) {
        if from==self.id {return}
        let start = self.starts.read().unwrap().get(&(commitment.slot/16)).copied();
        // Base affects event offsets only. The exchange always anchors its deadline to
        // the first real arrival, even when the clock notice is still behind this packet.
        let base = start.map(|start|start+Duration::from_millis((commitment.slot%16)*50)).unwrap_or(received_at);
        self.exchange.receive_commit(self.id,from,commitment,signature,received_at,base).await;
    }
    pub(crate) fn start(&self,number:u64,unix_us:u64)->Result<Instant> {
        let mut starts = self.starts.write().unwrap();
        if let Some(start) = starts.get(&number) {return Ok(*start)}
        let start=host_instant(unix_us)?;
        starts.insert(number,start);
        starts.retain(|old,_|old.saturating_add(4)>=number);
        Ok(start)
    }
}
#[derive(Clone)]
struct FastContext {schedules:Arc<ProposerSchedules>,keys:Arc<Vec<VerifyingKey>>,member:Option<CommitInbox>,identity:String,hub:String}
/// This reader-side lane stays independent of expensive SVM replay and its event loop.
#[derive(Clone,Default)]
pub(crate) struct FastLane {context:Arc<RwLock<Option<FastContext>>>}
impl FastLane {
    pub(crate) fn update(&self,schedules:ProposerSchedules,keys:Vec<VerifyingKey>,member:Option<CommitInbox>,identity:String,hub:String) {
        *self.context.write().unwrap()=Some(FastContext{schedules:Arc::new(schedules),keys:Arc::new(keys),member,identity,hub});
    }
    pub(crate) async fn receive(&self,packet:&Packet,received_at:Instant)->Result<()> {
        // Envelope starts with a fixed-width usize; Wire's appended discriminants stay stable.
        // Decode only the timing-sensitive kinds, leaving bulk Rotor frames to normal intake.
        let Some(tag)=packet.payload.get(8..12) else {return Ok(())};
        if tag!=2u32.to_le_bytes() && tag!=12u32.to_le_bytes() && tag!=13u32.to_le_bytes() {return Ok(())}
        let envelope:Envelope=crate::propose::decode(&packet.payload)?;
        let context=self.context.read().unwrap().clone();
        let Some(context)=context else {return Ok(())};
        let Some(member)=context.member else {return Ok(())};
        let window=match &envelope.body {Wire::Commit(commit)=>commit.slot/16,Wire::CommitReceipt(receipt)=>receipt.slot/16,Wire::WindowStart{number,..}=>*number,_=>return Ok(())};
        if !context.schedules.ready(window) {return Ok(())}
        let draw=context.schedules.window(window)?;
        if draw.hub_code!=context.hub || !draw.proposers.contains(&context.identity) {return Ok(())}
        let epoch=window*16/context.schedules.config.epoch_slots;
        if !context.schedules.admissions.get(&epoch).is_some_and(|set|set.members.iter().any(|item|item.identity.to_string()==context.identity&&item.hub==context.hub)) {return Ok(())}
        ensure!(packet.from==envelope.from,"Fast lane envelope has another sender");
        let key=context.keys.get(envelope.from).context("Fast lane sender is not in the finalized catalog")?;
        crypto::verify_bytes(key,&bincode::serialize(&envelope.body)?,&envelope.signature)?;
        let co_ids=context.schedules.ids(window)?;
        member.set_window(window,draw.hub_code.clone(),context.schedules.catalog.clone(),co_ids.clone());
        match envelope.body {
            Wire::WindowStart{number,started_at_unix_us,..}=>{
                let hub=context.schedules.config.hubs.iter().find(|hub|hub.code==context.hub).context("Missing configured hub")?;
                ensure!(context.schedules.catalog.get(envelope.from)==hub.members.first().map(|member|&member.identity),"Window notice is not from the fixed hub representative");
                member.start(number,started_at_unix_us)?;
            }
            Wire::Commit(commitment)=>{
                ensure!((1..=3).contains(&commitment.proposer)
                    && context.schedules.ids(window)?.get(commitment.proposer-1)==Some(&envelope.from),"Fast lane commitment is not from a drawn proposer");
                member.receive(envelope.from,commitment,envelope.signature,received_at).await;
            }
            Wire::CommitReceipt(receipt)=>{
                ensure!((1..=3).contains(&receipt.proposer),"Invalid receipt proposer");
                // Hub mirrors can contain receipts for another co-proposer's commitment.
                if packet.to==member.id && co_ids[receipt.proposer-1]==member.id {
                    ensure!(envelope.from!=member.id&&co_ids.contains(&envelope.from),"Receipt signer is not a drawn co-proposer");
                    member.receive_receipt(envelope.from,receipt,received_at);
                }
            }
            _=>{},
        }
        Ok(())
    }
}

struct Window {number:u64,parent:Option<BlockRef>,start:Instant,co_ids:Vec<usize>,role:usize,next:u64,view:Value}
struct Building {slot:u64,base:Instant,role:usize,co_ids:Vec<usize>,task:JoinHandle<Result<BuiltBlock>>}
pub(crate) struct MemberProposer {
    id:usize,key:SigningKey,identity:String,control:Arc<ExternalControl>,seed:u64,
    gateway:SharedGateway,exchange:Arc<ExchangeEngine>,inbox:CommitInbox,
    windows:BTreeMap<u64,Window>,building:Option<Building>,
    mempool:BTreeMap<String,ValidatedTx>,fragments:BTreeMap<(String,u8),VerifiedFragment>,
    in_flight:BTreeMap<String,(u64,usize,ValidatedTx)>,
}
impl MemberProposer {
    pub(crate) fn new(id:usize,key:SigningKey,identity:String,control:Arc<ExternalControl>,seed:u64)->Self {
        let (commands,receiver)=mpsc::channel(128);drop(receiver);
        let gateway=Arc::new(tokio::sync::RwLock::new(GatewayState::new(commands)));
        let exchange=ExchangeEngine::new_member(id,key.clone(),control.clone(),gateway.clone(),Profiling::new());
        let inbox=CommitInbox{id,exchange:exchange.clone(),starts:Arc::new(RwLock::new(BTreeMap::new()))};
        Self{id,key,identity,control,seed,gateway,exchange,inbox,windows:BTreeMap::new(),building:None,mempool:BTreeMap::new(),fragments:BTreeMap::new(),in_flight:BTreeMap::new()}
    }
    pub(crate) fn inbox(&self)->CommitInbox {self.inbox.clone()}
    pub(crate) fn metrics_engine(&self)->Arc<ExchangeEngine> {self.exchange.clone()}
    pub(crate) fn start_window(&mut self,number:u64,parent:Option<BlockRef>,unix_us:u64,co_ids:Vec<usize>,view:Value,catalog:Vec<String>)->Result<()> {
        let role=co_ids.iter().position(|id|*id==self.id).context("Own identity is not drawn")?+1;
        self.exchange.set_window(number,view["hubCode"].as_str().context("Window hub code")?.to_owned(),catalog,co_ids.clone());
        if let Some(known)=self.windows.get(&number) {ensure!(known.parent==parent&&known.co_ids==co_ids,"Conflicting window notice");return Ok(())}
        let start=self.inbox.start(number,unix_us)?;
        self.windows.insert(number,Window{number,parent,start,co_ids,role,next:number*16,view});
        self.windows.retain(|old,_|old.saturating_add(4)>=number);
        Ok(())
    }
    pub(crate) fn transaction(&mut self,executor:&SvmExecutor,transaction:ValidatedTx)->Result<()> {
        let checked=executor.validate_transaction(&transaction.bytes,transaction.header.target_slot)?;
        ensure!(checked.header==transaction.header,"Signed transaction header differs from its bytes");
        ensure!(self.mempool.len()+self.fragments.len()<10_000||self.mempool.contains_key(&checked.header.id),"Member mempool is full");
        if !executor.is_executed(&checked.header.id) && !self.in_flight.contains_key(&checked.header.id) {self.mempool.entry(checked.header.id.clone()).or_insert(checked);}
        Ok(())
    }
    pub(crate) fn fragment(&mut self,fragment:VerifiedFragment)->Result<()> {
        let fragment=fragment.revalidate()?;
        ensure!(self.mempool.len()+self.fragments.len()<10_000||self.fragments.contains_key(&(fragment.header.id.clone(),fragment.receiver)),"Member mempool is full");
        self.fragments.insert((fragment.header.id.clone(),fragment.receiver),fragment);
        Ok(())
    }
    pub(crate) fn finalized(&mut self,executor:&SvmExecutor) {
        self.mempool.retain(|id,tx|!executor.is_executed(id)&&executor.blockhash_valid(&tx.bytes));
        self.fragments.retain(|(id,_),_|!executor.is_executed(id));
        self.in_flight.retain(|id,(_,_,tx)|!executor.is_executed(id)&&executor.blockhash_valid(&tx.bytes));
    }
    pub(crate) async fn tick(&mut self,executor:&SvmExecutor,mut reserve:impl FnMut(u64)->Result<bool>)->Result<()> {
        if self.building.as_ref().is_some_and(|building|building.task.is_finished()) {
            let building=self.building.take().unwrap();
            let block=building.task.await??;
            if reserve(building.slot)? {
                self.exchange.launch(self.id,building.slot,building.role,block.commitment.clone(),block.key,building.base,building.co_ids);
                proposer::stream_block(ProposalLink::Member{control:self.control.clone(),seed:self.seed,relay_stakes:vec![100_000_000;12]},self.id,self.key.clone(),block,building.base,self.gateway.clone(),self.identity.clone());
                println!("slot {} proposed by {}",building.slot,self.identity);
            }
        }
        if self.building.is_some() {return Ok(())}
        let now=Instant::now();
        let mut ready=None;
        for window in self.windows.values_mut() {
            while window.next<window.number*16+16 {
                let slot=window.next;
                let base=window.start+Duration::from_millis((slot%16)*50);
                if now<base {break}
                window.next+=1;
                // Building an already-missed commitment cannot move its protocol deadline.
                if now>base+Duration::from_millis(50) {continue}
                ready=Some((slot,base,window.role,window.co_ids.clone(),if slot%16==0 {window.parent}else{None},window.view.clone()));
                break
            }
            if ready.is_some(){break}
        }
        let Some((slot,base,role,co_ids,parent,view))=ready else{return Ok(())};
        let retry = self.in_flight.iter().filter(|(_, (sent,_,tx))| *sent/16 < slot/16 && !executor.is_executed(&tx.header.id) && executor.blockhash_valid(&tx.bytes))
            .map(|(id,_)|id.clone()).collect::<Vec<_>>();
        for id in retry {let (_,_,tx)=self.in_flight.remove(&id).unwrap();self.mempool.entry(id).or_insert(tx);}
        self.fragments.retain(|(_,receiver),fragment|fragment.header.target_slot>slot||*receiver==role as u8);
        let (transactions,fragments)=proposer::pack(executor,&mut self.mempool,&mut self.fragments,&mut self.in_flight,slot,role,None);
        let payload=BlockPayload{transactions,fragments,credits:vec![]}.encode(true)?;
        {let mut gateway=self.gateway.write().await;gateway.slots.insert(slot,crate::view::pending_slot(slot,view,String::new()));gateway.slots.retain(|old,_|old.saturating_add(64)>=slot);}
        let signer=self.key.clone();
        let task=tokio::task::spawn_blocking(move||proposer::build_block(&signer,slot,role,parent,payload));
        self.building=Some(Building{slot,base,role,co_ids,task});
        Ok(())
    }
    pub(crate) fn current_slot(&self)->Option<u64> {
        self.windows.values().next_back().map(|window|window.number*16+(window.start.elapsed().as_millis()/50).min(15) as u64)
    }
}
impl Drop for MemberProposer {fn drop(&mut self){if let Some(building)=&self.building{building.task.abort();}}}


pub(crate) struct RpcRequest {
    pub method:String,pub params:Value,pub client:std::net::IpAddr,
    pub reply:tokio::sync::oneshot::Sender<Result<Value,String>>,
}
/// Only this optional listener is started by `node propose --rpc-port`.
pub(crate) async fn bind_rpc(port:u16)->Result<(mpsc::Receiver<RpcRequest>,JoinHandle<Result<()>>)> {
    use axum::{extract::{ConnectInfo,DefaultBodyLimit,State},routing::post,Json,Router};
    use std::net::{IpAddr,SocketAddr};
    async fn rpc(State(sender):State<mpsc::Sender<RpcRequest>>,ConnectInfo(peer):ConnectInfo<SocketAddr>,Json(body):Json<Value>)->Json<Value> {
        let id=body.get("id").cloned().unwrap_or(Value::Null);
        let failure=|message:&str|Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}}));
        let Some(method)=body["method"].as_str() else {return failure("A JSON-RPC method is required")};
        let submit=matches!(method,"sendTransaction"|"submitTransaction"|"sendPlainTransaction"|"submitFragment");
        let limits=crate::limits::limits();
        if !(if submit {limits.submit(peer.ip())} else {limits.read(peer.ip())}) {return failure("RATE_LIMITED: slow down")}
        let (reply,response)=tokio::sync::oneshot::channel();
        if sender.try_send(RpcRequest{method:method.into(),params:body.get("params").cloned().unwrap_or_else(||json!({})),client:peer.ip(),reply}).is_err(){return failure("Member RPC capacity exceeded")}
        match tokio::time::timeout(Duration::from_secs(10),response).await {
            Ok(Ok(Ok(result)))=>Json(json!({"jsonrpc":"2.0","id":id,"result":result})),
            Ok(Ok(Err(error)))=>failure(&error),
            _=>failure("Member RPC deadline exceeded"),
        }
    }
    let (sender,receiver)=mpsc::channel(128);
    let bind:IpAddr=std::env::var("ACRUX_BIND").unwrap_or_else(|_|"127.0.0.1".into()).parse()?;
    let listener=tokio::net::TcpListener::bind((bind,port)).await?;
    let router=Router::new().route("/",post(rpc)).layer(DefaultBodyLimit::max(64*1024)).with_state(sender);
    let task=tokio::spawn(async move {axum::serve(listener,router.into_make_service_with_connect_info::<SocketAddr>()).await?;Ok(())});
    println!("member RPC listening on {bind}:{port}");
    Ok((receiver,task))
}

pub(crate) fn rpc(request:RpcRequest,member:Option<&mut MemberProposer>,executor:&mut SvmExecutor,slot:u64) {
    let result=(||->Result<Value>{
        let arg=|index:usize|request.params.get(index).cloned().unwrap_or(Value::Null);
        let context=json!({"slot":slot});
        match request.method.as_str() {
            "getSlot"=>Ok(json!(slot)),
            "getBalance"=>{
                let address=if request.params.is_array(){arg(0).as_str().context("address")?.to_owned()}else{request.params["address"].as_str().context("address")?.to_owned()};
                let value=executor.balance(&address)?;
                Ok(if request.params.is_array(){json!({"context":context,"value":value})}else{json!({"address":address,"lamports":value.to_string()})})
            }
            "getLatestBlockhash"=>{
                let hash=executor.latest_blockhash();
                Ok(if request.params.is_array(){json!({"context":context,"value":{"blockhash":hash,"lastValidBlockHeight":slot+crate::execution::BLOCKHASH_AGE_SLOTS-8}})}else{json!({"blockhash":hash,"startedSlot":slot})})
            }
            "sendTransaction"=>{
                let member=member.context("This identity is not in the finalized validator catalog yet")?;
                let encoded=arg(0).as_str().context("transaction")?.to_owned();
                ensure!(encoded.len()<=1700,"Transaction exceeds Solana packet size");
                let bytes=if arg(1)["encoding"]=="base64"{B64.decode(encoded)?}else{bs58::decode(encoded).into_vec()?};
                let valid=executor.validate_transaction(&bytes,slot.checked_add(1).context("Target slot overflow")?)?;
                if arg(1)["skipPreflight"]!=json!(true) {
                    let limits=crate::limits::limits();
                    ensure!(limits.simulation_ready(Some(request.client)),"RATE_LIMITED: simulation budget exceeded");
                    let simulation=executor.simulate(&bytes,true,false)?;
                    limits.simulation_charge(Some(request.client),simulation.units);
                    ensure!(simulation.err.is_none(),"Transaction simulation failed: {:?}",simulation.err);
                }
                let transaction:solana_sdk::transaction::VersionedTransaction=bincode::deserialize(&bytes)?;
                let signature=transaction.signatures.first().context("Unsigned transaction")?.to_string();
                member.transaction(executor,valid)?;
                Ok(json!(signature))
            }
            "submitTransaction"|"sendPlainTransaction"=>{
                let member=member.context("This identity is not in the finalized validator catalog yet")?;
                let header:crate::execution::TxHeader=serde_json::from_value(request.params["header"].clone())?;
                let bytes=B64.decode(request.params["transaction"].as_str().context("transaction")?)?;
                let transaction=executor.validate_transaction(&bytes,header.target_slot)?;
                ensure!(transaction.header==header,"Signed transaction header differs from its bytes");
                let id=transaction.header.id.clone();member.transaction(executor,transaction)?;
                Ok(json!({"id":id,"state":"received"}))
            }
            "submitFragment"=>{
                let member=member.context("This identity is not in the finalized validator catalog yet")?;
                let fragment:crate::threshold::FragmentSubmission=serde_json::from_value(request.params.clone())?;
                let verified=fragment.verify()?;let id=verified.header.id.clone();member.fragment(verified)?;
                Ok(json!({"id":id,"state":"received"}))
            }
            _=>anyhow::bail!("Unsupported member RPC method"),
        }
    })().map_err(|error|error.to_string());
    let _=request.reply.send(result);
}


#[derive(Clone,Default)]
pub(crate) struct MemberMetrics {engine:Arc<RwLock<Option<Arc<ExchangeEngine>>>>}
impl MemberMetrics {
    pub(crate) fn set(&self,engine:Arc<ExchangeEngine>) {*self.engine.write().unwrap()=Some(engine);}
    fn snapshot(&self)->Value {
        let engine=self.engine.read().unwrap().clone();
        engine.map_or_else(crate::exchange_metrics::empty_json,|engine|engine.metrics_json())
    }
}
pub(crate) async fn bind_metrics(port:u16,metrics:MemberMetrics)->Result<JoinHandle<Result<()>>> {
    use axum::{extract::State,routing::get,Json,Router};
    async fn snapshot(State(metrics):State<MemberMetrics>)->Json<Value> {Json(metrics.snapshot())}
    async fn prometheus(State(metrics):State<MemberMetrics>)->impl axum::response::IntoResponse {
        ([("content-type","text/plain; version=0.0.4")],crate::exchange_metrics::prometheus(&metrics.snapshot()))
    }
    let bind:std::net::IpAddr=std::env::var("ACRUX_BIND").unwrap_or_else(|_|"127.0.0.1".into()).parse()?;
    let listener=tokio::net::TcpListener::bind((bind,port)).await?;
    let router=Router::new().route("/metrics",get(prometheus)).route("/internal/hash-exchange",get(snapshot)).with_state(metrics);
    println!("member metrics listening on {bind}:{port}");
    Ok(tokio::spawn(async move {axum::serve(listener,router).await?;Ok(())}))
}
