//! Periodic stderr + optional CSV reporting for swarm runs.
//!
//! Owns FINAL/FINAL_SPACETIMEDB summary emission consumed by benchmark parsers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time;

use crate::metrics::{Metrics, MetricsSnapshot};

/// Arguments for [`run_reporter`].
pub struct ReporterConfig<'a> {
    pub metrics: Arc<Metrics>,
    pub action_metrics: Arc<Metrics>,
    pub read_metrics: Arc<Metrics>,
    pub stop: Arc<AtomicBool>,
    pub players: u32,
    pub backend_name: &'a str,
    pub actions_per_sec: f64,
    pub read_rate: f64,
    pub csv_file: Arc<tokio::sync::Mutex<Option<std::io::BufWriter<std::fs::File>>>>,
}

pub fn fmt_bytes(b: u64) -> String {
    if b >= 1_048_576 {
        format!("{:.1}MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.1}KB", b as f64 / 1024.0)
    } else {
        format!("{}B", b)
    }
}

pub async fn run_reporter(cfg: ReporterConfig<'_>) {
    let ReporterConfig {
        metrics,
        action_metrics,
        read_metrics,
        stop,
        players,
        backend_name,
        actions_per_sec,
        read_rate,
        csv_file,
    } = cfg;

    let start = Instant::now();
    let mut interval = time::interval(Duration::from_secs(1));
    interval.tick().await;
    let has_actions = actions_per_sec > 0.0;
    let has_reads = read_rate > 0.0;
    let mut total_oks: u64 = 0;
    let mut total_errs: u64 = 0;
    let mut total_calls: u64 = 0;
    let mut total_latency_sum_us: u64 = 0;
    let mut total_latency_samples: u64 = 0;
    let mut total_action_calls: u64 = 0;
    let mut total_action_oks: u64 = 0;
    let mut total_action_errs: u64 = 0;
    loop {
        interval.tick().await;
        let s = metrics.snapshot_and_reset();
        let r = read_metrics.snapshot_and_reset();
        total_oks += s.ok;
        total_errs += s.err;
        total_calls += s.ok + s.err;
        total_latency_sum_us += s.latency_sum_us;
        total_latency_samples += s.latency_samples;

        let elapsed = start.elapsed().as_secs();
        let w_ops = s.ok + s.err;
        let r_ops = r.ok + r.err;

        let a = if has_actions {
            action_metrics.snapshot_and_reset()
        } else {
            MetricsSnapshot::default()
        };
        let a_ops = a.ok + a.err;
        total_action_calls += a.ok + a.err;
        total_action_oks += a.ok;
        total_action_errs += a.err;

        let mut line = format!(
            "[{:>4}s] [{}] players={} writes/s={} ok={} err={} lat={:.1}ms",
            elapsed,
            backend_name,
            players,
            w_ops,
            s.ok,
            s.err,
            s.avg_latency_us as f64 / 1000.0,
        );

        if has_reads {
            line.push_str(&format!(
                " | reads/s={} ok={} err={} lat={:.1}ms rx={}",
                r_ops,
                r.ok,
                r.err,
                r.avg_latency_us as f64 / 1000.0,
                fmt_bytes(r.bytes),
            ));
        }

        if has_actions {
            line.push_str(&format!(
                " | acts/s={} ok={} err={} lat={:.1}ms",
                a_ops,
                a.ok,
                a.err,
                a.avg_latency_us as f64 / 1000.0,
            ));
        }

        eprintln!("{}", line);

        if let Some(ref mut w) = *csv_file.lock().await {
            use std::io::Write;
            let _ = writeln!(
                w,
                "{},{},{},{},{},{:.2},{:.2},{},{},{},{:.2},{},{},{},{:.2}",
                elapsed,
                players,
                s.ok,
                s.err,
                w_ops,
                s.avg_latency_us as f64 / 1000.0,
                s.max_latency_us as f64 / 1000.0,
                r.ok,
                r.err,
                r_ops,
                r.avg_latency_us as f64 / 1000.0,
                r.bytes,
                a.ok,
                a.err,
                a.avg_latency_us as f64 / 1000.0,
            );
            let _ = w.flush();
        }

        if stop.load(Ordering::Relaxed) {
            let lat_avg_ms = if total_latency_samples > 0 {
                total_latency_sum_us as f64 / 1000.0 / total_latency_samples as f64
            } else {
                0.0
            };
            eprintln!(
                "FINAL: players={} total_calls={} total_oks={} total_errs={} lat_avg_ms={:.2}",
                players, total_calls, total_oks, total_errs, lat_avg_ms,
            );
            if has_actions {
                let duration_secs = start.elapsed().as_secs().max(1);
                let spacetimedb_ops_per_sec = total_action_calls / duration_secs;
                eprintln!(
                    "FINAL_SPACETIMEDB: action_calls={} action_oks={} action_errs={} spacetimedb_ops_per_sec={}",
                    total_action_calls, total_action_oks, total_action_errs, spacetimedb_ops_per_sec,
                );
            }
            break;
        }
    }
}
