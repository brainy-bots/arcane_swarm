//! Atomic counters and rolling latency stats for swarm HTTP/WS operations.
//!
//! Used by backend loops and reporter tasks; intentionally lock-free for high player counts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Explicit benchmark error taxonomy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Timeout,
    NotDelivered,
    HttpStatus,
    Transport,
    ConnectionDrop,
}

impl ErrorKind {
    pub const ALL: [ErrorKind; 5] = [
        ErrorKind::Timeout,
        ErrorKind::NotDelivered,
        ErrorKind::HttpStatus,
        ErrorKind::Transport,
        ErrorKind::ConnectionDrop,
    ];

    pub fn key(self) -> &'static str {
        match self {
            ErrorKind::Timeout => "timeout",
            ErrorKind::NotDelivered => "not_delivered",
            ErrorKind::HttpStatus => "http_status",
            ErrorKind::Transport => "transport",
            ErrorKind::ConnectionDrop => "connection_drop",
        }
    }
}

/// Per-category error counters emitted in summaries.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ErrorBreakdown {
    pub timeout: u64,
    pub not_delivered: u64,
    pub http_status: u64,
    pub transport: u64,
    pub connection_drop: u64,
}

impl ErrorBreakdown {
    pub fn total(&self) -> u64 {
        self.timeout + self.not_delivered + self.http_status + self.transport + self.connection_drop
    }

    pub fn to_json(self) -> String {
        format!(
            "{{\"timeout\":{},\"not_delivered\":{},\"http_status\":{},\"transport\":{},\"connection_drop\":{}}}",
            self.timeout,
            self.not_delivered,
            self.http_status,
            self.transport,
            self.connection_drop
        )
    }
}

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
    pub errors: ErrorBreakdown,
}

/// Thread-safe rolling metrics for OK/err counts and latencies (microseconds).
pub struct Metrics {
    ok: AtomicU64,
    err: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_max_us: AtomicU64,
    latency_samples: AtomicU64,
    bytes: AtomicU64,
    err_timeout: AtomicU64,
    err_not_delivered: AtomicU64,
    err_http_status: AtomicU64,
    err_transport: AtomicU64,
    err_connection_drop: AtomicU64,
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
            err_timeout: AtomicU64::new(0),
            err_not_delivered: AtomicU64::new(0),
            err_http_status: AtomicU64::new(0),
            err_transport: AtomicU64::new(0),
            err_connection_drop: AtomicU64::new(0),
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
        self.record_err_kind(ErrorKind::Transport);
    }

    pub fn record_err_kind(&self, kind: ErrorKind) {
        self.err.fetch_add(1, Ordering::Relaxed);
        match kind {
            ErrorKind::Timeout => {
                self.err_timeout.fetch_add(1, Ordering::Relaxed);
            }
            ErrorKind::NotDelivered => {
                self.err_not_delivered.fetch_add(1, Ordering::Relaxed);
            }
            ErrorKind::HttpStatus => {
                self.err_http_status.fetch_add(1, Ordering::Relaxed);
            }
            ErrorKind::Transport => {
                self.err_transport.fetch_add(1, Ordering::Relaxed);
            }
            ErrorKind::ConnectionDrop => {
                self.err_connection_drop.fetch_add(1, Ordering::Relaxed);
            }
        }
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
        let errors = ErrorBreakdown {
            timeout: self.err_timeout.swap(0, Ordering::Relaxed),
            not_delivered: self.err_not_delivered.swap(0, Ordering::Relaxed),
            http_status: self.err_http_status.swap(0, Ordering::Relaxed),
            transport: self.err_transport.swap(0, Ordering::Relaxed),
            connection_drop: self.err_connection_drop.swap(0, Ordering::Relaxed),
        };
        MetricsSnapshot {
            ok,
            err,
            avg_latency_us: avg,
            max_latency_us: max,
            latency_sum_us: sum,
            latency_samples: n,
            bytes,
            errors,
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
        m.record_err_kind(ErrorKind::Timeout);
        let s = m.snapshot_and_reset();
        assert_eq!(s.ok, 2);
        assert_eq!(s.bytes, 42);
        assert_eq!(s.errors.timeout, 1);
        assert_eq!(s.errors.total(), 1);
        let s2 = m.snapshot_and_reset();
        assert_eq!(s2.ok, 0);
        assert_eq!(s2.bytes, 0);
        assert_eq!(s2.errors.total(), 0);
    }
}
