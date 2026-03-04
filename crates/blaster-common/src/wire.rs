use serde::{Deserialize, Serialize};

/// Payloads at or above this size get lz4-compressed before sending.
pub const COMPRESSION_THRESHOLD: usize = 500 * 1024; // 500 KB

/// Max bytes we'll accept from a single uni stream (128 MiB).
pub const MAX_STREAM_SIZE: usize = 128 * 1024 * 1024;

/// Tiny fixed-size header — only this part goes through bincode.
/// Payload bytes follow immediately after, avoiding serde overhead on bulk data.
#[derive(Serialize, Deserialize)]
struct WireHeader {
    sequence_id: u64,
    timestamp_ns: u64,
    /// Original (uncompressed) payload size.
    payload_size: u32,
    /// If true, the trailing payload bytes are lz4-compressed.
    compressed: bool,
}

/// Bincode-serialized size of WireHeader: u64(8) + u64(8) + u32(4) + bool(1) = 21 bytes.
const HEADER_SIZE: usize = 21;

/// Decoded packet returned to callers.
pub struct WireBlastPacket {
    pub sequence_id: u64,
    pub timestamp_ns: u64,
    pub payload_size: u32,
    pub compressed: bool,
    pub payload: Vec<u8>,
}

/// Encode a packet to wire bytes: [21-byte bincode header][raw payload bytes].
/// Compresses payload with lz4 if it exceeds the threshold.
/// Records total encode time to metrics.
pub fn encode_and_serialize(
    seq_id: u64,
    payload: Vec<u8>,
    metrics: &crate::serde_metrics::SerdeMetrics,
) -> Vec<u8> {
    let start = std::time::Instant::now();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;

    let original_size = payload.len() as u32;

    let (data, compressed) = if payload.len() >= COMPRESSION_THRESHOLD {
        (lz4_flex::compress_prepend_size(&payload), true)
    } else {
        (payload, false)
    };

    let header = WireHeader {
        sequence_id: seq_id,
        timestamp_ns: now,
        payload_size: original_size,
        compressed,
    };

    let header_bytes = bincode::serialize(&header).expect("bincode serialize header");
    debug_assert_eq!(header_bytes.len(), HEADER_SIZE);

    let mut out = Vec::with_capacity(header_bytes.len() + data.len());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(&data);

    metrics.record(start.elapsed().as_nanos() as u64);
    out
}

/// Deserialize a packet from wire bytes: [21-byte header][raw payload].
/// Records deserialize time to metrics.
pub fn deserialize_packet(
    buf: &[u8],
    metrics: &crate::serde_metrics::SerdeMetrics,
) -> anyhow::Result<WireBlastPacket> {
    let start = std::time::Instant::now();

    anyhow::ensure!(
        buf.len() >= HEADER_SIZE,
        "wire buffer too small: {} < {}",
        buf.len(),
        HEADER_SIZE
    );

    let header: WireHeader = bincode::deserialize(&buf[..HEADER_SIZE])?;
    let payload = buf[HEADER_SIZE..].to_vec();

    metrics.record(start.elapsed().as_nanos() as u64);

    Ok(WireBlastPacket {
        sequence_id: header.sequence_id,
        timestamp_ns: header.timestamp_ns,
        payload_size: header.payload_size,
        compressed: header.compressed,
        payload,
    })
}
