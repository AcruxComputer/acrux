//! Local wire probes for propose-observer-check.mjs. These do not send protocol blocks/votes.
use acrux_transport::external::{
    authentication_message, open_client, pinned_client_config, read_server_frame,
    write_client_frame, Authentication, ClientFrame, ServerFrame,
};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::Parser;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use solana_sdk::signature::Keypair;
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
struct Args {
    #[arg(long, value_parser = ["bad-signature", "slow-reader"])]
    mode: String,
    #[arg(long)]
    net: String,
    #[arg(long)]
    hub: String,
    #[arg(long)]
    identity: PathBuf,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let keypair: Vec<u8> = serde_json::from_slice(&std::fs::read(&args.identity)?)?;
    ensure!(keypair.len() == 64, "Identity must be a Solana keypair JSON");
    let identity = SigningKey::from_bytes(keypair[..32].try_into()?);
    ensure!(identity.verifying_key().as_bytes() == &keypair[32..], "Identity public key differs from its seed");
    let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?;
    let response: Value = http.post(&args.net).json(&json!({"jsonrpc":"2.0","id":1,"method":"getHubPeers","params":{"hub":args.hub}}))
        .send().await?.error_for_status()?.json().await?;
    ensure!(response.get("error").is_none(), "Hub peer discovery failed");
    let peers = &response["result"];
    ensure!(peers["hub"].as_str() == Some(args.hub.as_str()), "Hub peer discovery returned another hub");
    let address = peers["address"].as_str().context("Missing peer address")?.parse()?;
    let certificate = STANDARD.decode(peers["certificate"].as_str().context("Missing peer certificate")?)?;
    let finalized: Value = http.post(&args.net).json(&json!({"jsonrpc":"2.0","id":2,"method":"getFinalizedCblock","params":{}}))
        .send().await?.error_for_status()?.json().await?;
    let from_slot = finalized["result"]["consensusSlot"].as_u64().context("No finalized cblock for the probe")?
        .checked_add(1).and_then(|next|next.checked_mul(4)).context("Probe cursor overflow")?;
    let mut client_config = pinned_client_config(&certificate)?;
    if args.mode == "slow-reader" {
        // This peer deliberately grants a tiny receive window. Authentication fits, while an
        // unread slot payload must exercise the server's ordinary bounded-write deadline.
        let mut transport = quinn::TransportConfig::default();
        transport.stream_receive_window(4096u32.into());
        transport.receive_window(8192u32.into());
        transport.max_concurrent_uni_streams(1u32.into());
        transport.max_idle_timeout(Some(Duration::from_secs(60).try_into()?));
        transport.keep_alive_interval(Some(Duration::from_secs(5)));
        client_config.transport_config(Arc::new(transport));
    }
    let mut raw = open_client(address,client_config).await?;
    let challenge = match tokio::time::timeout(Duration::from_secs(10),read_server_frame(&mut raw.recv)).await?? {
        ServerFrame::Challenge(challenge) => challenge,
        _ => bail!("Server did not challenge the new identity"),
    };
    let public = identity.verifying_key().to_bytes();
    let message = authentication_message(&args.hub,&public,&challenge,from_slot);
    let signature = if args.mode == "bad-signature" {
        let wrong = Keypair::new().to_bytes();
        SigningKey::from_bytes(wrong[..32].try_into()?).sign(&message).to_bytes()
    } else { identity.sign(&message).to_bytes() };
    write_client_frame(&mut raw.send,&ClientFrame::Authenticate(Authentication {
        hub:args.hub,identity:public,from_slot,signature,
    })).await?;
    if args.mode == "bad-signature" {
        let response = tokio::time::timeout(Duration::from_secs(10),read_server_frame(&mut raw.recv)).await
            .context("Server did not refuse the invalid challenge signature")?;
        ensure!(response.is_err(), "Server accepted a challenge signed by another identity");
        let reason = tokio::time::timeout(Duration::from_secs(5),raw.connection.closed()).await?;
        match &reason {
            quinn::ConnectionError::ApplicationClosed(close) => ensure!(close.error_code == 1u32.into()
                && close.reason.as_ref() == b"observer identity proof rejected", "Invalid proof was not refused by authentication"),
            _ => bail!("Invalid proof ended through a transport failure: {reason}"),
        }
        println!("{}",json!({"event":"propose_probe_bad_signature_refused","reason":reason.to_string()}));
    } else {
        match tokio::time::timeout(Duration::from_secs(10),read_server_frame(&mut raw.recv)).await?? {
            ServerFrame::Accepted => {},
            _ => bail!("Server did not authenticate the slow peer"),
        }
        println!("{}",json!({"event":"propose_probe_authenticated"}));
        // Keep both streams and the endpoint alive, but never accept or read a payload stream.
        // QUIC keepalive prevents mistaking idle expiry for the server's slow-reader protection.
        let began = std::time::Instant::now();
        let reason = tokio::time::timeout(Duration::from_secs(30),raw.connection.closed()).await
            .context("Authenticated stopped-reading peer was not disconnected")?;
        match &reason {
            quinn::ConnectionError::ApplicationClosed(close) => ensure!(close.error_code == 2u32.into()
                && matches!(close.reason.as_ref(), b"observer queue limit exceeded" | b"observer write deadline exceeded"),
                "Slow peer was not dropped by bounded backpressure"),
            _ => bail!("Slow peer ended through a transport failure: {reason}"),
        }
        println!("{}",json!({"event":"propose_probe_dropped","elapsedMs":began.elapsed().as_millis(),"reason":reason.to_string()}));
    }
    raw.endpoint.close(0u32.into(),b"probe complete");
    raw.endpoint.wait_idle().await;
    Ok(())
}
