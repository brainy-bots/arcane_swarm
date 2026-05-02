//! Cluster `/stats` collector — orchestrator component C3.
//!
//! Polls each registered cluster's `/stats` endpoint every 2 seconds,
//! maintains a rolling 5-minute time series in memory, and exposes derived
//! rates (`bytes_out_per_sec`, `delta_hit_rate`, `egress_aggregate_gbps`).
//!
//! Decoupled from the wire transport via `ClusterEndpoint`. Production wires
//! it to an HTTP client; tests inject a `MockEndpoint` that emits scripted
//! samples.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Snapshot of one `/stats` response from a cluster, as returned by the
/// cluster's HTTP endpoint. Mirrors the existing `arcane-infra` cluster_stats
/// shape; new fields (per #94) extend additively.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterStats {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub last_tick_us: u64,
    pub broadcast_lagged_events: u64,
    pub entities_current: u64,
    /// Wall-clock time when this snapshot was taken (provided by collector,
    /// not the cluster — keeps the data shape independent of clock skew).
    pub sampled_at: Instant,
}

/// Derived rates computed from the time series.
#[derive(Debug, Clone, PartialEq)]
pub struct RatesSnapshot {
    pub bytes_out_per_sec: f64,
    /// Fraction of broadcasts that hit subscribers; bounded \[0, 1\].
    pub delta_hit_rate: f64,
    /// Aggregate egress bandwidth across all clusters, in Gbps.
    pub egress_aggregate_gbps: f64,
}

/// One stored point in the per-cluster time series.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsSample {
    pub stats: ClusterStats,
    pub recorded_at: Instant,
}

/// Trait abstraction over an `/stats` endpoint poll. Production wires it to
/// an HTTP client; tests use a `MockEndpoint` that emits scripted samples.
///
/// Uses Rust 1.75+ async fn in traits.
pub trait ClusterEndpoint: Send + Sync {
    /// URL or label identifying this endpoint (used in collector keys).
    fn url(&self) -> &str;
    /// Fetch the latest `/stats` snapshot. Errors propagate; the collector
    /// treats Err as a transient failure and continues polling.
    fn fetch(&self) -> impl std::future::Future<Output = Result<ClusterStats, String>> + Send;
}

/// Collector state visible to observers. Read-only snapshot.
#[derive(Debug, Clone)]
pub struct CollectorStatus {
    pub clusters_polled: usize,
    pub last_poll_ok: bool,
    pub time_series_window: Duration,
}

/// The stats collector itself.
pub struct StatsCollector<E: ClusterEndpoint + 'static> {
    endpoints: Vec<Arc<E>>,
    poll_interval: Duration,
    /// Maximum time-series window to retain in memory.
    retention: Duration,
    /// Inner state: per-endpoint time series, keyed by `endpoint.url()`.
    series: Arc<RwLock<std::collections::HashMap<String, Vec<StatsSample>>>>,
}

impl<E: ClusterEndpoint + 'static> StatsCollector<E> {
    /// Construct a new collector with the design-doc defaults
    /// (2-second poll interval, 5-minute retention).
    pub fn new(endpoints: Vec<Arc<E>>) -> Self {
        Self {
            endpoints,
            poll_interval: Duration::from_secs(2),
            retention: Duration::from_secs(5 * 60),
            series: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// Override the default poll interval (tests use a tighter one).
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Override the default retention window. Used in tests that exercise
    /// memory-bounded behavior with a smaller window.
    pub fn with_retention(mut self, retention: Duration) -> Self {
        self.retention = retention;
        self
    }

    /// Fetch the latest derived rates across all polled clusters. Returns
    /// `None` if no samples have been collected yet.
    ///
    /// Implementation lands in C3 PR. Test contract is in
    /// `tests/stats_collector.rs`.
    pub async fn current_rates(&self) -> Option<RatesSnapshot> {
        unimplemented!("C3: rate computation — see tests/stats_collector.rs")
    }

    /// Read the full per-endpoint time series for one cluster. Returns
    /// `None` if no samples for that URL.
    pub async fn time_series_for(&self, _url: &str) -> Option<Vec<StatsSample>> {
        unimplemented!("C3: per-endpoint time-series read — see tests/stats_collector.rs")
    }

    /// Drive one poll round across all endpoints. Public so tests can step
    /// the collector deterministically without spawning the run loop.
    /// Each Err from `endpoint.fetch()` is logged and skipped (the collector
    /// continues polling on the next tick).
    pub async fn poll_once(&self) -> Result<usize, String> {
        unimplemented!("C3: single poll round — see tests/stats_collector.rs")
    }

    /// Spawn the polling loop as a background tokio task. Returns a handle
    /// so the caller can join or cancel.
    ///
    /// Production code calls this once at startup. Tests prefer `poll_once`.
    pub async fn run(&self) -> Result<(), String> {
        let _ = (&self.endpoints, &self.poll_interval, &self.retention);
        unimplemented!("C3: collector run loop — see tests/stats_collector.rs")
    }

    /// Read-only status snapshot.
    pub async fn status(&self) -> CollectorStatus {
        let map = self.series.read().await;
        CollectorStatus {
            clusters_polled: map.len(),
            last_poll_ok: !map.is_empty(),
            time_series_window: self.retention,
        }
    }
}
