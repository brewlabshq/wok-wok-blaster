use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Barrier;

use blaster_common::datagen;
use blaster_common::proto::SerdeStatsProto;
use blaster_common::serde_metrics::SerdeStats;

use crate::grpc_client;
use crate::quic_client;
use crate::tcp_client;

pub struct SizeTierResult {
    pub size_name: String,
    pub size_bytes: usize,
    pub grpc: grpc_client::GrpcResult,
    pub quic: quic_client::QuicResult,
    pub tcp: tcp_client::TcpResult,
    pub grpc_first_count: u64,
    pub quic_first_count: u64,
    pub tcp_first_count: u64,
    pub tie_count: u64,
    pub quic_decode_stats: Option<SerdeStats>,
    pub tcp_decode_stats: Option<SerdeStats>,
}

pub struct BenchmarkConfig {
    pub grpc_addr: String,
    pub quic_addr: SocketAddr,
    pub tcp_addr: SocketAddr,
    pub sizes: Vec<(&'static str, usize)>,
    pub packets_per_size: usize,
    pub quic_streams: usize,
    pub zero_rtt: bool,
}

struct ArrivalStatsResult {
    grpc_first: u64,
    quic_first: u64,
    tcp_first: u64,
    ties: u64,
    quic_decode_stats: Option<SerdeStats>,
    tcp_decode_stats: Option<SerdeStats>,
}

pub async fn run_benchmark(config: BenchmarkConfig) -> Result<Vec<SizeTierResult>> {
    let mut results = Vec::new();

    // Create a persistent QUIC endpoint that survives across size tiers.
    // This lets session tickets accumulate so 0-RTT works on the 2nd+ tier.
    let client_config = blaster_common::tls::client_config()?;
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.set_nonblocking(true)?;
    blaster_common::tls::enlarge_socket_buffers(&socket, 8 * 1024 * 1024);
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("no async runtime"))?;
    let mut endpoint = quinn::Endpoint::new(
        blaster_common::tls::blaster_endpoint_config(),
        None,
        socket,
        runtime,
    )?;
    endpoint.set_default_client_config(client_config);

    // If 0-RTT is enabled, do a warmup handshake to get a session ticket
    if config.zero_rtt {
        tracing::info!("0-RTT warmup: establishing initial connection to get session ticket...");
        let warmup_conn = endpoint.connect(config.quic_addr, "localhost")?.await?;
        // Send a tiny sentinel packet on a uni stream to complete the handshake
        let mut send = warmup_conn.open_uni().await?;
        let dummy_metrics = blaster_common::serde_metrics::SerdeMetrics::new();
        let msg = blaster_common::wire::encode_and_serialize(u64::MAX, vec![0u8], &dummy_metrics);
        send.write_all(&msg).await?;
        send.finish()?;
        // Close warmup connection — session ticket is now cached in the endpoint
        warmup_conn.close(0u32.into(), b"warmup");
        endpoint.wait_idle().await;
        tracing::info!("0-RTT warmup complete — session ticket cached");
    }

    for (size_name, size_bytes) in &config.sizes {
        tracing::info!(
            "Benchmarking {} ({} packets, {} streams)...",
            size_name,
            config.packets_per_size,
            config.quic_streams,
        );

        let payloads = datagen::generate_payloads(*size_bytes, config.packets_per_size);

        // Create (seq_id, payload) tuples — same data for all transports
        let grpc_payloads: Vec<(u64, Vec<u8>)> = payloads
            .iter()
            .enumerate()
            .map(|(i, p)| (i as u64, p.clone()))
            .collect();
        let quic_payloads = grpc_payloads.clone();
        let tcp_payloads = grpc_payloads.clone();

        // Barrier for 3 transports
        let barrier = Arc::new(Barrier::new(3));

        let grpc_addr = config.grpc_addr.clone();
        let grpc_barrier = barrier.clone();
        let quic_barrier = barrier.clone();
        let tcp_barrier = barrier;

        let grpc_handle = tokio::spawn(async move {
            grpc_client::blast_grpc(grpc_addr, grpc_payloads, grpc_barrier).await
        });

        let quic_streams = config.quic_streams;
        let quic_addr = config.quic_addr;
        let zero_rtt = config.zero_rtt;
        let endpoint_clone = endpoint.clone();
        let quic_handle = tokio::spawn(async move {
            if zero_rtt {
                quic_client::blast_quic_0rtt(
                    quic_addr,
                    quic_payloads,
                    quic_barrier,
                    quic_streams,
                    &endpoint_clone,
                )
                .await
            } else {
                quic_client::blast_quic(quic_addr, quic_payloads, quic_barrier, quic_streams).await
            }
        });

        let tcp_addr = config.tcp_addr;
        let tcp_handle = tokio::spawn(async move {
            tcp_client::blast_tcp(tcp_addr, tcp_payloads, tcp_barrier).await
        });

        let (grpc_result, quic_result, tcp_result) =
            tokio::try_join!(grpc_handle, quic_handle, tcp_handle)?;
        let grpc = grpc_result?;
        let quic = quic_result?;
        let tcp = tcp_result?;

        // Fetch arrival report from server to determine first-arrival winners
        let stats = fetch_arrival_stats(&config.grpc_addr).await?;

        results.push(SizeTierResult {
            size_name: size_name.to_string(),
            size_bytes: *size_bytes,
            grpc,
            quic,
            tcp,
            grpc_first_count: stats.grpc_first,
            quic_first_count: stats.quic_first,
            tcp_first_count: stats.tcp_first,
            tie_count: stats.ties,
            quic_decode_stats: stats.quic_decode_stats,
            tcp_decode_stats: stats.tcp_decode_stats,
        });
    }

    Ok(results)
}

fn proto_to_stats(proto: &SerdeStatsProto) -> SerdeStats {
    SerdeStats {
        count: proto.count as usize,
        median_ns: proto.median_ns,
        min_ns: proto.min_ns,
        max_ns: proto.max_ns,
        mean_ns: proto.mean_ns,
    }
}

async fn fetch_arrival_stats(grpc_addr: &str) -> Result<ArrivalStatsResult> {
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
    let mut tcp_first = 0u64;
    let mut ties = 0u64;

    for entry in &report.arrivals {
        let times: Vec<(&str, u64)> = [
            ("grpc", entry.grpc_arrival_ns),
            ("quic", entry.quic_arrival_ns),
            ("tcp", entry.tcp_arrival_ns),
        ]
        .into_iter()
        .filter(|(_, t)| *t > 0)
        .collect();

        if times.is_empty() {
            continue;
        }

        let min_time = times.iter().map(|(_, t)| *t).min().unwrap();
        let winners: Vec<&str> = times
            .iter()
            .filter(|(_, t)| *t == min_time)
            .map(|(name, _)| *name)
            .collect();

        if winners.len() > 1 {
            ties += 1;
        } else {
            match winners[0] {
                "grpc" => grpc_first += 1,
                "quic" => quic_first += 1,
                "tcp" => tcp_first += 1,
                _ => {}
            }
        }
    }

    Ok(ArrivalStatsResult {
        grpc_first,
        quic_first,
        tcp_first,
        ties,
        quic_decode_stats: report.quic_decode_stats.as_ref().map(proto_to_stats),
        tcp_decode_stats: report.tcp_decode_stats.as_ref().map(proto_to_stats),
    })
}

fn print_serde_line(label: &str, stats: &SerdeStats) {
    println!(
        "      {} ({} packets): median={:.1}us  min={:.1}us  max={:.1}us  mean={:.1}us",
        label,
        stats.count,
        stats.median_us(),
        stats.min_us(),
        stats.max_us(),
        stats.mean_us(),
    );
}

pub fn print_results(results: &[SizeTierResult]) {
    println!();
    println!("╔══════════╦════════════════╦════════════════╦═══════════════╦══════════════╦══════════════╦══════════════╦══════════════╗");
    println!("║ Size     ║ gRPC p50 (ms)  ║ QUIC p50 (ms)  ║ TCP p50 (ms)  ║ gRPC p90     ║ QUIC p90     ║ TCP p90      ║ First Wins   ║");
    println!("╠══════════╬════════════════╬════════════════╬═══════════════╬══════════════╬══════════════╬══════════════╬══════════════╣");

    let mut total_lost = 0u64;
    let mut total_sent = 0u64;

    for r in results {
        let total = r.grpc_first_count + r.quic_first_count + r.tcp_first_count + r.tie_count;
        let counts = [
            ("gRPC", r.grpc_first_count),
            ("QUIC", r.quic_first_count),
            ("TCP", r.tcp_first_count),
        ];
        let (winner_name, winner_count) = counts.iter().max_by_key(|(_, c)| c).unwrap();
        let winner = format!(
            "{}: {:.0}%",
            winner_name,
            *winner_count as f64 / total.max(1) as f64 * 100.0
        );

        println!(
            "║ {:<8} ║ {:>12.3}   ║ {:>12.3}   ║ {:>11.3}   ║ {:>10.3}   ║ {:>10.3}   ║ {:>10.3}   ║ {:<12} ║",
            r.size_name,
            r.grpc.metrics.p50_ms(),
            r.quic.metrics.p50_ms(),
            r.tcp.metrics.p50_ms(),
            r.grpc.metrics.p90_ms(),
            r.quic.metrics.p90_ms(),
            r.tcp.metrics.p90_ms(),
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

    println!("╠══════════╩════════════════╩════════════════╩═══════════════╩══════════════╩══════════════╩══════════════╩══════════════╣");
    println!(
        "║ QUIC Packet Drop Rate: {:.4}% ({} lost / {} sent){:<55}║",
        drop_rate, total_lost, total_sent, ""
    );
    println!("╚════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════╝");

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
        let quic_encode = r.quic.encode_metrics.stats();
        println!("      Serde:");
        print_serde_line("Encode", &quic_encode);
        if let Some(ref decode) = r.quic_decode_stats {
            print_serde_line("Decode", decode);
        }
        println!("    TCP:");
        println!(
            "      Latency: p50={:.3}ms  p90={:.3}ms  p99={:.3}ms  mean={:.3}ms  min={:.3}ms  max={:.3}ms",
            r.tcp.metrics.p50_ms(),
            r.tcp.metrics.p90_ms(),
            r.tcp.metrics.p99_ms(),
            r.tcp.metrics.mean_ms(),
            r.tcp.metrics.min_ms(),
            r.tcp.metrics.max_ms(),
        );
        println!(
            "      Throughput: {:.2} MB/s  ({} bytes in {:.3}s)",
            r.tcp.metrics.throughput_mbps(r.tcp.elapsed),
            r.tcp.metrics.total_bytes(),
            r.tcp.elapsed.as_secs_f64(),
        );
        let tcp_encode = r.tcp.encode_metrics.stats();
        println!("      Serde:");
        print_serde_line("Encode", &tcp_encode);
        if let Some(ref decode) = r.tcp_decode_stats {
            print_serde_line("Decode", decode);
        }
        println!(
            "    First arrival: gRPC={} QUIC={} TCP={} tie={}",
            r.grpc_first_count, r.quic_first_count, r.tcp_first_count, r.tie_count
        );
    }
}
