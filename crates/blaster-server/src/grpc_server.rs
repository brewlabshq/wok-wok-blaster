use blaster_common::proto::blaster_service_server::BlasterService;
use blaster_common::proto::{
    ArrivalEntry, BenchmarkReport, BlastAck, BlastPacket, ReportRequest, SerdeStatsProto,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::tracker::{now_ns, ArrivalTracker, Transport};

pub struct BlasterGrpcService {
    tracker: ArrivalTracker,
}

impl BlasterGrpcService {
    pub fn new(tracker: ArrivalTracker) -> Self {
        Self { tracker }
    }
}

#[tonic::async_trait]
impl BlasterService for BlasterGrpcService {
    type BlastStreamStream = ReceiverStream<Result<BlastAck, Status>>;

    async fn blast_stream(
        &self,
        request: Request<Streaming<BlastPacket>>,
    ) -> Result<Response<Self::BlastStreamStream>, Status> {
        let tracker = self.tracker.clone();
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(1024);

        tokio::spawn(async move {
            while let Some(result) = stream.message().await.transpose() {
                match result {
                    Ok(packet) => {
                        tracker.record(
                            Transport::Grpc,
                            packet.sequence_id,
                            packet.payload_size,
                        );
                        let ack = BlastAck {
                            sequence_id: packet.sequence_id,
                            received_at_ns: now_ns(),
                        };
                        if tx.send(Ok(ack)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!("gRPC stream error: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn get_report(
        &self,
        _request: Request<ReportRequest>,
    ) -> Result<Response<BenchmarkReport>, Status> {
        let arrivals = self
            .tracker
            .arrivals()
            .into_iter()
            .map(|(seq_id, record)| ArrivalEntry {
                sequence_id: seq_id,
                grpc_arrival_ns: record.grpc_arrival_ns.unwrap_or(0),
                quic_arrival_ns: record.quic_arrival_ns.unwrap_or(0),
                tcp_arrival_ns: record.tcp_arrival_ns.unwrap_or(0),
                payload_size: record.payload_size,
            })
            .collect();

        let quic_stats = self.tracker.quic_decode_metrics.stats();
        let tcp_stats = self.tracker.tcp_decode_metrics.stats();

        Ok(Response::new(BenchmarkReport {
            arrivals,
            quic_decode_stats: Some(SerdeStatsProto {
                count: quic_stats.count as u64,
                median_ns: quic_stats.median_ns,
                min_ns: quic_stats.min_ns,
                max_ns: quic_stats.max_ns,
                mean_ns: quic_stats.mean_ns,
            }),
            tcp_decode_stats: Some(SerdeStatsProto {
                count: tcp_stats.count as u64,
                median_ns: tcp_stats.median_ns,
                min_ns: tcp_stats.min_ns,
                max_ns: tcp_stats.max_ns,
                mean_ns: tcp_stats.mean_ns,
            }),
        }))
    }
}
