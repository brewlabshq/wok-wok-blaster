use anyhow::Result;
use quinn::Endpoint;
use std::net::SocketAddr;

use blaster_common::tls;
use blaster_common::wire::{self, MAX_STREAM_SIZE};

use crate::tracker::{ArrivalTracker, Transport};

pub async fn run_quic_server(addr: SocketAddr, tracker: ArrivalTracker) -> Result<()> {
    let cert_pair = tls::generate_self_signed()?;
    let server_config = tls::server_config(&cert_pair)?;

    let socket = std::net::UdpSocket::bind(addr)?;
    let _ = socket.set_nonblocking(true);
    // 8 MB socket buffers — prevents kernel-level drops under high throughput
    tls::enlarge_socket_buffers(&socket, 8 * 1024 * 1024);
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("no async runtime"))?;
    let endpoint = Endpoint::new(
        tls::blaster_endpoint_config(),
        Some(server_config),
        socket,
        runtime,
    )?;
    tracing::info!("QUIC server listening on {}", addr);

    while let Some(incoming) = endpoint.accept().await {
        let tracker = tracker.clone();
        let conn = incoming.accept()?;
        let connection = conn.await.unwrap();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(connection, tracker).await {
                tracing::error!("QUIC connection error: {}", e);
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    conn: quinn::Connection,
    tracker: ArrivalTracker,
) -> Result<()> {
    tracing::info!("QUIC connection from {}", conn.remote_address());

    loop {
        let stream = conn.accept_uni().await;
        match stream {
            Ok(recv_stream) => {
                let tracker = tracker.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_stream(recv_stream, tracker).await {
                        tracing::warn!("QUIC stream ended: {}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) => {
                tracing::debug!("QUIC connection closed by client");
                break;
            }
            Err(e) => {
                tracing::warn!("QUIC accept_uni ended: {}", e);
                break;
            }
        }
    }

    Ok(())
}

/// Each uni stream carries exactly one bincode-encoded WireBlastPacket.
/// Read the entire stream (up to 24 MiB) in one shot and deserialize.
async fn handle_stream(
    mut recv: quinn::RecvStream,
    tracker: ArrivalTracker,
) -> Result<()> {
    let buf = recv.read_to_end(MAX_STREAM_SIZE).await?;
    let packet = wire::deserialize_packet(&buf, &tracker.quic_decode_metrics)?;
    tracker.record(Transport::Quic, packet.sequence_id, packet.payload_size);
    Ok(())
}
