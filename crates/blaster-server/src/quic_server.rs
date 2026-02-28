use anyhow::Result;
use prost::Message;
use quinn::Endpoint;
use std::net::SocketAddr;

use blaster_common::proto::{BlastAck, BlastPacket};
use blaster_common::tls;

use crate::tracker::{now_ns, ArrivalTracker, Transport};

pub async fn run_quic_server(addr: SocketAddr, tracker: ArrivalTracker) -> Result<()> {
    let cert_pair = tls::generate_self_signed()?;
    let server_config = tls::server_config(&cert_pair)?;

    // Bind UDP socket with large receive buffer
    let socket = std::net::UdpSocket::bind(addr)?;
    // Try to set 8MB receive buffer (OS may cap this lower)
    let _ = socket.set_nonblocking(true);
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("no async runtime"))?;
    let endpoint = Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        runtime,
    )?;
    tracing::info!("QUIC server listening on {}", addr);

    while let Some(incoming) = endpoint.accept().await {
        let tracker = tracker.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(incoming, tracker).await {
                tracing::error!("QUIC connection error: {}", e);
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    incoming: quinn::Incoming,
    tracker: ArrivalTracker,
) -> Result<()> {
    let conn = incoming.await?;
    tracing::info!("QUIC connection from {}", conn.remote_address());

    loop {
        let stream = conn.accept_bi().await;
        match stream {
            Ok((send, recv)) => {
                let tracker = tracker.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_stream(send, recv, tracker).await {
                        tracing::warn!("QUIC stream ended: {}", e);
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) => {
                tracing::debug!("QUIC connection closed by client");
                break;
            }
            Err(e) => {
                tracing::warn!("QUIC accept_bi ended: {}", e);
                break;
            }
        }
    }

    Ok(())
}

async fn handle_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    tracker: ArrivalTracker,
) -> Result<()> {
    loop {
        // Read 4-byte length prefix
        let mut len_buf = [0u8; 4];
        match recv.read_exact(&mut len_buf).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(_)) => break,
            Err(quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(_))) => break,
            Err(e) => return Err(e.into()),
        }
        let msg_len = u32::from_be_bytes(len_buf) as usize;

        // Read the message
        let mut msg_buf = vec![0u8; msg_len];
        match recv.read_exact(&mut msg_buf).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(_)) => break,
            Err(quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(_))) => break,
            Err(e) => return Err(e.into()),
        }

        let packet = BlastPacket::decode(&msg_buf[..])?;
        tracker.record(Transport::Quic, packet.sequence_id, packet.payload_size);

        // Send ack back
        let ack = BlastAck {
            sequence_id: packet.sequence_id,
            received_at_ns: now_ns(),
        };
        let ack_bytes = ack.encode_to_vec();
        let ack_len = (ack_bytes.len() as u32).to_be_bytes();
        if send.write_all(&ack_len).await.is_err() {
            break;
        }
        if send.write_all(&ack_bytes).await.is_err() {
            break;
        }
    }

    Ok(())
}
