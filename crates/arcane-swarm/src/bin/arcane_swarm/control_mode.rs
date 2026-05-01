//! Control mode orchestration for dynamic player spawning via TCP commands.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

use arcane_swarm::{sample_proc, Config, Metrics};

use crate::runtime::BackendRuntime;
use crate::spawn_context::{spawn_control_mode_player, ControlSpawnKit, PlayerLoopShared};

pub(crate) async fn run_control_mode(cfg: Config, tick_interval: Duration) {
    let run_started = std::time::Instant::now();
    let stdb_base = cfg.spacetimedb_uri.trim_end_matches('/').to_string();
    let ws_uri = stdb_base
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);

    let metrics = Arc::new(Metrics::new());
    let action_metrics = Arc::new(Metrics::new());
    let read_metrics = Arc::new(Metrics::new());

    let backend_runtime: Arc<dyn BackendRuntime> =
        crate::create_backend_runtime(&cfg, ws_uri.clone());
    let backend_name = backend_runtime.name();
    eprintln!(
        "arcane-swarm(control): initial_players={}, max_players={}, tick_rate={}, mode={}, backend={}, server_physics={}, actions/s={:.1}, read_rate={:.1}Hz burst_enabled={} control_port={}",
        cfg.players,
        cfg.max_players,
        cfg.tick_rate,
        if cfg.mode == arcane_swarm::SwarmMode::Clustered { "clustered" } else { "spread" },
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

    {
        let stop_all = stop_all.clone();
        let total_players_atomic = total_players_atomic.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(1));
            interval.tick().await;
            let mut prev_cpu_ticks: Option<u64> = None;
            loop {
                interval.tick().await;
                if stop_all.load(Ordering::Relaxed) {
                    break;
                }
                let (drv_cpu_pct, drv_rss_mb) = match sample_proc() {
                    Some((cpu_ticks, rss_kb)) => {
                        let cpu_pct = match prev_cpu_ticks {
                            Some(prev) => cpu_ticks.saturating_sub(prev) as f64,
                            None => 0.0,
                        };
                        prev_cpu_ticks = Some(cpu_ticks);
                        (Some(cpu_pct), Some(rss_kb as f64 / 1024.0))
                    }
                    None => (None, None),
                };
                if let (Some(cpu), Some(rss)) = (drv_cpu_pct, drv_rss_mb) {
                    let elapsed = run_started.elapsed().as_secs();
                    let players = total_players_atomic.load(Ordering::Relaxed);
                    eprintln!(
                        "[{:>4}s] [driver] players={} drv_cpu={:.1}% drv_rss={:.0}MB",
                        elapsed, players, cpu, rss,
                    );
                }
            }
        });
    }

    let max_players = cfg.max_players;
    let mut handles: Vec<Option<tokio::task::JoinHandle<()>>> =
        (0..max_players as usize).map(|_| None).collect();

    let all_ids: Arc<Vec<uuid::Uuid>> =
        Arc::new((0..max_players).map(|_| uuid::Uuid::new_v4()).collect());

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(max_players as usize * 2)
        .build()
        .expect("HTTP client");

    let cluster_flag = cfg.cluster_command.clone();

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
        all_ids: all_ids.clone(),
        total_players: total_players_atomic.clone(),
        actions_per_sec: cfg.actions_per_sec,
        burst: cfg.burst,
        run_started,
    };

    let initial = desired_players.load(Ordering::Relaxed) as usize;
    let mut current_spawned: usize = 0;
    let inter_spawn = Duration::from_millis(cfg.inter_spawn_delay_ms as u64);

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
        if cfg.inter_spawn_delay_ms > 0 {
            tokio::time::sleep(inter_spawn).await;
        }
    }

    let control_task = if cfg.control_port > 0 {
        let desired_players = desired_players.clone();
        let stop_all = stop_all.clone();
        let player_stop_flags = player_stop_flags.clone();
        let metrics = metrics.clone();
        let action_metrics = action_metrics.clone();
        let read_metrics = read_metrics.clone();
        let backend_runtime = backend_runtime.clone();
        let max_players_per_driver = cfg.max_players_per_driver;
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
                let backend_runtime = backend_runtime.clone();

                tokio::spawn(async move {
                    let _ = handle_control_connection(
                        stream,
                        desired_players,
                        stop_all,
                        player_stop_flags,
                        metrics,
                        action_metrics,
                        read_metrics,
                        backend_runtime,
                        max_players_per_driver,
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
                if cfg.inter_spawn_delay_ms > 0 {
                    tokio::time::sleep(inter_spawn).await;
                }
            }
            current_spawned = target;
        } else if target < current_spawned {
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
}

#[allow(clippy::too_many_arguments)]
async fn handle_control_connection(
    stream: tokio::net::TcpStream,
    desired_players: Arc<AtomicU32>,
    stop_all: Arc<AtomicBool>,
    player_stop_flags: Arc<Vec<Arc<AtomicBool>>>,
    metrics: Arc<Metrics>,
    action_metrics: Arc<Metrics>,
    read_metrics: Arc<Metrics>,
    backend_runtime: Arc<dyn BackendRuntime>,
    max_players_per_driver: u32,
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
                        let target = if max_players_per_driver > 0 && v > max_players_per_driver {
                            eprintln!(
                                "  [cap] SET_PLAYERS desired={} cap={} — refusing to spawn beyond cap; provision more drivers",
                                v, max_players_per_driver
                            );
                            max_players_per_driver
                        } else {
                            v
                        };
                        desired_players.store(target, Ordering::Relaxed);
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
                let wire_avg_ms = if snap.wire_latency_samples > 0 {
                    snap.avg_wire_latency_us as f64 / 1000.0
                } else {
                    0.0
                };
                let drain_avg_ms = if snap.drain_latency_samples > 0 {
                    snap.avg_drain_latency_us as f64 / 1000.0
                } else {
                    0.0
                };
                let (cache_hits, cache_misses) = backend_runtime.snapshot_cache_counters();
                let cache_hit_pct = if cache_hits + cache_misses > 0 {
                    100.0 * cache_hits as f64 / (cache_hits + cache_misses) as f64
                } else {
                    0.0
                };
                eprintln!(
                    "FINAL: players={} total_calls={} total_oks={} total_errs={} lat_avg_ms={:.2} wire_avg_ms={:.2} drain_avg_ms={:.2} wire_samples={} drain_samples={} cache_hits={} cache_misses={} cache_hit_pct={:.1} err_json={}",
                    players,
                    total_calls,
                    snap.ok,
                    snap.err,
                    lat_avg_ms,
                    wire_avg_ms,
                    drain_avg_ms,
                    snap.wire_latency_samples,
                    snap.drain_latency_samples,
                    cache_hits,
                    cache_misses,
                    cache_hit_pct,
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
            _ => {}
        }
    }
    Ok(())
}
