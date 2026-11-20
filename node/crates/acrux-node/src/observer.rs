//! One consumer projects protocol observations; validator actors never render shared snapshots.
use crate::{exchange::ExchangeEngine, gateway::GatewayState, profiling::Profiling};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{collections::BTreeSet, sync::Arc, time::Instant};
use tokio::sync::{mpsc, oneshot, RwLock};

pub struct Event {
    pub slot: Option<u64>,
    pub anchor: &'static str,
    pub at: u64,
    pub kind: &'static str,
    pub data: Value,
}

enum Message {
    Batch {
        node: usize,
        events: Vec<Event>,
        persist: Vec<(String, Value)>,
    },
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct Observer(mpsc::Sender<Message>);

impl Observer {
    pub fn start(
        gateway: Arc<RwLock<GatewayState>>,
        persistence: mpsc::Sender<(String, Value)>,
        identities: Vec<String>,
        exchange: Arc<ExchangeEngine>,
        profile: Arc<Profiling>,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Message>(4096);
        tokio::spawn(async move {
            let mut finalized = BTreeSet::new();
            let mut skipped = BTreeSet::new();
            while let Some(message) = receiver.recv().await {
                let (node, events, mut persist) = match message {
                    Message::Batch {
                        node,
                        events,
                        persist,
                    } => (node, events, persist),
                    Message::Flush(reply) => {
                        let _ = reply.send(());
                        continue;
                    }
                };
                let lock_start = Instant::now();
                let mut g = gateway.write().await;
                profile.record(
                    Some(node),
                    "observer.gateway_lock_wait",
                    lock_start.elapsed(),
                );
                let projection = Instant::now();
                for mut event in events {
                    if let Some(slot) = event.slot {
                        if event.kind == "cblock.certificate" && event.data["kind"] == "skip" {
                            skipped.insert(slot / 4);
                        }
                        let proposer_event = matches!(
                            event.kind,
                            "block.slice"
                                | "commitment.sent"
                                | "attestation.sent"
                                | "commitment.arrived"
                        );
                        if proposer_event
                            || matches!(
                                event.kind,
                                "attestation.arrived" | "cblock.vote" | "cblock.certificate"
                            )
                            || node == 0
                        {
                            if event.kind.starts_with("cblock.") {
                                for s in slot / 4 * 4..slot / 4 * 4 + 4 {
                                    if let Some(view) = g.slots.get_mut(&s) {
                                        crate::projection::apply(view, event.kind, &event.data);
                                    }
                                }
                            } else if matches!(event.kind, "slot.started" | "slot.executed") {
                                if let Some(view) = g.slots.get_mut(&slot) {
                                    if event.kind == "slot.executed" {
                                        let mut blocks = view["blocks"].clone();
                                        if let Some(blocks) = blocks.as_array_mut() {
                                            for block in blocks {
                                                if let Some(executed) = event.data["blocks"]
                                                    .as_array()
                                                    .and_then(|all| {
                                                        all.iter().find(|candidate| {
                                                            candidate["proposer"]
                                                                == block["proposer"]
                                                        })
                                                    })
                                                {
                                                    block["status"] = executed["status"].clone();
                                                }
                                            }
                                        }
                                        event.data["blocks"] = blocks;
                                        event.data["attestations"] = view["attestations"].clone();
                                    }
                                    crate::projection::apply(view, event.kind, &event.data);
                                    if event.kind == "slot.executed" {
                                        event.data = view.clone();
                                    }
                                } else {
                                    g.slots.insert(slot, event.data.clone());
                                }
                            } else if let Some(view) = g.slots.get_mut(&slot) {
                                crate::projection::apply(view, event.kind, &event.data);
                            }
                        }
                        if node == 0
                            && event.kind == "cblock.finalized"
                            && finalized.insert(slot / 4)
                        {
                            g.finalized_cblock = Some(g.finalized_cblock.map_or(slot / 4, |held| held.max(slot / 4)));
                            let certificate = g
                                .slots
                                .get(&(slot / 4 * 4))
                                .and_then(|view| view["cblock"]["certificates"].as_array())
                                .and_then(|certs| {
                                    certs.iter().rev().find(|cert| {
                                        matches!(
                                            cert["kind"].as_str(),
                                            Some("fast_finalization" | "slow_finalization")
                                        )
                                    })
                                })
                                .cloned();
                            let ids = g
                                .transactions_by_cblock
                                .remove(&(slot / 4))
                                .unwrap_or_default();
                            let updates = ids
                                .into_iter()
                                .filter_map(|id| {
                                    let tx = g.transactions.get_mut(&id)?;
                                    // A transaction that ran and failed is final with its cblock too,
                                    // as Solana finalizes it with its error; it keeps its `failed` state.
                                    match tx["state"].as_str() {
                                        Some("executed") => tx["state"] = json!("finalized"),
                                        Some("failed") if tx["executedSlot"].is_u64() => {}
                                        _ => return None,
                                    }
                                    tx["finalization"] = certificate.clone().unwrap_or(Value::Null);
                                    Some(tx.clone())
                                })
                                .collect::<Vec<_>>();
                            for tx in &updates {
                                // Finalized records are served from the store from here on.
                                g.transactions.remove(tx["header"]["id"].as_str().unwrap());
                            }
                            for tx in updates {
                                persist.push((
                                    format!("transaction/{}", tx["header"]["id"].as_str().unwrap()),
                                    tx.clone(),
                                ));
                                g.emit(
                                    Some(identities[node].clone()),
                                    tx["executedSlot"].as_u64(),
                                    "consensus",
                                    event.at,
                                    "transaction.updated",
                                    tx,
                                );
                            }
                        }
                        if matches!(event.kind, "cblock.finalized" | "cblock.certificate") {
                            for s in slot / 4 * 4..slot / 4 * 4 + 4 {
                                if let Some(view) = g.slots.get(&s) {
                                    persist.push((format!("slot/{s}"), view.clone()));
                                }
                            }
                        } else if matches!(event.kind, "slot.executed" | "slot.skipped" | "commitment.arrived") {
                            if let Some(view) = g.slots.get(&slot) {
                                persist.push((format!("slot/{slot}"), view.clone()));
                            }
                        }
                    }
                    // Executed transactions, failed ones included, wait for their cblock to finalize.
                    if event.kind == "transaction.updated" && matches!(event.data["state"].as_str(), Some("executed" | "failed")) {
                        if let (Some(slot), Some(id)) = (
                            event.data["executedSlot"].as_u64(),
                            event.data["header"]["id"].as_str(),
                        ) {
                            g.transactions_by_cblock
                                .entry(slot / 4)
                                .or_default()
                                .push(id.to_owned());
                        }
                    }
                    if node == 0 && event.kind == "window.started" {
                        g.current_window = event.data.clone();
                    }
                    g.emit(
                        Some(identities[node].clone()),
                        event.slot,
                        event.anchor,
                        event.at,
                        event.kind,
                        event.data,
                    );
                }
                g.counters = json!({"slots":g.slots.keys().next_back().map_or(0,|s|s+1),"finalizedCblocks":g.counter_base.0+finalized.len() as u64,"skips":g.counter_base.1+skipped.len() as u64,"averageCommitSpreadUs":exchange.average_spread()});
                // Member arrival reports can follow finalization by a few seconds.
                // Keep their views through the reporting interval, then retain the updated disk copy.
                let retained=if g.admission_from_slot.is_some(){256}else{64};
                while g.slots.len() > retained {
                    let first = *g.slots.keys().next().unwrap();
                    g.slots.remove(&first);
                }
                drop(g);
                profile.record(Some(node), "observer.projection", projection.elapsed());
                let wait = Instant::now();
                for record in persist {
                    if persistence.send(record).await.is_err() {
                        tracing::error!("Storage writer stopped");
                        return;
                    }
                }
                profile.record(Some(node), "observer.store_queue_wait", wait.elapsed());
            }
        });
        Self(sender)
    }

    pub async fn submit(
        &self,
        node: usize,
        events: Vec<Event>,
        persist: Vec<(String, Value)>,
    ) -> Result<()> {
        self.0
            .send(Message::Batch {
                node,
                events,
                persist,
            })
            .await
            .context("Observer stopped")
    }

    pub async fn flush(&self) -> Result<()> {
        let (reply, done) = oneshot::channel();
        self.0
            .send(Message::Flush(reply))
            .await
            .context("Observer stopped")?;
        done.await.context("Observer stopped before flush")
    }
}
