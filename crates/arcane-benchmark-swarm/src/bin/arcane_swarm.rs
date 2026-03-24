//! Headless client swarm for load testing SpacetimeDB and Arcane+SpacetimeDB.
//!
//! Each logical player is a separate async task simulating a real game client.
//!
//! Build:  cargo build -p arcane-demo --bin arcane-swarm --features swarm --release
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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time;
#[path = "arcane_swarm/backends_arcane.rs"]
mod runtime_arcane;
#[path = "arcane_swarm/backends_spacetimedb.rs"]
mod runtime_spacetime;

const VISIBILITY_RADIUS: f64 = 1500.0;

// -- CLI -------------------------------------------------------------------

/// How each player resolves the Arcane cluster WebSocket URL.
#[derive(Clone)]
enum ArcaneEndpoint {
    /// All players connect to this single URL (one cluster server).
    SingleUrl(String),
    /// Each player does GET base/join; manager returns server_host:port (round-robin across clusters).
    ManagerJoin { base_url: String },
}

#[derive(Clone)]
struct Config {
    backend: Backend,
    spacetimedb_uri: String,
    database: String,
    arcane_ws: String,
    arcane_manager: Option<String>,
    players: u32,
    /// Max players the swarm is allowed to spawn (used for incremental SET_PLAYERS without reallocating).
    max_players: u32,
    tick_rate: u32,
    duration_secs: u64,
    mode: SwarmMode,
    csv_path: Option<String>,
    cluster_command: Arc<AtomicBool>,
    actions_per_sec: f64,
    read_rate: f64,
    /// If true for the `spacetimedb` backend:
    /// - first tick uses `update_player` (initial position spawn)
    /// - subsequent ticks use `update_player_input` (direction only)
    /// This matches the SpacetimeDB module's server_physics mode.
    server_physics: bool,
    /// If true, ignore duration and run until QUIT is received on the control port.
    run_forever: bool,
    /// If > 0, enable a line-based TCP control server on 127.0.0.1:control_port.
    /// Commands:
    ///  - SET_PLAYERS <n>
    ///  - RESET
    ///  - REPORT
    ///  - QUIT
    control_port: u16,
}

#[derive(Clone, Copy, PartialEq)]
enum SwarmMode { Spread, Clustered }

#[derive(Clone, Copy, PartialEq)]
enum Backend { SpacetimeDb, Arcane }

trait BackendRuntime: Send + Sync {
    fn name(&self) -> &'static str;
    fn spawn_player(
        &self,
        idx: u32,
        desired_total: u32,
        tick_interval: Duration,
        http_client: reqwest::Client,
        metrics: Arc<Metrics>,
        read_metrics: Arc<Metrics>,
        stop: Arc<AtomicBool>,
        cluster_flag: Arc<AtomicBool>,
        positions: Arc<runtime_spacetime::SharedPositions>,
    ) -> tokio::task::JoinHandle<()>;

    fn spawn_read(
        &self,
        idx: u32,
        http_client: reqwest::Client,
        read_rate: f64,
        read_metrics: Arc<Metrics>,
        stop: Arc<AtomicBool>,
        positions: Arc<runtime_spacetime::SharedPositions>,
    ) -> Option<tokio::task::JoinHandle<()>>;
}

struct SpacetimeRuntime {
    url_update_player: String,
    url_update_player_input: String,
    url_remove: String,
    sql_url: String,
    server_physics: bool,
}

impl BackendRuntime for SpacetimeRuntime {
    fn name(&self) -> &'static str { "spacetimedb" }

    fn spawn_player(
        &self,
        idx: u32,
        desired_total: u32,
        tick_interval: Duration,
        http_client: reqwest::Client,
        metrics: Arc<Metrics>,
        _read_metrics: Arc<Metrics>,
        stop: Arc<AtomicBool>,
        cluster_flag: Arc<AtomicBool>,
        positions: Arc<runtime_spacetime::SharedPositions>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(runtime_spacetime::player_loop_spacetimedb(
            http_client,
            self.url_update_player.clone(),
            self.url_update_player_input.clone(),
            self.url_remove.clone(),
            idx,
            desired_total,
            tick_interval,
            metrics,
            stop,
            cluster_flag,
            self.server_physics,
            positions,
        ))
    }

    fn spawn_read(
        &self,
        idx: u32,
        http_client: reqwest::Client,
        read_rate: f64,
        read_metrics: Arc<Metrics>,
        stop: Arc<AtomicBool>,
        positions: Arc<runtime_spacetime::SharedPositions>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if read_rate <= 0.0 {
            return None;
        }
        Some(tokio::spawn(runtime_spacetime::read_loop_spacetimedb(
            http_client,
            self.sql_url.clone(),
            read_rate,
            read_metrics,
            stop,
            idx,
            positions,
        )))
    }
}

struct ArcaneRuntime {
    endpoint: ArcaneEndpoint,
}

impl BackendRuntime for ArcaneRuntime {
    fn name(&self) -> &'static str { "arcane" }

    fn spawn_player(
        &self,
        idx: u32,
        desired_total: u32,
        tick_interval: Duration,
        http_client: reqwest::Client,
        metrics: Arc<Metrics>,
        read_metrics: Arc<Metrics>,
        stop: Arc<AtomicBool>,
        cluster_flag: Arc<AtomicBool>,
        _positions: Arc<runtime_spacetime::SharedPositions>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(runtime_arcane::player_loop_arcane(
            self.endpoint.clone(),
            http_client,
            idx,
            desired_total,
            tick_interval,
            metrics,
            read_metrics,
            stop,
            cluster_flag,
        ))
    }

    fn spawn_read(
        &self,
        _idx: u32,
        _http_client: reqwest::Client,
        _read_rate: f64,
        _read_metrics: Arc<Metrics>,
        _stop: Arc<AtomicBool>,
        _positions: Arc<runtime_spacetime::SharedPositions>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }
}

fn parse_args() -> Config {
    let mut players: u32 = 100;
    let mut max_players: u32 = 0;
    let mut tick_rate: u32 = 20;
    let mut duration_secs: u64 = 60;
    let mut mode = SwarmMode::Spread;
    let mut csv_path: Option<String> = None;
    let mut uri = std::env::var("SPACETIMEDB_URI").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let mut database = std::env::var("DATABASE_NAME").unwrap_or_else(|_| "arcane".into());
    let mut arcane_ws = std::env::var("ARCANE_WS").unwrap_or_else(|_| "ws://127.0.0.1:8080".into());
    let mut arcane_manager: Option<String> = std::env::var("ARCANE_MANAGER").ok();
    let mut backend = Backend::SpacetimeDb;
    let mut actions_per_sec: f64 = 0.0;
    let mut read_rate: f64 = 5.0;
    let mut server_physics: bool = false;
    let mut run_forever: bool = false;
    let mut control_port: u16 = 0;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--players" | "-n" => { i += 1; players = args[i].parse().unwrap_or(players); }
            "--tick-rate" | "-t" => { i += 1; tick_rate = args[i].parse().unwrap_or(tick_rate); }
            "--duration" | "-d" => { i += 1; duration_secs = args[i].parse().unwrap_or(duration_secs); }
            "--mode" | "-m" => { i += 1; mode = if args[i] == "clustered" { SwarmMode::Clustered } else { SwarmMode::Spread }; }
            "--csv" => { i += 1; csv_path = Some(args[i].clone()); }
            "--uri" => { i += 1; uri = args[i].clone(); }
            "--database" | "--db" => { i += 1; database = args[i].clone(); }
            "--arcane-ws" => { i += 1; arcane_ws = args[i].clone(); }
            "--arcane-manager" => { i += 1; arcane_manager = Some(args[i].clone()); }
            "--backend" | "-b" => { i += 1; backend = if args[i] == "arcane" { Backend::Arcane } else { Backend::SpacetimeDb }; }
            "--actions-per-sec" | "--aps" => { i += 1; actions_per_sec = args[i].parse().unwrap_or(0.0); }
            "--read-rate" => { i += 1; read_rate = args[i].parse().unwrap_or(5.0); }
            "--server-physics" => { server_physics = true; }
            "--max-players" => { i += 1; max_players = args[i].parse().unwrap_or(players); }
            "--run-forever" => { run_forever = true; }
            "--control-port" => { i += 1; control_port = args[i].parse().unwrap_or(0); }
            "--help" | "-h" => {
                eprintln!("arcane-swarm: headless client swarm\n");
                eprintln!("  --backend MODE        spacetimedb | arcane (default spacetimedb)");
                eprintln!("  --players N            number of simulated players (default 100)");
                eprintln!("  --max-players N       max players for incremental mode (default = --players)");
                eprintln!("  --tick-rate HZ         ticks per second per player (default 20)");
                eprintln!("  --duration SECS        how long to run (default 60)");
                eprintln!("  --mode MODE            spread | clustered (default spread)");
                eprintln!("  --actions-per-sec N    persistent actions per player per second (default 0)");
                eprintln!("  --read-rate HZ         world-state reads per player per second (default 5)");
                eprintln!("  --server-physics      for spacetimedb backend: use update_player_input for movement");
                eprintln!("  --run-forever          keep running until QUIT");
                eprintln!("  --control-port PORT   enable TCP control server at 127.0.0.1:PORT");
                eprintln!("  --csv PATH             write metrics CSV to this file");
                eprintln!("  --uri URL              SpacetimeDB URI (default http://127.0.0.1:3000)");
                eprintln!("  --database NAME        database name (default arcane)");
                eprintln!("  --arcane-ws URL        Arcane cluster WebSocket (default ws://127.0.0.1:8080)");
                eprintln!("  --arcane-manager URL   Use manager /join for cluster assignment (round-robin)");
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }

    Config {
        backend,
        spacetimedb_uri: uri,
        database,
        arcane_ws,
        arcane_manager,
        players,
        max_players: if max_players == 0 { players } else { max_players },
        tick_rate: tick_rate.max(1),
        duration_secs,
        mode,
        csv_path,
        cluster_command: Arc::new(AtomicBool::new(mode == SwarmMode::Clustered)),
        actions_per_sec,
        read_rate,
        server_physics,
        run_forever,
        control_port,
    }
}

// -- Metrics ---------------------------------------------------------------

struct Metrics {
    ok: AtomicU64,
    err: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_max_us: AtomicU64,
    latency_samples: AtomicU64,
    bytes: AtomicU64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            ok: AtomicU64::new(0),
            err: AtomicU64::new(0),
            latency_sum_us: AtomicU64::new(0),
            latency_max_us: AtomicU64::new(0),
            latency_samples: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    fn record_ok(&self, latency: Duration) {
        self.ok.fetch_add(1, Ordering::Relaxed);
        let us = latency.as_micros() as u64;
        self.latency_sum_us.fetch_add(us, Ordering::Relaxed);
        self.latency_samples.fetch_add(1, Ordering::Relaxed);
        self.latency_max_us.fetch_max(us, Ordering::Relaxed);
    }

    fn record_ok_bytes(&self, latency: Duration, bytes: u64) {
        self.record_ok(latency);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn record_err(&self) {
        self.err.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot_and_reset(&self) -> MetricsSnapshot {
        let ok = self.ok.swap(0, Ordering::Relaxed);
        let err = self.err.swap(0, Ordering::Relaxed);
        let sum = self.latency_sum_us.swap(0, Ordering::Relaxed);
        let max = self.latency_max_us.swap(0, Ordering::Relaxed);
        let n = self.latency_samples.swap(0, Ordering::Relaxed);
        let avg = if n > 0 { sum / n } else { 0 };
        let bytes = self.bytes.swap(0, Ordering::Relaxed);
        MetricsSnapshot { ok, err, avg_latency_us: avg, max_latency_us: max, latency_sum_us: sum, latency_samples: n, bytes }
    }
}

struct MetricsSnapshot {
    ok: u64,
    err: u64,
    avg_latency_us: u64,
    max_latency_us: u64,
    latency_sum_us: u64,
    latency_samples: u64,
    bytes: u64,
}

// -- Arcane PLAYER_STATE JSON helper ----------------------------------------

fn player_state_json(id: &uuid::Uuid, x: f64, y: f64, z: f64, vx: f64, vy: f64, vz: f64) -> String {
    format!(
        r#"{{"type":"PLAYER_STATE","entity_id":"{}","position":{{"x":{},"y":{},"z":{}}},"velocity":{{"x":{},"y":{},"z":{}}}}}"#,
        id, x, y, z, vx, vy, vz
    )
}

// -- Movement --------------------------------------------------------------

const WORLD_SIZE: f64 = 5000.0;
const WORLD_CENTER: f64 = 2500.0;
const MOVE_SPEED: f64 = 600.0;
const CLUSTER_RADIUS: f64 = 300.0;
const SPREAD_MARGIN: f64 = 200.0;

struct Player {
    id: uuid::Uuid,
    x: f64, y: f64, z: f64,
    vx: f64, vy: f64, vz: f64,
    dir_x: f64, dir_z: f64,
    ticks_until_turn: u32,
}

impl Player {
    fn new(idx: u32, total: u32, clustered: bool) -> Self {
        let angle = (idx as f64 / total.max(1) as f64) * std::f64::consts::TAU;
        let radius = if clustered { CLUSTER_RADIUS } else { WORLD_SIZE * 0.35 };
        Self {
            id: uuid::Uuid::new_v4(),
            x: WORLD_CENTER + radius * angle.cos(),
            y: 0.0,
            z: WORLD_CENTER + radius * angle.sin(),
            vx: 0.0, vy: 0.0, vz: 0.0,
            dir_x: angle.cos(), dir_z: angle.sin(),
            ticks_until_turn: 60 + (idx % 80),
        }
    }

    fn tick(&mut self, tick_dt: f64, clustered: bool) {
        self.ticks_until_turn = self.ticks_until_turn.saturating_sub(1);
        if self.ticks_until_turn == 0 {
            let a = (self.id.as_bytes()[0] as f64 * 0.1 + self.x * 0.001).sin() * std::f64::consts::TAU;
            self.dir_x = a.cos();
            self.dir_z = a.sin();
            self.ticks_until_turn = 40 + ((self.id.as_bytes()[1] as u32) % 80);
        }
        let speed = MOVE_SPEED * tick_dt;
        self.vx = self.dir_x * speed;
        self.vz = self.dir_z * speed;
        self.x += self.vx;
        self.z += self.vz;

        let (min, max) = if clustered {
            (WORLD_CENTER - CLUSTER_RADIUS, WORLD_CENTER + CLUSTER_RADIUS)
        } else {
            (SPREAD_MARGIN, WORLD_SIZE - SPREAD_MARGIN)
        };
        if self.x < min { self.x = min; self.dir_x = self.dir_x.abs(); }
        if self.x > max { self.x = max; self.dir_x = -self.dir_x.abs(); }
        if self.z < min { self.z = min; self.dir_z = self.dir_z.abs(); }
        if self.z > max { self.z = max; self.dir_z = -self.dir_z.abs(); }
    }
}

// -- Metrics reporter ------------------------------------------------------

fn fmt_bytes(b: u64) -> String {
    if b >= 1_048_576 { format!("{:.1}MB", b as f64 / 1_048_576.0) }
    else if b >= 1024 { format!("{:.1}KB", b as f64 / 1024.0) }
    else { format!("{}B", b) }
}

async fn run_reporter(
    metrics: Arc<Metrics>,
    action_metrics: Arc<Metrics>,
    read_metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
    players: u32,
    backend_name: &str,
    actions_per_sec: f64,
    read_rate: f64,
    csv_file: Arc<tokio::sync::Mutex<Option<std::io::BufWriter<std::fs::File>>>>,
) {
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
            runtime_spacetime::empty_snapshot()
        };
        let a_ops = a.ok + a.err;
        total_action_calls += a.ok + a.err;
        total_action_oks += a.ok;
        total_action_errs += a.err;

        let mut line = format!(
            "[{:>4}s] [{}] players={} writes/s={} ok={} err={} lat={:.1}ms",
            elapsed, backend_name, players, w_ops, s.ok, s.err,
            s.avg_latency_us as f64 / 1000.0,
        );

        if has_reads {
            line.push_str(&format!(
                " | reads/s={} ok={} err={} lat={:.1}ms rx={}",
                r_ops, r.ok, r.err,
                r.avg_latency_us as f64 / 1000.0,
                fmt_bytes(r.bytes),
            ));
        }

        if has_actions {
            line.push_str(&format!(
                " | acts/s={} ok={} err={} lat={:.1}ms",
                a_ops, a.ok, a.err,
                a.avg_latency_us as f64 / 1000.0,
            ));
        }

        eprintln!("{}", line);

        if let Some(ref mut w) = *csv_file.lock().await {
            use std::io::Write;
            let _ = writeln!(w, "{},{},{},{},{},{:.2},{:.2},{},{},{},{:.2},{},{},{},{:.2}",
                elapsed, players,
                s.ok, s.err, w_ops, s.avg_latency_us as f64 / 1000.0, s.max_latency_us as f64 / 1000.0,
                r.ok, r.err, r_ops, r.avg_latency_us as f64 / 1000.0, r.bytes,
                a.ok, a.err, a.avg_latency_us as f64 / 1000.0,
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

async fn run_control_mode(cfg: Config, tick_interval: Duration) {
    let stdb_base = cfg.spacetimedb_uri.trim_end_matches('/').to_string();
    let base = format!("{}/v1/database/{}/call", stdb_base, cfg.database);
    let sql_url = format!("{}/v1/database/{}/sql", stdb_base, cfg.database);
    let backend_runtime: Arc<dyn BackendRuntime> = match cfg.backend {
        Backend::SpacetimeDb => Arc::new(SpacetimeRuntime {
            url_update_player: format!("{}/update_player", base),
            url_update_player_input: format!("{}/update_player_input", base),
            url_remove: format!("{}/remove_player", base),
            sql_url: sql_url.clone(),
            server_physics: cfg.server_physics,
        }),
        Backend::Arcane => {
            let endpoint = match &cfg.arcane_manager {
                Some(base) => ArcaneEndpoint::ManagerJoin { base_url: base.clone() },
                None => ArcaneEndpoint::SingleUrl(cfg.arcane_ws.clone()),
            };
            Arc::new(ArcaneRuntime { endpoint })
        }
    };
    let backend_name = backend_runtime.name();
    eprintln!(
        "arcane-swarm(control): initial_players={}, max_players={}, tick_rate={}, mode={}, backend={}, server_physics={}, actions/s={:.1}, read_rate={:.1}Hz control_port={}",
        cfg.players,
        cfg.max_players,
        cfg.tick_rate,
        if cfg.mode == SwarmMode::Clustered { "clustered" } else { "spread" },
        backend_name,
        cfg.server_physics,
        cfg.actions_per_sec,
        cfg.read_rate,
        cfg.control_port
    );

    let desired_players = Arc::new(AtomicU32::new(cfg.players.min(cfg.max_players)));
    let total_players_atomic = desired_players.clone();
    let stop_all = Arc::new(AtomicBool::new(false));

    let metrics = Arc::new(Metrics::new());
    let action_metrics = Arc::new(Metrics::new());
    let read_metrics = Arc::new(Metrics::new());

    let max_players = cfg.max_players;
    // 0..max => player tasks, max..2*max => action tasks
    let mut handles: Vec<Option<tokio::task::JoinHandle<()>>> = (0..(max_players as usize * 2)).map(|_| None).collect();

    let all_ids: Arc<Vec<uuid::Uuid>> = Arc::new((0..max_players).map(|_| uuid::Uuid::new_v4()).collect());

    let action_urls = runtime_spacetime::ActionUrls {
        pickup: format!("{}/pickup_item", base),
        use_item: format!("{}/use_item", base),
        interact: format!("{}/player_interact", base),
    };

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(max_players as usize * 2)
        .build()
        .expect("HTTP client");

    let positions = Arc::new(runtime_spacetime::SharedPositions::new(max_players));
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

    // Spawn initial players (and their optional read/action tasks).
    let initial = desired_players.load(Ordering::Relaxed) as usize;
    let mut current_spawned: usize = 0;

    let spawn_player = |idx: usize,
                         desired_total: u32,
                         handles: &mut Vec<Option<tokio::task::JoinHandle<()>>>,
                         player_stop_flags: &Arc<Vec<Arc<AtomicBool>>>,
                         http_client: &reqwest::Client,
                         metrics: &Arc<Metrics>,
                         action_metrics: &Arc<Metrics>,
                         read_metrics: &Arc<Metrics>,
                         positions: &Arc<runtime_spacetime::SharedPositions>,
                         cluster_flag: &Arc<AtomicBool>,
                         backend_runtime: &Arc<dyn BackendRuntime>| {
        player_stop_flags[idx].store(false, Ordering::Relaxed);
        let stop = player_stop_flags[idx].clone();
        handles[idx] = Some(backend_runtime.spawn_player(
            idx as u32,
            desired_total,
            tick_interval,
            http_client.clone(),
            metrics.clone(),
            read_metrics.clone(),
            stop.clone(),
            cluster_flag.clone(),
            positions.clone(),
        ));
        if let Some(_read_handle) = backend_runtime.spawn_read(
            idx as u32,
            http_client.clone(),
            cfg.read_rate,
            read_metrics.clone(),
            stop.clone(),
            positions.clone(),
        ) {
            // Read task is intentionally detached.
        }

        // Optional actions per player.
        if cfg.actions_per_sec > 0.0 {
            let action_idx = max_players as usize + idx;
            let player_id = all_ids[idx];
            let stop2 = player_stop_flags[idx].clone();
            let total_players = total_players_atomic.clone();
            let fut = runtime_spacetime::action_loop(
                http_client.clone(),
                runtime_spacetime::ActionUrls {
                    pickup: action_urls.pickup.clone(),
                    use_item: action_urls.use_item.clone(),
                    interact: action_urls.interact.clone(),
                },
                player_id,
                idx as u32,
                total_players,
                all_ids.clone(),
                cfg.actions_per_sec,
                action_metrics.clone(),
                stop2,
            );
            handles[action_idx] = Some(tokio::spawn(fut));
        }
    };

    while current_spawned < initial {
        let idx = current_spawned;
        let desired_total = desired_players.load(Ordering::Relaxed);
        spawn_player(
            idx,
            desired_total,
            &mut handles,
            &player_stop_flags,
            &http_client,
            &metrics,
            &action_metrics,
            &read_metrics,
            &positions,
            &cluster_flag,
            &backend_runtime,
        );
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

            let listener = TcpListener::bind(("127.0.0.1", cfg.control_port)).await
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
                spawn_player(
                    idx,
                    desired_total,
                    &mut handles,
                    &player_stop_flags,
                    &http_client,
                    &metrics,
                    &action_metrics,
                    &read_metrics,
                    &positions,
                    &cluster_flag,
                    &backend_runtime,
                );
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
            let n = reader.read_line(&mut buf).await.map_err(|e| e.to_string())?;
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
                        "FINAL: players={} total_calls={} total_oks={} total_errs={} lat_avg_ms={:.2}",
                        players, total_calls, snap.ok, snap.err, lat_avg_ms
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
    let tick_interval = Duration::from_micros(1_000_000 / cfg.tick_rate as u64);
    let stdb_base = cfg.spacetimedb_uri.trim_end_matches('/').to_string();
    let base = format!("{}/v1/database/{}/call", stdb_base, cfg.database);
    let sql_url = format!("{}/v1/database/{}/sql", stdb_base, cfg.database);
    let backend_runtime: Arc<dyn BackendRuntime> = match cfg.backend {
        Backend::SpacetimeDb => Arc::new(SpacetimeRuntime {
            url_update_player: format!("{}/update_player", base),
            url_update_player_input: format!("{}/update_player_input", base),
            url_remove: format!("{}/remove_player", base),
            sql_url: sql_url.clone(),
            server_physics: cfg.server_physics,
        }),
        Backend::Arcane => {
            let endpoint = match &cfg.arcane_manager {
                Some(base) => ArcaneEndpoint::ManagerJoin { base_url: base.clone() },
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

    let metrics = Arc::new(Metrics::new());
    let action_metrics = Arc::new(Metrics::new());
    let read_metrics = Arc::new(Metrics::new());
    let stop = Arc::new(AtomicBool::new(false));
    let total_players_atomic = Arc::new(AtomicU32::new(cfg.players));
    let mut handles = Vec::with_capacity(cfg.players as usize * 3);

    let all_ids: Arc<Vec<uuid::Uuid>> = Arc::new((0..cfg.players).map(|_| uuid::Uuid::new_v4()).collect());

    let action_urls = runtime_spacetime::ActionUrls {
        pickup: format!("{}/pickup_item", base),
        use_item: format!("{}/use_item", base),
        interact: format!("{}/player_interact", base),
    };

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(cfg.players as usize * 2)
        .build()
        .expect("HTTP client");

    let positions = Arc::new(runtime_spacetime::SharedPositions::new(cfg.players));

    if backend_name == "spacetimedb" {
        eprintln!("  SpacetimeDB: {}/database/{}", cfg.spacetimedb_uri, cfg.database);
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

    for i in 0..cfg.players {
        handles.push(backend_runtime.spawn_player(
            i,
            cfg.players,
            tick_interval,
            http_client.clone(),
            metrics.clone(),
            read_metrics.clone(),
            stop.clone(),
            cfg.cluster_command.clone(),
            positions.clone(),
        ));

        let _ = backend_runtime.spawn_read(
            i,
            http_client.clone(),
            cfg.read_rate,
            read_metrics.clone(),
            stop.clone(),
            positions.clone(),
        );
    }

    if cfg.actions_per_sec > 0.0 {
        for i in 0..cfg.players {
            let player_id = all_ids[i as usize];
            handles.push(tokio::spawn(runtime_spacetime::action_loop(
                http_client.clone(),
                runtime_spacetime::ActionUrls {
                    pickup: action_urls.pickup.clone(),
                    use_item: action_urls.use_item.clone(),
                    interact: action_urls.interact.clone(),
                },
                player_id,
                i,
                total_players_atomic.clone(),
                all_ids.clone(),
                cfg.actions_per_sec,
                action_metrics.clone(),
                stop.clone(),
            )));
        }
    }

    let csv_file = cfg.csv_path.as_ref().map(|p| {
        let f = std::fs::File::create(p).expect("cannot create CSV file");
        let mut w = std::io::BufWriter::new(f);
        use std::io::Write;
        writeln!(w, "elapsed_s,players,w_ok,w_err,w_ops,w_avg_ms,w_max_ms,r_ok,r_err,r_ops,r_avg_ms,r_bytes,a_ok,a_err,a_avg_ms").unwrap();
        w
    });
    let csv_file = Arc::new(tokio::sync::Mutex::new(csv_file));

    let reporter = tokio::spawn(run_reporter(
        metrics.clone(), action_metrics.clone(), read_metrics.clone(), stop.clone(),
        cfg.players, backend_name, cfg.actions_per_sec, cfg.read_rate, csv_file.clone(),
    ));

    time::sleep(Duration::from_secs(cfg.duration_secs)).await;
    eprintln!("\narcane-swarm: duration reached, shutting down...");
    stop.store(true, Ordering::Relaxed);

    for h in handles { let _ = h.await; }
    let _ = reporter.await;
    eprintln!("arcane-swarm: done.");
}
