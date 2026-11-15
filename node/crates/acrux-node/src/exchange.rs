//! Algorithm 1's local hash exchange runs independently of validator execution.
use crate::{
    crypto,
    gateway::GatewayState,
    profiling::Profiling,
    transport::LocalTransport,
    types::{Attestation, Commitment, Envelope, Fault, Wire},
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

#[derive(Default)]
struct Exchange {
    start: Option<Instant>,
    received: BTreeMap<usize, (Instant, Commitment)>,
    signatures: BTreeMap<usize, Vec<u8>>,
    equivocated: bool,
    notify: Arc<Notify>,
    sent: bool,
    done: bool,
    role: Option<usize>,
}

impl Exchange {
    fn attestation_vector(&self, own: usize, proposers: usize) -> Option<Vec<Option<[u8; 32]>>> {
        if !self.sent {
            return None;
        }
        let deadline = self.start? + Duration::from_millis(20);
        Some(
            (1..=proposers)
                .map(|i| {
                    self.received
                        .get(&i)
                        .filter(|(at, _)| i == own || *at <= deadline)
                        .map(|(_, commitment)| commitment.hash)
                })
                .collect(),
        )
    }
}

pub struct ExchangeEngine {
    timer_handle: tokio::runtime::Handle,
    transport: Arc<LocalTransport>,
    keys: Vec<SigningKey>,
    gateway: Arc<RwLock<GatewayState>>,
    slots: Mutex<BTreeMap<(usize, u64), Exchange>>,
    spreads: Mutex<SpreadMeasurements>,
    omitted: Mutex<Vec<serde_json::Value>>,
    commit_times: Mutex<BTreeMap<u64, Instant>>,
    sent_at: Mutex<BTreeMap<(usize, u64), Instant>>,
    profile: Arc<Profiling>,
    fault: Option<Fault>,
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
            transport,
            keys,
            gateway,
            slots: Mutex::new(BTreeMap::new()),
            spreads: Mutex::new(SpreadMeasurements::default()),
            omitted: Mutex::new(Vec::new()),
            commit_times: Mutex::new(BTreeMap::new()),
            sent_at: Mutex::new(BTreeMap::new()),
            profile,
            fault,
        })
    }

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
            let notify = {
                let lock_started=Instant::now();
                let mut slots = this.slots.lock().unwrap();
                this.profile.record(Some(id),"exchange.state_lock_wait",lock_started.elapsed());
                slots.retain(|(owner, old), value| *owner!=id || !value.done || *old + 64 >= slot);
                let exchange = slots.entry((id, slot)).or_default();
                if exchange.sent {
                    return;
                }
                exchange.sent = true;
                exchange.role = Some(q);
                exchange.start = Some(exchange.start.map_or(sent, |first| first.min(sent)));
                exchange.received.insert(q, (sent, commitment.clone()));
                exchange.notify.clone()
            };
            let body = Wire::Commit(commitment.clone());
            let bytes = bincode::serialize(&body).unwrap();
            let signature = crypto::sign_bytes(&this.keys[id], &bytes);
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
                let signature = crypto::sign_bytes(&this.keys[id], &bincode::serialize(&body).unwrap());
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
                        exchange.received.len() == co_ids.len(),
                        exchange.start.unwrap() + Duration::from_millis(20),
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
                    let first = exchange.received.values().map(|(at, _)| *at).min().unwrap();
                    let last = exchange.received.values().map(|(at, _)| *at).max().unwrap();
                    let value=offset(last,first);
                    let mut spreads=this.spreads.lock().unwrap();
                    spreads.total+=u128::from(value);
                    spreads.values.push(value);
                }
                let pi = exchange
                    .attestation_vector(q, co_ids.len())
                    .expect("the local commitment has been sent");
                let omitted = pi.iter().any(Option::is_none).then(|| json!({
                    "owner": id,
                    "slot": slot,
                    "proposer": q,
                    "anchor": "slot",
                    "ownSentUs": offset(sent, base),
                    "startUs": offset(exchange.start.unwrap(), base),
                    "deadlineUs": offset(exchange.start.unwrap() + Duration::from_millis(20), base),
                    "decisionUs": offset(attested, base),
                    "received": exchange.received.iter().map(|(proposer, (at, _))| json!({
                        "proposer": proposer,
                        "arrivedUs": offset(*at, base),
                        "relativeToStartUs": offset(*at, exchange.start.unwrap())
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
            let signature = crypto::sign_bytes(&this.keys[id], &encoded);
            let packet = bincode::serialize(&Envelope {
                from: id,
                body,
                signature: signature.clone(),
            })
            .unwrap();
            for to in 0..this.keys.len() {
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
        let (notify, start, role) = {
            let lock_started = Instant::now();
            let mut slots = self.slots.lock().unwrap();
            self.profile.record(
                Some(receiver),
                "exchange.state_lock_wait",
                lock_started.elapsed(),
            );
            let exchange = slots.entry((receiver, slot)).or_default();
            if let Some(first) = exchange.received.get(&commitment.proposer).map(|(_, c)| c.clone()) {
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
            exchange.start = Some(
                exchange
                    .start
                    .map_or(received_at, |first| first.min(received_at)),
            );
            exchange
                .received
                .insert(commitment.proposer, (received_at, commitment.clone()));
            (
                exchange.notify.clone(),
                exchange.start.unwrap(),
                exchange.role,
            )
        };
        notify.notify_one();
        // Receiver role comes from the same scheduled window, supplied by the node identities.
        let receiver_role = if role.is_some() {
            role
        } else {
            let identity =
                bs58::encode(self.keys[receiver].verifying_key().as_bytes()).into_string();
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
        let signature = crypto::sign_bytes(&self.keys[from], &bincode::serialize(&body).unwrap());
        let packet = bincode::serialize(&Envelope { from, body, signature }).unwrap();
        for to in 0..self.keys.len() {
            let transport = self.transport.clone();
            let packet = packet.clone();
            tokio::spawn(async move {
                let _ = transport.send(from, to, packet).await;
            });
        }
    }

    async fn event(&self, id: usize, slot: u64, at: u64, kind: &str, data: serde_json::Value) {
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
            Some(bs58::encode(self.keys[id].verifying_key().as_bytes()).into_string()),
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
        let mut exchange = Exchange {
            start: Some(first.min(own_sent)),
            sent: true,
            ..Exchange::default()
        };
        exchange.received.insert(1, (own_sent, commitment(1)));
        exchange
            .received
            .insert(2, (first + Duration::from_millis(20), commitment(2)));
        exchange
            .received
            .insert(3, (first + Duration::from_micros(20_001), commitment(3)));
        assert_eq!(
            exchange.attestation_vector(1, 3),
            Some(vec![Some([1; 32]), Some([2; 32]), None])
        );
        // Anchoring at our own send would wrongly accept proposer 3 for another seven ms.
        assert!(exchange.received[&3].0 < own_sent + Duration::from_millis(20));
    }

    #[test]
    fn key_release_requires_own_commit_and_always_attests_own_block() {
        let first = Instant::now();
        let mut exchange = Exchange {
            start: Some(first),
            ..Exchange::default()
        };
        exchange.received.insert(2, (first, commitment(2)));
        assert_eq!(exchange.attestation_vector(1, 3), None);
        // Even an adversarially early peer hash cannot erase the local commitment.
        exchange.sent = true;
        exchange
            .received
            .insert(1, (first + Duration::from_millis(25), commitment(1)));
        assert_eq!(
            exchange.attestation_vector(1, 3),
            Some(vec![Some([1; 32]), Some([2; 32]), None])
        );
    }
}
