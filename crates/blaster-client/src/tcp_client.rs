use anyhow::Result;
use blaster_common::metrics::MetricsCollector;
use blaster_common::serde_metrics::SerdeMetrics;
use blaster_common::wire;
use std::net::SocketAddr;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

pub struct TcpResult {
    pub metrics: MetricsCollector,
    pub elapsed: std::time::Duration,
    pub encode_metrics: SerdeMetrics,
}

pub async fn blast_tcp(
    target: SocketAddr,
    payloads: Vec<(u64, Vec<u8>)>,
    barrier: std::sync::Arc<tokio::sync::Barrier>,
) -> Result<TcpResult> {
    let stream = TcpStream::connect(target).await?;
    stream.set_nodelay(true)?;

    let total = payloads.len();
    let total_bytes: u64 = payloads.iter().map(|(_, p)| p.len() as u64).sum();
    let encode_metrics = SerdeMetrics::new();

    barrier.wait().await;
    let start = Instant::now();

    send_all(stream, payloads, &encode_metrics).await?;

    let elapsed = start.elapsed();

    let mut metrics = MetricsCollector::new();
    metrics.add_bytes(total_bytes);
    let elapsed_ns = elapsed.as_nanos() as u64;
    for _ in 0..total {
        metrics.record_latency_ns(elapsed_ns / total as u64);
    }

    Ok(TcpResult {
        metrics,
        elapsed,
        encode_metrics,
    })
}

async fn send_all(
    mut stream: TcpStream,
    payloads: Vec<(u64, Vec<u8>)>,
    encode_metrics: &SerdeMetrics,
) -> Result<()> {
    for (seq_id, payload) in payloads {
        let msg = wire::encode_and_serialize(seq_id, payload, encode_metrics);
        let len = (msg.len() as u32).to_le_bytes();

        // Combine length prefix + payload into one write to avoid extra syscalls
        let mut combined = Vec::with_capacity(4 + msg.len());
        combined.extend_from_slice(&len);
        combined.extend_from_slice(&msg);
        stream.write_all(&combined).await?;
    }

    stream.flush().await?;
    Ok(())
}
