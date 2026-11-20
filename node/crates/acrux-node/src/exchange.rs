//! Algorithm 1's local hash exchange runs independently of validator execution.
use crate::{
    crypto,
    exchange_metrics::Recorder,
    gateway::GatewayState,
    profiling::Profiling,
    transport::LocalTransport,
    types::{Attestation, Commitment, CommitReceipt, Envelope, Fault, Wire},
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::SigningKey;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Notify, RwLock};

/// Algorithm 1's hash-exchange state, independent of which link carries a commitment.
#[derive(Clone, Default)]
pub(crate) struct LocalExchange {
    pub start: Option<Instant>,
    pub received: BTreeMap<usize, (Instant, Commitment)>,
    pub sent: bool,
}
impl LocalExchange {
    /// The first authenticated commitment anchors the same inclusive 2-delta-local deadline.
    pub fn observe(&mut self, commitment: Commitment, at: Instant) -> bool {
        if self.received.contains_key(&commitment.proposer) { return false }
        self.start = Some(self.start.map_or(at,|first| first.min(at)));
        self.received.insert(commitment.proposer,(at,commitment));
        true
    }
    pub fn sent(&mut self, own: usize, commitment: Commitment, at: Instant) -> bool {
        if self.sent { return false }
        self.sent = true;
        self.start = Some(self.start.map_or(at,|first| first.min(at)));
        self.received.insert(own,(at,commitment));
        true
    }
    pub fn deadline(&self) -> Option<Instant> { self.start.map(|start| start + Duration::from_millis(20)) }
    pub fn vector(&self, own: usize, proposers: usize) -> Option<Vec<Option<[u8;32]>>> {
        if !self.sent { return None }
        let deadline = self.deadline()?;
        Some((1..=proposers).map(|role| self.received.get(&role)
            .filter(|(at,_)| role == own || *at <= deadline)
            .map(|(_,commitment)|commitment.hash)).collect())
    }
}
#[derive(Default)]
struct Exchange {
    local: LocalExchange,
    signatures: BTreeMap<usize, Vec<u8>>,
    equivocated: bool,
    notify: Arc<Notify>,
    done: bool,
    role: Option<usize>,
}

#[derive(Clone)]
enum ExchangeLink {
    Local(Arc<LocalTransport>),
    Member(Arc<crate::member_link::MemberLink>),
}
impl ExchangeLink {
    async fn send(&self, from: usize, to: usize, bytes: Vec<u8>) -> anyhow::Result<()> {
        match self {
            Self::Local(transport) => {
                // Receipt traffic stays out of the legacy hub mirror. Only a current outside
                // connection that has sent a valid receipt can receive this appended Wire kind.
                if to>=12 && bytes.get(8..12)==Some(13u32.to_le_bytes().as_slice()) {
                    if let Some(server)=transport.external_server() {
                        if server.supports_receipts(to) {
                            let envelope:Envelope=bincode::deserialize(&bytes)?;
                            if let Wire::CommitReceipt(receipt)=envelope.body {
                                server.publish_receipt(from,to,receipt.slot,Arc::new(bytes));
                            }
                        }
                    }
                    return Ok(())
                }
                transport.send(from,to,bytes).await
            }
            Self::Member(control) => control.send(from,to,bytes).await,
        }
    }
}

pub struct ExchangeEngine {
    timer_handle: tokio::runtime::Handle,
    transport: ExchangeLink,
    keys: BTreeMap<usize,SigningKey>,
    targets: Vec<usize>,
    gateway: Arc<RwLock<GatewayState>>,
    slots: Mutex<BTreeMap<(usize, u64), Exchange>>,
    spreads: Mutex<SpreadMeasurements>,
    omitted: Mutex<Vec<serde_json::Value>>,
    commit_times: Mutex<BTreeMap<u64, Instant>>,
    sent_at: Mutex<BTreeMap<(usize, u64), Instant>>,
    profile: Arc<Profiling>,
    fault: Option<Fault>,
    metrics: Recorder,
}

#[derive(Default)]
struct SpreadMeasurements {
    values: Vec<u64>,
    total: u128,
}

impl ExchangeEngine {
    pub fn new(
        transport: Arc<LocalTransport>,
        keys: Vec<SigningKey>,
        gateway: Arc<RwLock<GatewayState>>,
        profile: Arc<Profiling>,
        fault: Option<Fault>,
    ) -> Arc<Self> {
        Arc::new(Self {
            timer_handle: tokio::runtime::Handle::current(),
            transport: ExchangeLink::Local(transport),
            targets: (0..keys.len()).collect(),
            keys: keys.into_iter().enumerate().collect(),
            gateway,
            slots: Mutex::new(BTreeMap::new()),
            spreads: Mutex::new(SpreadMeasurements::default()),
            omitted: Mutex::new(Vec::new()),
            commit_times: Mutex::new(BTreeMap::new()),
            sent_at: Mutex::new(BTreeMap::new()),
            profile,
            fault,
            metrics: Recorder::new(),
        })
    }

    /// Runs the identical commitment/key-release engine on an admitted outside validator.
    pub(crate) fn new_member(
        id: usize,
        key: SigningKey,
        control: Arc<crate::member_link::MemberLink>,
        gateway: Arc<RwLock<GatewayState>>,
        profile: Arc<Profiling>,
    ) -> Arc<Self> {
        Arc::new(Self {
            timer_handle: tokio::runtime::Handle::current(),
            transport: ExchangeLink::Member(control),
            keys: BTreeMap::from([(id,key)]),
            targets: (0..12).collect(),
            gateway,
            slots: Mutex::new(BTreeMap::new()),
            spreads: Mutex::new(SpreadMeasurements::default()),
            omitted: Mutex::new(Vec::new()),
            commit_times: Mutex::new(BTreeMap::new()),
            sent_at: Mutex::new(BTreeMap::new()),
            profile,
            fault: None,
            metrics: Recorder::new(),
        })
    }

    /// Finalized draw metadata labels observations; it never changes a protocol deadline.
    pub fn set_window(&self, number:u64, hub:String, catalog:Vec<String>, co_ids:Vec<usize>) {
        self.metrics.set_window(number,hub,catalog,co_ids);
    }

    /// The caller has authenticated this signer and checked the scheduled receipt route.
    pub fn receive_receipt(&self, owner:usize, from:usize, receipt:CommitReceipt, received_at:Instant) {
        self.metrics.received(owner,from,receipt.slot,receipt.proposer,receipt.hash,received_at);
    }

    pub fn metrics_json(&self)->serde_json::Value { self.metrics.json() }
    pub(crate) fn metrics_report(&self)->crate::exchange_metrics::ExchangeReport {self.metrics.report()}
    pub(crate) fn include_report(&self,identity:String,report:crate::exchange_metrics::ExchangeReport) {self.metrics.include_report(identity,report);}

    pub fn launch(
        self: &Arc<Self>,
        id: usize,
        slot: u64,
        q: usize,
        commitment: Commitment,
        key: [u8; 32],
        base: Instant,
        co_ids: Vec<usize>,
    ) {
        let this = self.clone();
        // The Fault Lab's attacker: the scenario's proposer role in the scenario's window.
        let attack = self
            .fault
            .as_ref()
            .filter(|f| f.window == slot / 16 && f.attacker == q)
            .map(|f| f.scenario.clone());
        let delay = if attack.as_deref() == Some("last-look") { Duration::from_millis(28) } else { Duration::ZERO };
        self.timer_handle.spawn(async move {
            let wake_deadline=base+Duration::from_millis(50)+delay;
            tokio::time::sleep_until(wake_deadline.into()).await;
            let sent = Instant::now();
            {
                let mut sent_at = this.sent_at.lock().unwrap();
                sent_at.insert((id, slot), sent);
                sent_at.retain(|(_, old), _| *old + 64 >= slot);
            }
            this.commit_times.lock().unwrap().entry(slot).and_modify(|t| *t = (*t).min(sent)).or_insert(sent);
            this.profile.record(Some(id),"exchange.commit_wake_overshoot",sent.saturating_duration_since(wake_deadline));
            let (notify,received) = {
                let lock_started=Instant::now();
                let mut slots = this.slots.lock().unwrap();
                this.profile.record(Some(id),"exchange.state_lock_wait",lock_started.elapsed());
                slots.retain(|(owner, old), value| *owner!=id || !value.done || *old + 64 >= slot);
                let exchange = slots.entry((id, slot)).or_default();
                if !exchange.local.sent(q,commitment.clone(),sent) { return }
                exchange.role = Some(q);
                (exchange.notify.clone(),exchange.local.received.clone())
            };
            this.metrics.sent(id,slot,commitment.hash,sent);
            this.metrics.spreads(id,slot,received,sent);
            let body = Wire::Commit(commitment.clone());
            let bytes = bincode::serialize(&body).unwrap();
            let signature = crypto::sign_bytes(&this.keys[&id], &bytes);
            let packet = bincode::serialize(&Envelope {
                from: id,
                body,
                signature: signature.clone(),
            })
            .unwrap();
            // Split commitments: the attacker's commitment reaches only co-proposer 1.
            let recipients = co_ids
                .iter()
                .copied()
                .enumerate()
                .filter(|(role, to)| *to != id && (attack.as_deref() != Some("split-commitments") || *role == 0))
                .map(|(_, to)| to)
                .collect::<Vec<_>>();
            if attack.as_deref() == Some("equivocation") && slot % 16 == 0 {
                // A second, conflicting commitment for the same slot, signed by the same key.
                let mut conflicting = commitment.clone();
                conflicting.hash[0] ^= 0xff;
                let body = Wire::Commit(conflicting);
                let signature = crypto::sign_bytes(&this.keys[&id], &bincode::serialize(&body).unwrap());
                let packet = bincode::serialize(&Envelope { from: id, body, signature }).unwrap();
                for to in recipients.iter().copied() {
                    let transport = this.transport.clone();
                    let packet = packet.clone();
                    tokio::spawn(async move {
                        let _ = transport.send(id, to, packet).await;
                    });
                }
            }
            for repeat in 0..3 {
                for to in recipients.iter().copied() {
                    let transport = this.transport.clone();
                    let packet = packet.clone();
                    tokio::spawn(async move {
                        if repeat > 0 {
                            tokio::time::sleep(Duration::from_micros(repeat * 250)).await;
                        }
                        if let Err(error) = transport.send(id, to, packet).await {
                            tracing::warn!(%error,"commitment send failed");
                        }
                    });
                }
            }
            let sent_us = offset(sent, base);
            {let observer=this.clone();let commitment=commitment.clone();let signature=signature.clone();tokio::spawn(async move {observer.event(id,slot,sent_us,"commitment.sent",json!({"slot":slot,"proposer":q,"hash":hex::encode(commitment.hash),"keyHash":hex::encode(commitment.key_hash),"signature":bs58::encode(&signature).into_string(),"sentUs":sent_us,"arrivals":[]})).await;});}
            loop {
                let notified = notify.notified();
                let (all, deadline) = {
                    let slots = this.slots.lock().unwrap();
                    let exchange = &slots[&(id, slot)];
                    (
                        exchange.local.received.len() == co_ids.len(),
                        exchange.local.deadline().unwrap(),
                    )
                };
                if all || Instant::now() >= deadline {
                    break;
                }
                tokio::select! {_=notified=>{},_=tokio::time::sleep_until(deadline.into())=>{}}
            }
            let attested = Instant::now();
            this.profile.record(Some(id),"exchange.commit_to_attest",attested.saturating_duration_since(sent));
            let (pi, omitted) = {
                let mut slots = this.slots.lock().unwrap();
                let exchange = slots.get_mut(&(id, slot)).unwrap();
                if exchange.done {
                    return;
                }
                exchange.done = true;
                if q == 1 {
                    let first = exchange.local.received.values().map(|(at, _)| *at).min().unwrap();
                    let last = exchange.local.received.values().map(|(at, _)| *at).max().unwrap();
                    let value=offset(last,first);
                    let mut spreads=this.spreads.lock().unwrap();
                    spreads.total+=u128::from(value);
                    spreads.values.push(value);
                }
                let pi = exchange.local
                    .vector(q, co_ids.len())
                    .expect("the local commitment has been sent");
                let omitted = pi.iter().any(Option::is_none).then(|| json!({
                    "owner": id,
                    "slot": slot,
                    "proposer": q,
                    "anchor": "slot",
                    "ownSentUs": offset(sent, base),
                    "startUs": offset(exchange.local.start.unwrap(), base),
                    "deadlineUs": offset(exchange.local.deadline().unwrap(), base),
                    "decisionUs": offset(attested, base),
                    "received": exchange.local.received.iter().map(|(proposer, (at, _))| json!({
                        "proposer": proposer,
                        "arrivedUs": offset(*at, base),
                        "relativeToStartUs": offset(*at, exchange.local.start.unwrap())
                    })).collect::<Vec<_>>(),
                    "omittedProposers": pi.iter().enumerate().filter_map(|(index, entry)| entry.is_none().then_some(index + 1)).collect::<Vec<_>>()
                }));
                (pi, omitted)
            };
            if let Some(omitted) = omitted {
                this.omitted.lock().unwrap().push(omitted);
            }
            let attestation = Attestation {
                slot,
                proposer: q,
                pi,
                key,
            };
            let body = Wire::Attest(attestation.clone());
            let encoded = bincode::serialize(&body).unwrap();
            let signature = crypto::sign_bytes(&this.keys[&id], &encoded);
            let packet = bincode::serialize(&Envelope {
                from: id,
                body,
                signature: signature.clone(),
            })
            .unwrap();
            for to in this.targets.iter().copied() {
                let transport = this.transport.clone();
                let bytes = packet.clone();
                tokio::spawn(async move {
                    if let Err(error) = transport.send(id, to, bytes).await {
                        tracing::warn!(%error,"attestation send failed");
                    }
                });
            }
            let at = offset(attested, base);
            this.event(id,slot,at,"attestation.sent",json!({"slot":slot,"proposer":q,"pi":attestation.pi.iter().map(|h|h.map(hex::encode)).collect::<Vec<_>>(),"key":B64.encode(key),"signature":bs58::encode(signature).into_string(),"sentUs":at,"validatorArrivals":[]})).await;
        });
    }

    /// Commit spreads recorded from `from` on, and the index to continue from.
    pub fn spreads_from(&self, from: usize) -> (Vec<u64>, usize) {
        let spreads = self.spreads.lock().unwrap();
        (spreads.values.get(from..).unwrap_or_default().to_vec(), spreads.values.len())
    }

    /// Drops commitment times below `below`; the facts have already read them.
    pub fn prune_commit_times(&self, below: u64) {
        let mut times = self.commit_times.lock().unwrap();
        *times = times.split_off(&below);
    }

    pub fn spreads(&self) -> Vec<u64> {
        self.spreads.lock().unwrap().values.clone()
    }

    /// The earliest commitment of each slot across its proposers, on the one process clock.
    pub fn commit_times(&self) -> BTreeMap<u64, Instant> {
        self.commit_times.lock().unwrap().clone()
    }

    pub fn omitted_commitments(&self) -> Vec<serde_json::Value> {
        self.omitted.lock().unwrap().clone()
    }

    pub fn average_spread(&self) -> u64 {
        let spreads = self.spreads.lock().unwrap();
        if spreads.values.is_empty() {
            0
        } else {
            (spreads.total / spreads.values.len() as u128) as u64
        }
    }

    /// Called immediately after authenticating the direct commitment, before any validator queue.
    pub async fn receive_commit(
        &self,
        receiver: usize,
        from: usize,
        commitment: Commitment,
        signature: Vec<u8>,
        received_at: Instant,
        base: Instant,
    ) {
        let slot = commitment.slot;
        if let Some(sent) = self.sent_at.lock().unwrap().get(&(from, slot)).copied() {
            self.profile.record(Some(receiver), "exchange.commit_flight", received_at.saturating_duration_since(sent));
        }
        let (notify, start, role, spread_received) = {
            let lock_started = Instant::now();
            let mut slots = self.slots.lock().unwrap();
            self.profile.record(
                Some(receiver),
                "exchange.state_lock_wait",
                lock_started.elapsed(),
            );
            let exchange = slots.entry((receiver, slot)).or_default();
            if let Some(first) = exchange.local.received.get(&commitment.proposer).map(|(_, c)| c.clone()) {
                if first.hash != commitment.hash && !exchange.equivocated {
                    exchange.equivocated = true;
                    let proof = Wire::Equivocation {
                        first: (first, exchange.signatures[&commitment.proposer].clone()),
                        second: (commitment.clone(), signature.clone()),
                    };
                    drop(slots);
                    self.broadcast(receiver, proof);
                }
                return;
            }
            exchange.signatures.insert(commitment.proposer, signature.clone());
            self.profile.record(
                Some(receiver),
                "exchange.receive_to_processing",
                received_at.elapsed(),
            );
            exchange.local.observe(commitment.clone(),received_at);
            (
                exchange.notify.clone(),
                exchange.local.start.unwrap(),
                exchange.role,
                exchange.local.sent.then(||exchange.local.received.clone()),
            )
        };
        notify.notify_one();
        if receiver!=from {
            let link=self.transport.clone();
            let signer=self.keys[&receiver].clone();
            let receipt=CommitReceipt{slot,proposer:commitment.proposer,hash:commitment.hash};
            tokio::spawn(async move {
                let body=Wire::CommitReceipt(receipt);
                let signature=crypto::sign_bytes(&signer,&bincode::serialize(&body).unwrap());
                let bytes=bincode::serialize(&Envelope{from:receiver,body,signature}).unwrap();
                if let Err(error)=link.send(receiver,from,bytes).await {tracing::warn!(%error,"commit receipt send failed");}
            });
        }
        if let Some(received)=spread_received {
            self.metrics.spreads(receiver,slot,received,received_at);
        }
        // Receiver role comes from the same scheduled window, supplied by the node identities.
        let receiver_role = if role.is_some() {
            role
        } else {
            let identity =
                bs58::encode(self.keys[&receiver].verifying_key().as_bytes()).into_string();
            let g = self.gateway.read().await;
            g.slots
                .get(&slot)
                .and_then(|s| s.pointer("/window/proposers"))
                .and_then(|v| v.as_array())
                .and_then(|ps| {
                    ps.iter()
                        .position(|p| p["identity"].as_str() == Some(identity.as_str()))
                })
                .map(|i| i + 1)
        };
        if let Some(receiver_role) = receiver_role {
            let relative = offset(received_at, start);
            self.event(receiver,slot,offset(received_at,base),"commitment.arrived",json!({"proposer":commitment.proposer,"hash":hex::encode(commitment.hash),"arrival":{"receiver":receiver_role,"startUs":offset(start,base),"arrivedUs":offset(received_at,base),"relativeToStartUs":relative,"deadlineUs":20000,"onTime":relative<=20000}})).await;
        }
        let _ = (from, signature);
    }

    /// Sends a message signed by `from` to every validator.
    fn broadcast(&self, from: usize, body: Wire) {
        let signature = crypto::sign_bytes(&self.keys[&from], &bincode::serialize(&body).unwrap());
        let packet = bincode::serialize(&Envelope { from, body, signature }).unwrap();
        for to in self.targets.iter().copied() {
            let transport = self.transport.clone();
            let packet = packet.clone();
            tokio::spawn(async move {
                let _ = transport.send(from, to, packet).await;
            });
        }
    }

    async fn event(&self, id: usize, slot: u64, at: u64, kind: &str, data: serde_json::Value) {
        if matches!(&self.transport,ExchangeLink::Member(_)) && kind=="commitment.arrived" {self.metrics.arrival(slot,&data);}
        let lock_started = Instant::now();
        let mut gateway = self.gateway.write().await;
        self.profile.record(
            Some(id),
            "exchange.gateway_lock_wait",
            lock_started.elapsed(),
        );
        let _hold = self.profile.measure(Some(id), "exchange.gateway_lock_hold");
        if let Some(view) = gateway.slots.get_mut(&slot) {
            crate::projection::apply(view, kind, &data);
        }
        gateway.emit(
            Some(bs58::encode(self.keys[&id].verifying_key().as_bytes()).into_string()),
            Some(slot),
            "slot",
            at,
            kind,
            data,
        );
    }
}
fn offset(now: Instant, base: Instant) -> u64 {
    now.saturating_duration_since(base).as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commitment(proposer: usize) -> Commitment {
        Commitment {
            slot: 7,
            proposer,
            hash: [proposer as u8; 32],
            key_hash: [9; 32],
        }
    }

    #[test]
    fn attestation_uses_first_arrival_anchor_and_inclusive_twenty_ms_deadline() {
        let first = Instant::now();
        let own_sent = first + Duration::from_millis(7);
        let mut exchange = LocalExchange {
            start: Some(first.min(own_sent)),
            sent: true,
            ..LocalExchange::default()
        };
        exchange.received.insert(1, (own_sent, commitment(1)));
        exchange
            .received
            .insert(2, (first + Duration::from_millis(20), commitment(2)));
        exchange
            .received
            .insert(3, (first + Duration::from_micros(20_001), commitment(3)));
        assert_eq!(
            exchange.vector(1, 3),
            Some(vec![Some([1; 32]), Some([2; 32]), None])
        );
        // Anchoring at our own send would wrongly accept proposer 3 for another seven ms.
        assert!(exchange.received[&3].0 < own_sent + Duration::from_millis(20));
    }

    #[test]
    fn key_release_requires_own_commit_and_always_attests_own_block() {
        let first = Instant::now();
        let mut exchange = LocalExchange {
            start: Some(first),
            ..LocalExchange::default()
        };
        exchange.received.insert(2, (first, commitment(2)));
        assert_eq!(exchange.vector(1, 3), None);
        // Even an adversarially early peer hash cannot erase the local commitment.
        exchange.sent = true;
        exchange
            .received
            .insert(1, (first + Duration::from_millis(25), commitment(1)));
        assert_eq!(
            exchange.vector(1, 3),
            Some(vec![Some([1; 32]), Some([2; 32]), None])
        );
    }
    #[test]
    fn late_external_commitment_has_the_same_inclusive_deadline_as_genesis() {
        let first = Instant::now();
        let mut genesis = LocalExchange::default();
        let mut external = LocalExchange::default();
        for exchange in [&mut genesis,&mut external] {
            exchange.observe(commitment(2),first);
            assert!(exchange.sent(1,commitment(1),first + Duration::from_millis(7)));
        }
        genesis.observe(commitment(3),first + Duration::from_micros(20_001));
        let mut outside = commitment(3); outside.hash = [77;32];
        external.observe(outside.clone(),first + Duration::from_micros(20_001));
        let expected = Some(vec![Some([1;32]),Some([2;32]),None]);
        assert_eq!(genesis.vector(1,3),expected);
        assert_eq!(external.vector(1,3),expected,"an outside commitment gets no extra network allowance");
        assert_eq!(external.deadline(),Some(first + Duration::from_millis(20)));
        // At exactly 2 delta-local the same outside commitment is included.
        let mut on_time = LocalExchange::default();
        on_time.observe(commitment(2),first);
        on_time.sent(1,commitment(1),first + Duration::from_millis(7));
        on_time.observe(outside,first + Duration::from_millis(20));
        assert_eq!(on_time.vector(1,3),Some(vec![Some([1;32]),Some([2;32]),Some([77;32])]));
    }

}
