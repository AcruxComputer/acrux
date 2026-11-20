//! Authenticated mirrors of a hub's existing QUIC envelopes.
//! Admitted members submit signed votes and targeted proposer messages through bounded ingress.
use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use quinn::{ClientConfig, Connection, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig};
use rand::RngCore;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{atomic::Ordering, Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::{sync::{Notify, Semaphore}, task::JoinHandle, time::{timeout, timeout_at}};

const AUTH_DEADLINE: Duration = Duration::from_secs(5);
// A far observer catching up from its snapshot can leave its window full for seconds; the held
// history, not this deadline, bounds how far behind it may fall.
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
const CONTROL_LIMIT: usize = 128;
const MAX_PACKET_BYTES: usize = 1024 * 1024;
const MAX_MEMBER_ENVELOPE_BYTES: usize = 2048;
const MAX_ROUTED_RECIPIENTS: usize = 32;
const MAX_ROUTED_FRAME_BYTES: usize = MAX_PACKET_BYTES + 2 + 4 * MAX_ROUTED_RECIPIENTS;
// Rotor sends 64 shreds for 32 data shreds, in groups to the twelve relays, for 20 slots/s.
const MAX_ROUTED_FRAMES_PER_SECOND: usize = 8192;
const MAX_ROUTED_BYTES_PER_SECOND: usize = 64 * 1024 * 1024;
const HISTORY_BYTES: usize = 64 * 1024 * 1024;
// Bytes bound the history; the packet count only caps the map's overhead. A live net's three hubs
// fill 8,192 packets in seconds, less than a far observer's snapshot download.
const HISTORY_PACKETS: usize = 262_144;
const DATA_MAGIC: &[u8] = b"acrux-observer-packets-v1";
const CONTROL_MAGIC: &[u8] = b"acrux-observer-control-v1";
const AUTH_DOMAIN: &[u8] = b"acrux-external-observer-v1\0";
pub const MAX_IDENTITIES: usize = 32;

#[derive(Clone, Debug)]
pub struct Packet {
    pub from: usize,
    pub to: usize,
    pub slot: u64,
    pub payload: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct ExternalIngress {
    pub identity: [u8; 32],
    pub hub: String,
    pub envelope: Vec<u8>,
    pub recipients: Option<Vec<usize>>,
    pub received_at: Instant,
    connection: Connection,
}
impl ExternalIngress {
    /// Refuse the connection that submitted this frame, even if its identity has reconnected.
    pub fn reject(&self, reason: &str) {
        refused(reason, &"member input rejected");
        self.connection.close(3_u32.into(), reason.as_bytes());
    }
}
#[derive(Clone, Debug)]
pub struct Authentication {
    pub hub: String,
    pub identity: [u8; 32],
    pub from_slot: u64,
    pub signature: [u8; 64],
}
#[derive(Clone, Debug)]
pub enum ClientFrame {
    Authenticate(Authentication),
    Ping { peer: usize, nonce: u64 },
    Envelope(Vec<u8>),
    Routed { recipients: Vec<usize>, envelope: Vec<u8> },
}
#[derive(Clone, Debug)]
pub enum ServerFrame {
    Challenge([u8; 32]),
    Accepted,
    Pong { peer: usize, nonce: u64 },
}

fn valid_hub(hub: &str) -> bool {
    !hub.is_empty() && hub.len() <= 16 && hub.bytes().all(|byte| byte.is_ascii_uppercase())
}
/// The requested history frontier is signed as well as the hub and connection's fresh nonce.
pub fn authentication_message(hub: &str, identity: &[u8; 32], challenge: &[u8; 32], from_slot: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(AUTH_DOMAIN.len() + 1 + hub.len() + 72);
    message.extend_from_slice(AUTH_DOMAIN);
    message.push(hub.len() as u8);
    message.extend_from_slice(hub.as_bytes());
    message.extend_from_slice(identity);
    message.extend_from_slice(challenge);
    message.extend_from_slice(&from_slot.to_le_bytes());
    message
}
fn verify_authentication(auth: &Authentication, challenge: &[u8; 32]) -> Result<()> {
    ensure!(valid_hub(&auth.hub), "invalid observer hub");
    VerifyingKey::from_bytes(&auth.identity)?.verify_strict(
        &authentication_message(&auth.hub, &auth.identity, challenge, auth.from_slot),
        &Signature::from_bytes(&auth.signature),
    ).context("observer identity proof failed")
}
fn challenge() -> [u8; 32] {
    let mut bytes = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes
}
fn frame(body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + body.len());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes
}
async fn read_frame(stream: &mut RecvStream, limit: usize) -> Result<Vec<u8>> {
    let mut length = [0; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length > 0 && length <= limit, "observer frame exceeds its limit");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    Ok(body)
}
async fn write_bytes(stream: &mut SendStream, bytes: &[u8]) -> Result<()> {
    timeout(WRITE_DEADLINE, stream.write_all(bytes)).await.context("observer write deadline exceeded")??;
    Ok(())
}
pub async fn write_client_frame(stream: &mut SendStream, message: &ClientFrame) -> Result<()> {
    let body = match message {
        ClientFrame::Authenticate(auth) => {
            ensure!(valid_hub(&auth.hub), "invalid observer hub");
            let mut body = vec![0, auth.hub.len() as u8];
            body.extend_from_slice(auth.hub.as_bytes());
            body.extend_from_slice(&auth.identity);
            body.extend_from_slice(&auth.from_slot.to_le_bytes());
            body.extend_from_slice(&auth.signature);
            body
        }
        ClientFrame::Ping { peer, nonce } => {
            let peer = u32::try_from(*peer).context("observer peer exceeds range")?;
            [vec![1], peer.to_le_bytes().to_vec(), nonce.to_le_bytes().to_vec()].concat()
        }
        ClientFrame::Envelope(envelope) => {
            ensure!(!envelope.is_empty() && envelope.len() <= MAX_MEMBER_ENVELOPE_BYTES, "member envelope exceeds its limit");
            [vec![2], envelope.clone()].concat()
        }
        ClientFrame::Routed {recipients,envelope} => {
            ensure!(!recipients.is_empty() && recipients.len() <= MAX_ROUTED_RECIPIENTS
                && recipients.iter().collect::<BTreeSet<_>>().len() == recipients.len(), "invalid member recipients");
            ensure!(!envelope.is_empty() && envelope.len() <= MAX_PACKET_BYTES, "member routed envelope exceeds its limit");
            let mut body = vec![3,recipients.len() as u8];
            for recipient in recipients { body.extend_from_slice(&u32::try_from(*recipient)?.to_le_bytes()); }
            body.extend_from_slice(envelope);
            body
        }
    };
    write_bytes(stream, &frame(&body)).await
}
async fn read_client_frame(stream: &mut RecvStream) -> Result<ClientFrame> {
    let mut length = [0; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(length > 0 && length <= MAX_ROUTED_FRAME_BYTES, "member frame exceeds its limit");
    let mut tag = [0];
    stream.read_exact(&mut tag).await?;
    ensure!(match tag[0] {2 => length <= MAX_MEMBER_ENVELOPE_BYTES + 1,3 => true,_ => length <= CONTROL_LIMIT}, "observer control frame exceeds its limit");
    let mut body = vec![0; length];
    body[0] = tag[0];
    stream.read_exact(&mut body[1..]).await?;
    match body[0] {
        0 => {
            let size = usize::from(*body.get(1).context("missing observer hub")?);
            ensure!(size <= 16 && body.len() == 106 + size, "invalid observer identity frame");
            let hub = String::from_utf8(body[2..2 + size].to_vec())?;
            let offset = 2 + size;
            Ok(ClientFrame::Authenticate(Authentication {
                hub,
                identity: body[offset..offset + 32].try_into().unwrap(),
                from_slot: u64::from_le_bytes(body[offset + 32..offset + 40].try_into().unwrap()),
                signature: body[offset + 40..].try_into().unwrap(),
            }))
        }
        1 if body.len() == 13 => Ok(ClientFrame::Ping {
            peer: u32::from_le_bytes(body[1..5].try_into().unwrap()) as usize,
            nonce: u64::from_le_bytes(body[5..13].try_into().unwrap()),
        }),
        2 if body.len() > 1 => Ok(ClientFrame::Envelope(body[1..].to_vec())),
        3 if body.len() > 2 => {
            let count = body[1] as usize;
            ensure!(count > 0 && count <= MAX_ROUTED_RECIPIENTS, "invalid member recipient count");
            let offset = 2 + count * 4;
            ensure!(body.len() > offset && body.len() - offset <= MAX_PACKET_BYTES, "invalid member routed envelope size");
            let recipients = body[2..offset].chunks_exact(4).map(|bytes|u32::from_le_bytes(bytes.try_into().unwrap()) as usize).collect::<Vec<_>>();
            ensure!(recipients.iter().collect::<BTreeSet<_>>().len() == count, "duplicate member recipients");
            Ok(ClientFrame::Routed {recipients,envelope:body[offset..].to_vec()})
        },
        _ => bail!("observer input must be an identity proof, ping or signed member envelope"),
    }
}
async fn write_server_frame(stream: &mut SendStream, message: &ServerFrame) -> Result<()> {
    let body = match message {
        ServerFrame::Challenge(nonce) => [vec![0], nonce.to_vec()].concat(),
        ServerFrame::Accepted => vec![1],
        ServerFrame::Pong { peer, nonce } => [vec![2], u32::try_from(*peer)?.to_le_bytes().to_vec(), nonce.to_le_bytes().to_vec()].concat(),
    };
    write_bytes(stream, &frame(&body)).await
}
pub async fn read_server_frame(stream: &mut RecvStream) -> Result<ServerFrame> {
    let body = read_frame(stream, CONTROL_LIMIT).await?;
    match body[0] {
        0 if body.len() == 33 => Ok(ServerFrame::Challenge(body[1..].try_into().unwrap())),
        1 if body.len() == 1 => Ok(ServerFrame::Accepted),
        2 if body.len() == 13 => Ok(ServerFrame::Pong {
            peer: u32::from_le_bytes(body[1..5].try_into().unwrap()) as usize,
            nonce: u64::from_le_bytes(body[5..13].try_into().unwrap()),
        }),
        _ => bail!("invalid observer server frame"),
    }
}
pub async fn read_packet(stream: &mut RecvStream) -> Result<Packet> {
    let body = read_frame(stream, MAX_PACKET_BYTES + 16).await?;
    ensure!(body.len() >= 16, "invalid observer packet");
    Ok(Packet {
        from: u32::from_le_bytes(body[..4].try_into().unwrap()) as usize,
        to: u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize,
        slot: u64::from_le_bytes(body[8..16].try_into().unwrap()),
        payload: body[16..].to_vec(),
    })
}
fn packet_frame(from: usize, to: usize, slot: u64, payload: &[u8]) -> Arc<Vec<u8>> {
    let mut bytes = Vec::with_capacity(20 + payload.len());
    bytes.extend_from_slice(&((16 + payload.len()) as u32).to_le_bytes());
    bytes.extend_from_slice(&(from as u32).to_le_bytes());
    bytes.extend_from_slice(&(to as u32).to_le_bytes());
    bytes.extend_from_slice(&slot.to_le_bytes());
    bytes.extend_from_slice(payload);
    Arc::new(bytes)
}

/// The active identity reservation is separate from pre-authentication connection permits.
struct Identities {
    cap: usize,
    held: BTreeSet<[u8; 32]>,
}
impl Identities {
    fn new(cap: usize) -> Result<Self> {
        ensure!((1..=MAX_IDENTITIES).contains(&cap), "observer identity cap must be 1 through 32");
        Ok(Self { cap, held: BTreeSet::new() })
    }
    fn reserve(&mut self, identity: [u8; 32]) -> Result<()> {
        ensure!(!self.held.contains(&identity), "observer identity is already connected");
        ensure!(self.held.len() < self.cap, "observer identity cap reached");
        self.held.insert(identity);
        Ok(())
    }
}
struct Observer {
    connection: Connection,
    receipts: bool,
}
#[derive(Clone, Copy)]
enum Audience { Hub(usize), Identity([u8;32]) }
impl Audience {
    fn includes(self, hub: usize, identity: &[u8;32]) -> bool {
        match self { Self::Hub(expected) => expected == hub, Self::Identity(expected) => &expected == identity }
    }
}
struct HistoryPacket {
    audience: Audience,
    needs_receipts: bool,
    slot: u64,
    bytes: Arc<Vec<u8>>,
}
struct State {
    identities: Identities,
    observers: BTreeMap<[u8; 32], Observer>,
    history: BTreeMap<u64, HistoryPacket>,
    history_bytes: usize,
    next_sequence: u64,
    oldest_slot: Vec<u64>,
    evicted_sequence: Vec<Option<u64>>,
    catalog: BTreeMap<usize,[u8;32]>,
    /// (oldest available slot, last evicted sequence) for private identity traffic.
    targeted_evicted: BTreeMap<[u8;32],(u64,u64)>,
    /// Registry hubs may use a hosted replay representative while their own peers link directly.
    registry_hubs: BTreeMap<String,Vec<usize>>,
}
pub struct ExternalServer {
    endpoint: Endpoint,
    certificate: Vec<u8>,
    hubs: Vec<(String, Vec<usize>)>,
    representatives: BTreeMap<usize, usize>,
    state: Mutex<State>,
    /// Woken on every packet recorded, for peers that have caught up with the history.
    published: Notify,
    pending: Arc<Semaphore>,
    accept_task: Mutex<Option<JoinHandle<()>>>,
    ingress: Mutex<Option<tokio::sync::mpsc::Sender<ExternalIngress>>>,
}
impl ExternalServer {
    pub async fn bind(bind: SocketAddr, hubs: Vec<(String, Vec<usize>)>, cap: usize) -> Result<Arc<Self>> {
        let identities = Identities::new(cap)?;
        ensure!(!hubs.is_empty() && hubs.len() <= 64, "observer endpoint requires known hubs");
        let mut names = BTreeSet::new();
        let mut representatives = BTreeMap::new();
        for (index, (hub, peers)) in hubs.iter().enumerate() {
            ensure!(valid_hub(hub) && names.insert(hub.clone()), "invalid or duplicate observer hub");
            ensure!(!peers.is_empty() && peers.len() <= 64 && peers.iter().all(|peer| *peer < 64), "invalid observer hub peers");
            ensure!(representatives.insert(peers[0], index).is_none(), "duplicate observer representative");
        }
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let cert_der = CertificateDer::from(certificate.cert);
        let certificate_bytes = cert_der.as_ref().to_vec();
        let key_der = PrivatePkcs8KeyDer::from(certificate.key_pair.serialize_der());
        let mut config = ServerConfig::with_single_cert(vec![cert_der], key_der.into())?;
        config.max_incoming(cap);
        config.incoming_buffer_size(16 * 1024);
        config.incoming_buffer_size_total((cap * 16 * 1024) as u64);
        let mut transport = TransportConfig::default();
        transport.max_concurrent_bidi_streams(1_u32.into());
        transport.max_concurrent_uni_streams(0_u32.into());
        transport.datagram_receive_buffer_size(None);
        transport.stream_receive_window((2_u32 * 1024 * 1024).into());
        transport.receive_window((4_u32 * 1024 * 1024).into());
        transport.send_window(4 * 1024 * 1024);
        transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
        transport.keep_alive_interval(Some(Duration::from_secs(5)));
        config.transport_config(Arc::new(transport));
        let endpoint = Endpoint::server(config, bind)?;
        let count = hubs.len();
        let server = Arc::new(Self {
            endpoint, certificate: certificate_bytes, hubs, representatives,
            state: Mutex::new(State { identities, observers: BTreeMap::new(), history: BTreeMap::new(), history_bytes: 0, next_sequence: 0, oldest_slot: vec![0; count], evicted_sequence: vec![None; count], catalog:BTreeMap::new(),targeted_evicted:BTreeMap::new(),registry_hubs:BTreeMap::new() }),
            published: Notify::new(), pending: Arc::new(Semaphore::new(cap)), accept_task: Mutex::new(None), ingress: Mutex::new(None),
        });
        let endpoint = server.endpoint.clone();
        let weak = Arc::downgrade(&server);
        let pending = server.pending.clone();
        *server.accept_task.lock().unwrap() = Some(tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let permit = match pending.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => { refused("too many pending handshakes", &"pending limit"); incoming.refuse(); continue }
                };
                let weak = weak.clone();
                tokio::spawn(async move {
                    let outcome = serve(weak, incoming, permit).await;
                    if let Err(error) = outcome { tracing::debug!(event = "external_observer_closed", %error); }
                });
            }
        }));
        Ok(server)
    }
    /// The node verifies membership, signatures, proposer roles and recipients at this boundary.
    pub fn set_ingress(&self, ingress: tokio::sync::mpsc::Sender<ExternalIngress>) {
        *self.ingress.lock().unwrap() = Some(ingress);
    }
    pub fn cert_der(&self) -> Vec<u8> { self.certificate.clone() }
    pub fn local_addr(&self) -> Result<SocketAddr> { Ok(self.endpoint.local_addr()?) }
    pub fn set_registry_hubs(&self,hubs:Vec<(String,Vec<usize>)>)->Result<()> {
        ensure!(hubs.iter().all(|(hub,peers)|valid_hub(hub)&&peers.iter().all(|peer|u32::try_from(*peer).is_ok())),"invalid registry peer hubs");
        self.state.lock().unwrap().registry_hubs=hubs.into_iter().collect();
        Ok(())
    }
    pub fn connected(&self,to:usize)->bool {
        let state=self.state.lock().unwrap();
        state.catalog.get(&to).is_some_and(|identity|state.observers.contains_key(identity))
    }
    /// Catalog IDs never change identity. Only hashed finalized membership learns this mapping.
    pub fn set_catalog(&self, catalog: Vec<(usize,[u8;32])>) -> Result<()> {
        let count = catalog.len();
        ensure!(catalog.iter().all(|(id,_)|u32::try_from(*id).is_ok())
            && catalog.iter().map(|(_,identity)|identity).collect::<BTreeSet<_>>().len() == count, "invalid external validator catalog");
        let catalog = catalog.into_iter().collect::<BTreeMap<_,_>>();
        ensure!(catalog.len() == count, "duplicate external catalog IDs");
        let mut state = self.state.lock().unwrap();
        ensure!(state.catalog.iter().all(|(id,identity)|catalog.get(id) == Some(identity)), "external validator catalog changed existing IDs");
        state.catalog = catalog;
        Ok(())
    }
    pub fn observes(&self, to: usize) -> bool {
        self.representatives.contains_key(&to) || (!self.hubs.iter().any(|(_,peers)|peers.contains(&to)) && self.state.lock().unwrap().catalog.contains_key(&to))
    }
    pub fn active_connections(&self) -> usize { self.state.lock().unwrap().observers.len() }
    /// Receipt support belongs to this authenticated connection, never a previous session.
    pub fn enable_receipts(&self, input: &ExternalIngress) {
        let mut state = self.state.lock().unwrap();
        if let Some(observer) = state.observers.get_mut(&input.identity) {
            if observer.connection.stable_id() == input.connection.stable_id() {
                observer.receipts = true;
            }
        }
        drop(state);
        self.published.notify_waiters();
    }
    pub fn supports_receipts(&self, to: usize) -> bool {
        let state = self.state.lock().unwrap();
        state.catalog.get(&to).and_then(|identity|state.observers.get(identity)).is_some_and(|observer|observer.receipts)
    }
    /// The host may discover an invalid codeword only after enough authenticated shreds arrive.
    pub fn reject_identity(&self, identity: &[u8;32], reason: &str) {
        let connection = self.state.lock().unwrap().observers.get(identity).map(|observer|observer.connection.clone());
        if let Some(connection) = connection {
            refused(reason,&"invalid member material");
            connection.close(3_u32.into(),reason.as_bytes());
        }
    }

    /// A bounded memory operation only: no network await, timer or geography delay.
    pub fn publish(&self, from: usize, to: usize, slot: u64, payload: Arc<Vec<u8>>) {
        self.publish_inner(from,to,slot,payload,false);
    }
    /// Optional telemetry uses the same held history but cannot make replay fall behind.
    pub fn publish_receipt(&self, from: usize, to: usize, slot: u64, payload: Arc<Vec<u8>>) {
        self.publish_inner(from,to,slot,payload,true);
    }
    fn publish_inner(&self, from: usize, to: usize, slot: u64, payload: Arc<Vec<u8>>, needs_receipts: bool) {
        assert!(u32::try_from(from).is_ok() && u32::try_from(to).is_ok() && payload.len() <= MAX_PACKET_BYTES, "invalid internal observer envelope");
        let bytes = packet_frame(from, to, slot, &payload);
        let mut state = self.state.lock().unwrap();
        let audience = if let Some(hub) = self.representatives.get(&to) { Audience::Hub(*hub) }
            else if !self.hubs.iter().any(|(_,peers)|peers.contains(&to)) {
                let Some(identity) = state.catalog.get(&to).copied() else { return };
                Audience::Identity(identity)
            } else { return };
        let sequence = state.next_sequence;
        state.next_sequence = sequence.checked_add(1).expect("observer sequence exhausted");
        state.history_bytes += bytes.len();
        state.history.insert(sequence, HistoryPacket { audience, needs_receipts, slot, bytes: bytes.clone() });
        while state.history_bytes > HISTORY_BYTES || state.history.len() > HISTORY_PACKETS {
            let (sequence, packet) = state.history.pop_first().unwrap();
            state.history_bytes -= packet.bytes.len();
            if packet.needs_receipts { continue; }
            match packet.audience {
                Audience::Hub(hub) => {
                    state.oldest_slot[hub] = state.oldest_slot[hub].max(packet.slot.saturating_add(1));
                    state.evicted_sequence[hub] = Some(sequence);
                }
                Audience::Identity(identity) => {
                    let oldest = state.targeted_evicted.entry(identity).or_insert((0,sequence));
                    oldest.0 = oldest.0.max(packet.slot.saturating_add(1));
                    oldest.1 = sequence;
                }
            }
        }
        drop(state);
        self.published.notify_waiters();
    }
    /// The observer's next packet from the shared history, read at the pace its link drains it: one
    /// path for the replay and the live stream, with nothing queued per peer. A peer slower than the
    /// net falls behind the history's start and is closed there.
    fn next_packet(&self, hub: usize, identity: &[u8;32], cursor: u64, from_slot: u64) -> Result<Option<(u64, Arc<Vec<u8>>)>> {
        let state = self.state.lock().unwrap();
        ensure!(state.evicted_sequence[hub].is_none_or(|evicted| evicted < cursor)
            && state.targeted_evicted.get(identity).is_none_or(|(_,evicted)| *evicted < cursor), "observer fell behind the held history");
        let receipts = state.observers.get(identity).is_some_and(|observer|observer.receipts);
        Ok(state.history.range(cursor..).find(|(_, packet)| (!packet.needs_receipts || receipts) && packet.audience.includes(hub,identity) && packet.slot >= from_slot)
            .map(|(sequence, packet)| (*sequence, packet.bytes.clone())))
    }
    fn remove(&self, identity: &[u8; 32], connection: &Connection) {
        let mut state = self.state.lock().unwrap();
        if state.observers.get(identity).is_some_and(|observer| observer.connection.stable_id() == connection.stable_id()) {
            state.observers.remove(identity);
            state.identities.held.remove(identity);
        }
    }
    pub async fn close(&self) {
        self.endpoint.close(0_u32.into(), b"observer endpoint stopped");
        if let Some(task) = self.accept_task.lock().unwrap().take() { task.abort(); }
        {
            let mut state = self.state.lock().unwrap();
            state.observers.clear();
            state.identities.held.clear();
        }
        let _ = timeout(Duration::from_secs(3), self.endpoint.wait_idle()).await;
    }
}
impl Drop for ExternalServer {
    fn drop(&mut self) {
        self.endpoint.close(0_u32.into(), b"observer endpoint dropped");
        if let Some(task) = self.accept_task.get_mut().unwrap().take() { task.abort(); }
    }
}
/// One `warn` line per refused outside peer, with its reason, at most one a second: a peer that
/// retries in a loop can't flood the net's log. The count of lines held back rides on the next one.
fn refused(reason: &str, error: &dyn std::fmt::Display) {
    use std::sync::atomic::AtomicU64;
    static LAST: AtomicU64 = AtomicU64::new(0);
    static HELD: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let last = LAST.load(Ordering::Relaxed);
    if now > last && LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
        tracing::warn!(event = "external_observer_refused", reason, error = %error, held_back = HELD.swap(0, Ordering::Relaxed));
    } else {
        HELD.fetch_add(1, Ordering::Relaxed);
    }
}
async fn serve(server: Weak<ExternalServer>, incoming: quinn::Incoming, permit: tokio::sync::OwnedSemaphorePermit) -> Result<()> {
    let deadline = tokio::time::Instant::now() + AUTH_DEADLINE;
    let connection = timeout_at(deadline, incoming).await.context("observer TLS deadline exceeded")??;
    let authentication = timeout_at(deadline, async {
        let (mut send, mut recv) = connection.accept_bi().await?;
        let mut magic = vec![0; CONTROL_MAGIC.len()];
        recv.read_exact(&mut magic).await?;
        ensure!(magic == CONTROL_MAGIC, "invalid observer control stream header");
        let nonce = challenge();
        write_server_frame(&mut send, &ServerFrame::Challenge(nonce)).await?;
        let ClientFrame::Authenticate(auth) = read_client_frame(&mut recv).await? else { bail!("observer identity proof is required") };
        verify_authentication(&auth, &nonce)?;
        Ok::<_, anyhow::Error>((send, recv, auth))
    }).await.context("observer authentication deadline exceeded").and_then(|result| result);
    let (mut control_send, mut control_recv, auth) = match authentication {
        Ok(auth) => auth,
        Err(error) => { refused("identity proof rejected", &error); connection.close(1_u32.into(), b"observer identity proof rejected"); return Err(error) }
    };
    let register = (|| {
        let server = server.upgrade().context("observer endpoint stopped")?;
        let mut state = server.state.lock().unwrap();
        let configured=server.hubs.iter().position(|(name,_)|*name==auth.hub);
        let peers=state.registry_hubs.get(&auth.hub).cloned()
            .or_else(||configured.map(|index|server.hubs[index].1.clone())).context("unknown observer hub")?;
        // Node zero already receives the whole finalized wire. A newly registered hub
        // bootstraps from that same held stream; its local commitments use its own endpoints.
        let hub=configured.unwrap_or(0);
        ensure!(auth.from_slot >= state.oldest_slot[hub]
            && state.targeted_evicted.get(&auth.identity).is_none_or(|(oldest,_)|auth.from_slot >= *oldest), "observer requested history is no longer held");
        state.identities.reserve(auth.identity)?;
        let cursor = state.history.iter().find(|(_, packet)| !packet.needs_receipts && packet.audience.includes(hub,&auth.identity) && packet.slot >= auth.from_slot).map_or(state.next_sequence, |(sequence, _)| *sequence);
        state.observers.insert(auth.identity, Observer { connection: connection.clone(), receipts: false });
        Ok::<_, anyhow::Error>((hub, cursor, peers))
    })();
    let (hub, mut cursor, peers) = match register {
        Ok(registered) => registered,
        Err(error) => { refused("registration rejected", &error); connection.close(1_u32.into(), b"observer registration rejected"); return Err(error) }
    };
    drop(permit);
    let outcome = async {
        write_server_frame(&mut control_send, &ServerFrame::Accepted).await?;
        let packets = async {
            let mut stream = timeout(WRITE_DEADLINE, connection.open_uni()).await.context("observer packet stream deadline exceeded")??;
            write_bytes(&mut stream, DATA_MAGIC).await?;
            loop {
                let server = server.upgrade().context("observer endpoint stopped")?;
                // Armed before the read, so a packet recorded between the two still wakes it.
                let published = server.published.notified();
                tokio::pin!(published);
                published.as_mut().enable();
                let next = server.next_packet(hub, &auth.identity, cursor, auth.from_slot);
                let next = match next {
                    Ok(next) => next,
                    Err(error) => { refused("fell behind the held history", &error); return Err(error) }
                };
                match next {
                    Some((sequence, bytes)) => { write_bytes(&mut stream, &bytes).await?; cursor = sequence + 1; }
                    None => published.await,
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        };
        let controls = async {
            let mut window = Instant::now();
            let mut pings = 0;
            let mut envelopes = 0;
            let mut routed_frames = 0;
            let mut routed_bytes = 0;
            loop {
                let message = match read_client_frame(&mut control_recv).await {
                    Ok(message) => message,
                    Err(error) => {
                        // A normal disconnect is not a malformed protocol message.
                        if error.downcast_ref::<quinn::ReadExactError>().is_none() { refused("invalid member frame", &error); }
                        return Err(error)
                    }
                };
                let received_at = Instant::now();
                if window.elapsed() >= Duration::from_secs(1) {
                    window = Instant::now(); pings = 0; envelopes = 0; routed_frames = 0; routed_bytes = 0;
                }
                let (recipients,envelope) = match message {
                    ClientFrame::Ping { peer, nonce } => {
                        pings += 1;
                        if pings > 10 {
                            refused("observer ping rate exceeded", &"10 pings per second");
                            bail!("observer ping rate exceeded")
                        }
                        if !peers.contains(&peer) {
                            refused("observer ping names another hub", &"invalid ping peer");
                            bail!("observer ping names another hub")
                        }
                        write_server_frame(&mut control_send, &ServerFrame::Pong { peer, nonce }).await?;
                        continue;
                    }
                    ClientFrame::Envelope(envelope) => {
                        envelopes += 1;
                        if envelopes > 32 {
                            refused("member envelope rate exceeded", &"32 envelopes per second");
                            bail!("member envelope rate exceeded")
                        }
                        (None,envelope)
                    }
                    ClientFrame::Routed {recipients,envelope} => {
                        routed_frames += 1;
                        routed_bytes += envelope.len() + 6 + 4 * recipients.len();
                        if routed_frames > MAX_ROUTED_FRAMES_PER_SECOND || routed_bytes > MAX_ROUTED_BYTES_PER_SECOND {
                            refused("member proposing rate exceeded", &"8192 frames or 64 MiB per second");
                            bail!("member proposing rate exceeded")
                        }
                        (Some(recipients),envelope)
                    }
                    ClientFrame::Authenticate(_) => {
                        refused("duplicate identity proof", &"already authenticated");
                        bail!("observer cannot authenticate twice")
                    },
                };
                let server = server.upgrade().context("observer endpoint stopped")?;
                let ingress = server.ingress.lock().unwrap().clone().context("member ingress is not enabled")?;
                if let Err(error) = ingress.try_send(ExternalIngress {identity:auth.identity,hub:auth.hub.clone(),recipients,envelope,received_at,connection:connection.clone()}) {
                    refused("member ingress is full or closed", &error);
                    return Err(error.into())
                }
            }
        };
        tokio::select! {
            result = packets => result,
            result = controls => result,
        }
    }.await;
    let reason: &[u8] = if outcome.as_ref().err().is_some_and(|error| error.to_string().contains("observer write deadline exceeded")) {
        b"observer write deadline exceeded"
    } else { b"observer stream closed" };
    connection.close(2_u32.into(), reason);
    if let Some(server) = server.upgrade() { server.remove(&auth.identity, &connection); }
    outcome
}

/// Trust only the exact self-signed certificate obtained from the gateway's HTTPS origin.
pub fn pinned_client_config(cert_der: &[u8]) -> Result<ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(cert_der.to_vec()))?;
    let mut config = ClientConfig::with_root_certificates(Arc::new(roots))?;
    let mut transport = TransportConfig::default();
    transport.max_concurrent_bidi_streams(0_u32.into());
    transport.max_concurrent_uni_streams(1_u32.into());
    transport.stream_receive_window((4_u32 * 1024 * 1024).into());
    transport.receive_window((8_u32 * 1024 * 1024).into());
    transport.send_window(4 * 1024 * 1024);
    transport.max_idle_timeout(Some(Duration::from_secs(30).try_into()?));
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    config.transport_config(Arc::new(transport));
    Ok(config)
}
pub struct RawClient {
    pub endpoint: Endpoint,
    pub connection: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
}
/// Raw framed control streams are also useful to callers inspecting authentication failures.
pub async fn open_client(address: SocketAddr, config: ClientConfig) -> Result<RawClient> {
    let bind = SocketAddr::new(if address.is_ipv6() { IpAddr::V6(Ipv6Addr::UNSPECIFIED) } else { IpAddr::V4(Ipv4Addr::UNSPECIFIED) }, 0);
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(config);
    let deadline = tokio::time::Instant::now() + AUTH_DEADLINE;
    let connection = timeout_at(deadline, endpoint.connect(address, "localhost")?).await.context("observer TLS deadline exceeded")??;
    let (mut send, recv) = timeout_at(deadline, connection.open_bi()).await.context("observer control stream deadline exceeded")??;
    // QUIC does not expose a newly opened stream to its peer until the first bytes are sent.
    write_bytes(&mut send, CONTROL_MAGIC).await?;
    Ok(RawClient { endpoint, connection, send, recv })
}
struct ControlStreams {
    send: SendStream,
    recv: RecvStream,
    nonce: u64,
}
#[derive(Clone)]
pub struct ExternalControl {
    streams: Arc<tokio::sync::Mutex<ControlStreams>>,
}
impl ExternalControl {
    /// The existing small vote frame; admission and signed vote validation belong to the node.
    pub async fn send_envelope(&self, envelope: &[u8]) -> Result<()> {
        let mut streams = self.streams.lock().await;
        write_client_frame(&mut streams.send, &ClientFrame::Envelope(envelope.to_vec())).await
    }
    /// The node authenticates the envelope and validates these recipients against its role.
    pub async fn send_to(&self, recipients: &[usize], envelope: &[u8]) -> Result<()> {
        let mut streams = self.streams.lock().await;
        write_client_frame(&mut streams.send, &ClientFrame::Routed {recipients:recipients.to_vec(),envelope:envelope.to_vec()}).await
    }
    pub async fn ping(&self, peer: usize) -> Result<Duration> {
        let mut streams = self.streams.lock().await;
        streams.nonce = streams.nonce.checked_add(1).context("observer ping sequence exhausted")?;
        let nonce = streams.nonce;
        let started = Instant::now();
        write_client_frame(&mut streams.send, &ClientFrame::Ping { peer, nonce }).await?;
        match timeout(AUTH_DEADLINE, read_server_frame(&mut streams.recv)).await.context("observer ping deadline exceeded")?? {
            ServerFrame::Pong { peer: echoed, nonce: echoed_nonce } if peer == echoed && nonce == echoed_nonce => Ok(started.elapsed()),
            _ => bail!("observer ping response differs"),
        }
    }
}
pub struct ExternalClient {
    _endpoint: Endpoint,
    pub connection: Connection,
    pub control: ExternalControl,
    pub packets: RecvStream,
}
impl ExternalClient {
    pub async fn connect(address: SocketAddr, cert_der: &[u8], hub: &str, key: &SigningKey, from_slot: u64) -> Result<Self> {
        ensure!(valid_hub(hub), "invalid observer hub");
        let mut raw = open_client(address, pinned_client_config(cert_der)?).await?;
        let deadline = tokio::time::Instant::now() + AUTH_DEADLINE;
        let nonce = match timeout_at(deadline, read_server_frame(&mut raw.recv)).await.context("observer challenge deadline exceeded")?? {
            ServerFrame::Challenge(nonce) => nonce,
            _ => bail!("observer endpoint did not issue a challenge"),
        };
        let identity = key.verifying_key().to_bytes();
        let signature = key.sign(&authentication_message(hub, &identity, &nonce, from_slot)).to_bytes();
        write_client_frame(&mut raw.send, &ClientFrame::Authenticate(Authentication { hub: hub.into(), identity, from_slot, signature })).await?;
        ensure!(matches!(timeout_at(deadline, read_server_frame(&mut raw.recv)).await.context("observer admission deadline exceeded")??, ServerFrame::Accepted), "observer endpoint did not admit identity");
        let mut packets = timeout_at(deadline, raw.connection.accept_uni()).await.context("observer packet stream deadline exceeded")??;
        let mut magic = vec![0; DATA_MAGIC.len()];
        timeout_at(deadline, packets.read_exact(&mut magic)).await.context("observer packet header deadline exceeded")??;
        ensure!(magic == DATA_MAGIC, "invalid observer packet stream header");
        Ok(Self { _endpoint: raw.endpoint, connection: raw.connection, control: ExternalControl { streams: Arc::new(tokio::sync::Mutex::new(ControlStreams { send: raw.send, recv: raw.recv, nonce: 0 })) }, packets })
    }
    pub async fn next_packet(&mut self) -> Result<Packet> { read_packet(&mut self.packets).await }
}
impl Drop for ExternalClient {
    fn drop(&mut self) { self.connection.close(0_u32.into(), b"observer client stopped"); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn external_challenge_authentication_binds_fresh_nonce_hub_identity_and_frontier() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let identity = key.verifying_key().to_bytes();
        let nonce = challenge();
        let mut auth = Authentication { hub: "FRA".into(), identity, from_slot: 123, signature: [0; 64] };
        auth.signature = key.sign(&authentication_message(&auth.hub, &identity, &nonce, auth.from_slot)).to_bytes();
        verify_authentication(&auth, &nonce).unwrap();
        assert!(verify_authentication(&auth, &challenge()).is_err(), "an old proof cannot authenticate a fresh connection");
        let mut changed = auth.clone(); changed.hub = "NYC".into();
        assert!(verify_authentication(&changed, &nonce).is_err());
        let mut changed = auth.clone(); changed.identity = SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes();
        assert!(verify_authentication(&changed, &nonce).is_err());
        let mut changed = auth.clone(); changed.from_slot += 1;
        assert!(verify_authentication(&changed, &nonce).is_err());
        auth.signature[0] ^= 1;
        assert!(verify_authentication(&auth, &nonce).is_err());
    }
    #[test]
    fn external_identity_registry_is_bounded_and_allows_one_connection_per_identity() {
        assert!(Identities::new(0).is_err());
        assert!(Identities::new(MAX_IDENTITIES + 1).is_err());
        let mut registry = Identities::new(MAX_IDENTITIES).unwrap();
        for byte in 0..MAX_IDENTITIES { registry.reserve([byte as u8; 32]).unwrap(); }
        assert!(registry.reserve([0; 32]).is_err(), "a connected identity cannot open a second connection");
        assert!(registry.reserve([99; 32]).is_err(), "a fresh identity cannot bypass the cap");
        assert_eq!(registry.held.len(), MAX_IDENTITIES);
        registry.held.remove(&[0; 32]);
        registry.reserve([99; 32]).unwrap();
        assert_eq!(registry.held.len(), MAX_IDENTITIES);
    }
}
