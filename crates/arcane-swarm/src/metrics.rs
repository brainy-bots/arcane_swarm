//! Atomic counters and rolling latency stats for swarm HTTP/WS operations.
//!
//! Used by backend loops and reporter tasks; intentionally lock-free for high player counts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Aggregated stats; produced by [`Metrics::snapshot_and_reset`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetricsSnapshot {
    pub ok: u64,
    pub err: u64,
    pub avg_latency_us: u64,
    pub max_latency_us: u64,
    pub latency_sum_us: u64,
    pub latency_samples: u64,
    pub bytes: u64,
}

/// Thread-safe rolling metrics for OK/err counts and latencies (microseconds).
pub struct Metrics {
    ok: AtomicU64,
    err: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_max_us: AtomicU64,
    latency_samples: AtomicU64,
    bytes: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            ok: AtomicU64::new(0),
            err: AtomicU64::new(0),
            latency_sum_us: AtomicU64::new(0),
            latency_max_us: AtomicU64::new(0),
            latency_samples: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    pub fn record_ok(&self, latency: Duration) {
        self.ok.fetch_add(1, Ordering::Relaxed);
        let us = latency.as_micros() as u64;
        self.latency_sum_us.fetch_add(us, Ordering::Relaxed);
        self.latency_samples.fetch_add(1, Ordering::Relaxed);
        self.latency_max_us.fetch_max(us, Ordering::Relaxed);
    }

    pub fn record_ok_bytes(&self, latency: Duration, bytes: u64) {
        self.record_ok(latency);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_err(&self) {
        self.err.fetch_add(1, Ordering::Relaxed);
    }

    /// One successful inbound message (e.g. WebSocket frame), counted as an OK with payload bytes.
    pub fn record_inbound_ok(&self, payload_bytes: u64) {
        self.ok.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(payload_bytes, Ordering::Relaxed);
    }

    pub fn snapshot_and_reset(&self) -> MetricsSnapshot {
        let ok = self.ok.swap(0, Ordering::Relaxed);
        let err = self.err.swap(0, Ordering::Relaxed);
        let sum = self.latency_sum_us.swap(0, Ordering::Relaxed);
        let max = self.latency_max_us.swap(0, Ordering::Relaxed);
        let n = self.latency_samples.swap(0, Ordering::Relaxed);
        let avg = if n > 0 { sum / n } else { 0 };
        let bytes = self.bytes.swap(0, Ordering::Relaxed);
        MetricsSnapshot {
            ok,
            err,
            avg_latency_us: avg,
            max_latency_us: max,
            latency_sum_us: sum,
            latency_samples: n,
            bytes,
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn snapshot_and_reset_clears_counters() {
        let m = Metrics::new();
        m.record_ok(Duration::from_micros(100));
        m.record_inbound_ok(42);
        let s = m.snapshot_and_reset();
        assert_eq!(s.ok, 2);
        assert_eq!(s.bytes, 42);
        let s2 = m.snapshot_and_reset();
        assert_eq!(s2.ok, 0);
        assert_eq!(s2.bytes, 0);
    }
}
