use acrux_node::{
    gateway::{run_gateway, GatewayState},
    protocol::Network,
    types::Configuration,
};
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Net {
        #[command(subcommand)]
        command: NetCommand,
    },
    /// A local cluster of hubs with their delays: `acrux cluster up --hubs FRA,NYC,TYO`.
    Cluster {
        #[command(subcommand)]
        command: ClusterCommand,
    },
    /// Writes the twelve validators' identity keypair files (from ACRUX_IDENTITY_SEED) for the
    /// Hub Registry, and prints each with its hub.
    Identities {
        #[arg(long)]
        out: String,
    },
    /// Writes a new Solana keypair file and prints its address.
    Keygen {
        #[arg(long, default_value = "acrux-key.json")]
        out: String,
    },
    /// Sends a transfer on an Acrux net, plain or as t-of-n fragments, and follows it to finality.
    Send {
        #[arg(long, default_value = "http://127.0.0.1:4930")]
        rpc: String,
        #[arg(long)]
        keypair: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        lamports: u64,
        #[arg(long, default_value_t = 1)]
        n: u8,
        #[arg(long, default_value_t = 1)]
        t: u8,
        /// Priority fee in micro-lamports per compute unit.
        #[arg(long, default_value_t = 1000)]
        priority: u64,
    },
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// The Hub Registry on Solana.
    Hub {
        #[command(subcommand)]
        command: HubCommand,
        #[arg(long, global = true, default_value = "http://127.0.0.1:4990")]
        rpc: String,
        #[arg(long, global = true, default_value = "ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm")]
        program: String,
        /// The validator identity (or registrant) keypair that signs and pays.
        #[arg(long, global = true, default_value = "acrux-key.json")]
        identity: String,
    },
    /// Algorithm 3 for a set of hubs: reads `{ epoch, seed, windows, totalStake, hubs }` as JSON on
    /// stdin and prints `{ hubStakes, schedule }`, the code path the node's own schedule runs.
    Schedule,
    /// The Fault Lab.
    Lab {
        #[command(subcommand)]
        command: LabCommand,
    },
}
#[derive(Subcommand)]
enum ClusterCommand {
    Up {
        #[arg(long, default_value = "FRA,NYC,TYO")]
        hubs: String,
        #[arg(long, default_value_t = 4930)]
        gateway_port: u16,
        #[arg(long, default_value_t = 4931)]
        ws_port: u16,
        #[arg(long, default_value_t = 4950)]
        base_port: u16,
        #[arg(long, default_value_t = 4989)]
        metrics_port: u16,
        #[arg(long, default_value = "target/runs/cluster")]
        store: String,
        #[arg(long, default_value = "../chain/genesis/acrux-net.json")]
        genesis: String,
    },
}
#[derive(Subcommand)]
enum NodeCommand {
    /// Follows a net from its latest finalized cblock: decrypts, merges and re-executes every
    /// finalized slot and checks the state hashes against the net's and its certificates.
    Follow {
        url: String,
        /// The net's genesis, for its programs.
        #[arg(long, default_value = "../chain/genesis/acrux-net.json")]
        genesis: String,
    },
}
#[derive(Subcommand)]
enum HubCommand {
    Register {
        code: String,
        #[arg(long, allow_hyphen_values = true)]
        latitude: f64,
        #[arg(long, allow_hyphen_values = true)]
        longitude: f64,
    },
    Join { code: String },
    Leave { code: String },
    Move { from: String, to: String },
    /// Releases a hub inactive past its period; anyone may send it.
    Release { code: String },
}
#[derive(Subcommand)]
enum LabCommand {
    /// Runs one scenario on a sandbox cluster; writes `events.jsonl` and `outcome.json` to `--out`.
    Run {
        #[arg(long, value_parser = ["last-look", "censorship", "crash", "equivocation", "split-commitments", "private-front-run"])]
        scenario: String,
        #[arg(long)]
        seed: String,
        /// First of the twelve sandbox QUIC ports.
        #[arg(long, default_value_t = 5016)]
        base_port: u16,
        #[arg(long)]
        out: String,
        #[arg(long, default_value = "../chain/genesis/acrux-net.json")]
        genesis: String,
    },
}
#[derive(Subcommand)]
enum NetCommand {
    Up {
        #[arg(long, env = "ACRUX_GATEWAY_PORT", default_value_t = 4930)]
        gateway_port: u16,
        #[arg(long, env = "ACRUX_WS_PORT", default_value_t = 4931)]
        ws_port: u16,
        /// First of the twelve node-to-node QUIC ports.
        #[arg(long, env = "ACRUX_BASE_PORT", default_value_t = 4950)]
        base_port: u16,
        #[arg(long, env = "ACRUX_METRICS_PORT", default_value_t = 4989)]
        metrics_port: u16,
        #[arg(long, env = "ACRUX_TRAFFIC_RATE", default_value_t = 50)]
        traffic_rate: u64,
        #[arg(long)]
        crash_window: Option<u64>,
        #[arg(long, default_value_t = 3)]
        crash_proposer: usize,
        #[arg(long)]
        run_seconds: Option<u64>,
        #[arg(long)]
        program: Vec<String>,
        /// Genesis accounts and programs; the demo pool, its mints and the faucet.
        #[arg(long, default_value = "../chain/genesis/acrux-net.json")]
        genesis: String,
        #[arg(long, default_value = "target/runs/net")]
        store: String,
        /// Start a new chain from genesis, setting aside the saved state instead of continuing it.
        #[arg(long)]
        fresh: bool,
    },
    /// Takes a running net's finalized state (0.3.7 or later) into `--store` as the saved state the
    /// next `acrux net up` there continues from.
    Import {
        url: String,
        #[arg(long, default_value = "target/runs/net")]
        store: String,
        #[arg(long, default_value = "../chain/genesis/acrux-net.json")]
        genesis: String,
    },
}
fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(8)
        .on_thread_start(acrux_node::raise_thread_priority)
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    load_local_env()?;
    #[cfg(windows)]
    unsafe {
        timeBeginPeriod(1);
        // Slot timing is measured in milliseconds; keep desktop work from preempting the net.
        SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
    if std::env::args().nth(1).as_deref() == Some("schedule-fixture") {
        acrux_node::schedule::write_schedule_fixture(std::path::Path::new(
            "fixtures/schedule/genesis.json",
        ))?;
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .json()
        .init();
    let command = Args::parse().command;
    use acrux_node::cli;
    match command {
        Command::Schedule => return schedule_preview(),
        Command::Keygen { out } => return cli::keygen(&out),
        Command::Identities { out } => return cli::identities(&out),
        Command::Send { rpc, keypair, to, lamports, n, t, priority } => return cli::send(&rpc, &keypair, &to, lamports, n, t, priority).await,
        Command::Node { command: NodeCommand::Follow { url, genesis } } => return cli::follow(&url, &genesis).await,
        Command::Net { command: NetCommand::Import { url, store, genesis } } => return cli::import(&url, &store, &genesis).await,
        Command::Hub { command, rpc, program, identity } => {
            let action = match command {
                HubCommand::Register { code, latitude, longitude } => cli::HubAction::Register { code, latitude, longitude },
                HubCommand::Join { code } => cli::HubAction::Join { code },
                HubCommand::Leave { code } => cli::HubAction::Leave { code },
                HubCommand::Move { from, to } => cli::HubAction::Move { from, to },
                HubCommand::Release { code } => cli::HubAction::Release { code },
            };
            return cli::hub(&rpc, &program, &identity, action).await;
        }
        _ => {}
    }
    let mut config = match command {
        Command::Net {
            command:
                NetCommand::Up {
                    gateway_port,
                    ws_port,
                    base_port,
                    metrics_port,
                    traffic_rate,
                    crash_window,
                    crash_proposer,
                    run_seconds,
                    store,
                    program,
                    genesis,
                    fresh,
                },
        } => {
            let saved = std::path::Path::new(&store).join("state.redb");
            if fresh && saved.exists() {
                let aside = std::path::Path::new(&store).join(format!("state-{}.redb", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs()));
                std::fs::rename(&saved, &aside)?;
                eprintln!("the saved state is set aside as {}", aside.display());
            }
            Configuration {
            gateway_port,
            websocket_port: ws_port,
            base_port,
            metrics_port,
            traffic_per_second: traffic_rate,
            crash_window,
            crash_proposer,
            run_seconds,
            store_path: store,
            genesis_programs: program,
            genesis_path: Some(genesis),
            persist: true,
            ..Default::default()
        }}
        Command::Cluster { command: ClusterCommand::Up { hubs, gateway_port, ws_port, base_port, metrics_port, store, genesis } } => {
            anyhow::ensure!(hubs.split(',').map(str::trim).collect::<Vec<_>>() == ["FRA", "NYC", "TYO"], "This build's genesis has the hubs FRA, NYC and TYO");
            Configuration { gateway_port, websocket_port: ws_port, base_port, metrics_port, store_path: store, genesis_path: Some(genesis), ..Default::default() }
        }
        Command::Net { command: NetCommand::Import { .. } } => unreachable!("handled above"),
        Command::Schedule | Command::Identities { .. } | Command::Keygen { .. } | Command::Send { .. } | Command::Node { .. } | Command::Hub { .. } => unreachable!("handled above"),
        Command::Lab { command: LabCommand::Run { scenario, seed, base_port, out, genesis } } => {
            // The faulty window is the fifth, after the net has settled; its attacker is the
            // scenario's proposer role (for censorship, the honest proposer that carries the transaction).
            let window = 4;
            let attacker = if scenario == "private-front-run" { 1 } else { 3 };
            Configuration {
                base_port,
                seed: u64::from_le_bytes(acrux_node::crypto::hash(seed.as_bytes())[..8].try_into()?),
                crash_window: (scenario == "crash").then_some(window),
                crash_proposer: attacker,
                run_seconds: Some(12),
                store_path: format!("{out}/store"),
                genesis_path: Some(genesis),
                fault: Some(acrux_node::types::Fault { scenario, window, attacker, out }),
                ..Default::default()
            }
        }
    };
    config.track_capacity_cu = std::env::var("ACRUX_BCU")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(2_000_000);
    config.inclusion_fee = std::env::var("ACRUX_INCLUSION_FEE")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(1000);
    config.base_fee = std::env::var("ACRUX_BASE_FEE")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(5000);
    config.reserve = std::env::var("ACRUX_RESERVE")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(1_000_000);
    config.fragment_expiry_slots = std::env::var("ACRUX_FRAGMENT_EXPIRY")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(8);
    config.keep_days = std::env::var("ACRUX_KEEP_DAYS").ok().map(|v| v.parse()).transpose()?.unwrap_or(7);
    config.keep_max = std::env::var("ACRUX_KEEP_MAX").ok().map(|v| v.parse()).transpose()?.unwrap_or(200_000);
    config.retain_slots = std::env::var("ACRUX_RETAIN_SLOTS")
        .ok()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(24_000);
    anyhow::ensure!(
        config.track_capacity_cu > 0 && config.reserve > 0 && config.fragment_expiry_slots > 0,
        "Capacity, reserve and fragment expiry must be positive"
    );
    anyhow::ensure!(
        (1..=3).contains(&config.crash_proposer),
        "crash-proposer must be 1, 2 or 3"
    );
    let (tx, rx) = mpsc::channel(128);
    let state = Arc::new(RwLock::new(GatewayState::new(tx)));
    if let Some(fault) = config.fault.clone() {
        return acrux_node::lab::run(config, fault, state, rx).await;
    }
    let net = Network::new(config.clone(), state.clone(), rx).await?;
    // The gateway serves the public on threads of its own, so its
    // viewers and callers never take a validator's time.
    let serving = Box::leak(Box::new(
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("gateway").enable_all().build()?,
    ));
    let mut gateway = serving.spawn(run_gateway(
        state,
        config.gateway_port,
        config.websocket_port,
        config.metrics_port,
    ));
    let result = tokio::select! {result=net.run()=>result, result=&mut gateway=>{match result{Ok(result)=>result,Err(error)=>Err(error.into())}}};
    gateway.abort();
    result
}

fn schedule_preview() -> anyhow::Result<()> {
    use acrux_node::schedule::{build_schedule, cap_stakes, Hub};
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Input {
        epoch: u64,
        seed: String,
        windows: u64,
        total_stake: u64,
        hubs: Vec<Hub>,
    }
    let input: Input = serde_json::from_reader(std::io::stdin())?;
    let hub_stakes = input
        .hubs
        .iter()
        .filter(|hub| hub.members.len() >= acrux_node::schedule::PROPOSERS)
        .map(cap_stakes)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let schedule = build_schedule(input.epoch, &input.seed, &input.hubs, input.total_stake, input.windows).ok();
    println!("{}", serde_json::json!({ "hubStakes": hub_stakes, "schedule": schedule }));
    Ok(())
}

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
}

#[cfg(windows)]
const HIGH_PRIORITY_CLASS: u32 = 0x80;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn SetPriorityClass(process: *mut std::ffi::c_void, class: u32) -> i32;
}

fn load_local_env() -> anyhow::Result<()> {
    let path = std::path::Path::new(".env");
    if !path.exists() {
        return Ok(());
    }
    for (index, line) in std::fs::read_to_string(path)?.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("Invalid .env line {}", index + 1))?;
        if std::env::var_os(key.trim()).is_none() {
            std::env::set_var(key.trim(), value.trim());
        }
    }
    Ok(())
}
