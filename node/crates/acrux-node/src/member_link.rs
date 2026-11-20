//! One member's hosted uplink and authenticated direct co-proposer links.
use crate::{external::{ExternalClient,ExternalControl,ExternalServer,Packet},external_proposer::FastLane,types::{Envelope,Wire}};
use anyhow::{Context,Result,ensure};
use base64::{engine::general_purpose::STANDARD as B64,Engine};
use ed25519_dalek::SigningKey;
use serde::Deserialize;
use std::{collections::{BTreeMap,BTreeSet},sync::{Arc,Mutex,RwLock},time::Instant};
use tokio::task::JoinHandle;

impl crate::protocol::ProposerSchedules {
    pub(crate) fn window_representative(&self,number:u64)->Result<usize> {
        let window=self.window(number)?;
        match self.config.hubs.iter().find(|hub|hub.code==window.hub_code) {
            Some(hub)=>self.catalog.iter().position(|identity|Some(identity)==hub.members.first().map(|member|&member.identity)).context("Hub representative is absent"),
            None=>Ok(0),
        }
    }
}

#[derive(Clone,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(crate) struct HubPeer {
    pub id:usize,pub identity:String,
    #[serde(default)] pub address:Option<String>,
    #[serde(default)] pub certificate:Option<String>,
}
pub(crate) struct MemberLink {
    uplink:Arc<ExternalControl>,
    pub server:Option<Arc<ExternalServer>>,
    clients:RwLock<BTreeMap<usize,Arc<ExternalControl>>>,
    connecting:Mutex<BTreeSet<usize>>,
    tasks:Mutex<Vec<JoinHandle<()>>>,
    reported:Mutex<BTreeSet<(u64,[u8;32])>>,
}
impl MemberLink {
    pub fn new(uplink:Arc<ExternalControl>,server:Option<Arc<ExternalServer>>)->Arc<Self> {
        Arc::new(Self{uplink,server,clients:RwLock::new(BTreeMap::new()),connecting:Mutex::new(BTreeSet::new()),tasks:Mutex::new(vec![]),reported:Mutex::new(BTreeSet::new())})
    }
    pub async fn send(&self,from:usize,to:usize,bytes:Vec<u8>)->Result<()> {
        if to<12 || self.server.is_none() {return self.uplink.send_to(&[to],&bytes).await}
        let slot=crate::protocol::observer_packet_slot(&bytes).or_else(|| {
            bincode::deserialize::<Envelope>(&bytes).ok().and_then(|envelope|match envelope.body {Wire::CommitReceipt(receipt)=>Some(receipt.slot),_=>None})
        }).context("Direct link has no slot")?;
        let control={self.clients.read().unwrap().get(&to).cloned()};
        if let Some(control)=control {control.send_to(&[to],&bytes).await?;}
        else if let Some(server)=&self.server {
            if server.connected(to) {server.publish(from,to,slot,Arc::new(bytes.clone()));}
            // A missing direct connection is a missed block, never a slower hosted commit route.
        }
        if let Wire::Commit(commitment)=bincode::deserialize::<Envelope>(&bytes)?.body {
            let publish={
                let mut sent=self.reported.lock().unwrap();
                sent.retain(|(old,_)|old.saturating_add(64)>=slot);
                sent.insert((slot,commitment.hash))
            };
            if publish {self.uplink.send_to(&[0],&bytes).await?;}
        }
        Ok(())
    }
    pub fn connect(self:&Arc<Self>,own:usize,hub:&str,peers:Vec<HubPeer>,key:&SigningKey,from_slot:u64,fast:FastLane)->Result<()> {
        let Some(server)=&self.server else{return Ok(())};
        server.set_registry_hubs(vec![(hub.to_owned(),peers.iter().map(|peer|peer.id).collect())])?;
        self.tasks.lock().unwrap().retain(|task|!task.is_finished());
        for peer in peers.into_iter().filter(|peer|peer.id>own) {
            let (Some(address),Some(certificate))=(peer.address,peer.certificate) else {continue};
            if self.clients.read().unwrap().contains_key(&peer.id) || !self.connecting.lock().unwrap().insert(peer.id) {continue}
            let weak=Arc::downgrade(self);let signer=key.clone();let hub=hub.to_owned();let fast=fast.clone();
            self.tasks.lock().unwrap().push(tokio::spawn(async move {
                let outcome=async {
                    let address=tokio::net::lookup_host(&address).await?.next().context("Peer address did not resolve")?;
                    let mut client=ExternalClient::connect(address,&B64.decode(certificate)?,&hub,&signer,from_slot).await?;
                    let identity:[u8;32]=bs58::decode(&peer.identity).into_vec()?.try_into().map_err(|_|anyhow::anyhow!("Invalid peer identity"))?;
                    let Some(link)=weak.upgrade() else {return Ok(())};
                    link.clients.write().unwrap().insert(peer.id,Arc::new(client.control.clone()));
                    drop(link);
                    let rtt=client.control.ping(own).await?;
                    println!("hub {hub} direct link {} RTT {:.3} ms",peer.identity,rtt.as_secs_f64()*1000.0);
                    loop {
                        let packet=client.next_packet().await?;
                        fast.receive_direct(&packet,&identity,Instant::now()).await?;
                    }
                    #[allow(unreachable_code)] Ok::<(),anyhow::Error>(())
                }.await;
                if let Some(link)=weak.upgrade() {
                    link.clients.write().unwrap().remove(&peer.id);
                    link.connecting.lock().unwrap().remove(&peer.id);
                }
                if let Err(error)=outcome {tracing::warn!(event="external_peer_closed",peer=%peer.identity,%error);}
            }));
        }
        Ok(())
    }
    pub fn receive(self:&Arc<Self>,mut input:tokio::sync::mpsc::Receiver<crate::external::ExternalIngress>,fast:FastLane) {
        let weak=Arc::downgrade(self);
        self.tasks.lock().unwrap().push(tokio::spawn(async move {
            while let Some(input)=input.recv().await {
                let checked=async {
                    ensure!(input.envelope.len()<=2048,"Direct member frame too large");
                    let to=input.recipients.as_ref().filter(|targets|targets.len()==1).context("Direct member needs one recipient")?[0];
                    let envelope:Envelope=crate::propose::decode(&input.envelope)?;
                    let slot=match &envelope.body {Wire::Commit(commit)=>commit.slot,Wire::CommitReceipt(receipt)=>receipt.slot,_=>anyhow::bail!("Invalid direct member frame")};
                    fast.receive_direct(&Packet{from:envelope.from,to,slot,payload:input.envelope.clone()},&input.identity,input.received_at).await
                }.await;
                if let Err(error)=checked {input.reject(&format!("Invalid direct peer: {error}"));}
                if weak.upgrade().is_none(){break}
            }
        }));
    }
    pub async fn close(&self) {
        let tasks=std::mem::take(&mut *self.tasks.lock().unwrap());
        for task in &tasks {task.abort();}
        for task in tasks {let _=task.await;}
        self.clients.write().unwrap().clear();
        if let Some(server)=&self.server {server.close().await;}
    }
}

pub(crate) async fn bind_peer(port:u16,address:Option<String>,hub:&str)->Result<(Arc<ExternalServer>,tokio::sync::mpsc::Receiver<crate::external::ExternalIngress>,String)> {
    let ip:std::net::IpAddr=std::env::var("ACRUX_BIND").unwrap_or_else(|_|"127.0.0.1".into()).parse()?;
    ensure!(port>0,"Member peer port must be positive");
    let address=match address {Some(address)=>address,None if ip.is_loopback()=>std::net::SocketAddr::new(ip,port).to_string(),None=>anyhow::bail!("--peer-address is required when the member listens beyond loopback")};
    let (host,advertised_port)=address.rsplit_once(':').context("Peer address must contain a host and UDP port")?;
    ensure!(!host.is_empty()&&address.len()<=255&&address.is_ascii()&&!address.bytes().any(|byte|byte.is_ascii_whitespace())
        &&advertised_port.bytes().all(|byte|byte.is_ascii_digit())&&advertised_port.parse::<u16>().is_ok_and(|port|port>0),"Invalid public peer address");
    let server=ExternalServer::bind(std::net::SocketAddr::new(ip,port),vec![(hub.to_string(),vec![0])],32).await?;
    let (sender,receiver)=tokio::sync::mpsc::channel(256);
    server.set_ingress(sender);
    println!("member peer endpoint {address}");
    Ok((server,receiver,address))
}

/// Discovery, paid on-chain advertisement and read-only metric reports never run on replay's lane.
pub(crate) async fn maintain(url:String,hub:String,key:SigningKey,link:Arc<MemberLink>,fast:FastLane,metrics:crate::external_proposer::MemberMetrics,advertised:Option<String>)->Result<()> {
    use crate::cli::rpc;
    use serde_json::json;
    use solana_sdk::{signature::Keypair,signer::Signer,instruction::{Instruction,AccountMeta},transaction::Transaction};
    use std::str::FromStr;
    let identity=bs58::encode(key.verifying_key().as_bytes()).into_string();
    let payer=Keypair::from_bytes(&key.to_keypair_bytes()).map_err(|error|anyhow::anyhow!("Member identity: {error}"))?;
    let mut interval=tokio::time::interval(std::time::Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending:Option<(String,u64)>=None;
    let mut announced=false;
    loop {
        interval.tick().await;
        let Some((own,epoch_slots,catalog))=fast.peer_context() else {continue};
        if let Some(server)=&link.server {server.set_catalog(catalog.clone())?;}
        let info=rpc(&url,"getHubPeers",json!({"hub":hub,"direct":true})).await?;
        ensure!(info["hub"].as_str()==Some(&hub),"Peer discovery returned another hub");
        let mut peers:Vec<HubPeer>=serde_json::from_value(info["peers"].clone())?;
        ensure!(peers.len()<=64&&peers.iter().map(|peer|peer.id).collect::<BTreeSet<_>>().len()==peers.len(),"Invalid discovered peers");
        for peer in &peers {
            let bytes:[u8;32]=bs58::decode(&peer.identity).into_vec()?.try_into().map_err(|_|anyhow::anyhow!("Invalid peer identity"))?;
            if let Some((_,identity))=catalog.iter().find(|(id,_)|*id==peer.id) {ensure!(*identity==bytes,"Discovered peer differs from finalized catalog");}
        }
        peers.retain(|peer|catalog.iter().any(|(id,_)|*id==peer.id));
        let slot=rpc(&url,"getSlot",json!([])).await?.as_u64().context("Invalid live slot")?;
        link.connect(own,&hub,peers.clone(),&key,slot.saturating_sub(32),fast.clone())?;
        if let (Some(address),Some(server))=(&advertised,&link.server) {
            let certificate=B64.encode(server.cert_der());
            let recorded=peers.iter().find(|peer|peer.identity==identity);
            if recorded.is_some_and(|peer|peer.address.as_ref()==Some(address)&&peer.certificate.as_ref()==Some(&certificate)) {
                if !announced {println!("peer announcement finalized {address}");announced=true;}
                pending=None;
            } else if !announced&&fast.effective() {
                if let Some((signature,expiry))=&pending {
                    let status=rpc(&url,"getSignatureStatuses",json!([[signature],{"searchTransactionHistory":true}])).await?;
                    let status=&status["value"][0];
                    ensure!(status["err"].is_null(),"Peer announcement transaction failed: {}",status["err"]);
                    if status["confirmationStatus"]=="finalized" {
                        let registry=solana_sdk::pubkey::Pubkey::from_str("ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm")?;
                        let account=rpc(&url,"getAccountInfo",json!([crate::execution::peer_address(&registry,&payer.pubkey()).to_string(),{"encoding":"base64","commitment":"finalized"}])).await?;
                        let value=&account["value"];
                        ensure!(value["owner"]==registry.to_string(),"Finalized peer announcement has no funded metadata account");
                        let bytes=B64.decode(value["data"][0].as_str().context("Missing peer metadata")?)?;
                        let peer:crate::execution::PeerAnnouncement=bincode::deserialize(bytes.get(8..).context("Invalid peer metadata")?)?;
                        ensure!(peer.identity==payer.pubkey()&&peer.address==*address&&peer.certificate==server.cert_der(),"Finalized peer announcement differs");
                        println!("peer announcement finalized {address}");announced=true;pending=None;continue
                    }
                    let height=rpc(&url,"getBlockHeight",json!([])).await?.as_u64().context("Invalid block height")?;
                    if height>*expiry {pending=None;}
                } else {
                    let peer=crate::execution::PeerAnnouncement{identity:payer.pubkey(),address:address.clone(),certificate:server.cert_der()};
                    let size=8+bincode::serialized_size(&peer)?;
                    let rent=rpc(&url,"getMinimumBalanceForRentExemption",json!([size])).await?.as_u64().context("Invalid peer rent")?;
                    let registry=solana_sdk::pubkey::Pubkey::from_str("ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm")?;
                    let previous=rpc(&url,"getAccountInfo",json!([crate::execution::peer_address(&registry,&payer.pubkey()).to_string(),{"encoding":"base64","commitment":"finalized"}])).await?;
                    let rent=rent.saturating_sub(previous["value"]["lamports"].as_u64().unwrap_or(0));
                    let balance=rpc(&url,"getBalance",json!({"address":identity})).await?;
                    let reserve=balance["reserveLamports"].as_str().context("Invalid member reserve")?.parse::<u64>()?;
                    let balance=balance["lamports"].as_str().context("Invalid member balance")?.parse::<u64>()?;
                    if balance<rent.saturating_add(reserve) {continue}
                    let latest=rpc(&url,"getLatestBlockhash",json!([])).await?;
                    let hash=solana_sdk::hash::Hash::from_str(latest["value"]["blockhash"].as_str().context("Invalid blockhash")?)?;
                    let expiry=latest["value"]["lastValidBlockHeight"].as_u64().context("Invalid blockhash expiry")?;
                    let data=format!("acrux-peer-v1:{}",json!({"address":address,"certificate":certificate})).into_bytes();
                    let instruction=Instruction{program_id:solana_sdk::pubkey::Pubkey::from_str("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr")?,accounts:vec![AccountMeta::new_readonly(payer.pubkey(),true)],data};
                    // Memo charges by the byte, and the certificate runs past the default 200,000 units.
                    // A limit with no unit price costs no priority fee; the payer stays account 0.
                    let budget=solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
                    let transaction=Transaction::new_signed_with_payer(&[budget,instruction],Some(&payer.pubkey()),&[&payer],hash);
                    let fee=rpc(&url,"getFeeForMessage",json!([B64.encode(transaction.message_data()),{"commitment":"finalized"}])).await?["value"].as_u64().context("Invalid advertisement fee")?;
                    if balance<rent.saturating_add(reserve).saturating_add(fee) {continue}
                    let bytes=bincode::serialize(&transaction)?;
                    ensure!(bytes.len()<=1232,"Peer advertisement exceeds transaction packet size");
                    let signature=rpc(&url,"sendTransaction",json!([B64.encode(bytes),{"encoding":"base64"}])).await?.as_str().context("Invalid announcement signature")?.to_owned();
                    pending=Some((signature,expiry));
                }
            }
        }
        let epoch=slot/epoch_slots;
        if !fast.admitted(epoch) {continue}
        let mut report=metrics.report();
        let active=fast.active_members(epoch);
        report.pairs.retain(|row|row.members[0]==identity&&row.hub==hub&&row.members.iter().all(|member|active.contains(member)&&catalog.iter().any(|(id,key)|*id>=12&&bs58::encode(key).into_string()==*member)));
        let snapshot=bincode::serialize(&report)?;
        if snapshot.len()<=128*1024 {
            let body=Wire::ExchangeMetrics{epoch,snapshot};
            let signature=crate::crypto::sign_bytes(&key,&bincode::serialize(&body)?);
            link.uplink.send_to(&[0],&bincode::serialize(&Envelope{from:own,body,signature})?).await?;
        }
    }
}
