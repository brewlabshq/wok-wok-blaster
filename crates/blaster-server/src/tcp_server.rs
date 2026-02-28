use anyhow::Result;
use std::net::SocketAddr;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use blaster_common::wire;

use crate::tracker::{ArrivalTracker, Transport};

pub async fn run_tcp_server(addr: SocketAddr, tracker: ArrivalTracker) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("TCP server listening on {}", addr);

    loop {
        let (stream, peer) = listener.accept().await?;
        stream.set_nodelay(true)?;
        let tracker = tracker.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, tracker).await {
                tracing::debug!("TCP connection from {} ended: {}", peer, e);
            }
        });
    }
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    tracker: ArrivalTracker,
) -> Result<()> {
    // Wire format: [4-byte little-endian length][bincode payload]
    // Minimal framing for lowest overhead.
    let mut len_buf = [0u8; 4];
    loop {
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;
        let packet = wire::deserialize_packet(&buf, &tracker.tcp_decode_metrics)?;
        tracker.record(Transport::Tcp, packet.sequence_id, packet.payload_size);
    }
    Ok(())
}
