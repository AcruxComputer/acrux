//! Public limits on the gateway: token buckets per client IP for reads and for submits, and a cap
//! on event-stream connections per IP. Behind a reverse proxy (`ACRUX_TRUST_PROXY=1`) the client is
//! the last address of `X-Forwarded-For`, the one the proxy itself appended.
use axum::http::HeaderMap;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Mutex, OnceLock},
    time::Instant,
};

struct Bucket {
    rate: f64,
    burst: f64,
    clients: Mutex<HashMap<IpAddr, (f64, Instant)>>,
}

impl Bucket {
    fn new(rate: f64, burst: f64) -> Self {
        Self { rate, burst, clients: Mutex::new(HashMap::new()) }
    }
    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut clients = self.clients.lock().unwrap();
        if clients.len() > 50_000 {
            // Forget clients whose bucket has refilled.
            let full = self.burst / self.rate;
            clients.retain(|_, (_, at)| now.duration_since(*at).as_secs_f64() < full);
        }
        let (tokens, at) = clients.entry(ip).or_insert((self.burst, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * self.rate).min(self.burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
    /// Whether the client has anything left, without taking from it.
    fn ready(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut clients = self.clients.lock().unwrap();
        let (tokens, at) = clients.entry(ip).or_insert((self.burst, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * self.rate).min(self.burst);
        *at = now;
        *tokens > 0.0
    }
    /// Takes `cost` after the fact; the bucket may go below zero and refills from there.
    fn charge(&self, ip: IpAddr, cost: f64) {
        if let Some((tokens, _)) = self.clients.lock().unwrap().get_mut(&ip) {
            *tokens -= cost;
        }
    }
}

/// Every client's simulations together are charged here as well.
const ALL: IpAddr = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);

pub struct Limits {
    reads: Bucket,
    submits: Bucket,
    /// Compute units of simulation (`simulateTransaction`, `sendTransaction`'s preflight), per IP
    /// and for all clients together: each runs on validator 0's own executor.
    simulation: Bucket,
    simulation_all: Bucket,
    sockets: Mutex<HashMap<IpAddr, usize>>,
    sockets_per_ip: usize,
    /// Stream connections in all (`ACRUX_WS_MAX`).
    sockets_total: usize,
    trust_proxy: bool,
}

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// The limits, from `ACRUX_RPC_RATE` (reads a second per IP, default 50, burst twice that),
/// `ACRUX_SUBMIT_RATE` (submits a second per IP, default 50, burst ten seconds of it: a program
/// deploy sends its few hundred buffer writes at once), `ACRUX_SIMULATION_CU` (compute units of
/// simulation a second per IP, default 4,000,000, burst twice that; ten times it for all clients)
/// and `ACRUX_WS_PER_IP` (8).
pub fn limits() -> &'static Limits {
    static LIMITS: OnceLock<Limits> = OnceLock::new();
    LIMITS.get_or_init(|| {
        let reads = env("ACRUX_RPC_RATE", 50.0);
        Limits {
            reads: Bucket::new(reads, reads * 2.0),
            submits: {
                let rate = env("ACRUX_SUBMIT_RATE", 50.0);
                Bucket::new(rate, rate * 10.0)
            },
            simulation: {
                let rate = env("ACRUX_SIMULATION_CU", 4_000_000.0);
                Bucket::new(rate, rate * 2.0)
            },
            simulation_all: {
                let rate = env("ACRUX_SIMULATION_CU", 4_000_000.0) * 10.0;
                Bucket::new(rate, rate * 2.0)
            },
            sockets: Mutex::new(HashMap::new()),
            sockets_per_ip: env("ACRUX_WS_PER_IP", 8),
            sockets_total: env("ACRUX_WS_MAX", 200),
            trust_proxy: env("ACRUX_TRUST_PROXY", 0u8) == 1,
        }
    })
}

impl Limits {
    pub fn client(&self, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
        if self.trust_proxy {
            if let Some(ip) = headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit(',').next())
                .and_then(|v| v.trim().parse().ok())
            {
                return ip;
            }
        }
        peer.ip()
    }
    pub fn read(&self, ip: IpAddr) -> bool {
        self.reads.allow(ip)
    }
    pub fn submit(&self, ip: IpAddr) -> bool {
        self.submits.allow(ip)
    }
    /// Whether a simulation for this client may run now; `None` is the node's own.
    pub fn simulation_ready(&self, ip: Option<IpAddr>) -> bool {
        ip.map_or(true, |ip| self.simulation_all.ready(ALL) && self.simulation.ready(ip))
    }
    pub fn simulation_charge(&self, ip: Option<IpAddr>, units: u64) {
        if let Some(ip) = ip {
            self.simulation.charge(ip, units as f64);
            self.simulation_all.charge(ALL, units as f64);
        }
    }
    /// Takes one of the client's event-stream connections; `None` when it holds all of them.
    pub fn open_socket(&'static self, ip: IpAddr) -> Option<SocketPermit> {
        let mut sockets = self.sockets.lock().unwrap();
        if sockets.values().sum::<usize>() >= self.sockets_total {
            return None;
        }
        let open = sockets.entry(ip).or_default();
        if *open >= self.sockets_per_ip {
            return None;
        }
        *open += 1;
        Some(SocketPermit { limits: self, ip })
    }
}

pub struct SocketPermit {
    limits: &'static Limits,
    ip: IpAddr,
}

impl Drop for SocketPermit {
    fn drop(&mut self) {
        let mut sockets = self.limits.sockets.lock().unwrap();
        if let Some(open) = sockets.get_mut(&self.ip) {
            *open -= 1;
            if *open == 0 {
                sockets.remove(&self.ip);
            }
        }
    }
}
