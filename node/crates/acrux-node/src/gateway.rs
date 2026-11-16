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
    Quote {
        params: Value,
        reply: CommandReply,
    },
}

pub struct GatewayState {
    pub slots: BTreeMap<u64, Value>,
    /// Same run store as Network; serves records evicted from the recent-memory cache.
    pub store: Option<Store>,
    pub transactions: BTreeMap<String, Value>,
    pub transactions_by_cblock: BTreeMap<u64, Vec<String>>,
    pub current_window: Value,
    pub upcoming_windows: Vec<Value>,
    pub counters: Value,
    pub facts: Value,
    pub parameters: Value,
    pub balances: BTreeMap<String, u64>,
    pub stream_id: String,
    pub sequence: u64,
    pub history: VecDeque<Value>,
    pub events: broadcast::Sender<Value>,
    pub commands: mpsc::Sender<GatewayCommand>,
}

impl GatewayState {
    pub fn new(commands: mpsc::Sender<GatewayCommand>) -> Self {
        let (events, _) = broadcast::channel(8192);
        Self {
            slots: BTreeMap::new(),
            store: None,
            transactions: BTreeMap::new(),
            transactions_by_cblock: BTreeMap::new(),
            current_window: Value::Null,
            upcoming_windows: vec![],
            facts: Value::Null,
            counters: json!({"slots":0,"finalizedCblocks":0,"skips":0,"averageCommitSpreadUs":0}),
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
        let _ = self.events.send(event);
    }
    fn control(&self, kind: &str, data: Value) -> Value {
        json!({"version":1,"sequence":self.sequence,"streamId":self.stream_id,"slot":null,"anchor":"stream","atUs":0,"type":kind,"data":data})
    }
}

/// Browser origins allowed to call the gateway: `ACRUX_CORS_ORIGINS` (comma separated), by default
/// the site's local ports.
fn origins() -> Vec<HeaderValue> {
    std::env::var("ACRUX_CORS_ORIGINS")
        .unwrap_or_else(|_| "http://127.0.0.1:4910,http://localhost:4910,http://127.0.0.1:4911,http://localhost:4911".into())
        .split(',')
        .map(|s| HeaderValue::from_str(s.trim()).expect("ACRUX_CORS_ORIGINS holds origins"))
        .collect()
}
fn valid_origin(headers: &HeaderMap) -> bool {
    headers
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
        .allow_origin(origins())
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(AllowHeaders::mirror_request());
    let http = Router::new()
        .route("/", post(rpc))
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
        .route("/health", get(health))
        .route("/internal/faucet", post(credit))
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

async fn health(State(state): State<SharedGateway>) -> Json<Value> {
    let state = state.read().await;
    Json(
        json!({"status":if state.current_window.is_null(){"waiting"}else{"ready"},"contractVersion":"0.1.8","missingConfiguration":[]}),
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
    let params = request.get("params").cloned().unwrap_or(json!({}));
    // Solana's methods take array params; Acrux's take an object. Solana-only methods may omit them.
    let solana_only = matches!(method, "getHealth" | "getVersion" | "getBlockHeight" | "getEpochInfo" | "getAccountInfo" | "getMultipleAccounts" | "getProgramAccounts" | "getMinimumBalanceForRentExemption" | "getSignaturesForAddress" | "getVoteAccounts" | "sendTransaction" | "getSignatureStatuses");
    if params.is_array() || (solana_only && !params.is_object()) || (solana_only && params.as_object().is_some_and(|o| o.is_empty())) {
        let params = if params.is_array() { params } else { json!([]) };
        return match command(&state, |reply| GatewayCommand::Solana { method: method.to_owned(), params, reply }).await {
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
        "getCurrentWindow" => Ok(state.read().await.current_window.clone()),
        "getCounters" => Ok(state.read().await.counters.clone()),
        "getFacts" => match state.read().await.facts.clone() {
            Value::Null => Err(("NET_UNAVAILABLE", "The net has not measured enough slots yet.")),
            facts => Ok(facts),
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
                    "Slot not found.",
                )
            }
            None => Err(("INVALID_INPUT", "slot must be a nonnegative safe integer.")),
        },
        "getTransaction" => match params
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| valid_hash(id))
        {
            Some(tx) => {
                let gateway = state.read().await;
                stored_read(
                    &gateway.transactions.get(tx).cloned(),
                    gateway.store.as_ref(),
                    &format!("transaction/{tx}"),
                    "Transaction not found.",
                )
            }
            None => Err((
                "INVALID_INPUT",
                "id must be a lowercase 32-byte hexadecimal hash.",
            )),
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
        "getExecutionSnapshot" => command(&state, |reply| GatewayCommand::ExecutionSnapshot { reply }).await.map_err(|_| ("NET_UNAVAILABLE", "No finalized cblock to start from yet.")),
        "getSlotBlocks" => match params.get("slot").and_then(safe_integer) {
            Some(slot) => command(&state, |reply| GatewayCommand::SlotBlocks { slot, reply }).await.map_err(|_| ("NOT_FOUND", "This slot's blocks are no longer held (the last 64 slots are).")),
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
fn valid_address(address: &str) -> bool {
    bs58::decode(address)
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
        Err(message) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error("NET_UNAVAILABLE", &message)),
        )
            .into_response(),
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
async fn upgrade(
    State(state): State<SharedGateway>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let limits = crate::limits::limits();
    let Some(permit) = limits.open_socket(limits.client(peer, &headers)) else {
        return (StatusCode::TOO_MANY_REQUESTS, Json(error("RATE_LIMITED", "Too many event-stream connections from this address."))).into_response();
    };
    if !valid_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(error("ORIGIN_NOT_ALLOWED", "This origin is not allowed.")),
        )
            .into_response();
    }
    upgrade
        .max_message_size(8192)
        .on_upgrade(move |socket| async move {
            stream(socket, state).await;
            drop(permit);
        })
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
async fn stream(mut socket: WebSocket, state: SharedGateway) {
    let mut events = state.read().await.events.subscribe();
    let initial = {
        let s = state.read().await;
        s.control("stream.snapshot", s.snapshot())
    };
    if !send(&mut socket, initial).await {
        return;
    }
    let mut filter = Filter::All;
    let mut active = false;
    let mut last_sequence = 0;
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    let mut pong = true;
    loop {
        tokio::select! {
            incoming=socket.next()=>match incoming{
                Some(Ok(WsMessage::Text(text)))=>{
                    let parsed:Value=match serde_json::from_str(&text){Ok(v)=>v,Err(_)=>{let event=control(&state,"stream.error",error("INVALID_INPUT","Expected a stream command.")).await;if !send(&mut socket,event).await{break;}continue;}};
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
                Some(Ok(WsMessage::Close(_)))|None|Some(Err(_))=>break,
                _=>{},
            },
            event=events.recv()=>{match event{Ok(event)=>{let sequence=event["sequence"].as_u64().unwrap_or(0);if active&&sequence>last_sequence&&matches(&event,&filter){last_sequence=sequence;if !send(&mut socket,event).await{break;}}},Err(broadcast::error::RecvError::Lagged(_))=>{let event={let s=state.read().await;s.control("stream.resync",json!({"reason":"cursor_expired","snapshot":s.snapshot()}))};last_sequence=event["sequence"].as_u64().unwrap_or(last_sequence);if !send(&mut socket,event).await{break;}},Err(_)=>break}},
            _=heartbeat.tick()=>{if !pong{break;}pong=false;if !matches!(tokio::time::timeout(std::time::Duration::from_secs(5),socket.send(WsMessage::Ping(vec![].into()))).await,Ok(Ok(()))){break;}let event={let s=state.read().await;let latest=s.slots.keys().next_back().copied().unwrap_or(0);s.control("stream.heartbeat",json!({"latestSlot":latest,"status":"live"}))};if !send(&mut socket,event).await{break;}},
        }
    }
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
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        output,
    )
}
