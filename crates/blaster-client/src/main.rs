mod grpc_client;
mod orchestrator;
mod quic_client;
mod tcp_client;

use clap::Parser;
use std::net::SocketAddr;

use blaster_common::datagen;

use crate::orchestrator::{print_results, run_benchmark, BenchmarkConfig};

#[derive(Parser)]
#[command(name = "blaster")]
#[command(about = "gRPC vs QUIC packet blaster benchmark")]
struct Args {
    /// Target server IP
    #[arg(long)]
    target: String,

    /// gRPC server port
    #[arg(long, default_value = "50051")]
    grpc_port: u16,

    /// QUIC server port
    #[arg(long, default_value = "50052")]
    quic_port: u16,

    /// Raw TCP server port
    #[arg(long, default_value = "50053")]
    tcp_port: u16,

    /// Comma-separated payload sizes (1kb,10kb,100kb,1mb,10mb)
    #[arg(long, default_value = "1kb,10kb,100kb,1mb,10mb")]
    sizes: String,

    /// Number of packets per payload size
    #[arg(long, default_value = "100")]
    packets_per_size: usize,

    /// Number of concurrent QUIC streams for multiplexing (1 = single stream)
    #[arg(long, default_value = "1")]
    quic_streams: usize,

    /// Enable 0-RTT for QUIC (makes a warmup connection first to get session ticket)
    #[arg(long, default_value = "false")]
    zero_rtt: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let sizes = datagen::parse_sizes(&args.sizes);
    if sizes.is_empty() {
        anyhow::bail!(
            "No valid sizes provided. Valid options: 1kb, 10kb, 100kb, 1mb, 10mb"
        );
    }

    let grpc_addr = format!("{}:{}", args.target, args.grpc_port);
    let quic_addr: SocketAddr = format!("{}:{}", args.target, args.quic_port).parse()?;
    let tcp_addr: SocketAddr = format!("{}:{}", args.target, args.tcp_port).parse()?;

    println!("Blaster Benchmark");
    println!("═════════════════");
    println!("  Target:      {}", args.target);
    println!("  gRPC:        {}", grpc_addr);
    println!("  QUIC:        {}", quic_addr);
    println!("  TCP:         {}", tcp_addr);
    println!("  Sizes:       {:?}", sizes.iter().map(|(n, _)| *n).collect::<Vec<_>>());
    println!("  Packets/size: {}", args.packets_per_size);
    println!("  QUIC streams: {}", args.quic_streams);
    println!("  0-RTT:        {}", args.zero_rtt);
    println!();

    let config = BenchmarkConfig {
        grpc_addr,
        quic_addr,
        tcp_addr,
        sizes,
        packets_per_size: args.packets_per_size,
        quic_streams: args.quic_streams,
        zero_rtt: args.zero_rtt,
    };

    let results = run_benchmark(config).await?;
    print_results(&results);

    Ok(())
}
