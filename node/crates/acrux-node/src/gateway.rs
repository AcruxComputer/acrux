//! Read-only protocol snapshots and the browser's plain-transaction submit boundary.
use crate::storage::Store;
use anyhow::Result;
use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        ConnectInfo, DefaultBodyLimit, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};
use tower_http::cors::{AllowHeaders, CorsLayer};

pub type SharedGateway = Arc<RwLock<GatewayState>>;
pub type CommandReply = oneshot::Sender<std::result::Result<Value, String>>;
pub enum GatewayCommand {
    LatestBlockhash {
        reply: CommandReply,
    },
    SubmitPlain {
        params: Value,
        reply: CommandReply,
    },
    SubmitFragment {
        params: Value,
        reply: CommandReply,
    },
    Credit {
        address: String,
        lamports: u64,
        reply: CommandReply,
    },
    /// What the faucet can still grant.
    Faucet {
        reply: CommandReply,
    },
    /// Native ACRUX's counters, from the executor's hashed cutover account.
    NativeSupply {
        reply: CommandReply,
    },
    ProposerEpoch {
        epoch: u64,
        reply: CommandReply,
    },
    ValidatorSet { epoch:Option<u64>,reply:CommandReply },
    ExecutionAgreement {slot:u64,reply:CommandReply},
    Balance {
        address: String,
        reply: CommandReply,
    },
    DemoPool {
        reply: CommandReply,
    },
    /// Solana's JSON-RPC, called with array params.
    Solana {
        method: String,
        params: Value,
        /// The caller's address, charged for simulations; `None` for the gateway's own reads.
        client: Option<std::net::IpAddr>,
        reply: CommandReply,
    },
    /// For `acrux node follow`: the executed state at the latest finalized cblock.
    ExecutionSnapshot {
        reply: CommandReply,
    },
    /// For `acrux node follow`: a slot's encrypted blocks and its attestations.
    SlotBlocks {
        slot: u64,
        reply: CommandReply,
    },
    FollowerBlocks {
        from: u64,
        count: u64,
        reply: CommandReply,
    },
    Quote {
        params: Value,
        reply: CommandReply,
    },
}

pub struct GatewayState {
    pub exchange_metrics: Option<std::sync::Weak<crate::exchange::ExchangeEngine>>,
    pub slots: BTreeMap<u64, Value>,
    /// Same run store as Network; serves records evicted from the recent-memory cache.
    pub store: Option<Store>,
    pub transactions: BTreeMap<String, Value>,
    /// Solana signatures of executed transactions, to their ids, while the store keeps them.
    pub signatures: BTreeMap<String, String>,
    pub transactions_by_cblock: BTreeMap<u64, Vec<String>>,
    /// Public addresses and pinned QUIC certificate, published only when the peer endpoint runs.
    pub hub_peers: BTreeMap<String, Value>,
    pub hub_member_peers: BTreeMap<String, Value>,
    pub current_window: Value,
    pub upcoming_windows: Vec<Value>,
    pub counters: Value,
    /// Finalized cblocks and skips before this run, for a chain continued from its saved state.
    pub counter_base: (u64, u64),
    /// The transactions people sent, kept for days, when the net persists.
    pub kept: Option<Arc<crate::history::History>>,
    pub facts: Value,
    /// Published by node zero's durable finalization event, independently of the explorer cache.
    pub finalized_cblock: Option<u64>,
    pub follower_retained_slots: u64,
    pub follower_retained_block_bytes: u64,
    pub follower_earliest_slot: Option<u64>,
    /// Finalized cblocks' proofs for Solana's light client, by consensus slot: the hash's
    /// preimage, the outbox and the signed finalizing votes.
    pub cblock_proofs: BTreeMap<u64, Value>,
    /// Archived payloads stay in redb; only published slots/epochs/sequences are indexed here.
    pub bridge_archive: Option<Arc<crate::bridge::ProofArchiveStore>>,
    pub archived_withdrawals: BTreeMap<u64, Vec<u64>>,
    pub epoch_cblock_proofs: BTreeMap<u64, u64>,
    pub bridge_program: Option<String>,
    pub bridge_epoch_slots: Option<u64>,
    pub fee_burn_from_slot: Option<u64>,
    pub admission_from_slot: Option<u64>,
    pub fee_burns: Option<crate::fee_burns::FeeBurnStore>,
    /// Volatile acknowledgement of the relayer's durable Postgres save cursor.
    pub bridge_saved_cursor: Option<u64>,
    pub parameters: Value,
    pub balances: BTreeMap<String, u64>,
    pub stream_id: String,
    pub sequence: u64,
    pub history: VecDeque<Value>,
    /// Each event with its JSON text, serialized once for every viewer.
    pub events: broadcast::Sender<Arc<(Value, String)>>,
    pub commands: mpsc::Sender<GatewayCommand>,
}

impl GatewayState {
    pub fn keep_cblock_proof(&mut self, slot: u64, proof: Value) {
        self.cblock_proofs.insert(slot, proof);
        while self.cblock_proofs.len() > 4096 { self.cblock_proofs.pop_first(); }
    }
    pub fn new(commands: mpsc::Sender<GatewayCommand>) -> Self {
        let (events, _) = broadcast::channel(8192);
        Self {
            exchange_metrics: None,
            slots: BTreeMap::new(),
            store: None,
            transactions: BTreeMap::new(),
            signatures: BTreeMap::new(),
            transactions_by_cblock: BTreeMap::new(),
            hub_peers: BTreeMap::new(),
            hub_member_peers: BTreeMap::new(),
            current_window: Value::Null,
            upcoming_windows: vec![],
            facts: Value::Null,
            finalized_cblock: None,
            follower_retained_slots: 0,
            follower_retained_block_bytes: 0,
            follower_earliest_slot: None,
            cblock_proofs: BTreeMap::new(),
            bridge_archive: None,
            archived_withdrawals: BTreeMap::new(),
            epoch_cblock_proofs: BTreeMap::new(),
            bridge_program: None,
            bridge_epoch_slots: None,
            fee_burn_from_slot: None, admission_from_slot: None,
            fee_burns: None,
            bridge_saved_cursor: None,
            counters: json!({"slots":0,"finalizedCblocks":0,"skips":0,"averageCommitSpreadUs":0}),
            counter_base: (0, 0),
            kept: None,
            parameters: Value::Null,
            balances: BTreeMap::new(),
            stream_id: format!("acrux-{}", rand::random::<u64>()),
            sequence: 0,
            history: VecDeque::new(),
            events,
            commands,
        }
    }
    pub fn snapshot(&self) -> Value {
        json!({"streamId":self.stream_id,"cursor":self.sequence,"status":"live","currentWindow":self.current_window,
            "recentSlots":self.slots.values().rev().take(64).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),
            "upcomingWindows":self.upcoming_windows,"counters":self.counters,"parameters":self.parameters})
    }
    pub fn emit(
        &mut self,
        node: Option<String>,
        slot: Option<u64>,
        anchor: &str,
        at_us: u64,
        kind: &str,
        data: Value,
    ) {
        self.sequence += 1;
        let mut event = json!({"version":1,"sequence":self.sequence,"streamId":self.stream_id,"slot":slot,"anchor":anchor,"atUs":at_us,"type":kind,"data":data});
        if let Some(node) = node {
            event["node"] = node.into();
        }
        self.history.push_back(event.clone());
        while self.history.len() > 24_000 {
            self.history.pop_front();
        }
        if self.events.receiver_count() > 0 {
            let text = event.to_string();
            let _ = self.events.send(Arc::new((event, text)));
        }
    }
    fn control(&self, kind: &str, data: Value) -> Value {
        json!({"version":1,"sequence":self.sequence,"streamId":self.stream_id,"slot":null,"anchor":"stream","atUs":0,"type":kind,"data":data})
    }
}

/// Browser origins allowed to call the gateway: `ACRUX_CORS_ORIGINS` (comma separated), by default
/// the site's local ports.
fn origins() -> Vec<HeaderValue> {
    if any_origin() {
        return vec![];
    }
    std::env::var("ACRUX_CORS_ORIGINS")
        .unwrap_or_else(|_| "http://127.0.0.1:4910,http://localhost:4910,http://127.0.0.1:4911,http://localhost:4911".into())
        .split(',')
        .map(|s| HeaderValue::from_str(s.trim()).expect("ACRUX_CORS_ORIGINS holds origins"))
        .collect()
}
/// `ACRUX_CORS_ORIGINS=*`: every origin, as Solana's public RPCs serve wallets and dapps.
fn any_origin() -> bool {
    std::env::var("ACRUX_CORS_ORIGINS").is_ok_and(|v| v.trim() == "*")
}
fn valid_origin(headers: &HeaderMap) -> bool {
    any_origin()
        || headers
            .get(header::ORIGIN)
            .map(|origin| origins().contains(origin))
            .unwrap_or(true)
}
fn error(code: &str, message: &str) -> Value {
    json!({"error":{"code":code,"message":message,"retryable":(["NET_UNAVAILABLE","NET_STALLED","PROGRAM_UNAVAILABLE"].contains(&code)),"stage":"read"}})
}
fn rpc_error(id: Value, rpc_code: i32, code: &str, message: &str) -> Json<Value> {
    Json(
        json!({"jsonrpc":"2.0","id":id,"error":{"code":rpc_code,"message":message,"data":error(code,message)}}),
    )
}

pub async fn run_gateway(
    state: SharedGateway,
    http_port: u16,
    websocket_port: u16,
    metrics_port: u16,
) -> Result<()> {
    let cors = CorsLayer::new()
        .allow_origin(if any_origin() { tower_http::cors::AllowOrigin::any() } else { tower_http::cors::AllowOrigin::list(origins()) })
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(AllowHeaders::mirror_request());
    let http = Router::new()
        .route("/", post(rpc).get(upgrade_solana))
        .route("/rpc", post(rpc))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(cors.clone())
        .with_state(state.clone());
    let websocket = Router::new()
        .route("/", get(upgrade))
        .route("/events", get(upgrade))
        .layer(cors)
        .with_state(state.clone());
    // The private port: metrics, and the faucet's grant route that only the API calls. It never
    // gets a public domain.
    let metrics = Router::new()
        .route("/metrics", get(metrics))
        .route("/internal/hash-exchange", get(hash_exchange))
        .route("/health", get(health))
        .route("/internal/faucet", post(credit).get(faucet))
        .route("/internal/bridge-saved", post(bridge_saved).layer(DefaultBodyLimit::max(256)))
        .with_state(state);
    // 127.0.0.1 locally; a container sets ACRUX_BIND=0.0.0.0.
    let bind: IpAddr = std::env::var("ACRUX_BIND").unwrap_or_else(|_| "127.0.0.1".into()).parse()?;
    let http_listener =
        tokio::net::TcpListener::bind(SocketAddr::new(bind, http_port))
            .await?;
    let ws_listener = tokio::net::TcpListener::bind(SocketAddr::new(bind, websocket_port))
    .await?;
    let metrics_listener = tokio::net::TcpListener::bind(SocketAddr::new(bind, metrics_port))
    .await?;
    tokio::try_join!(
        async { axum::serve(http_listener, http.into_make_service_with_connect_info::<SocketAddr>()).await },
        async { axum::serve(ws_listener, websocket.into_make_service_with_connect_info::<SocketAddr>()).await },
        async { axum::serve(metrics_listener, metrics).await }
    )?;
    Ok(())
}

/// The `@acrux/contract` version whose shapes the gateway answers in.
pub const CONTRACT_VERSION: &str = "0.1.27";

async fn health(State(state): State<SharedGateway>) -> Json<Value> {
    let state = state.read().await;
    Json(
        // A chain state that cannot be written holds every finalization back, so the node is not ready.
        json!({"status":if state.current_window.is_null() || crate::state::failing(){"waiting"}else{"ready"},"contractVersion":CONTRACT_VERSION,"missingConfiguration":[],
            "chainState":{"durableCblock":match crate::state::durable(){u64::MAX=>Value::Null,cblock=>json!(cblock)},"writeFailing":crate::state::failing()}}),
    )
}
async fn rpc(State(state): State<SharedGateway>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, body: String) -> Response {
    let client = crate::limits::limits().client(peer, &headers);
    if !valid_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(error("ORIGIN_NOT_ALLOWED", "This origin is not allowed.")),
        )
            .into_response();
    }
    let request: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => {
            return rpc_error(Value::Null, -32700, "INVALID_INPUT", "Expected JSON.")
                .into_response()
        }
    };
    // A batch, as web3.js sends for some calls: each request answered in order, each counted
    // against the client's limits on its own.
    if let Some(batch) = request.as_array() {
        if batch.is_empty() || batch.len() > 100 {
            return rpc_error(Value::Null, -32600, "INVALID_INPUT", "A batch holds 1 to 100 requests.").into_response();
        }
        let mut answers = Vec::with_capacity(batch.len());
        for one in batch.clone() {
            let response = Box::pin(rpc_one(state.clone(), client, one)).await;
            let bytes = axum::body::to_bytes(response.into_body(), 8 << 20).await.unwrap_or_default();
            answers.push(serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null));
        }
        return Json(Value::Array(answers)).into_response();
    }
    rpc_one(state, client, request).await
}
async fn rpc_one(state: SharedGateway, client: std::net::IpAddr, request: Value) -> Response {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !(id.is_string() || safe_integer(&id).is_some())
    {
        return rpc_error(
            id,
            -32600,
            "INVALID_INPUT",
            "Expected a JSON-RPC 2.0 request with an id.",
        )
        .into_response();
    }
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let limits = crate::limits::limits();
    let allowed = if matches!(method, "submitTransaction" | "submitFragment" | "sendTransaction") { limits.submit(client) } else { limits.read(client) };
    if !allowed {
        return (StatusCode::TOO_MANY_REQUESTS, rpc_error(id, -32005, "RATE_LIMITED", "Too many requests from this address; slow down.")).into_response();
    }
    // A call that leaves `params` out is Solana's form for the methods both sides name: web3.js and
    // wallets send `getSlot` and `getLatestBlockhash` that way. Acrux-only methods default to `{}`.
    let omitted = request.get("params").is_none();
    let params = request.get("params").cloned().unwrap_or(json!({}));
    // Solana's methods take array params; Acrux's take an object. Solana-only methods may omit them.
    let solana_only = matches!(method, "getHealth" | "getVersion" | "getGenesisHash" | "getFeeForMessage" | "simulateTransaction" | "getRecentPrioritizationFees" | "getBlockTime" | "getTokenAccountsByOwner" | "getTokenAccountBalance" | "getTokenSupply" | "getSupply" | "getBlockHeight" | "getEpochInfo" | "getAccountInfo" | "getMultipleAccounts" | "getProgramAccounts" | "getMinimumBalanceForRentExemption" | "getSignaturesForAddress" | "getVoteAccounts" | "sendTransaction" | "getSignatureStatuses" | "getRecentPerformanceSamples");
    let dual = matches!(method, "getSlot" | "getLatestBlockhash" | "getBalance" | "getTransaction");
    if params.is_array() || (omitted && dual) || (solana_only && !params.is_object()) || (solana_only && params.as_object().is_some_and(|o| o.is_empty())) {
        let params = if params.is_array() { params } else { json!([]) };
        // ACRUX_LOG_RPC=1: one line per Solana call, its method and encoding, to see what a wallet asks.
        if std::env::var("ACRUX_LOG_RPC").is_ok_and(|v| v == "1") {
            let encoding = params.as_array().into_iter().flatten().find_map(|p| p.get("encoding").and_then(Value::as_str)).unwrap_or("");
            eprintln!("solana-rpc {method} {encoding}");
        }
        // Solana's own caps on a list argument: 256 signatures a status call, 100 accounts otherwise.
        let list_cap = if method == "getSignatureStatuses" { 256 } else { 100 };
        if !bounded_strings(&params) || params.get(0).and_then(Value::as_array).is_some_and(|keys| keys.len() > list_cap) {
            return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "Invalid params: a string or list is too long."}})).into_response();
        }
        return match command(&state, |reply| GatewayCommand::Solana { method: method.to_owned(), params, client: Some(client), reply }).await {
            Ok(result) => Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response(),
            Err(message) => {
                let code = if message.starts_with("Method not found") { -32601 } else { -32002 };
                Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})).into_response()
            }
        };
    }
    if !params.is_object() {
        return rpc_error(id, -32602, "INVALID_INPUT", "Params must be an object.").into_response();
    }
    let result: std::result::Result<Value, (&str, &str)> = match method {
        "getSnapshot" => Ok(state.read().await.snapshot()),
        "getHubPeers" => {
            match params.as_object().filter(|params|params.keys().all(|key|key=="hub"||key=="direct")&&params.get("direct").is_none_or(|value|value==true)).and_then(|params|params.get("hub")).and_then(Value::as_str) {
                Some(hub) if !hub.is_empty() && hub.len() <= 16 => {
                    let state=state.read().await;
                    let peers=if params["direct"]==true {state.hub_member_peers.get(hub)}
                        else{state.hub_peers.get(hub).or_else(||state.hub_member_peers.get(hub))};
                    peers.cloned().ok_or(("NOT_FOUND","This hub has no public peer endpoint."))
                },
                _ => Err(("INVALID_INPUT","Give {hub: string}, optionally with direct: true.")),
            }
        },
        "getCurrentWindow" => Ok(state.read().await.current_window.clone()),
        "getCounters" => Ok(state.read().await.counters.clone()),
        "getValidatorSet" => {
            if params.as_object().is_none_or(|params|params.keys().any(|key|key!="epoch")) || params.get("epoch").is_some_and(|value|safe_integer(value).is_none()) {
                Err(("INVALID_INPUT","Give an optional nonnegative epoch."))
            } else {command(&state,|reply|GatewayCommand::ValidatorSet{epoch:params.get("epoch").and_then(safe_integer),reply}).await.map_err(|_|("NET_UNAVAILABLE","This epoch's validator snapshot is not finalized yet."))}
        },
        "getProposerEpoch" => {
            match params.get("epoch").and_then(safe_integer) {
                Some(epoch) => match command(&state, |reply| GatewayCommand::ProposerEpoch { epoch, reply }).await {
                    Ok(value) => Ok(value),
                    Err(_) => Err(("NET_UNAVAILABLE", "This epoch's finalized proposer schedule is not available.")),
                },
                None => Err(("INVALID_INPUT", "epoch must be a nonnegative safe integer.")),
            }
        },
        "getNativeSupply" => {
            if params.as_object().is_some_and(|fields| !fields.is_empty()) {
                Err(("INVALID_INPUT", "getNativeSupply takes no parameters."))
            } else {
                match command(&state, |reply| GatewayCommand::NativeSupply { reply }).await {
                    Ok(value) => Ok(value),
                    Err(_) => Err(("NET_UNAVAILABLE", "The native supply could not be read.")),
                }
            }
        },
        "getFeeBurns" => fee_burn_page(&state,&params).await,
        "getCblockProof" => {
            let state = state.read().await;
            let selectors = [params.get("consensusSlot"), params.get("epoch"), params.get("latest"), params.get("feeBurnEpoch")].iter().filter(|selector| selector.is_some()).count();
            let slot = params.get("consensusSlot").and_then(safe_integer);
            let epoch = params.get("epoch").and_then(safe_integer);
            let fee_burn_epoch = params.get("feeBurnEpoch").and_then(safe_integer);
            if selectors != 1 || (slot.is_none() && epoch.is_none() && fee_burn_epoch.is_none() && params.get("latest") != Some(&Value::Bool(true))) {
                Err(("INVALID_INPUT", "Give exactly one of consensusSlot, epoch, feeBurnEpoch, or latest: true."))
            } else {
                let selected = if let Some(slot) = slot {
                    Ok(Some(slot))
                } else if let Some(epoch) = epoch {
                    Ok(state.epoch_cblock_proofs.get(&epoch).copied().or_else(|| {
                        state.cblock_proofs.iter().find_map(|(slot, proof)| {
                            let length = proof["epochSlots"].as_u64().filter(|length| *length > 0)?;
                            (slot*4/length==epoch && crate::bridge::handoff_proof(proof).unwrap_or(false)).then_some(*slot)
                        })
                    }))
                } else if let Some(epoch) = fee_burn_epoch {
                    match (state.fee_burn_from_slot, state.bridge_epoch_slots) {
                        (Some(from), Some(length)) if epoch >= from / length => epoch.checked_add(1).and_then(|next| next.checked_mul(length / 4)).and_then(|next| next.checked_sub(1))
                            .map(Some).ok_or(("INVALID_INPUT", "feeBurnEpoch is too large.")),
                        _ => Err(("INVALID_INPUT", "The fee-burn rule is not active for this epoch.")),
                    }
                } else {
                    Ok(state.cblock_proofs.keys().next_back().into_iter().chain(state.archived_withdrawals.keys().next_back()).max().copied())
                };
                match selected {
                    Err(error) => Err(error),
                    Ok(Some(slot)) => match state.cblock_proofs.get(&slot) {
                        Some(proof) => Ok(proof.clone()),
                        None if state.archived_withdrawals.contains_key(&slot) => match state.bridge_archive.as_ref().map(|archive| archive.get(slot)).transpose() {
                            Ok(Some(Some(proof))) => Ok(proof),
                            Ok(_) => Err(("NOT_FOUND", "No finalized bridge proof is held for this selector.")),
                            Err(_) => Err(("INTERNAL_ERROR", "The archived bridge proof could not be read.")),
                        },
                        // Epoch-end proofs are permanent. Only a durable descendant past an absent
                        // end proves it was skipped; an incomplete epoch remains NOT_FOUND.
                        None if fee_burn_epoch.is_some() && state.finalized_cblock.is_some_and(|head| head > slot) => Ok(Value::Null),
                        None => Err(("NOT_FOUND", "No finalized bridge proof is held for this selector.")),
                    },
                    Ok(None) => Err(("NOT_FOUND", "No finalized bridge proof is held for this selector.")),
                }
            }
        },
        "getFacts" => {
            let (facts,exchange)={let gateway=state.read().await;(gateway.facts.clone(),gateway.exchange_metrics.as_ref().and_then(std::sync::Weak::upgrade))};
            match facts {
                Value::Null => Err(("NET_UNAVAILABLE", "The net has not measured enough slots yet.")),
                mut facts => {if let Some(exchange)=exchange {crate::facts::include_hash_exchange(&mut facts,exchange.metrics_json());}Ok(facts)},
            }
        },
        "getRecentSlots" => match read_limit(&params, 64) {
            Ok(limit) => Ok(json!(state
                .read()
                .await
                .slots
                .values()
                .rev()
                .take(limit)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>())),
            Err(error) => Err(error),
        },
        "getSchedule" => match read_limit(&params, 8) {
            Ok(limit) => Ok(json!(state
                .read()
                .await
                .upcoming_windows
                .iter()
                .take(limit)
                .collect::<Vec<_>>())),
            Err(error) => Err(error),
        },
        "getSlot" => match params.get("slot").and_then(safe_integer) {
            Some(slot) => {
                let gateway = state.read().await;
                stored_read(
                    &gateway.slots.get(&slot).cloned(),
                    gateway.store.as_ref(),
                    &format!("slot/{slot}"),
                    "Slot not found: the gateway holds about the last 20 minutes of slots. A transaction sent to the net is kept for days; look it up by its signature or id.",
                )
            }
            None => Err(("INVALID_INPUT", "slot must be a nonnegative safe integer.")),
        },
        "getTransaction" => {
            let signature = params.get("signature").and_then(Value::as_str).filter(|s| s.len() <= MAX_SIGNATURE_CHARS);
            let id = params.get("id").and_then(Value::as_str).filter(|id| valid_hash(id));
            if signature.is_none() && id.is_none() {
                Err((
                    "INVALID_INPUT",
                    "Give id, a lowercase 32-byte hexadecimal hash, or signature, a Solana signature of an executed transaction.",
                ))
            } else {
                let gateway = state.read().await;
                let held = match signature {
                    Some(signature) => gateway.signatures.get(signature).cloned(),
                    None => id.map(str::to_owned),
                };
                match held.and_then(|tx| stored_read(&gateway.transactions.get(&tx).cloned(), gateway.store.as_ref(), &format!("transaction/{tx}"), "").ok()) {
                    Some(record) => Ok(record),
                    None => kept_record(gateway.kept.as_deref(), signature, id),
                }
            }
        }
        "getOutsideUse" => match state.read().await.kept.clone() {
            Some(kept) => match kept.use_by_hour(params.get("hours").and_then(Value::as_u64).unwrap_or(24).clamp(1, 168)) {
                Ok(hours) => Ok(json!({"hours": hours.into_iter().map(|(hour, transactions, payers)| json!({
                    "hour": crate::protocol::utc(hour * 3_600_000),
                    "transactions": transactions,
                    "feePayers": payers,
                })).collect::<Vec<_>>()})),
                Err(_) => Err(("INTERNAL_ERROR", "The kept transactions could not be read.")),
            },
            None => Err(("NET_UNAVAILABLE", "This net keeps no transactions.")),
        },
        "getLatestBlockhash" => {
            if params.as_object().is_some_and(|fields| !fields.is_empty()) {
                Err(("INVALID_INPUT", "getLatestBlockhash takes no parameters."))
            } else {
                match command(&state, |reply| GatewayCommand::LatestBlockhash { reply }).await {
                    Ok(value) => Ok(value),
                    Err(_) => Err((
                        "NET_UNAVAILABLE",
                        "The node command channel is unavailable.",
                    )),
                }
            }
        }
        "getBalance" => {
            let address = params.get("address").and_then(Value::as_str).unwrap_or("");
            if !valid_address(address) {
                Err((
                    "INVALID_INPUT",
                    "address must be a 32-byte base58 public key.",
                ))
            } else {
                match command(&state, |reply| GatewayCommand::Balance {
                    address: address.to_owned(),
                    reply,
                })
                .await
                {
                    Ok(value) => Ok(value),
                    Err(_) => Err((
                        "NET_UNAVAILABLE",
                        "The node command channel is unavailable.",
                    )),
                }
            }
        }
        "submitTransaction" => {
            if !valid_submission(&params) {
                Err(("INVALID_INPUT", "Invalid plain transaction submission."))
            } else {
                match command(&state, |reply| GatewayCommand::SubmitPlain {
                    params,
                    reply,
                })
                .await
                {
                    Ok(value) => Ok(value),
                    Err(message) => {
                        let code = match message.as_str() {
                            "INVALID_INPUT" | "TRANSACTION_EXPIRED" | "IDEMPOTENCY_CONFLICT" => {
                                message.as_str()
                            }
                            _ => "CHAIN_REFUSED",
                        };
                        return rpc_error(id, -32000, code, &message).into_response();
                    }
                }
            }
        }
        "submitFragment" => {
            if !valid_fragment(&params) {
                Err(("INVALID_INPUT", "Invalid threshold fragment submission."))
            } else {
                match command(&state, |reply| GatewayCommand::SubmitFragment {
                    params,
                    reply,
                })
                .await
                {
                    Ok(value) => Ok(value),
                    Err(message) => {
                        let code = match message.as_str() {
                            "INVALID_INPUT" | "TRANSACTION_EXPIRED" | "IDEMPOTENCY_CONFLICT" => {
                                message.as_str()
                            }
                            _ => "CHAIN_REFUSED",
                        };
                        return rpc_error(id, -32000, code, &message).into_response();
                    }
                }
            }
        }
        // For followers: up to 64 consecutive slots in one call, each with its view and, once
        // agreed, its blocks and attestations (null for a skipped slot), so a follower far from
        // the gateway pays one round trip for many slots.
        "getSlotRange" => match (params.get("from").and_then(safe_integer), params.get("count").and_then(safe_integer)) {
            (Some(from), Some(count @ 1..=64)) => {
                let mut range = vec![];
                for slot in from..from + count {
                    let view = {
                        let gateway = state.read().await;
                        stored_read(&gateway.slots.get(&slot).cloned(), gateway.store.as_ref(), &format!("slot/{slot}"), "Slot not found.")
                    };
                    let Ok(view) = view else { break };
                    let blocks = command(&state, |reply| GatewayCommand::SlotBlocks { slot, reply }).await.ok();
                    range.push(json!({"slot": slot, "view": view, "blocks": blocks}));
                }
                Ok(json!(range))
            }
            _ => Err(("INVALID_INPUT", "from must be a slot and count 1 to 64.")),
        },
        "getFinalizedCblock" => Ok(finalized_head(&*state.read().await)),
        "getExecutionAgreement" => match params.get("slot").and_then(safe_integer) {
            Some(slot)=>command(&state,|reply|GatewayCommand::ExecutionAgreement {slot,reply}).await.map_err(|_|("NET_UNAVAILABLE","Execution agreement could not be read.")),
            None=>Err(("INVALID_INPUT","slot must be a nonnegative safe integer.")),
        },
        "getFollowerPage" => match (params.get("from").and_then(safe_integer), params.get("count").and_then(safe_integer)) {
            (Some(from), Some(count @ 1..=256)) => {
                let (finalized, views) = {
                    let gateway = state.read().await;
                    let views = (from..from + count).map(|slot| {
                        stored_read(&gateway.slots.get(&slot).cloned(), gateway.store.as_ref(), &format!("slot/{slot}"), "Slot not found.")
                            .ok().map(|view| follower_view(&view))
                    }).collect::<Vec<_>>();
                    (finalized_head(&gateway), views)
                };
                match command(&state, |reply| GatewayCommand::FollowerBlocks { from, count, reply }).await {
                    Ok(blocks) => Ok(json!({"finalized": finalized, "slots": views.into_iter().enumerate().map(|(i, view)| {
                        json!({"slot": from + i as u64, "view": view, "blocks": blocks[i]})
                    }).collect::<Vec<_>>()})),
                    Err(_) => Err(("NET_UNAVAILABLE", "The node cannot serve follower blocks.")),
                }
            }
            _ => Err(("INVALID_INPUT", "from must be a slot and count 1 to 256.")),
        },
        "getExecutionSnapshot" => command(&state, |reply| GatewayCommand::ExecutionSnapshot { reply }).await.map_err(|_| ("NET_UNAVAILABLE", "No finalized cblock to start from yet.")),
        "getSlotBlocks" => match params.get("slot").and_then(safe_integer) {
            Some(slot) => command(&state, |reply| GatewayCommand::SlotBlocks { slot, reply }).await.map_err(|_| ("NOT_FOUND", "This slot's blocks are no longer held (the last 2400 slots are).")),
            None => Err(("INVALID_INPUT", "slot must be a non-negative safe integer.")),
        },
        "getDemoPool" => command(&state, |reply| GatewayCommand::DemoPool { reply }).await.map_err(|_| ("PROGRAM_UNAVAILABLE", "The demo pool is not in this net's genesis.")),
        "getQuote" => command(&state, |reply| GatewayCommand::Quote { params: params.clone(), reply }).await.map_err(|_| ("INVALID_INPUT", "Quote needs inputMint (a pool mint), a positive amountIn and slippageBps up to 10000.")),
        "getLabReplay" => Err((
            "NOT_FOUND",
            "The live node does not serve fixture lab replays.",
        )),
        _ => return rpc_error(id, -32601, "NOT_FOUND", "Unknown RPC method.").into_response(),
    };
    match result {
        Ok(value) => Json(json!({"jsonrpc":"2.0","id":id,"result":value})).into_response(),
        Err((code, message)) => rpc_error(
            id,
            if code == "INVALID_INPUT" {
                -32602
            } else {
                -32000
            },
            code,
            message,
        )
        .into_response(),
    }
}

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
async fn fee_burn_page(state: &SharedGateway, params: &Value) -> std::result::Result<Value, (&'static str, &'static str)> {
    let cursor = match params.get("cursor") {
        None => None,
        Some(Value::String(cursor)) if !cursor.is_empty() && cursor.len() <= 16
            && cursor.bytes().all(|b|b.is_ascii_digit()) && (cursor == "0" || !cursor.starts_with('0')) => {
            Some(cursor.parse::<u64>().ok().filter(|n|*n <= MAX_SAFE_INTEGER).ok_or(("INVALID_INPUT","cursor must be an epoch's canonical decimal string."))?)
        },
        _ => return Err(("INVALID_INPUT","cursor must be an epoch's canonical decimal string.")),
    };
    let limit = match params.get("limit") {
        // The contract's PageQuery default, so the gateway and /v1/burn page alike.
        None => 24,
        Some(value) => safe_integer(value).filter(|limit|(1..=100).contains(limit)).ok_or(("INVALID_INPUT","limit must be an integer between 1 and 100."))?,
    };
    if params.as_object().unwrap().keys().any(|key|key != "cursor" && key != "limit") {
        return Err(("INVALID_INPUT","getFeeBurns takes only cursor and limit."));
    }
    let (burns,archive,head,program,epoch_slots) = {
        let gateway = state.read().await;
        (gateway.fee_burns.clone(),gateway.bridge_archive.clone(),gateway.finalized_cblock,
            gateway.bridge_program.clone(),gateway.bridge_epoch_slots.unwrap_or(432_000))
    };
    let Some(burns) = burns else {
        return Ok(json!({"program":program,"mint":null,"nativeFromSlot":null,"epochSlots":epoch_slots,
            "items":[],"nextCursor":null,"feeBurnedBaseUnits":"0"}));
    };
    tokio::task::spawn_blocking(move || -> Result<Value> {
        use base64::{engine::general_purpose::STANDARD,Engine};
        burns.page(head,cursor,limit,|epoch| {
            let end = (epoch+1).checked_mul(burns.config.epoch_slots).ok_or_else(||anyhow::anyhow!("Burn epoch overflow"))? / 4 - 1;
            let proof = archive.as_ref().map(|archive|archive.get(end)).transpose()?.flatten();
            let Some(proof) = proof else { return Ok(None) };
            let entries = proof["outbox"].as_array().ok_or_else(||anyhow::anyhow!("Archived proof is missing its outbox"))?;
            for entry in entries {
                let bytes = STANDARD.decode(entry.as_str().ok_or_else(||anyhow::anyhow!("Invalid archived outbox entry"))?)?;
                if let crate::bridge::OutboxEntry::FeeBurn { epoch:burn_epoch,amount } = bincode::deserialize(&bytes)? {
                    anyhow::ensure!(epoch == burn_epoch,"Archived fee burn epoch differs");
                    return Ok(Some(amount));
                }
            }
            Ok(Some(0))
        })
    }).await.map_err(|_|("INTERNAL_ERROR","The fee burn read failed."))?
        .map_err(|_|("INTERNAL_ERROR","The fee burn history could not be read."))
}

fn safe_integer(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value <= MAX_SAFE_INTEGER)
}
fn read_limit(
    params: &Value,
    default: usize,
) -> std::result::Result<usize, (&'static str, &'static str)> {
    match params.get("limit") {
        None => Ok(default),
        Some(value) => match safe_integer(value) {
            Some(limit @ 1..=64) => Ok(limit as usize),
            _ => Err((
                "INVALID_INPUT",
                "limit must be an integer between 1 and 64.",
            )),
        },
    }
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
/// A transaction the gateway's own records no longer hold, from the kept transactions.
fn kept_record(kept: Option<&crate::history::History>, signature: Option<&str>, id: Option<&str>) -> std::result::Result<Value, (&'static str, &'static str)> {
    let missing = ("NOT_FOUND", "Transaction not found.");
    let Some(kept) = kept else { return Err(missing) };
    let signature = match signature {
        Some(signature) => Some(signature.to_owned()),
        None => kept.signature_of(id.unwrap_or("")).map_err(|_| ("INTERNAL_ERROR", "The kept transactions could not be read."))?,
    };
    let Some(signature) = signature else { return Err(missing) };
    match kept.get(&signature) {
        Ok(Some(entry)) => Ok(entry.record().unwrap_or_else(|| {
            let e = &entry.executed;
            json!({"header": {"id": e.id}, "signature": signature, "state": "finalized", "executedSlot": e.slot,
                "result": if e.err.is_none() { "success" } else { "failed" }, "error": e.err, "logs": e.logs, "fee": e.fee.to_string()})
        })),
        Ok(None) => Err(missing),
        Err(_) => Err(("INTERNAL_ERROR", "The kept transactions could not be read.")),
    }
}
fn stored_read(
    memory: &Option<Value>,
    store: Option<&Store>,
    key: &str,
    missing: &'static str,
) -> std::result::Result<Value, (&'static str, &'static str)> {
    if let Some(value) = memory {
        return Ok(value.clone());
    }
    match store {
        Some(store) => match store.get(key) {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(("NOT_FOUND", missing)),
            Err(_) => Err(("INTERNAL_ERROR", "The stored record could not be read.")),
        },
        None => Err(("NOT_FOUND", missing)),
    }
}
/// Base58 decoding is quadratic in its input, so every length is bounded before a decode: an address
/// is at most 44 characters, a signature 88, a 1,232-byte transaction 1,684.
const MAX_ADDRESS_CHARS: usize = 44;
const MAX_SIGNATURE_CHARS: usize = 88;
const MAX_TRANSACTION_CHARS: usize = 1_684;
fn bounded_strings(value: &Value) -> bool {
    match value {
        Value::String(text) => text.len() <= MAX_TRANSACTION_CHARS,
        Value::Array(values) => values.iter().all(bounded_strings),
        Value::Object(map) => map.values().all(bounded_strings),
        _ => true,
    }
}
fn valid_address(address: &str) -> bool {
    address.len() <= MAX_ADDRESS_CHARS
        && bs58::decode(address)
        .into_vec()
        .map(|bytes| bytes.len() == 32)
        .unwrap_or(false)
}
fn valid_header(header: &Value) -> bool {
    if !header.is_object()
        || !header
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(valid_hash)
        || !header
            .get("feePayer")
            .and_then(Value::as_str)
            .is_some_and(valid_address)
    {
        return false;
    }
    if header
        .get("computeLimit")
        .and_then(safe_integer)
        .unwrap_or(0)
        == 0
        || header.get("targetSlot").and_then(safe_integer).is_none()
    {
        return false;
    }
    let Some(priority) = header.get("priorityFeePerCu").and_then(Value::as_str) else {
        return false;
    };
    if priority.is_empty()
        || (priority.len() > 1 && priority.starts_with('0'))
        || !priority.bytes().all(|byte| byte.is_ascii_digit())
        || priority.parse::<u64>().is_err()
    {
        return false;
    }
    header
        .get("writableAccounts")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .all(|value| value.as_str().is_some_and(valid_address))
        })
        .unwrap_or(false)
}
fn valid_submission(params: &Value) -> bool {
    if !params.get("header").is_some_and(valid_header) {
        return false;
    }
    let Some(receivers) = params.get("receivers").and_then(Value::as_array) else {
        return false;
    };
    if receivers.is_empty()
        || receivers.len() > 3
        || receivers.iter().any(|r| !matches!(r.as_u64(), Some(1..=3)))
    {
        return false;
    }
    for i in 0..receivers.len() {
        if receivers[..i].contains(&receivers[i]) {
            return false;
        }
    }
    params
        .get("transaction")
        .and_then(Value::as_str)
        .is_some_and(|tx| !tx.is_empty() && tx.len() <= 2048)
}
fn valid_fragment(params: &Value) -> bool {
    use base64::{engine::general_purpose::STANDARD, Engine};
    if !params.get("header").is_some_and(valid_header) {
        return false;
    }
    let Some(n @ 2..=3) = params.get("n").and_then(Value::as_u64) else {
        return false;
    };
    let Some(t @ 2..=3) = params.get("t").and_then(Value::as_u64) else {
        return false;
    };
    let Some(index) = params.get("index").and_then(Value::as_u64) else {
        return false;
    };
    if t > n || index >= n || !matches!(params.get("receiver").and_then(Value::as_u64), Some(1..=3))
    {
        return false;
    }
    for name in ["fragment", "keyShare"] {
        let Some(text) = params.get(name).and_then(Value::as_str) else {
            return false;
        };
        if text.is_empty() || text.len() > 16_384 || STANDARD.decode(text).is_err() {
            return false;
        }
    }
    params
        .get("headerSignature")
        .and_then(Value::as_str)
        .filter(|signature| signature.len() <= MAX_SIGNATURE_CHARS)
        .and_then(|signature| bs58::decode(signature).into_vec().ok())
        .is_some_and(|signature| signature.len() == 64)
}
async fn command(
    state: &SharedGateway,
    make: impl FnOnce(CommandReply) -> GatewayCommand,
) -> std::result::Result<Value, String> {
    let commands = state.read().await.commands.clone();
    let (tx, rx) = oneshot::channel();
    commands
        .send(make(tx))
        .await
        .map_err(|_| "Node stopped".to_owned())?;
    rx.await.map_err(|_| "Node stopped".to_owned())?
}
async fn credit(State(state): State<SharedGateway>, headers: HeaderMap, body: String) -> Response {
    if headers.contains_key(header::ORIGIN) {
        return (
            StatusCode::FORBIDDEN,
            Json(error(
                "ORIGIN_NOT_ALLOWED",
                "This endpoint is for the local API.",
            )),
        )
            .into_response();
    }
    let params: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(error("INVALID_INPUT", "Expected JSON.")),
            )
                .into_response()
        }
    };
    let address = params.get("address").and_then(Value::as_str).unwrap_or("");
    let amount = params
        .get("amountLamports")
        .and_then(Value::as_str)
        .and_then(|v| v.parse::<u64>().ok());
    if !valid_address(address) || amount.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(error("INVALID_INPUT", "Invalid faucet grant.")),
        )
            .into_response();
    }
    match command(&state, |reply| GatewayCommand::Credit {
        address: address.to_owned(),
        lamports: amount.unwrap(),
        reply,
    })
    .await
    {
        Ok(value) => Json(value).into_response(),
        Err(message) => faucet_failure(&message),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BridgeSaved {
    program: String,
    next_withdrawal_sequence: String,
}

/// Acknowledges only the relayer's durable save frontier, over the same trusted private port as
/// the faucet. It is intentionally forgotten on restart until the keeper reports its cursor again.
async fn bridge_saved(State(state): State<SharedGateway>, headers: HeaderMap, body: String) -> Response {
    if headers.contains_key(header::ORIGIN) {
        return (StatusCode::FORBIDDEN, Json(error("ORIGIN_NOT_ALLOWED", "This endpoint is for the local keeper."))).into_response();
    }
    let params = serde_json::from_str::<BridgeSaved>(&body).ok();
    let Some(params) = params.filter(|params| valid_address(&params.program)) else {
        return (StatusCode::BAD_REQUEST, Json(error("INVALID_INPUT", "Expected a bridge program and decimal nextWithdrawalSequence."))).into_response();
    };
    let sequence = &params.next_withdrawal_sequence;
    let cursor = (!sequence.is_empty() && sequence.len() <= 20 && sequence.bytes().all(|b| b.is_ascii_digit()))
        .then(|| sequence.parse::<u64>().ok()).flatten();
    let Some(cursor) = cursor else {
        return (StatusCode::BAD_REQUEST, Json(error("INVALID_INPUT", "nextWithdrawalSequence must be a decimal u64 string."))).into_response();
    };
    let mut state = state.write().await;
    if state.bridge_program.as_deref() != Some(params.program.as_str()) {
        return (StatusCode::CONFLICT, Json(error("BRIDGE_MISMATCH", "The program does not match this node's configured bridge."))).into_response();
    }
    let cursor = state.bridge_saved_cursor.map_or(cursor, |held| held.max(cursor));
    state.bridge_saved_cursor = Some(cursor);
    Json(json!({"program": params.program, "nextWithdrawalSequence": cursor.to_string()})).into_response()
}

fn faucet_failure(message: &str) -> Response {
    if let Some(cutover_slot) = message.strip_prefix("FAUCET_RETIRED: ").and_then(|slot| slot.parse::<u64>().ok()) {
        let mut body = error("FAUCET_RETIRED", "The faucet is retired. Bridge ACRUX from Solana to fund this wallet.");
        body["error"]["cutoverSlot"] = json!(cutover_slot);
        return (StatusCode::GONE, Json(body)).into_response();
    }
    match message.strip_prefix("FAUCET_EMPTY: ") {
        Some(message) => (StatusCode::SERVICE_UNAVAILABLE, Json(error("FAUCET_EMPTY", message))).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, Json(error("NET_UNAVAILABLE", message))).into_response(),
    }
}
/// The private port's read of what the faucet can still grant.
async fn faucet(State(state): State<SharedGateway>) -> Response {
    match command(&state, |reply| GatewayCommand::Faucet { reply }).await {
        Ok(value) => Json(value).into_response(),
        Err(message) => faucet_failure(&message),
    }
}

#[derive(Clone)]
enum Filter {
    All,
    Slots(u64, u64),
    Transaction(String),
}
fn parse_filter(value: &Value) -> Option<Filter> {
    match value.get("kind")?.as_str()? {
        "all" => Some(Filter::All),
        "slots" => {
            let from = safe_integer(value.get("from")?)?;
            let to = safe_integer(value.get("to")?)?;
            if from <= to {
                Some(Filter::Slots(from, to))
            } else {
                None
            }
        }
        "transaction" => {
            let id = value.get("id")?.as_str()?;
            if !valid_hash(id) {
                None
            } else {
                Some(Filter::Transaction(id.to_owned()))
            }
        }
        _ => None,
    }
}
fn matches(event: &Value, filter: &Filter) -> bool {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    if kind.starts_with("stream.") {
        return true;
    }
    match filter {
        Filter::All => true,
        Filter::Slots(from, to) => event
            .get("slot")
            .and_then(Value::as_u64)
            .map(|slot| slot >= *from && slot <= *to)
            .unwrap_or(false),
        Filter::Transaction(id) => {
            if kind == "transaction.updated" {
                return event.pointer("/data/header/id").and_then(Value::as_str)
                    == Some(id.as_str());
            }
            if kind == "block.decrypted" {
                return event
                    .pointer("/data/transactionIds")
                    .and_then(Value::as_array)
                    .map(|ids| ids.iter().any(|tx| tx.as_str() == Some(id.as_str())))
                    .unwrap_or(false);
            }
            let txs = if kind == "slot.merged" {
                event.pointer("/data/transactions")
            } else {
                event.pointer("/data/mergedOrder")
            };
            txs.and_then(Value::as_array)
                .map(|txs| {
                    txs.iter()
                        .any(|tx| tx.get("id").and_then(Value::as_str) == Some(id.as_str()))
                })
                .unwrap_or(false)
        }
    }
}
/// The JSON-RPC port's own socket, where web3.js and wallets look for it (`wss://rpc.<domain>`):
/// Solana's PubSub alone, with no Acrux snapshot ahead of its replies.
async fn upgrade_solana(
    state: State<SharedGateway>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    open_stream(state, peer, headers, upgrade, false).await
}
async fn upgrade(
    state: State<SharedGateway>,
    peer: ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    open_stream(state, peer, headers, upgrade, true).await
}
async fn open_stream(
    State(state): State<SharedGateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
    acrux: bool,
) -> Response {
    if !valid_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(error("ORIGIN_NOT_ALLOWED", "This origin is not allowed.")),
        )
            .into_response();
    }
    let limits = crate::limits::limits();
    let Some(permit) = limits.open_socket(limits.client(peer, &headers)) else {
        // A browser never sees a refused handshake's status, so the stream opens and closes at
        // once with 1013 (try again later) and the reason RATE_LIMITED.
        return upgrade.on_upgrade(|mut socket| async move {
            let close = axum::extract::ws::CloseFrame { code: 1013, reason: "RATE_LIMITED".into() };
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), socket.send(WsMessage::Close(Some(close)))).await;
        });
    };
    upgrade
        .max_message_size(8192)
        .on_upgrade(move |socket| async move {
            stream(socket, state, acrux).await;
            drop(permit);
        })
}
async fn send_text(socket: &mut WebSocket, text: String) -> bool {
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), socket.send(WsMessage::Text(text.into()))).await,
        Ok(Ok(()))
    )
}
async fn send(socket: &mut WebSocket, event: Value) -> bool {
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.send(WsMessage::Text(event.to_string().into()))
        )
        .await,
        Ok(Ok(()))
    )
}
async fn control(state: &SharedGateway, kind: &str, data: Value) -> Value {
    state.read().await.control(kind, data)
}
/// A Solana subscription on a stream connection, as Solana's PubSub defines them.
enum SolanaSub {
    /// One-shot: notified once the transaction reaches the commitment, then dropped.
    Signature { signature: String, finalized: bool },
    /// Notified after each executed slot in which the account changed.
    Account { address: String, encoding: String, last: Value },
    Slot,
}
/// Subscriptions one connection may hold.
const MAX_SOLANA_SUBS: usize = 64;

fn solana_reply(id: &Value, result: std::result::Result<Value, (i64, &str)>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    }
}
fn notification(method: &str, subscription: u64, slot: u64, value: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": {"result": {"context": {"slot": slot}, "value": value}, "subscription": subscription}})
}
async fn solana_account(state: &SharedGateway, address: &str, encoding: &str) -> Value {
    command(state, |reply| GatewayCommand::Solana { method: "getAccountInfo".into(), params: json!([address, {"encoding": encoding}]), client: None, reply })
        .await
        .map(|answer| answer["value"].clone())
        .unwrap_or(Value::Null)
}
/// The signature's notification if its transaction has reached the commitment: read from `update`
/// when it is that transaction's newest record, else from the gateway's.
async fn signature_reached(state: &SharedGateway, signature: &str, finalized: bool, update: Option<&Value>) -> Option<(u64, Value)> {
    let gateway = state.read().await;
    let id = gateway.signatures.get(signature)?;
    let tx = match update.filter(|tx| tx["header"]["id"].as_str() == Some(id.as_str())) {
        Some(tx) => tx,
        None => gateway.transactions.get(id)?,
    };
    let reached = match tx["state"].as_str()? {
        "finalized" => true,
        // A failed transaction is final once its cblock is: it then carries a finalization.
        "failed" => !finalized || !tx["finalization"].is_null(),
        "executed" => !finalized,
        _ => false,
    };
    reached.then(|| (
        tx["executedSlot"].as_u64().unwrap_or(0),
        json!({"err": if tx["result"] == "success" { Value::Null } else { crate::solana::FAILED.clone() }}),
    ))
}
/// One Solana PubSub request: the reply, and a notification to send at once when one is due.
async fn solana_request(state: &SharedGateway, subs: &mut BTreeMap<u64, SolanaSub>, next: &mut u64, request: &Value) -> Vec<Value> {
    // A notification (no id), such as web3.js's heartbeat `ping`, gets no answer, as JSON-RPC says.
    let Some(id) = request.get("id").cloned() else { return vec![] };
    let params = request.get("params").cloned().unwrap_or(json!([]));
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    // `confirmed` waits for finality: an executed transaction can still roll back with a skipped cblock.
    let finalized = |config: &Value| config["commitment"].as_str() != Some("processed");
    if method.ends_with("Unsubscribe") {
        let removed = params.get(0).and_then(Value::as_u64).is_some_and(|sub| subs.remove(&sub).is_some());
        return vec![solana_reply(&id, Ok(json!(removed)))];
    }
    if subs.len() >= MAX_SOLANA_SUBS {
        return vec![solana_reply(&id, Err((-32005, "Too many subscriptions on this connection.")))];
    }
    let sub = match method {
        "signatureSubscribe" => match params.get(0).and_then(Value::as_str).filter(|s| s.len() <= MAX_SIGNATURE_CHARS) {
            Some(signature) => SolanaSub::Signature { signature: signature.to_owned(), finalized: finalized(&params[1]) },
            None => return vec![solana_reply(&id, Err((-32602, "Invalid params: a signature")))],
        },
        "accountSubscribe" => match params.get(0).and_then(Value::as_str).filter(|a| valid_address(a)) {
            Some(address) => {
                let encoding = params[1]["encoding"].as_str().unwrap_or("base64").to_owned();
                let last = solana_account(state, address, &encoding).await;
                SolanaSub::Account { address: address.to_owned(), encoding, last }
            }
            None => return vec![solana_reply(&id, Err((-32602, "Invalid params: an address")))],
        },
        "slotSubscribe" => SolanaSub::Slot,
        _ => return vec![solana_reply(&id, Err((-32601, "Method not found")))],
    };
    *next += 1;
    let subscription = *next;
    let mut out = vec![solana_reply(&id, Ok(json!(subscription)))];
    if let SolanaSub::Signature { signature, finalized } = &sub {
        if let Some((slot, value)) = signature_reached(state, signature, *finalized, None).await {
            out.push(notification("signatureNotification", subscription, slot, value));
            return out;
        }
    }
    subs.insert(subscription, sub);
    out
}
/// The notifications an event brings to a connection's Solana subscriptions.
async fn solana_notifications(state: &SharedGateway, subs: &mut BTreeMap<u64, SolanaSub>, event: &Value, last_slot: &mut u64, last_executed: &mut u64) -> Vec<Value> {
    let kind = event["type"].as_str().unwrap_or("");
    let slot = event["slot"].as_u64().unwrap_or(0);
    let mut out = vec![];
    match kind {
        "transaction.updated" => {
            let mut done = vec![];
            for (subscription, sub) in subs.iter() {
                if let SolanaSub::Signature { signature, finalized } = sub {
                    if let Some((slot, value)) = signature_reached(state, signature, *finalized, Some(&event["data"])).await {
                        out.push(notification("signatureNotification", *subscription, slot, value));
                        done.push(*subscription);
                    }
                }
            }
            for subscription in done {
                subs.remove(&subscription);
            }
        }
        // Every validator reports each slot; a connection hears of it once.
        "slot.started" if slot > *last_slot => {
            *last_slot = slot;
            for (subscription, sub) in subs.iter() {
                if matches!(sub, SolanaSub::Slot) {
                    out.push(json!({"jsonrpc": "2.0", "method": "slotNotification", "params": {"result": {"parent": slot.saturating_sub(1), "root": slot.saturating_sub(16), "slot": slot}, "subscription": subscription}}));
                }
            }
        }
        "slot.executed" if slot > *last_executed => {
            *last_executed = slot;
            for (subscription, sub) in subs.iter_mut() {
                if let SolanaSub::Account { address, encoding, last } = sub {
                    let now = solana_account(state, address, encoding).await;
                    if now != *last {
                        *last = now.clone();
                        out.push(notification("accountNotification", *subscription, slot, now));
                    }
                }
            }
        }
        _ => {}
    }
    out
}
async fn stream(mut socket: WebSocket, state: SharedGateway, acrux: bool) {
    let mut events = state.read().await.events.subscribe();
    if acrux {
        let initial = {
            let s = state.read().await;
            s.control("stream.snapshot", s.snapshot())
        };
        if !send(&mut socket, initial).await {
            return;
        }
    }
    let mut filter = Filter::All;
    let mut active = false;
    let mut last_sequence = 0;
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    let mut pong = true;
    // Solana PubSub on the same connection: a message carrying `jsonrpc` is Solana's.
    let mut solana = BTreeMap::<u64, SolanaSub>::new();
    let mut next_subscription = 0u64;
    let mut solana_slot = 0u64;
    let mut last_resync = std::time::Instant::now();
    let mut solana_executed = 0u64;
    loop {
        tokio::select! {
            incoming=socket.next()=>match incoming{
                Some(Ok(WsMessage::Text(text)))=>{
                    let parsed:Value=match serde_json::from_str(&text){Ok(v)=>v,Err(_)=>{let event=control(&state,"stream.error",error("INVALID_INPUT","Expected a stream command.")).await;if !send(&mut socket,event).await{break;}continue;}};
                    if parsed.get("jsonrpc").is_some() {
                        let mut closed = false;
                        for message in solana_request(&state, &mut solana, &mut next_subscription, &parsed).await {
                            if !send(&mut socket, message).await { closed = true; break; }
                        }
                        if closed { break; }
                        continue;
                    }
                    if !acrux { continue; }
                    if parsed.get("version").and_then(Value::as_u64)!=Some(1){let event=control(&state,"stream.error",error("INVALID_INPUT","Unknown stream version.")).await;if !send(&mut socket,event).await{break;}continue;}
                    if parsed.get("type").and_then(Value::as_str)==Some("unsubscribe"){active=false;continue;}
                    if parsed.get("type").and_then(Value::as_str)!=Some("subscribe"){continue;}
                    let Some(next)=parsed.get("filter").and_then(parse_filter)else{let event=control(&state,"stream.error",error("INVALID_INPUT","Invalid subscription filter.")).await;if !send(&mut socket,event).await{break;}continue;};filter=next;active=true;
                    if parsed.get("afterSequence").is_some_and(|value|safe_integer(value).is_none()) || parsed.get("streamId").is_some_and(|value|!value.is_string()) {
                        let event=control(&state,"stream.error",error("INVALID_INPUT","Invalid subscription cursor or stream id.")).await;
                        if !send(&mut socket,event).await{break;}continue;
                    }
                    let (snapshot,replay)={let s=state.read().await;let cursor=parsed.get("afterSequence").and_then(safe_integer);let same=parsed.get("streamId").and_then(Value::as_str).map(|id|id==s.stream_id).unwrap_or(true);
                        if let Some(cursor)=cursor.filter(|cursor|same&&*cursor<=s.sequence&&*cursor>=s.history.front().and_then(|e|e.get("sequence")).and_then(Value::as_u64).unwrap_or(s.sequence)){(None,s.history.iter().filter(|event|event.get("sequence").and_then(Value::as_u64).unwrap_or(0)>cursor&&matches(event,&filter)).cloned().collect::<Vec<_>>())}else{(Some(s.control("stream.resync",json!({"reason":if same{"filter_changed"}else{"restarted"},"snapshot":s.snapshot()}))),vec![])}};
                    if let Some(snapshot)=snapshot{last_sequence=snapshot["sequence"].as_u64().unwrap_or(0);if !send(&mut socket,snapshot).await{break;}}for event in replay{last_sequence=event["sequence"].as_u64().unwrap_or(last_sequence);if !send(&mut socket,event).await{return;}}
                },
                Some(Ok(WsMessage::Pong(_)))=>pong=true,
                // Answer a client's close so its closing handshake completes: a dropped socket reads
                // as an abnormal close, and web3.js then reconnects every second, for ever.
                Some(Ok(WsMessage::Close(_)))=>{let _=futures_util::SinkExt::close(&mut socket).await;break;},
                None|Some(Err(_))=>break,
                _=>{},
            },
            event=events.recv()=>{match event{Ok(shared)=>{
                let event=&shared.0;
                if !solana.is_empty() {
                    let mut closed = false;
                    for message in solana_notifications(&state, &mut solana, event, &mut solana_slot, &mut solana_executed).await {
                        if !send(&mut socket, message).await { closed = true; break; }
                    }
                    if closed { break; }
                }
                let sequence=event["sequence"].as_u64().unwrap_or(0);if active&&sequence>last_sequence&&matches(event,&filter){last_sequence=sequence;if !send_text(&mut socket,shared.1.clone()).await{break;}}},Err(broadcast::error::RecvError::Lagged(_)) if !active||last_resync.elapsed()<std::time::Duration::from_secs(10)=>{},Err(broadcast::error::RecvError::Lagged(_))=>{last_resync=std::time::Instant::now();let event={let s=state.read().await;s.control("stream.resync",json!({"reason":"cursor_expired","snapshot":s.snapshot()}))};last_sequence=event["sequence"].as_u64().unwrap_or(last_sequence);if !send(&mut socket,event).await{break;}},Err(_)=>break}},
            _=heartbeat.tick()=>{if !pong{break;}pong=false;if !matches!(tokio::time::timeout(std::time::Duration::from_secs(5),socket.send(WsMessage::Ping(vec![].into()))).await,Ok(Ok(()))){break;}if !acrux{continue;}let event={let s=state.read().await;let latest=s.slots.keys().next_back().copied().unwrap_or(0);s.control("stream.heartbeat",json!({"latestSlot":latest,"status":"live"}))};if !send(&mut socket,event).await{break;}},
        }
    }
}
fn finalized_head(state: &GatewayState) -> Value {
    json!({"consensusSlot": state.finalized_cblock})
}

/// The follower checks execution, proposer signatures and finality summaries. Explorer timings,
/// votes, transaction schedules and the duplicate attestation list do not enter that check.
fn follower_view(view: &Value) -> Value {
    let proposers = view["window"]["proposers"].as_array().into_iter().flatten()
        .map(|p| json!({"index": p["index"], "identity": p["identity"]})).collect::<Vec<_>>();
    let certificates = view["cblock"]["certificates"].as_array().into_iter().flatten()
        .map(|c| json!({"kind": c["kind"], "stakeShare": c["stakeShare"], "threshold": c["threshold"]})).collect::<Vec<_>>();
    json!({"slot": view["slot"], "stateHash": view["stateHash"], "agreedSet": view["agreedSet"],
        "window": {"proposers": proposers}, "cblock": {"consensusSlot": view["cblock"]["consensusSlot"],
            "parent": view["cblock"]["parent"], "certificates": certificates}})
}

async fn metrics(State(state): State<SharedGateway>) -> impl IntoResponse {
    let state = state.read().await;
    let mut output = String::new();
    for (name, key) in [
        ("acrux_slots_total", "slots"),
        ("acrux_finalized_cblocks_total", "finalizedCblocks"),
        ("acrux_skips_total", "skips"),
        ("acrux_commit_spread_microseconds", "averageCommitSpreadUs"),
    ] {
        output.push_str(&format!(
            "{name} {}\n",
            state
                .counters
                .get(key)
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        ));
    }
    output.push_str(&format!("acrux_follower_retained_slots {}\nacrux_follower_retained_block_bytes {}\nacrux_follower_earliest_slot {}\n",
        state.follower_retained_slots, state.follower_retained_block_bytes, state.follower_earliest_slot.unwrap_or(0)));
    // Outside use this hour: transactions people sent (not the built-in traffic or the faucet's
    // credits) and their distinct fee payers.
    if let Some((_, sent, payers)) = state.kept.as_ref().and_then(|kept| kept.use_by_hour(1).ok()).and_then(|h| h.first().copied()) {
        output.push_str(&format!("acrux_outside_transactions_hour {sent}\nacrux_outside_fee_payers_hour {payers}\n"));
    }
    let exchange=state.exchange_metrics.as_ref().and_then(std::sync::Weak::upgrade);
    drop(state);
    if let Some(exchange)=exchange {output.push_str(&crate::exchange_metrics::prometheus(&exchange.metrics_json()));}
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        output,
    )
}

async fn hash_exchange(State(state):State<SharedGateway>)->std::result::Result<Json<Value>,StatusCode> {
    let exchange=state.read().await.exchange_metrics.as_ref().and_then(std::sync::Weak::upgrade).ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(exchange.metrics_json()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bridge_review_archived_proofs_are_served_from_redb_after_index_only_reload() {
        use crate::bridge::ProofArchive;
        let dir = std::path::Path::new("target").join(format!("bridge-review-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bridge-proofs.redb");
        let previous = std::env::var_os("ACRUX_BRIDGE_PROOF_MAX");
        std::env::set_var("ACRUX_BRIDGE_PROOF_MAX", "1");
        let outbox = |sequence| vec![base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bincode::serialize(&crate::bridge::OutboxEntry::Withdrawal {
            sequence, solana_mint: [1;32], recipient: [2;32], amount: 10,
        }).unwrap())];
        let proof = json!({"consensusSlot":7,"epochSlots":432000,"hash":"saved-proof","outbox":outbox(0)});
        let newer = json!({"consensusSlot":108000,"epochSlots":432000,"hash":"new-epoch-proof","outbox":outbox(1)});
        let rolling = json!({"consensusSlot":216000,"epochSlots":432000,"hash":"rolling-proof","outbox":outbox(2)});
        {
            let mut archive = ProofArchive::open(Some(&path),432000,Some(7)).unwrap();
            assert!(archive.keep(7,0,true,&proof).unwrap());
            assert!(archive.keep(108000,1,true,&newer).unwrap(), "the shedding level must not discard a new representative or withdrawal");
        }
        let archive = ProofArchive::open(Some(&path),432000,Some(108000)).unwrap();
        if let Some(value) = previous { std::env::set_var("ACRUX_BRIDGE_PROOF_MAX",value); } else { std::env::remove_var("ACRUX_BRIDGE_PROOF_MAX"); }
        let (commands, _) = mpsc::channel(1);
        let mut state = GatewayState::new(commands);
        state.bridge_archive = Some(archive.store.clone());
        state.archived_withdrawals = archive.withdrawals.clone();
        state.epoch_cblock_proofs = archive.epochs.clone();
        let state = Arc::new(RwLock::new(state));
        assert!(state.read().await.cblock_proofs.is_empty(), "reload restores only archive indexes and a DB reader");
        for (params, expected) in [(json!({"consensusSlot":7}), proof), (json!({"consensusSlot":108000}), newer.clone()), (json!({"epoch":1}), newer.clone()), (json!({"latest":true}), newer)] {
            let response = rpc_one(state.clone(), "127.0.0.1".parse().unwrap(), json!({"jsonrpc":"2.0","id":1,"method":"getCblockProof","params":params})).await;
            let bytes = axum::body::to_bytes(response.into_body(), 10000).await.unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["result"], expected);
        }
        // The rolling publication still serves proofs without an archived epoch representative.
        state.write().await.keep_cblock_proof(216000,rolling.clone());
        for params in [json!({"epoch":2}),json!({"latest":true})] {
            let response = rpc_one(state.clone(), "127.0.0.1".parse().unwrap(), json!({"jsonrpc":"2.0","id":1,"method":"getCblockProof","params":params})).await;
            let bytes = axum::body::to_bytes(response.into_body(),10000).await.unwrap();
            let body:Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["result"],rolling);
        }
        assert_eq!(state.read().await.cblock_proofs.len(),1, "archive reads never populate a proof payload cache");
        drop(state);
        drop(archive);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn bridge_review_saved_cursor_is_program_checked_monotonic_and_volatile() {
        let (commands,_) = mpsc::channel(1);
        let program = solana_sdk::pubkey::Pubkey::new_unique().to_string();
        let mut gateway = GatewayState::new(commands);
        gateway.bridge_program = Some(program.clone());
        let state = Arc::new(RwLock::new(gateway));
        assert_eq!(state.read().await.bridge_saved_cursor,None);
        for cursor in ["10","7","18446744073709551615"] {
            let response = bridge_saved(State(state.clone()),HeaderMap::new(),json!({"program":program,"nextWithdrawalSequence":cursor}).to_string()).await;
            assert_eq!(response.status(),StatusCode::OK);
        }
        assert_eq!(state.read().await.bridge_saved_cursor,Some(u64::MAX));
        for cursor in ["-1","+2"," 2","18446744073709551616",""] {
            let response = bridge_saved(State(state.clone()),HeaderMap::new(),json!({"program":program,"nextWithdrawalSequence":cursor}).to_string()).await;
            assert_eq!(response.status(),StatusCode::BAD_REQUEST);
        }
        let response = bridge_saved(State(state.clone()),HeaderMap::new(),json!({"program":solana_sdk::pubkey::Pubkey::new_unique().to_string(),"nextWithdrawalSequence":"1"}).to_string()).await;
        assert_eq!(response.status(),StatusCode::CONFLICT);
        assert_eq!(state.read().await.bridge_saved_cursor,Some(u64::MAX));
        let (commands,_) = mpsc::channel(1);
        assert_eq!(GatewayState::new(commands).bridge_saved_cursor,None, "an acknowledgement is never restored from disk");
    }

    #[test]
    fn oversized_base58_is_refused_before_decoding() {
        let started = std::time::Instant::now();
        assert!(!valid_address(&"2".repeat(1024 * 1024)));
        assert!(!bounded_strings(&json!(["2".repeat(40_000)])));
        assert!(started.elapsed() < std::time::Duration::from_millis(50), "{:?}", started.elapsed());
        assert!(valid_address("11111111111111111111111111111111"));
        assert!(bounded_strings(&json!(["x".repeat(MAX_TRANSACTION_CHARS), {"encoding": "base64"}])));
    }
}
