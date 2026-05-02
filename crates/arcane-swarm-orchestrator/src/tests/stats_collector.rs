//! Acceptance tests for the cluster /stats collector (C3).
//!
//! Tests are gated with `#[ignore]` until the implementation in
//! `src/stats_collector.rs` lands. To run them locally:
//!
//!   cargo test -p arcane-swarm-orchestrator -- --ignored
//!
//! The tests are the spec.

use crate::stats_collector::{ClusterEndpoint, ClusterStats, StatsCollector};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Mock endpoint: returns scripted `ClusterStats` samples. Each call pops
/// the next entry from a recipe queue. Optionally fails for the next N
/// calls to simulate transient unreachability.
struct MockEndpoint {
    url: String,
    samples: Mutex<Vec<ClusterStats>>,
    next_idx: AtomicUsize,
    fail_count: AtomicU64,
    fetched: AtomicUsize,
}

impl MockEndpoint {
    fn new(url: &str, samples: Vec<ClusterStats>) -> Self {
        Self {
            url: url.to_string(),
            samples: Mutex::new(samples),
            next_idx: AtomicUsize::new(0),
            fail_count: AtomicU64::new(0),
            fetched: AtomicUsize::new(0),
        }
    }

    fn fail_next(&self, n: u64) {
        self.fail_count.fetch_add(n, Ordering::SeqCst);
    }

    fn fetched_count(&self) -> usize {
        self.fetched.load(Ordering::SeqCst)
    }
}

impl ClusterEndpoint for MockEndpoint {
    fn url(&self) -> &str {
        &self.url
    }

    async fn fetch(&self) -> Result<ClusterStats, String> {
        self.fetched.fetch_add(1, Ordering::SeqCst);

        if self.fail_count.load(Ordering::SeqCst) > 0 {
            self.fail_count.fetch_sub(1, Ordering::SeqCst);
            return Err("simulated transient failure".to_string());
        }

        let idx = self.next_idx.fetch_add(1, Ordering::SeqCst);
        let samples = self.samples.lock().unwrap();
        samples
            .get(idx)
            .cloned()
            .ok_or_else(|| format!("no scripted sample at index {}", idx))
    }
}

fn stats_at(now: Instant, bytes_out: u64, entities: u64) -> ClusterStats {
    ClusterStats {
        bytes_in: 0,
        bytes_out,
        last_tick_us: 33_000, // 30 Hz baseline
        broadcast_lagged_events: 0,
        entities_current: entities,
        sampled_at: now,
    }
}

#[tokio::test]
#[ignore]
async fn collector_reports_correct_rates_and_counters() {
    // Acceptance: Mock cluster server emits known stats; collector reports
    // correct rates and counters.
    let t0 = Instant::now();
    let samples = vec![
        stats_at(t0, 0, 100),
        stats_at(t0 + Duration::from_secs(2), 200_000_000, 150),
        stats_at(t0 + Duration::from_secs(4), 400_000_000, 200),
    ];
    let endpoint = Arc::new(MockEndpoint::new("https://cluster-a/stats", samples));
    let collector =
        StatsCollector::new(vec![endpoint.clone()]).with_poll_interval(Duration::from_millis(20));

    // Step the collector through 3 polls.
    for _ in 0..3 {
        collector.poll_once().await.expect("poll should succeed");
    }

    let series = collector
        .time_series_for("https://cluster-a/stats")
        .await
        .expect("time series should exist for the polled endpoint");
    assert_eq!(series.len(), 3, "three samples should be retained");

    // bytes_out grew by 200 MB / 2 s = 100 MB/s = 0.8 Gbps per cluster.
    let rates = collector
        .current_rates()
        .await
        .expect("rates available after polls");
    assert!(
        (rates.bytes_out_per_sec - 100_000_000.0).abs() < 1_000_000.0,
        "bytes_out_per_sec should be ~100 MB/s; got {}",
        rates.bytes_out_per_sec
    );
    // Single cluster, so aggregate equals one cluster's rate, in Gbps.
    let expected_gbps = (100_000_000.0_f64 * 8.0) / 1_000_000_000.0;
    assert!(
        (rates.egress_aggregate_gbps - expected_gbps).abs() < 0.05,
        "egress_aggregate_gbps off; got {}, expected {}",
        rates.egress_aggregate_gbps,
        expected_gbps
    );
}

#[tokio::test]
#[ignore]
async fn polling_continues_when_cluster_unreachable() {
    // Acceptance: Polling continues when one cluster is briefly unreachable;
    // resumes on recovery.
    let t0 = Instant::now();
    let samples = vec![
        stats_at(t0, 0, 100),
        stats_at(t0 + Duration::from_secs(2), 200_000_000, 150),
    ];
    let endpoint = Arc::new(MockEndpoint::new("https://cluster-a/stats", samples));

    // First two fetch attempts fail; third onward succeeds.
    endpoint.fail_next(2);

    let collector =
        StatsCollector::new(vec![endpoint.clone()]).with_poll_interval(Duration::from_millis(10));

    let mut successful = 0;
    for _ in 0..4 {
        if let Ok(n) = collector.poll_once().await {
            successful += n;
        }
    }

    // Endpoint must have been polled at least 4 times (failures count as
    // polls — collector did not give up).
    assert!(
        endpoint.fetched_count() >= 4,
        "collector must keep polling through transient failures; only fetched {} times",
        endpoint.fetched_count()
    );

    assert!(
        successful >= 1,
        "at least one poll must have collected a sample after recovery"
    );

    let series = collector
        .time_series_for("https://cluster-a/stats")
        .await
        .expect("time series should exist after recovery");
    assert!(!series.is_empty(), "samples must accumulate post-recovery");
}

#[tokio::test]
#[ignore]
async fn time_series_memory_bounded() {
    // Acceptance: Time-series memory bounded to 5 minutes (no unbounded
    // growth over a 24-hour soak).
    //
    // Use a tight retention window (1 s) + tight poll interval (10 ms) to
    // exercise the bounded-growth invariant in a few seconds, then verify
    // the in-memory series stays bounded after many more polls than the
    // window can hold.
    let t0 = Instant::now();
    let samples: Vec<ClusterStats> = (0..1000)
        .map(|i| {
            stats_at(
                t0 + Duration::from_millis(i as u64 * 10),
                i as u64,
                i as u64,
            )
        })
        .collect();
    let endpoint = Arc::new(MockEndpoint::new("https://cluster-a/stats", samples));
    let collector = StatsCollector::new(vec![endpoint.clone()])
        .with_poll_interval(Duration::from_millis(10))
        .with_retention(Duration::from_secs(1));

    for _ in 0..500 {
        let _ = collector.poll_once().await;
    }

    let series = collector
        .time_series_for("https://cluster-a/stats")
        .await
        .expect("time series should exist after polls");

    // 1-second retention with 10 ms-spaced samples = at most ~100 entries.
    // Allow generous slack for boundary handling.
    assert!(
        series.len() < 200,
        "time series must be bounded by retention window; got {} samples \
         (retention=1s, poll=10ms, so expected at most ~100)",
        series.len()
    );
}
