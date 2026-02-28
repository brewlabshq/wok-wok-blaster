mod grpc_server;
mod quic_server;
mod tracker;

use std::net::SocketAddr;

use blaster_common::proto::blaster_service_server::BlasterServiceServer;
use clap::Parser;
use tonic::transport::Server;

use crate::grpc_server::BlasterGrpcService;
use crate::tracker::ArrivalTracker;

#[derive(Parser)]
#[command(name = "blaster-server")]
#[command(about = "gRPC + QUIC benchmark server")]
struct Args {
    #[arg(long, default_value = "50051")]
    grpc_port: u16,

    #[arg(long, default_value = "50052")]
    quic_port: u16,

    #[arg(long, default_value = "0.0.0.0")]
    bind: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let tracker = ArrivalTracker::new();

    let grpc_addr: SocketAddr = format!("{}:{}", args.bind, args.grpc_port).parse()?;
    let quic_addr: SocketAddr = format!("{}:{}", args.bind, args.quic_port).parse()?;

    tracing::info!("Starting blaster server");
    tracing::info!("  gRPC: {}", grpc_addr);
    tracing::info!("  QUIC: {}", quic_addr);

    let grpc_tracker = tracker.clone();
    let grpc_handle = tokio::spawn(async move {
        let svc = BlasterGrpcService::new(grpc_tracker);
        tracing::info!("gRPC server listening on {}", grpc_addr);
        Server::builder()
            .add_service(
                BlasterServiceServer::new(svc)
                    .max_decoding_message_size(64 * 1024 * 1024)
                    .max_encoding_message_size(64 * 1024 * 1024),
            )
            .serve(grpc_addr)
            .await
            .map_err(|e| anyhow::anyhow!("gRPC server error: {}", e))
    });

    let quic_handle = tokio::spawn(async move {
        quic_server::run_quic_server(quic_addr, tracker).await
    });

    tokio::select! {
        r = grpc_handle => { r??; }
        r = quic_handle => { r??; }
    }

    Ok(())
}
