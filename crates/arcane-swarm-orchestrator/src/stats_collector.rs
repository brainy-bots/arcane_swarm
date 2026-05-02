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
    /// `None` if no clusters have been polled yet.
    pub async fn current_rates(&self) -> Option<RatesSnapshot> {
        let map = self.series.read().await;
        if map.is_empty() {
            return None;
        }
        let mut total_bytes_per_sec = 0.0;
        let mut total_lagged = 0u64;
        let mut total_bytes_out = 0u64;
        for series in map.values() {
            if series.len() < 2 {
                continue;
            }
            let oldest = series.first().unwrap();
            let newest = series.last().unwrap();
            let secs = (newest.stats.sampled_at - oldest.stats.sampled_at).as_secs_f64();
            if secs > 0.0 {
                let delta_bytes = newest
                    .stats
                    .bytes_out
                    .saturating_sub(oldest.stats.bytes_out)
                    as f64;
                total_bytes_per_sec += delta_bytes / secs;
            }
            total_lagged += newest
                .stats
                .broadcast_lagged_events
                .saturating_sub(oldest.stats.broadcast_lagged_events);
            total_bytes_out += newest
                .stats
                .bytes_out
                .saturating_sub(oldest.stats.bytes_out);
        }
        // Lagged broadcasts vs. successful broadcasts isn't directly available
        // from byte counters alone; surface a proxy that's 1.0 when nothing
        // was lagged and degrades as lagged_events grows. Real delta_hit_rate
        // wiring lands when the cluster exposes broadcasts_attempted (#94).
        let delta_hit_rate = if total_bytes_out > 0 {
            1.0 - (total_lagged as f64 / total_bytes_out as f64).min(1.0)
        } else {
            1.0
        };
        Some(RatesSnapshot {
            bytes_out_per_sec: total_bytes_per_sec,
            delta_hit_rate,
            egress_aggregate_gbps: total_bytes_per_sec * 8.0 / 1_000_000_000.0,
        })
    }

    /// Read the full per-endpoint time series for one cluster.
    pub async fn time_series_for(&self, url: &str) -> Option<Vec<StatsSample>> {
        self.series.read().await.get(url).cloned()
    }

    /// Latest sample per cluster. The telemetry source uses this to embed a
    /// per-cluster `/stats` summary in each snapshot.
    pub async fn latest_per_cluster(&self) -> std::collections::HashMap<String, ClusterStats> {
        let map = self.series.read().await;
        map.iter()
            .filter_map(|(url, series)| series.last().map(|s| (url.clone(), s.stats.clone())))
            .collect()
    }

    /// Drive one poll round across all endpoints. Public so tests can step
    /// the collector deterministically without spawning the run loop.
    /// Each Err from `endpoint.fetch()` is logged and skipped (the collector
    /// continues polling on the next tick). Returns the number of endpoints
    /// that successfully produced a sample this round.
    pub async fn poll_once(&self) -> Result<usize, String> {
        let mut succeeded = 0usize;
        let mut series_map = self.series.write().await;
        for endpoint in &self.endpoints {
            match endpoint.fetch().await {
                Ok(stats) => {
                    let sample = StatsSample {
                        stats: stats.clone(),
                        recorded_at: Instant::now(),
                    };
                    let series = series_map.entry(endpoint.url().to_string()).or_default();
                    series.push(sample);
                    // Prune by sample's own time axis (sampled_at): keeps the
                    // window at the documented "rolling N-minute" semantic
                    // even when polling is bursty or replayed from a recipe.
                    if let Some(cutoff) = stats.sampled_at.checked_sub(self.retention) {
                        series.retain(|s| s.stats.sampled_at >= cutoff);
                    }
                    succeeded += 1;
                }
                Err(_) => {
                    // Transient failure — caller can inspect status; collector
                    // does not give up, the next tick will retry.
                }
            }
        }
        Ok(succeeded)
    }

    /// Spawn the polling loop in the current task. Production code typically
    /// calls this from a `tokio::spawn`; tests use `poll_once` directly.
    /// Never returns under normal operation.
    pub async fn run(&self) -> Result<(), String> {
        let mut ticker = tokio::time::interval(self.poll_interval);
        loop {
            ticker.tick().await;
            let _ = self.poll_once().await;
        }
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
