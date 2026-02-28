use anyhow::Result;
use blaster_common::metrics::MetricsCollector;
use blaster_common::proto::blaster_service_client::BlasterServiceClient;
use blaster_common::proto::BlastPacket;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub struct GrpcResult {
    pub metrics: MetricsCollector,
    pub elapsed: std::time::Duration,
}

pub async fn blast_grpc(
    addr: String,
    payloads: Vec<(u64, Vec<u8>)>,
    barrier: std::sync::Arc<tokio::sync::Barrier>,
) -> Result<GrpcResult> {
    let mut client = BlasterServiceClient::connect(format!("http://{}", addr))
        .await?
        .max_decoding_message_size(64 * 1024 * 1024)
        .max_encoding_message_size(64 * 1024 * 1024);
    let mut metrics = MetricsCollector::new();

    let (tx, rx) = mpsc::channel(1024);
    let stream = ReceiverStream::new(rx);

    let response = client.blast_stream(stream).await?;
    let mut ack_stream = response.into_inner();

    let total_count = payloads.len();
    let total_bytes: u64 = payloads.iter().map(|(_, p)| p.len() as u64).sum();

    // Wait for barrier so both transports start simultaneously
    barrier.wait().await;
    let start = Instant::now();

    // Spawn sender
    let send_handle = tokio::spawn(async move {
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
            if tx.send(packet).await.is_err() {
                break;
            }
        }
    });

    // Collect acks concurrently with sending
    let mut ack_count = 0u64;
    while let Some(ack) = ack_stream.message().await? {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let latency_ns = now.saturating_sub(ack.received_at_ns);
        metrics.record_latency_ns(latency_ns);
        ack_count += 1;
        if ack_count >= total_count as u64 {
            break;
        }
    }

    send_handle.await?;
    let elapsed = start.elapsed();
    metrics.add_bytes(total_bytes);

    Ok(GrpcResult { metrics, elapsed })
}
