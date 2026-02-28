use dashmap::DashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct ArrivalRecord {
    pub grpc_arrival_ns: Option<u64>,
    pub quic_arrival_ns: Option<u64>,
    pub payload_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Grpc,
    Quic,
}

#[derive(Clone)]
pub struct ArrivalTracker {
    records: Arc<DashMap<u64, ArrivalRecord>>,
}

impl ArrivalTracker {
    pub fn new() -> Self {
        Self {
            records: Arc::new(DashMap::new()),
        }
    }

    pub fn record(&self, transport: Transport, seq_id: u64, payload_size: u32) {
        let now = now_ns();
        self.records
            .entry(seq_id)
            .and_modify(|r| match transport {
                Transport::Grpc => r.grpc_arrival_ns = Some(now),
                Transport::Quic => r.quic_arrival_ns = Some(now),
            })
            .or_insert_with(|| {
                let mut rec = ArrivalRecord {
                    grpc_arrival_ns: None,
                    quic_arrival_ns: None,
                    payload_size,
                };
                match transport {
                    Transport::Grpc => rec.grpc_arrival_ns = Some(now),
                    Transport::Quic => rec.quic_arrival_ns = Some(now),
                }
                rec
            });
    }

    pub fn arrivals(&self) -> Vec<(u64, ArrivalRecord)> {
        self.records
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect()
    }

    pub fn clear(&self) {
        self.records.clear();
    }
}

pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
