use hdrhistogram::Histogram;
use std::time::Duration;

pub struct MetricsCollector {
    histogram: Histogram<u64>,
    total_bytes: u64,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            histogram: Histogram::new_with_bounds(1, 60_000_000_000, 3).unwrap(),
            total_bytes: 0,
        }
    }

    pub fn record_latency_ns(&mut self, ns: u64) {
        let _ = self.histogram.record(ns);
    }

    pub fn add_bytes(&mut self, bytes: u64) {
        self.total_bytes += bytes;
    }

    pub fn p50_ms(&self) -> f64 {
        self.histogram.value_at_quantile(0.50) as f64 / 1_000_000.0
    }

    pub fn p90_ms(&self) -> f64 {
        self.histogram.value_at_quantile(0.90) as f64 / 1_000_000.0
    }

    pub fn p99_ms(&self) -> f64 {
        self.histogram.value_at_quantile(0.99) as f64 / 1_000_000.0
    }

    pub fn mean_ms(&self) -> f64 {
        self.histogram.mean() / 1_000_000.0
    }

    pub fn min_ms(&self) -> f64 {
        self.histogram.min() as f64 / 1_000_000.0
    }

    pub fn max_ms(&self) -> f64 {
        self.histogram.max() as f64 / 1_000_000.0
    }

    pub fn count(&self) -> u64 {
        self.histogram.len()
    }

    pub fn throughput_mbps(&self, elapsed: Duration) -> f64 {
        if elapsed.is_zero() {
            return 0.0;
        }
        (self.total_bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}
