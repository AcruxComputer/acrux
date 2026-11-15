use acrux_node::transport::{LocalTransport,Peer};
#[tokio::main(flavor="multi_thread",worker_threads=4)]
async fn main()->anyhow::Result<()> {
 tracing_subscriber::fmt().with_env_filter("info").init();
 let peers=(0..2).map(|id|Peer{id,hub:"FRA".into(),latitude:50.0,longitude:8.0,stake:1}).collect();
 let (network,mut receiver)=LocalTransport::bind(peers,5037,42,acrux_node::profiling::Profiling::new()).await?;
 for from in 0..2 {for to in 0..2 {network.send(from,to,vec![from as u8,to as u8,99]).await?;}}
 for _ in 0..4 {let packet=tokio::time::timeout(std::time::Duration::from_secs(3),receiver.recv()).await?.unwrap();println!("{} -> {} {:?}",packet.from,packet.to,packet.payload);}
 network.close().await;Ok(())
}
