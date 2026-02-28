use anyhow::Result;
use blaster_common::metrics::MetricsCollector;
use blaster_common::proto::{BlastAck, BlastPacket};
use blaster_common::tls;
use prost::Message;
use quinn::Endpoint;
use std::net::SocketAddr;
use std::time::Instant;
use tokio::sync::mpsc;

pub struct QuicResult {
    pub metrics: MetricsCollector,
    pub elapsed: std::time::Duration,
    pub lost_packets: u64,
    pub sent_packets: u64,
}

pub async fn blast_quic(
    target: SocketAddr,
    payloads: Vec<(u64, Vec<u8>)>,
    barrier: std::sync::Arc<tokio::sync::Barrier>,
) -> Result<QuicResult> {
    let client_config = tls::client_config()?;

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint.connect(target, "localhost")?.await?;
    let (mut send, mut recv) = conn.open_bi().await?;

    let total = payloads.len();
    let total_bytes: u64 = payloads.iter().map(|(_, p)| p.len() as u64).sum();

    // Wait for barrier so both transports start simultaneously
    barrier.wait().await;
    let start = Instant::now();

    // Spawn ack reader concurrently with sending — this prevents flow control deadlock
    // where send blocks waiting for window, but window updates come via acks we're not reading
    let (ack_tx, mut ack_rx) = mpsc::channel::<u64>(1024);
    let recv_total = total;
    let recv_handle = tokio::spawn(async move {
        for _ in 0..recv_total {
            let mut len_buf = [0u8; 4];
            recv.read_exact(&mut len_buf).await?;
            let msg_len = u32::from_be_bytes(len_buf) as usize;

            let mut msg_buf = vec![0u8; msg_len];
            recv.read_exact(&mut msg_buf).await?;

            let ack = BlastAck::decode(&msg_buf[..])?;
            if ack_tx.send(ack.received_at_ns).await.is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    // Send all packets
    for (seq_id, payload) in &payloads {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        let packet = BlastPacket {
            sequence_id: *seq_id,
            timestamp_ns: now,
            payload_size: payload.len() as u32,
            payload: payload.clone(),
        };

        let msg = packet.encode_to_vec();
        let len = (msg.len() as u32).to_be_bytes();
        send.write_all(&len).await?;
        send.write_all(&msg).await?;
    }

    // Wait for all acks
    recv_handle.await??;

    let elapsed = start.elapsed();

    // Collect latencies from ack channel
    let mut metrics = MetricsCollector::new();
    while let Ok(received_at_ns) = ack_rx.try_recv() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let latency_ns = now.saturating_sub(received_at_ns);
        metrics.record_latency_ns(latency_ns);
    }
    metrics.add_bytes(total_bytes);

    // Get connection stats for packet drop info
    let stats = conn.stats();
    let lost_packets = stats.path.lost_packets;
    let sent_packets = stats.path.sent_packets;

    // Close cleanly
    conn.close(0u32.into(), b"done");
    endpoint.wait_idle().await;

    Ok(QuicResult {
        metrics,
        elapsed,
        lost_packets,
        sent_packets,
    })
}
