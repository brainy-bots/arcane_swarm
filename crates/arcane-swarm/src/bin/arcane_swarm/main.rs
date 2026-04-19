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
mod spacetimedb_bindings;
mod spawn_context;

use spawn_context::{
    spawn_control_mode_player, ControlSpawnKit, PlayerLoopShared, PlayerSpawnParams,
};

/// Backend-specific runtime selected at startup (`spacetimedb` vs `arcane`). Binary-internal only; CLI and wire formats are unchanged.
///
/// Both backends carry movement and action reducer calls on one WebSocket per
/// simulated player, so `spawn_player` owns the entire per-player lifecycle
/// (connect, movement loop, action loop, disconnect). There is no separate
/// action spawn.
pub(crate) trait BackendRuntime: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn_player(
        &self,
        shared: &PlayerLoopShared,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()>;

    fn spawn_read(
        &self,
        shared: &PlayerLoopShared,
        params: &PlayerSpawnParams,
        read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>>;
}

struct SpacetimeRuntime {
    /// Connection params handed to every player loop so it opens its own
    /// dedicated WebSocket. Multiplexing all players over one socket hit
    /// SpacetimeDB's per-client `incoming_queue_length` limit under load and
    /// silently dropped messages — see backends_spacetimedb.rs top-of-file.
    connect_params: backends_spacetimedb::SpacetimeConnectParams,
    sql_url: String,
    server_physics: bool,
}

impl BackendRuntime for SpacetimeRuntime {
    fn name(&self) -> &'static str {
        "spacetimedb"
    }

    fn spawn_player(
        &self,
        shared: &PlayerLoopShared,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(backends_spacetimedb::player_loop_spacetimedb(
            backends_spacetimedb::SpacetimePlayerLoop {
                connect_params: self.connect_params.clone(),
                idx: params.idx,
                entity_id: params.entity_id,
                total: params.desired_total,
                tick_interval: params.tick_interval,
                metrics: shared.metrics.clone(),
                action_metrics: shared.action_metrics.clone(),
                stop: params.stop,
                cluster_flag: shared.cluster_flag.clone(),
                server_physics: self.server_physics,
                positions: shared.positions.clone(),
                all_ids: shared.all_ids.clone(),
                total_players: shared.total_players.clone(),
                actions_per_sec: shared.actions_per_sec,
                burst: shared.burst,
                run_started: shared.run_started,
            },
        ))
    }

    fn spawn_read(
        &self,
        shared: &PlayerLoopShared,
        params: &PlayerSpawnParams,
        read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if read_rate <= 0.0 {
            return None;
        }
        Some(tokio::spawn(backends_spacetimedb::read_loop_spacetimedb(
            backends_spacetimedb::SpacetimeReadLoop {
                client: shared.http_client.clone(),
                sql_url: self.sql_url.clone(),
                read_rate,
                read_metrics: shared.read_metrics.clone(),
                stop: params.stop.clone(),
                player_idx: params.idx,
                positions: shared.positions.clone(),
            },
        )))
    }
}

struct ArcaneRuntime {
    endpoint: ArcaneEndpoint,
}

impl BackendRuntime for ArcaneRuntime {
    fn name(&self) -> &'static str {
        "arcane"
    }

    fn spawn_player(
        &self,
        shared: &PlayerLoopShared,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(backends_arcane::player_loop_arcane(
            backends_arcane::ArcanePlayerLoop {
                endpoint: self.endpoint.clone(),
                client: shared.http_client.clone(),
                idx: params.idx,
                entity_id: params.entity_id,
                total: params.desired_total,
                tick_interval: params.tick_interval,
                metrics: shared.metrics.clone(),
                read_metrics: shared.read_metrics.clone(),
                action_metrics: shared.action_metrics.clone(),
                stop: params.stop,
                cluster_flag: shared.cluster_flag.clone(),
                actions_per_sec: shared.actions_per_sec,
                burst: shared.burst,
                run_started: shared.run_started,
            },
        ))
    }

    fn spawn_read(
        &self,
        _shared: &PlayerLoopShared,
        _params: &PlayerSpawnParams,
        _read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }
}

async fn run_control_mode(cfg: Config, tick_interval: Duration) {
    let run_started = std::time::Instant::now();
    let stdb_base = cfg.spacetimedb_uri.trim_end_matches('/').to_string();
    let sql_url = format!("{}/v1/database/{}/sql", stdb_base, cfg.database);
    // SDK requires a ws:// or wss:// URI rather than http://.
    let ws_uri = stdb_base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);

    let metrics = Arc::new(Metrics::new());
    let action_metrics = Arc::new(Metrics::new());
    let read_metrics = Arc::new(Metrics::new());

    let backend_runtime: Arc<dyn BackendRuntime> = match cfg.backend {
        Backend::SpacetimeDb => Arc::new(SpacetimeRuntime {
            connect_params: backends_spacetimedb::SpacetimeConnectParams {
                ws_uri: ws_uri.clone(),
                database_name: cfg.database.clone(),
            },
            sql_url: sql_url.clone(),
            server_physics: cfg.server_physics,
        }),
        Backend::Arcane => {
            let endpoint = match &cfg.arcane_manager {
                Some(base) => ArcaneEndpoint::ManagerJoin {
                    base_url: base.clone(),
                },
                None => ArcaneEndpoint::SingleUrl(cfg.arcane_ws.clone()),
            };
            Arc::new(ArcaneRuntime { endpoint })
        }
    };
    let backend_name = backend_runtime.name();
    eprintln!(
        "arcane-swarm(control): initial_players={}, max_players={}, tick_rate={}, mode={}, backend={}, server_physics={}, actions/s={:.1}, read_rate={:.1}Hz burst_enabled={} control_port={}",
        cfg.players,
        cfg.max_players,
        cfg.tick_rate,
        if cfg.mode == SwarmMode::Clustered { "clustered" } else { "spread" },
        backend_name,
        cfg.server_physics,
        cfg.actions_per_sec,
        cfg.read_rate,
        cfg.burst.enabled,
        cfg.control_port
    );

    let desired_players = Arc::new(AtomicU32::new(cfg.players.min(cfg.max_players)));
    let total_players_atomic = desired_players.clone();
    let stop_all = Arc::new(AtomicBool::new(false));

    let max_players = cfg.max_players;
    // One task slot per simulated player — the player loop carries both
    // movement and actions on the same WebSocket, so there are no separate
    // action-task slots.
    let mut handles: Vec<Option<tokio::task::JoinHandle<()>>> =
        (0..max_players as usize).map(|_| None).collect();

    let all_ids: Arc<Vec<uuid::Uuid>> =
        Arc::new((0..max_players).map(|_| uuid::Uuid::new_v4()).collect());

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(max_players as usize * 2)
        .build()
        .expect("HTTP client");

    let positions = Arc::new(backends_spacetimedb::SharedPositions::new(max_players));
    let cluster_flag = cfg.cluster_command.clone();

    // Precreate per-player stop flags so control tasks can stop everyone without synchronization.
    let player_stop_flags: Arc<Vec<Arc<AtomicBool>>> = Arc::new(
        (0..max_players)
            .map(|_| Arc::new(AtomicBool::new(true)))
            .collect(),
    );

    let set_stop_for_all = {
        let player_stop_flags = player_stop_flags.clone();
        let stop_all = stop_all.clone();
        move || {
            stop_all.store(true, Ordering::Relaxed);
            for flag in player_stop_flags.iter() {
                flag.store(true, Ordering::Relaxed);
            }
        }
    };

    let loop_shared = PlayerLoopShared {
        http_client: http_client.clone(),
        metrics: metrics.clone(),
        read_metrics: read_metrics.clone(),
        action_metrics: action_metrics.clone(),
        cluster_flag: cluster_flag.clone(),
        positions: positions.clone(),
        all_ids: all_ids.clone(),
        total_players: total_players_atomic.clone(),
        actions_per_sec: cfg.actions_per_sec,
        burst: cfg.burst,
        run_started,
    };

    // Spawn initial players (and their optional read task — actions share the
    // movement WebSocket and are driven from inside the player loop).
    let initial = desired_players.load(Ordering::Relaxed) as usize;
    let mut current_spawned: usize = 0;

    while current_spawned < initial {
        let idx = current_spawned;
        let desired_total = desired_players.load(Ordering::Relaxed);
        let mut kit = ControlSpawnKit {
            handles: &mut handles,
            player_stop_flags: &player_stop_flags,
            loop_shared: &loop_shared,
            backend_runtime: &backend_runtime,
            tick_interval,
            read_rate: cfg.read_rate,
        };
        spawn_control_mode_player(&mut kit, idx, desired_total);
        current_spawned += 1;
    }

    // TCP control server
    let control_task = if cfg.control_port > 0 {
        let desired_players = desired_players.clone();
        let stop_all = stop_all.clone();
        let player_stop_flags = player_stop_flags.clone();
        let metrics = metrics.clone();
        let action_metrics = action_metrics.clone();
        let read_metrics = read_metrics.clone();
        Some(tokio::spawn(async move {
            use tokio::net::TcpListener;

            let listener = TcpListener::bind(("127.0.0.1", cfg.control_port))
                .await
                .expect("bind control port");
            eprintln!("  [control] listening on 127.0.0.1:{}", cfg.control_port);

            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                let desired_players = desired_players.clone();
                let stop_all = stop_all.clone();
                let player_stop_flags = player_stop_flags.clone();
                let metrics = metrics.clone();
                let action_metrics = action_metrics.clone();
                let read_metrics = read_metrics.clone();

                tokio::spawn(async move {
                    let _ = handle_control_connection(
                        stream,
                        desired_players,
                        stop_all,
                        player_stop_flags,
                        metrics,
                        action_metrics,
                        read_metrics,
                    )
                    .await;
                });
            }
        }))
    } else {
        None
    };

    while !stop_all.load(Ordering::Relaxed) {
        let target = desired_players.load(Ordering::Relaxed).min(max_players) as usize;
        if target > current_spawned {
            for idx in current_spawned..target {
                let desired_total = desired_players.load(Ordering::Relaxed);
                let mut kit = ControlSpawnKit {
                    handles: &mut handles,
                    player_stop_flags: &player_stop_flags,
                    loop_shared: &loop_shared,
                    backend_runtime: &backend_runtime,
                    tick_interval,
                    read_rate: cfg.read_rate,
                };
                spawn_control_mode_player(&mut kit, idx, desired_total);
            }
            current_spawned = target;
        } else if target < current_spawned {
            // Decreasing players stops those tasks, but the server may retain entities in-memory.
            // This benchmark currently only increases, so this path is mostly for hygiene.
            for idx in target..current_spawned {
                player_stop_flags[idx].store(true, Ordering::Relaxed);
            }
            current_spawned = target;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    set_stop_for_all();
    if let Some(h) = control_task {
        let _ = h.await;
    }
    eprintln!("arcane-swarm(control): exiting.");

    async fn handle_control_connection(
        stream: tokio::net::TcpStream,
        desired_players: Arc<AtomicU32>,
        stop_all: Arc<AtomicBool>,
        player_stop_flags: Arc<Vec<Arc<AtomicBool>>>,
        metrics: Arc<Metrics>,
        action_metrics: Arc<Metrics>,
        read_metrics: Arc<Metrics>,
    ) -> Result<(), String> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let mut reader = BufReader::new(stream);
        let mut buf = String::new();

        loop {
            buf.clear();
            let n = reader
                .read_line(&mut buf)
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            let line = buf.trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let cmd = parts.next().unwrap_or("");
            match cmd {
                "SET_PLAYERS" => {
                    if let Some(n) = parts.next() {
                        if let Ok(v) = n.parse::<u32>() {
                            desired_players.store(v, Ordering::Relaxed);
                        }
                    }
                }
                "RESET" => {
                    let _ = metrics.snapshot_and_reset();
                    let _ = action_metrics.snapshot_and_reset();
                    let _ = read_metrics.snapshot_and_reset();
                }
                "REPORT" => {
                    let players = desired_players.load(Ordering::Relaxed);
                    let snap = metrics.snapshot_and_reset();
                    let total_calls = snap.ok + snap.err;
                    let lat_avg_ms = if snap.latency_samples > 0 {
                        snap.latency_sum_us as f64 / 1000.0 / snap.latency_samples as f64
                    } else {
                        0.0
                    };
                    eprintln!(
                        "FINAL: players={} total_calls={} total_oks={} total_errs={} lat_avg_ms={:.2} err_json={}",
                        players,
                        total_calls,
                        snap.ok,
                        snap.err,
                        lat_avg_ms,
                        snap.errors.to_json(),
                    );
                }
                "QUIT" => {
                    stop_all.store(true, Ordering::Relaxed);
                    for flag in player_stop_flags.iter() {
                        flag.store(true, Ordering::Relaxed);
                    }
                    break;
                }
                _ => {
                    // Ignore unknown command.
                }
            }
        }
        Ok(())
    }
}

// -- Main ------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cfg = parse_args();
    let run_started = std::time::Instant::now();
    let tick_interval = Duration::from_micros(1_000_000 / cfg.tick_rate as u64);
    let stdb_base = cfg.spacetimedb_uri.trim_end_matches('/').to_string();
    let sql_url = format!("{}/v1/database/{}/sql", stdb_base, cfg.database);
    let ws_uri = stdb_base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);

    let metrics = Arc::new(Metrics::new());
    let action_metrics = Arc::new(Metrics::new());
    let read_metrics = Arc::new(Metrics::new());

    let backend_runtime: Arc<dyn BackendRuntime> = match cfg.backend {
        Backend::SpacetimeDb => Arc::new(SpacetimeRuntime {
            connect_params: backends_spacetimedb::SpacetimeConnectParams {
                ws_uri: ws_uri.clone(),
                database_name: cfg.database.clone(),
            },
            sql_url: sql_url.clone(),
            server_physics: cfg.server_physics,
        }),
        Backend::Arcane => {
            let endpoint = match &cfg.arcane_manager {
                Some(base) => ArcaneEndpoint::ManagerJoin {
                    base_url: base.clone(),
                },
                None => ArcaneEndpoint::SingleUrl(cfg.arcane_ws.clone()),
            };
            Arc::new(ArcaneRuntime { endpoint })
        }
    };
    let backend_name = backend_runtime.name();

    if cfg.run_forever || cfg.control_port > 0 {
        run_control_mode(cfg, tick_interval).await;
        return;
    }

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

    let positions = Arc::new(backends_spacetimedb::SharedPositions::new(cfg.players));

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

    if cfg.read_rate > 0.0 && backend_name == "spacetimedb" {
        eprintln!(
            "  Read simulation: spatial queries (radius={}) at {} Hz per player ({} total queries/s)",
            VISIBILITY_RADIUS,
            cfg.read_rate,
            cfg.read_rate as u64 * cfg.players as u64
        );
    }

    let loop_shared = PlayerLoopShared {
        http_client: http_client.clone(),
        metrics: metrics.clone(),
        read_metrics: read_metrics.clone(),
        action_metrics: action_metrics.clone(),
        cluster_flag: cfg.cluster_command.clone(),
        positions: positions.clone(),
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
