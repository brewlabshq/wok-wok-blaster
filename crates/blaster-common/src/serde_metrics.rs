use std::sync::{Arc, Mutex};

/// Collects per-packet serialization/deserialization timings (in nanoseconds).
/// Thread-safe — can be shared across tokio tasks.
#[derive(Clone)]
pub struct SerdeMetrics {
    samples: Arc<Mutex<Vec<u64>>>,
}

/// Computed stats from collected samples.
pub struct SerdeStats {
    pub count: usize,
    pub median_ns: u64,
    pub min_ns: u64,
    pub max_ns: u64,
    pub mean_ns: f64,
}

impl SerdeStats {
    pub fn median_us(&self) -> f64 {
        self.median_ns as f64 / 1_000.0
    }

    pub fn min_us(&self) -> f64 {
        self.min_ns as f64 / 1_000.0
    }

    pub fn max_us(&self) -> f64 {
        self.max_ns as f64 / 1_000.0
    }

    pub fn mean_us(&self) -> f64 {
        self.mean_ns / 1_000.0
    }
}

impl SerdeMetrics {
    pub fn new() -> Self {
        Self {
            samples: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn record(&self, duration_ns: u64) {
        self.samples.lock().unwrap().push(duration_ns);
    }

    pub fn stats(&self) -> SerdeStats {
        let mut samples = self.samples.lock().unwrap().clone();
        if samples.is_empty() {
            return SerdeStats {
                count: 0,
                median_ns: 0,
                min_ns: 0,
                max_ns: 0,
                mean_ns: 0.0,
            };
        }

        samples.sort_unstable();
        let count = samples.len();
        let median_ns = samples[count / 2];
        let min_ns = samples[0];
        let max_ns = samples[count - 1];
        let mean_ns = samples.iter().sum::<u64>() as f64 / count as f64;

        SerdeStats {
            count,
            median_ns,
            min_ns,
            max_ns,
            mean_ns,
        }
    }
}

impl Default for SerdeMetrics {
    fn default() -> Self {
        Self::new()
    }
}
