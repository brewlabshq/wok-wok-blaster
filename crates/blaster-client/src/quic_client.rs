use anyhow::Result;
use blaster_common::metrics::MetricsCollector;
use blaster_common::serde_metrics::SerdeMetrics;
use blaster_common::tls;
use blaster_common::wire;
use quinn::Endpoint;
use std::net::SocketAddr;
use std::time::Instant;

pub struct QuicResult {
    pub metrics: MetricsCollector,
    pub elapsed: std::time::Duration,
    pub lost_packets: u64,
    pub sent_packets: u64,
    pub encode_metrics: SerdeMetrics,
}

pub async fn blast_quic(
    target: SocketAddr,
    payloads: Vec<(u64, Vec<u8>)>,
    barrier: std::sync::Arc<tokio::sync::Barrier>,
    num_streams: usize,
) -> Result<QuicResult> {
    let client_config = tls::client_config()?;

    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.set_nonblocking(true)?;
    tls::enlarge_socket_buffers(&socket, 8 * 1024 * 1024);
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("no async runtime"))?;
    let mut endpoint = Endpoint::new(
        tls::blaster_endpoint_config(),
        None,
        socket,
        runtime,
    )?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint.connect(target, "localhost")?.await?;

    let total = payloads.len();
    let total_bytes: u64 = payloads.iter().map(|(_, p)| p.len() as u64).sum();
    let num_streams = num_streams.max(1).min(total);
    let encode_metrics = SerdeMetrics::new();

    barrier.wait().await;
    let start = Instant::now();

    if num_streams <= 1 {
        blast_single_stream(&conn, payloads, &encode_metrics).await?;
    } else {
        blast_multi_stream(&conn, payloads, num_streams, &encode_metrics).await?;
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
    endpoint.wait_idle().await;

    Ok(QuicResult {
        metrics,
        elapsed,
        lost_packets,
        sent_packets,
        encode_metrics,
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
    let encode_metrics = SerdeMetrics::new();

    let connecting = endpoint.connect(target, "localhost")?;

    let conn = match connecting.into_0rtt() {
        Ok((conn, zero_rtt_accepted)) => {
            tracing::info!("QUIC 0-RTT connection initiated");
            barrier.wait().await;
            let start_accept = Instant::now();
            zero_rtt_accepted.await;
            tracing::info!("0-RTT accepted in {:?}", start_accept.elapsed());
            conn
        }
        Err(connecting) => {
            tracing::info!("No 0-RTT session ticket, falling back to 1-RTT");
            let conn = connecting.await?;
            barrier.wait().await;
            conn
        }
    };

    let start = Instant::now();

    if num_streams <= 1 {
        blast_single_stream(&conn, payloads, &encode_metrics).await?;
    } else {
        blast_multi_stream(&conn, payloads, num_streams, &encode_metrics).await?;
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
        encode_metrics,
    })
}

/// One uni stream per packet. The stream IS the delimiter —
/// write bincode bytes, finish the stream, no header needed.
async fn blast_single_stream(
    conn: &quinn::Connection,
    payloads: Vec<(u64, Vec<u8>)>,
    encode_metrics: &SerdeMetrics,
) -> Result<()> {
    for (seq_id, payload) in payloads {
        let mut send = conn.open_uni().await?;
        let msg = wire::encode_and_serialize(seq_id, payload, encode_metrics);
        send.write_all(&msg).await?;
        send.finish()?;
    }

    Ok(())
}

/// Multi-stream: split payloads across N concurrent tasks, each opening
/// uni streams for its chunk of packets.
async fn blast_multi_stream(
    conn: &quinn::Connection,
    payloads: Vec<(u64, Vec<u8>)>,
    num_streams: usize,
    encode_metrics: &SerdeMetrics,
) -> Result<()> {
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
        let em = encode_metrics.clone();
        let handle = tokio::spawn(async move {
            for (seq_id, payload) in chunk {
                let mut send = conn.open_uni().await?;
                let msg = wire::encode_and_serialize(seq_id, payload, &em);
                send.write_all(&msg).await?;
                send.finish()?;
            }
            Ok::<(), anyhow::Error>(())
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.await??;
    }

    Ok(())
}
