//! The Fault Lab runner: one scenario on a sandbox cluster of twelve validators. Every protocol
//! event around the faulty window is appended to `events.jsonl` as it happens; the outcome and its
//! evidence go to `outcome.json` when the run ends.
use crate::{
    gateway::{GatewayCommand, GatewayState},
    protocol::Network,
    types::{Configuration, Fault},
};
use anyhow::Result;
use std::{io::Write, path::Path, sync::Arc};
use tokio::sync::{mpsc, RwLock};

pub async fn run(
    config: Configuration,
    fault: Fault,
    state: Arc<RwLock<GatewayState>>,
    commands: mpsc::Receiver<GatewayCommand>,
) -> Result<()> {
    anyhow::ensure!(fault.protocol!="lanes","lanes is not built yet");
    anyhow::ensure!(matches!(fault.protocol.as_str(),"today"|"agreement"),"Unknown Fault Lab protocol");
    let out = Path::new(&fault.out);
    std::fs::create_dir_all(out)?;
    let mut events = state.read().await.events.subscribe();
    let mut file = std::io::BufWriter::new(std::fs::File::create(out.join("events.jsonl"))?);
    // The window before the fault, the faulty window and the one after it.
    let slots = (fault.window - 1) * 16..(fault.window + 2) * 16;
    let writer = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(shared) => {
                    let event = &shared.0;
                    let kind = event["type"].as_str().unwrap_or_default();
                    let in_range = event["slot"].as_u64().is_some_and(|s| slots.contains(&s));
                    if !kind.starts_with("stream.") && (in_range || kind == "lab.evidence") {
                        serde_json::to_writer(&mut file, event)?;
                        file.write_all(b"\n")?;
                        file.flush()?;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        anyhow::Ok(())
    });
    let store = config.store_path.clone();
    Network::new(config, state.clone(), commands).await?.run().await?;
    drop(state);
    // Let the writer take the evidence events the run emitted last.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    writer.abort();
    let _ = writer.await;
    std::fs::remove_dir_all(store)?;
    Ok(())
}
