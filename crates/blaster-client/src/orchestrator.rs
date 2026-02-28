use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Barrier;

use blaster_common::datagen;

use crate::grpc_client;
use crate::quic_client;

pub struct SizeTierResult {
    pub size_name: String,
    pub size_bytes: usize,
    pub grpc: grpc_client::GrpcResult,
    pub quic: quic_client::QuicResult,
    pub grpc_first_count: u64,
    pub quic_first_count: u64,
    pub tie_count: u64,
}

pub struct BenchmarkConfig {
    pub grpc_addr: String,
    pub quic_addr: SocketAddr,
    pub sizes: Vec<(&'static str, usize)>,
    pub packets_per_size: usize,
}

pub async fn run_benchmark(config: BenchmarkConfig) -> Result<Vec<SizeTierResult>> {
    let mut results = Vec::new();

    for (size_name, size_bytes) in &config.sizes {
        tracing::info!(
            "Benchmarking {} ({} packets)...",
            size_name,
            config.packets_per_size
        );

        let payloads = datagen::generate_payloads(*size_bytes, config.packets_per_size);

        // Create (seq_id, payload) tuples — same data for both transports
        let grpc_payloads: Vec<(u64, Vec<u8>)> = payloads
            .iter()
            .enumerate()
            .map(|(i, p)| (i as u64, p.clone()))
            .collect();
        let quic_payloads = grpc_payloads.clone();

        let barrier = Arc::new(Barrier::new(2));

        let grpc_addr = config.grpc_addr.clone();
        let quic_addr = config.quic_addr;
        let grpc_barrier = barrier.clone();
        let quic_barrier = barrier;

        let grpc_handle = tokio::spawn(async move {
            grpc_client::blast_grpc(grpc_addr, grpc_payloads, grpc_barrier).await
        });

        let quic_handle = tokio::spawn(async move {
            quic_client::blast_quic(quic_addr, quic_payloads, quic_barrier).await
        });

        let (grpc_result, quic_result) = tokio::try_join!(grpc_handle, quic_handle)?;
        let grpc = grpc_result?;
        let quic = quic_result?;

        // Fetch arrival report from server to determine first-arrival winners
        let (grpc_first, quic_first, ties) =
            fetch_arrival_stats(&config.grpc_addr).await?;

        results.push(SizeTierResult {
            size_name: size_name.to_string(),
            size_bytes: *size_bytes,
            grpc,
            quic,
            grpc_first_count: grpc_first,
            quic_first_count: quic_first,
            tie_count: ties,
        });
    }

    Ok(results)
}

async fn fetch_arrival_stats(grpc_addr: &str) -> Result<(u64, u64, u64)> {
    use blaster_common::proto::blaster_service_client::BlasterServiceClient;
    use blaster_common::proto::ReportRequest;

    let mut client = BlasterServiceClient::connect(format!("http://{}", grpc_addr)).await?;
    let report = client
        .get_report(ReportRequest {
            session_id: String::new(),
        })
        .await?
        .into_inner();

    let mut grpc_first = 0u64;
    let mut quic_first = 0u64;
    let mut ties = 0u64;

    for entry in &report.arrivals {
        match (entry.grpc_arrival_ns, entry.quic_arrival_ns) {
            (0, _) => quic_first += 1,
            (_, 0) => grpc_first += 1,
            (g, q) if g < q => grpc_first += 1,
            (g, q) if q < g => quic_first += 1,
            _ => ties += 1,
        }
    }

    Ok((grpc_first, quic_first, ties))
}

pub fn print_results(results: &[SizeTierResult]) {
    println!();
    println!("╔══════════╦════════════════╦════════════════╦══════════════╦══════════════╦══════════════╗");
    println!("║ Size     ║ gRPC p50 (ms)  ║ QUIC p50 (ms)  ║ gRPC p90     ║ QUIC p90     ║ First Wins   ║");
    println!("╠══════════╬════════════════╬════════════════╬══════════════╬══════════════╬══════════════╣");

    let mut total_lost = 0u64;
    let mut total_sent = 0u64;

    for r in results {
        let total = r.grpc_first_count + r.quic_first_count + r.tie_count;
        let winner = if r.grpc_first_count > r.quic_first_count {
            format!(
                "gRPC: {:.0}%",
                r.grpc_first_count as f64 / total.max(1) as f64 * 100.0
            )
        } else {
            format!(
                "QUIC: {:.0}%",
                r.quic_first_count as f64 / total.max(1) as f64 * 100.0
            )
        };

        println!(
            "║ {:<8} ║ {:>12.3}   ║ {:>12.3}   ║ {:>10.3}   ║ {:>10.3}   ║ {:<12} ║",
            r.size_name,
            r.grpc.metrics.p50_ms(),
            r.quic.metrics.p50_ms(),
            r.grpc.metrics.p90_ms(),
            r.quic.metrics.p90_ms(),
            winner,
        );

        total_lost += r.quic.lost_packets;
        total_sent += r.quic.sent_packets;
    }

    let drop_rate = if total_sent > 0 {
        total_lost as f64 / total_sent as f64 * 100.0
    } else {
        0.0
    };

    println!("╠══════════╩════════════════╩════════════════╩══════════════╩══════════════╩══════════════╣");
    println!(
        "║ QUIC Packet Drop Rate: {:.4}% ({} lost / {} sent){:<38}║",
        drop_rate, total_lost, total_sent, ""
    );
    println!("╚═══════════════════════════════════════════════════════════════════════════════════════════╝");

    println!();
    println!("Detailed results per size:");
    println!("─────────────────────────────────────────────────────────────────");

    for r in results {
        println!();
        println!("  {} ({} packets):", r.size_name, r.grpc.metrics.count());
        println!("    gRPC:");
        println!(
            "      Latency: p50={:.3}ms  p90={:.3}ms  p99={:.3}ms  mean={:.3}ms  min={:.3}ms  max={:.3}ms",
            r.grpc.metrics.p50_ms(),
            r.grpc.metrics.p90_ms(),
            r.grpc.metrics.p99_ms(),
            r.grpc.metrics.mean_ms(),
            r.grpc.metrics.min_ms(),
            r.grpc.metrics.max_ms(),
        );
        println!(
            "      Throughput: {:.2} MB/s  ({} bytes in {:.3}s)",
            r.grpc.metrics.throughput_mbps(r.grpc.elapsed),
            r.grpc.metrics.total_bytes(),
            r.grpc.elapsed.as_secs_f64(),
        );
        println!("    QUIC:");
        println!(
            "      Latency: p50={:.3}ms  p90={:.3}ms  p99={:.3}ms  mean={:.3}ms  min={:.3}ms  max={:.3}ms",
            r.quic.metrics.p50_ms(),
            r.quic.metrics.p90_ms(),
            r.quic.metrics.p99_ms(),
            r.quic.metrics.mean_ms(),
            r.quic.metrics.min_ms(),
            r.quic.metrics.max_ms(),
        );
        println!(
            "      Throughput: {:.2} MB/s  ({} bytes in {:.3}s)",
            r.quic.metrics.throughput_mbps(r.quic.elapsed),
            r.quic.metrics.total_bytes(),
            r.quic.elapsed.as_secs_f64(),
        );
        println!(
            "      Packet drops: {} lost / {} sent ({:.4}%)",
            r.quic.lost_packets,
            r.quic.sent_packets,
            if r.quic.sent_packets > 0 {
                r.quic.lost_packets as f64 / r.quic.sent_packets as f64 * 100.0
            } else {
                0.0
            },
        );
        println!(
            "    First arrival: gRPC={} QUIC={} tie={}",
            r.grpc_first_count, r.quic_first_count, r.tie_count
        );
    }
}
