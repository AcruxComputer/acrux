//! Twelve independent QUIC endpoints with measured, reproducible local geography delays.
use anyhow::{ensure, Context, Result};
use crate::profiling::Profiling;
use quinn::{ClientConfig, Connection, Endpoint, ServerConfig, TransportConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, Mutex},
    task::JoinHandle,
};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;


#[derive(Clone, Debug)]
pub struct Peer {
    pub id: usize,
    pub hub: String,
    pub latitude: f64,
    pub longitude: f64,
    pub stake: u64,
}

#[derive(Debug)]
pub struct Received {
    pub from: usize,
    pub to: usize,
    pub payload: Vec<u8>,
    pub received_at: Instant,
}

pub struct LocalTransport {
    peers: Vec<Peer>,
    endpoints: Vec<Endpoint>,
    connections: Vec<Vec<Mutex<Option<Connection>>>>,
    incoming: mpsc::Sender<Received>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    seed: u64,
    serial: Vec<Vec<AtomicU64>>,
    base_port: u16,
    profile:Arc<Profiling>,
    external: std::sync::RwLock<Option<(Arc<crate::external::ExternalServer>, fn(&[u8]) -> Option<u64>)>>,
}

impl LocalTransport {
    pub async fn bind(
        peers: Vec<Peer>,
        base_port: u16,
        seed: u64,
        profile:Arc<Profiling>,
    ) -> Result<(Arc<Self>, mpsc::Receiver<Received>)> {
        ensure!(
            !peers.is_empty() && peers.len() <= 64,
            "transport requires 1 through 64 peers"
        );
        ensure!(
            peers.iter().enumerate().all(|(i, peer)| peer.id == i),
            "peer ids must be dense and ordered"
        );
        ensure!(
            (base_port as usize) + peers.len() <= u16::MAX as usize,
            "peer ports exceed range"
        );
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
        let cert_der = CertificateDer::from(certificate.cert);
        let key_der = PrivatePkcs8KeyDer::from(certificate.key_pair.serialize_der());
        let mut server_config =
            ServerConfig::with_single_cert(vec![cert_der.clone()], key_der.into())?;
        let mut transport = TransportConfig::default();
        transport.max_concurrent_uni_streams(2048_u32.into());
        transport.max_idle_timeout(Some(Duration::from_secs(60).try_into()?));
        transport.keep_alive_interval(Some(Duration::from_secs(10)));
        server_config.transport_config(Arc::new(transport));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der)?;
        let client_config = ClientConfig::with_root_certificates(Arc::new(roots))?;
        let mut endpoints = Vec::new();
        for i in 0..peers.len() {
            let mut endpoint = Endpoint::server(
                server_config.clone(),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), base_port + i as u16),
            )?;
            endpoint.set_default_client_config(client_config.clone());
            endpoints.push(endpoint);
        }
        let (incoming, receiver) = mpsc::channel(32768);
        let connections = (0..peers.len())
            .map(|_| (0..peers.len()).map(|_| Mutex::new(None)).collect())
            .collect();
        let serial = (0..peers.len())
            .map(|_| (0..peers.len()).map(|_| AtomicU64::new(0)).collect())
            .collect();
        let local = Arc::new(Self {
            peers,
            endpoints,
            connections,
            incoming,
            tasks: Mutex::new(Vec::new()),
            seed,
            serial,
            base_port,
            profile,
            external: std::sync::RwLock::new(None),
        });
        for to in 0..local.peers.len() {
            let endpoint = local.endpoints[to].clone();
            let sender = local.incoming.clone();
            let base = base_port;
            let count = local.peers.len();
            local.tasks.lock().await.push(tokio::spawn(async move {
                while let Some(connecting) = endpoint.accept().await {
                    let sender = sender.clone();
                    tokio::spawn(async move {
                        let connection = match connecting.await {
                            Ok(connection) => connection,
                            Err(error) => {
                                tracing::warn!(%error, "QUIC handshake rejected");
                                return;
                            }
                        };
                        let from = match connection.remote_address().port().checked_sub(base) {
                            Some(id) if (id as usize) < count => id as usize,
                            _ => return,
                        };
                        while let Ok(mut stream) = connection.accept_uni().await {
                            let sender = sender.clone();
                            tokio::spawn(async move {
                                match stream.read_to_end(MAX_MESSAGE_BYTES).await {
                                    Ok(payload) => {
                                        let received_at = Instant::now();
                                        let _ = sender
                                            .send(Received {
                                                from,
                                                to,
                                                payload,
                                                received_at,
                                            })
                                            .await;
                                    }
                                    Err(error) => {
                                        tracing::warn!(%error, from, to, "QUIC message rejected")
                                    }
                                }
                            });
                        }
                    });
                }
            }));
        }
        // Complete TLS handshakes before any node starts its slot clock.
        // Every later protocol send reuses this connection and measures only the configured link.
        let mut handshakes = tokio::task::JoinSet::new();
        for from in 0..local.peers.len() {
            for to in 0..local.peers.len() {
                let transport = local.clone();
                handshakes.spawn(async move {
                    let address = SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::LOCALHOST),
                        transport.base_port + to as u16,
                    );
                    let connection = transport.endpoints[from]
                        .connect(address, "localhost")?
                        .await
                        .context("QUIC startup handshake failed")?;
                    *transport.connections[from][to].lock().await = Some(connection);
                    Ok::<(), anyhow::Error>(())
                });
            }
        }
        while let Some(result) = handshakes.join_next().await {
            result??;
        }
        Ok((local, receiver))
    }

    /// The node owns its Wire type; the transport keeps those same signed bytes opaque.
    pub fn set_external(&self, server: Arc<crate::external::ExternalServer>, slot_of: fn(&[u8]) -> Option<u64>) {
        *self.external.write().unwrap() = Some((server,slot_of));
    }

    pub fn external_server(&self) -> Option<Arc<crate::external::ExternalServer>> {
        self.external.read().unwrap().as_ref().map(|(server,_)|server.clone())
    }
    /// Already authenticated member votes enter the ordinary receive path without geography.
    pub fn inject_external(&self,from:usize,to:usize,payload:Vec<u8>)->Result<()> {
        ensure!(to<self.peers.len() && payload.len()<=MAX_MESSAGE_BYTES,"Invalid member vote delivery");
        if let Some((server,slot_of))=self.external.read().unwrap().clone() {
            if server.observes(to) {if let Some(slot)=slot_of(&payload){server.publish(from,to,slot,Arc::new(payload.clone()));}}
        }
        self.incoming.try_send(Received{from,to,payload,received_at:Instant::now()}).context("Member vote receive capacity exceeded")?;
        Ok(())
    }

    pub fn peers(&self) -> &[Peer] {
        &self.peers
    }
    pub fn addresses(&self) -> Vec<SocketAddr> {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.local_addr().unwrap())
            .collect()
    }

    pub async fn send(&self, from: usize, to: usize, payload: Vec<u8>) -> Result<()> {
        let _whole=self.profile.measure(Some(from),"transport.send");
        ensure!(from<self.peers.len(),"unknown local transport sender");
        if to>=self.peers.len() {return Ok(())}
        ensure!(
            payload.len() <= MAX_MESSAGE_BYTES,
            "network message exceeds size limit"
        );
        // An outside link carries only its real delay. Publishing cannot wait for a peer's
        // socket or queue; the boundary drops an observer that does not keep up.
        if let Some((server,slot_of)) = self.external.read().unwrap().clone() {
            if server.observes(to) {
                if let Some(slot) = slot_of(&payload) { server.publish(from,to,slot,Arc::new(payload.clone())); }
            }
        }
        let serial = self.serial[from][to].fetch_add(1, Ordering::Relaxed);
        let delay=self.link_delay(from,to,serial);
        let delay_started=Instant::now();
        tokio::time::sleep(delay).await;
        self.profile.record(Some(from),"transport.timer_overshoot",delay_started.elapsed().saturating_sub(delay));
        // Even a self-directed packet crosses its node's actual QUIC endpoint.
        let lock_started=Instant::now();
        let mut slot = self.connections[from][to].lock().await;
        self.profile.record(Some(from),"transport.connection_lock_wait",lock_started.elapsed());
        if slot.is_none() {
            *slot = Some(
                self.endpoints[from]
                    .connect(
                        SocketAddr::new(
                            IpAddr::V4(Ipv4Addr::LOCALHOST),
                            self.base_port + to as u16,
                        ),
                        "localhost",
                    )?
                    .await
                    .context("QUIC connection failed")?,
            );
        }
        let connection = slot.as_ref().unwrap().clone();
        drop(slot);
        let open_started=Instant::now();
        let mut stream = connection.open_uni().await?;
        self.profile.record(Some(from),"transport.open_stream",open_started.elapsed());
        let write_started=Instant::now();
        stream.write_all(&payload).await?;
        stream.finish()?;
        self.profile.record(Some(from),"transport.write_stream",write_started.elapsed());
        Ok(())
    }

    pub fn link_delay(&self, from: usize, to: usize, sequence: u64) -> Duration {
        if from == to {
            return Duration::ZERO;
        }
        let a = &self.peers[from];
        let b = &self.peers[to];
        let random = mix(self.seed ^ ((from as u64) << 48) ^ ((to as u64) << 32) ^ sequence);
        let micros = if a.hub == b.hub {
            1000 + random % 3001 // Every local message is between 1 and 4 ms, below deltaLocal=10 ms.
        } else {
            let distance_km = great_circle_km(a.latitude, a.longitude, b.latitude, b.longitude);
            // Optical fibre at 2/3 c; 8 ms covers switching/host overhead. Jitter is <=2 ms.
            (distance_km / 199_861.638 * 1_000_000.0).round() as u64 + 8000 + random % 2001
        };
        Duration::from_micros(micros)
    }

    /// Rotor samples a stake-weighted relay independently for each slot, slice, proposer and shred.
    pub fn relay(&self, slot: u64, slice: u32, proposer: usize, shred: u8) -> usize {
        let total: u64 = self.peers.iter().map(|peer| peer.stake).sum();
        assert!(total > 0, "relay sampling requires positive stake");
        let mut point = mix(self.seed
            ^ slot.rotate_left(11)
            ^ ((slice as u64) << 32)
            ^ ((proposer as u64) << 8)
            ^ shred as u64)
            % total;
        for peer in &self.peers {
            if point < peer.stake {
                return peer.id;
            }
            point -= peer.stake;
        }
        unreachable!()
    }

    pub async fn close(&self) {
        let external = self.external.write().unwrap().take();
        if let Some((server,_)) = external { server.close().await; }
        for endpoint in &self.endpoints {
            endpoint.close(0_u32.into(), b"cluster stopped");
        }
        for task in self.tasks.lock().await.drain(..) {
            task.abort();
        }
        for endpoint in &self.endpoints {
            endpoint.wait_idle().await;
        }
    }
}

fn mix(mut n: u64) -> u64 {
    n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
    n ^ (n >> 31)
}
pub fn great_circle_km(lat_a: f64, lon_a: f64, lat_b: f64, lon_b: f64) -> f64 {
    let lat_delta = (lat_b - lat_a).to_radians();
    let lon_delta = (lon_b - lon_a).to_radians();
    let haversine = (lat_delta / 2.0).sin().powi(2)
        + lat_a.to_radians().cos() * lat_b.to_radians().cos() * (lon_delta / 2.0).sin().powi(2);
    6371.0088 * 2.0 * haversine.sqrt().atan2((1.0 - haversine).max(0.0).sqrt())
}
