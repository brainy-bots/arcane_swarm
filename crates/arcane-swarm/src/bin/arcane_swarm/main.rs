//! Headless client swarm for load testing SpacetimeDB and Arcane+SpacetimeDB.
//!
//! Each logical player is a separate async task simulating a real game client.
//! This binary composes backend implementations around shared library modules.
//!
//! Build:  `cargo build -p arcane-swarm --bin arcane-swarm --release`
//!
//! Backends:
//!   spacetimedb  — each player calls update_player reducer via HTTP (default)
//!   arcane       — each player connects to Arcane cluster server via WebSocket
//!
//! Usage:
//!   arcane-swarm --players 200 --mode spread --backend spacetimedb --duration 60
//!   arcane-swarm --players 200 --mode spread --backend arcane --arcane-ws ws://127.0.0.1:8080 --duration 60
//!   arcane-swarm --players 200 --backend arcane --arcane-manager http://127.0.0.1:8081 --duration 60
//!
//! With --arcane-ws: all players connect to one cluster (single server).
//! With --arcane-manager: each player does GET manager/join; players are spread round-robin across clusters (see docs/ARCANE_BENCHMARK_SETUP.md).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

use arcane_swarm::{
    parse_args, run_reporter, ArcaneEndpoint, Backend, Config, Metrics, ReporterConfig, SwarmMode,
    VISIBILITY_RADIUS,
};

mod backends_arcane;
mod backends_spacetimedb;
mod control_mode;
mod runtime;
mod spacetimedb_bindings;
mod spawn_context;

use runtime::{ArcaneRuntime, BackendRuntime, SpacetimeRuntime};
use spawn_context::{PlayerLoopShared, PlayerSpawnParams};

fn create_backend_runtime(cfg: &Config, ws_uri: String) -> Arc<dyn BackendRuntime> {
    match cfg.backend {
        Backend::SpacetimeDb => Arc::new(SpacetimeRuntime::new(
            ws_uri,
            cfg.database.clone(),
            cfg.server_physics,
        )),
        Backend::Arcane => {
            let endpoint = match &cfg.arcane_manager {
                Some(base) => ArcaneEndpoint::ManagerJoin {
                    base_url: base.clone(),
                },
                None => ArcaneEndpoint::SingleUrl(cfg.arcane_ws.clone()),
            };
            Arc::new(ArcaneRuntime::new(endpoint, cfg.user_data_bytes))
        }
    }
}

// -- Main ------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cfg = parse_args();
    let run_started = std::time::Instant::now();
    let tick_interval = Duration::from_micros(1_000_000 / cfg.tick_rate as u64);
    let stdb_base = cfg.spacetimedb_uri.trim_end_matches('/').to_string();
    let ws_uri = stdb_base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);

    if cfg.run_forever || cfg.control_port > 0 {
        control_mode::run_control_mode(cfg, tick_interval).await;
        return;
    }

    let metrics = Arc::new(Metrics::new());
    let action_metrics = Arc::new(Metrics::new());
    let read_metrics = Arc::new(Metrics::new());

    let backend_runtime = create_backend_runtime(&cfg, ws_uri.clone());
    let backend_name = backend_runtime.name();

    eprintln!("arcane-swarm: {} players, {} Hz, mode={}, backend={}, server_physics={}, duration={}s, actions/s={:.1}, read_rate={:.1}Hz",
        cfg.players, cfg.tick_rate,
        if cfg.mode == SwarmMode::Clustered { "clustered" } else { "spread" },
        backend_name, cfg.server_physics, cfg.duration_secs, cfg.actions_per_sec, cfg.read_rate,
    );
    if cfg.burst.enabled {
        eprintln!(
            "  Burst profile: period={}s cohort={}%% actions/player={} window={}ms zone_period={}s zone_window={}ms",
            cfg.burst.burst_period_secs,
            cfg.burst.burst_cohort_percent,
            cfg.burst.burst_actions_per_player,
            cfg.burst.burst_window_ms,
            cfg.burst.zone_event_period_secs,
            cfg.burst.zone_event_window_ms,
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    let total_players_atomic = Arc::new(AtomicU32::new(cfg.players));
    let mut handles = Vec::with_capacity(cfg.players as usize);

    let all_ids: Arc<Vec<uuid::Uuid>> =
        Arc::new((0..cfg.players).map(|_| uuid::Uuid::new_v4()).collect());

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(cfg.players as usize * 2)
        .build()
        .expect("HTTP client");

    if backend_name == "spacetimedb" {
        eprintln!(
            "  SpacetimeDB: {}/database/{}",
            cfg.spacetimedb_uri, cfg.database
        );
    } else if let Some(base) = &cfg.arcane_manager {
        eprintln!("  Arcane: manager join at {} (round-robin clusters)", base);
    } else {
        eprintln!("  Arcane WS: {} (single cluster)", cfg.arcane_ws);
    }

    if backend_name == "spacetimedb" {
        eprintln!(
            "  Read simulation: SpacetimeDB SDK subscription on `entity` in a {}-unit AOI box around each player's starting position (delivers updates via on_update, no HTTP polling).",
            VISIBILITY_RADIUS,
        );
    }

    let loop_shared = PlayerLoopShared {
        http_client: http_client.clone(),
        metrics: metrics.clone(),
        read_metrics: read_metrics.clone(),
        action_metrics: action_metrics.clone(),
        cluster_flag: cfg.cluster_command.clone(),
        all_ids: all_ids.clone(),
        total_players: total_players_atomic.clone(),
        actions_per_sec: cfg.actions_per_sec,
        burst: cfg.burst,
        run_started,
    };

    for i in 0..cfg.players {
        let params = PlayerSpawnParams {
            idx: i,
            entity_id: all_ids[i as usize],
            desired_total: cfg.players,
            tick_interval,
            stop: stop.clone(),
        };
        handles.push(backend_runtime.spawn_player(&loop_shared, params.clone()));
        let _ = backend_runtime.spawn_read(&loop_shared, &params, cfg.read_rate);
    }

    let csv_file = cfg.csv_path.as_ref().map(|p| {
        let f = std::fs::File::create(p).expect("cannot create CSV file");
        let mut w = std::io::BufWriter::new(f);
        use std::io::Write;
        writeln!(w, "elapsed_s,players,w_ok,w_err,w_ops,w_avg_ms,w_max_ms,r_ok,r_err,r_ops,r_avg_ms,r_bytes,a_ok,a_err,a_avg_ms,drv_cpu_pct,drv_rss_mb").unwrap();
        w
    });
    let csv_file = Arc::new(tokio::sync::Mutex::new(csv_file));

    let reporter = tokio::spawn(run_reporter(ReporterConfig {
        metrics: metrics.clone(),
        action_metrics: action_metrics.clone(),
        read_metrics: read_metrics.clone(),
        stop: stop.clone(),
        players: cfg.players,
        backend_name,
        actions_per_sec: cfg.actions_per_sec,
        read_rate: cfg.read_rate,
        csv_file: csv_file.clone(),
    }));

    time::sleep(Duration::from_secs(cfg.duration_secs)).await;
    eprintln!("\narcane-swarm: duration reached, shutting down...");
    stop.store(true, Ordering::Relaxed);

    for h in handles {
        let _ = h.await;
    }
    let _ = reporter.await;
    eprintln!("arcane-swarm: done.");
}
