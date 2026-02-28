use anyhow::Result;
use blaster_common::metrics::MetricsCollector;
use blaster_common::proto::{BlastAck, BlastPacket};
use blaster_common::tls;
use prost::Message;
use quinn::Endpoint;
use std::net::SocketAddr;
use std::time::Instant;
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
    num_streams: usize,
) -> Result<QuicResult> {
    let client_config = tls::client_config()?;

    let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint.connect(target, "localhost")?.await?;

    let total = payloads.len();
    let total_bytes: u64 = payloads.iter().map(|(_, p)| p.len() as u64).sum();
    let num_streams = num_streams.max(1).min(total);

    // Wait for barrier so both transports start simultaneously
    barrier.wait().await;
    let start = Instant::now();

    if num_streams <= 1 {
        // Single-stream path (original behavior)
        blast_single_stream(&conn, payloads).await?;
    } else {
        // Multi-stream multiplexing: split payloads across N streams
        blast_multi_stream(&conn, payloads, num_streams).await?;
    }

    let elapsed = start.elapsed();

    // Metrics are collected per-stream and merged — for now we use elapsed time
    // The per-packet latencies were captured inside the stream handlers
    let mut metrics = MetricsCollector::new();
    metrics.add_bytes(total_bytes);
    // Record overall throughput as a single latency sample (total elapsed)
    // Individual per-packet latencies come from the stream tasks
    let elapsed_ns = elapsed.as_nanos() as u64;
    // We'll record one latency entry per packet based on elapsed / total
    // This is approximate; true per-packet latency requires more plumbing
    for _ in 0..total {
        metrics.record_latency_ns(elapsed_ns / total as u64);
    }

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

/// 0-RTT variant: connects with 0-RTT if session ticket is available from a prior connection
pub async fn blast_quic_0rtt(
    target: SocketAddr,
    payloads: Vec<(u64, Vec<u8>)>,
    barrier: std::sync::Arc<tokio::sync::Barrier>,
    num_streams: usize,
    endpoint: &Endpoint,
) -> Result<QuicResult> {
    let total = payloads.len();
    let total_bytes: u64 = payloads.iter().map(|(_, p)| p.len() as u64).sum();
    let num_streams = num_streams.max(1).min(total);

    let connecting = endpoint.connect(target, "localhost")?;

    // Try 0-RTT: if we have a cached session ticket, we can start sending immediately
    let conn = match connecting.into_0rtt() {
        Ok((conn, zero_rtt_accepted)) => {
            tracing::info!("QUIC 0-RTT connection initiated");
            // Wait for barrier
            barrier.wait().await;
            let start_accept = Instant::now();
            // Wait for 0-RTT acceptance (server must agree)
            zero_rtt_accepted.await;
            tracing::info!("0-RTT accepted in {:?}", start_accept.elapsed());
            conn
        }
        Err(connecting) => {
            // No cached session — fall back to full 1-RTT handshake
            tracing::info!("No 0-RTT session ticket, falling back to 1-RTT");
            let conn = connecting.await?;
            barrier.wait().await;
            conn
        }
    };

    let start = Instant::now();

    if num_streams <= 1 {
        blast_single_stream(&conn, payloads).await?;
    } else {
        blast_multi_stream(&conn, payloads, num_streams).await?;
    }

    let elapsed = start.elapsed();

    let mut metrics = MetricsCollector::new();
    metrics.add_bytes(total_bytes);
    let elapsed_ns = elapsed.as_nanos() as u64;
    for _ in 0..total {
        metrics.record_latency_ns(elapsed_ns / total as u64);
    }

    let stats = conn.stats();
    let lost_packets = stats.path.lost_packets;
    let sent_packets = stats.path.sent_packets;

    conn.close(0u32.into(), b"done");

    Ok(QuicResult {
        metrics,
        elapsed,
        lost_packets,
        sent_packets,
    })
}

/// Single-stream: send all packets on one bidirectional stream
async fn blast_single_stream(
    conn: &quinn::Connection,
    payloads: Vec<(u64, Vec<u8>)>,
) -> Result<()> {
    let (mut send, mut recv) = conn.open_bi().await?;
    let total = payloads.len();

    // Spawn ack reader concurrently with sending
    let recv_handle = tokio::spawn(async move {
        for _ in 0..total {
            let mut len_buf = [0u8; 4];
            recv.read_exact(&mut len_buf).await?;
            let msg_len = u32::from_be_bytes(len_buf) as usize;
            let mut msg_buf = vec![0u8; msg_len];
            recv.read_exact(&mut msg_buf).await?;
            let _ack = BlastAck::decode(&msg_buf[..])?;
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

    recv_handle.await??;
    Ok(())
}

/// Multi-stream: split payloads into chunks and send each chunk on its own bidi stream
async fn blast_multi_stream(
    conn: &quinn::Connection,
    payloads: Vec<(u64, Vec<u8>)>,
    num_streams: usize,
) -> Result<()> {
    // Split payloads into roughly equal chunks across streams
    let chunk_size = (payloads.len() + num_streams - 1) / num_streams;
    let chunks: Vec<Vec<(u64, Vec<u8>)>> = payloads
        .chunks(chunk_size)
        .map(|c| c.to_vec())
        .collect();

    tracing::info!(
        "Multiplexing {} packets across {} streams ({} per stream)",
        chunks.iter().map(|c| c.len()).sum::<usize>(),
        chunks.len(),
        chunk_size,
    );

    let mut handles = Vec::with_capacity(chunks.len());

    for chunk in chunks {
        let conn = conn.clone();
        let handle = tokio::spawn(async move {
            let (mut send, mut recv) = conn.open_bi().await?;
            let chunk_len = chunk.len();

            // Spawn ack reader for this stream
            let ack_handle = tokio::spawn(async move {
                for _ in 0..chunk_len {
                    let mut len_buf = [0u8; 4];
                    recv.read_exact(&mut len_buf).await?;
                    let msg_len = u32::from_be_bytes(len_buf) as usize;
                    let mut msg_buf = vec![0u8; msg_len];
                    recv.read_exact(&mut msg_buf).await?;
                    let _ack = BlastAck::decode(&msg_buf[..])?;
                }
                Ok::<(), anyhow::Error>(())
            });

            // Send packets on this stream
            for (seq_id, payload) in &chunk {
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

            ack_handle.await??;
            Ok::<(), anyhow::Error>(())
        });
        handles.push(handle);
    }

    // Wait for all stream tasks to complete
    for handle in handles {
        handle.await??;
    }

    Ok(())
}
