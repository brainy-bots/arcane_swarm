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

use std::sync::atomic::{AtomicU64, AtomicI64, AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time;

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
}

#[derive(Clone, Copy, PartialEq)]
enum SwarmMode { Spread, Clustered }

#[derive(Clone, Copy, PartialEq)]
enum Backend { SpacetimeDb, Arcane }

fn parse_args() -> Config {
    let mut players: u32 = 100;
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
            "--help" | "-h" => {
                eprintln!("arcane-swarm: headless client swarm\n");
                eprintln!("  --backend MODE        spacetimedb | arcane (default spacetimedb)");
                eprintln!("  --players N            number of simulated players (default 100)");
                eprintln!("  --tick-rate HZ         ticks per second per player (default 20)");
                eprintln!("  --duration SECS        how long to run (default 60)");
                eprintln!("  --mode MODE            spread | clustered (default spread)");
                eprintln!("  --actions-per-sec N    persistent actions per player per second (default 0)");
                eprintln!("  --read-rate HZ         world-state reads per player per second (default 5)");
                eprintln!("  --server-physics      for spacetimedb backend: use update_player_input for movement");
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
        tick_rate: tick_rate.max(1),
        duration_secs,
        mode,
        csv_path,
        cluster_command: Arc::new(AtomicBool::new(mode == SwarmMode::Clustered)),
        actions_per_sec,
        read_rate,
        server_physics,
    }
}

#[derive(serde::Deserialize)]
struct ManagerJoinResponse {
    server_host: String,
    server_port: u16,
}

/// Resolve WebSocket URL for one player. If using manager, GET base/join and build ws://host:port.
async fn resolve_arcane_ws(
    endpoint: &ArcaneEndpoint,
    client: &reqwest::Client,
    player_idx: u32,
) -> String {
    match endpoint {
        ArcaneEndpoint::SingleUrl(url) => url.clone(),
        ArcaneEndpoint::ManagerJoin { base_url } => {
            let join_url = format!("{}/join", base_url.trim_end_matches('/'));
            const RETRIES: u32 = 3;
            for attempt in 0..RETRIES {
                match client.get(&join_url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(join) = resp.json::<ManagerJoinResponse>().await {
                            return format!("ws://{}:{}", join.server_host, join.server_port);
                        }
                    }
                    Ok(resp) => {
                        if player_idx == 0 && attempt == RETRIES - 1 {
                            let status = resp.status();
                            let t = resp.text().await.unwrap_or_default();
                            eprintln!("[player 0] manager join HTTP {}: {}", status, &t[..t.len().min(200)]);
                        }
                    }
                    Err(e) => {
                        if player_idx == 0 && attempt == RETRIES - 1 {
                            eprintln!("[player 0] manager join error (after {} attempts): {}", RETRIES, e);
                        }
                    }
                }
                if attempt < RETRIES - 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
            // Do not fall back to 8080: our clusters use 8090+. Falling back would send all traffic to one wrong process.
            if player_idx == 0 {
                eprintln!("[player 0] manager join failed after {} attempts; using invalid URL so this player fails (fix manager/ports).", RETRIES);
            }
            "ws://127.0.0.1:1".to_string()
        }
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

// -- SpacetimeDB JSON helpers ----------------------------------------------

fn uuid_json(id: &uuid::Uuid) -> u128 {
    u128::from_be_bytes(*id.as_bytes())
}

fn entity_json(id: &uuid::Uuid, x: f64, y: f64, z: f64, vx: f64, vy: f64, vz: f64) -> String {
    format!(
        r#"[{{"entity_id":{{"__uuid__":{}}},"x":{},"y":{},"z":{},"vx":{},"vy":{},"vz":{}}}]"#,
        uuid_json(id), x, y, z, vx, vy, vz
    )
}

fn player_input_json(id: &uuid::Uuid, dir_x: f64, dir_z: f64) -> String {
    // SpacetimeDB reducer signature: update_player_input(entity_id, dir_x, dir_z)
    // Spacetime's HTTP reducer endpoints expect a JSON array even for a single row.
    format!(
        r#"[{{"entity_id":{{"__uuid__":{}}},"dir_x":{},"dir_z":{}}}]"#,
        uuid_json(id), dir_x, dir_z
    )
}

fn pickup_item_json(owner_id: &uuid::Uuid, item_type: u32, quantity: u32) -> String {
    format!(
        r#"[{{"__uuid__":{}}},{},{}]"#,
        uuid_json(owner_id), item_type, quantity
    )
}

fn use_item_json(owner_id: &uuid::Uuid, item_type: u32) -> String {
    format!(
        r#"[{{"__uuid__":{}}},{}]"#,
        uuid_json(owner_id), item_type
    )
}

fn interact_json(actor_id: &uuid::Uuid, target_id: &uuid::Uuid, event_type: u32) -> String {
    format!(
        r#"[{{"__uuid__":{}}},{{"__uuid__":{}}},{}]"#,
        uuid_json(actor_id), uuid_json(target_id), event_type
    )
}

// -- Arcane PLAYER_STATE JSON helper ----------------------------------------

fn player_state_json(id: &uuid::Uuid, x: f64, y: f64, z: f64, vx: f64, vy: f64, vz: f64) -> String {
    format!(
        r#"{{"type":"PLAYER_STATE","entity_id":"{}","position":{{"x":{},"y":{},"z":{}}},"velocity":{{"x":{},"y":{},"z":{}}}}}"#,
        id, x, y, z, vx, vy, vz
    )
}

// -- Game action types for simulation --------------------------------------

#[derive(Clone, Copy)]
enum GameAction {
    PickupItem { item_type: u32, quantity: u32 },
    UseItem { item_type: u32 },
    Interact { target_idx: u32, event_type: u32 },
}

fn random_action(player_idx: u32, total_players: u32, tick: u64) -> GameAction {
    let seed = (player_idx as u64).wrapping_mul(31) ^ tick.wrapping_mul(7);
    match seed % 5 {
        0 => GameAction::PickupItem { item_type: (seed % 20) as u32, quantity: 1 + (seed % 5) as u32 },
        1 => GameAction::UseItem { item_type: (seed % 20) as u32 },
        _ => GameAction::Interact {
            target_idx: ((player_idx + 1 + (seed % total_players.max(2) as u64) as u32) % total_players),
            event_type: (seed % 4) as u32,
        },
    }
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

// -- Persistent-action loop (runs alongside movement for both backends) ----

struct ActionUrls {
    pickup: String,
    use_item: String,
    interact: String,
}

async fn action_loop(
    client: reqwest::Client,
    urls: ActionUrls,
    player_id: uuid::Uuid,
    player_idx: u32,
    total_players: u32,
    all_ids: Arc<Vec<uuid::Uuid>>,
    actions_per_sec: f64,
    action_metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
) {
    if actions_per_sec <= 0.0 { return; }
    let interval_us = (1_000_000.0 / actions_per_sec) as u64;
    let mut interval = time::interval(Duration::from_micros(interval_us));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut tick: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        tick += 1;
        let action = random_action(player_idx, total_players, tick);

        let (url, body) = match action {
            GameAction::PickupItem { item_type, quantity } => {
                (&urls.pickup, pickup_item_json(&player_id, item_type, quantity))
            }
            GameAction::UseItem { item_type } => {
                (&urls.use_item, use_item_json(&player_id, item_type))
            }
            GameAction::Interact { target_idx, event_type } => {
                let target = all_ids.get(target_idx as usize).copied().unwrap_or(player_id);
                (&urls.interact, interact_json(&player_id, &target, event_type))
            }
        };

        let t0 = Instant::now();
        match client.post(url.as_str()).header("Content-Type", "application/json").body(body).send().await {
            Ok(resp) if resp.status().is_success() => { action_metrics.record_ok(t0.elapsed()); }
            Ok(resp) => {
                action_metrics.record_err();
                if player_idx == 0 {
                    let s = resp.status();
                    let t = resp.text().await.unwrap_or_default();
                    eprintln!("[player 0 action] HTTP {}: {}", s, &t[..t.len().min(200)]);
                }
            }
            Err(_) => { action_metrics.record_err(); }
        }
    }
}

// -- Shared player positions for spatial read queries ----------------------

struct SharedPositions {
    xs: Vec<AtomicI64>,
    zs: Vec<AtomicI64>,
}

impl SharedPositions {
    fn new(count: u32) -> Self {
        let n = count as usize;
        Self {
            xs: (0..n).map(|_| AtomicI64::new(0)).collect(),
            zs: (0..n).map(|_| AtomicI64::new(0)).collect(),
        }
    }

    fn set(&self, idx: u32, x: f64, z: f64) {
        let i = idx as usize;
        self.xs[i].store(x as i64, Ordering::Relaxed);
        self.zs[i].store(z as i64, Ordering::Relaxed);
    }

    fn get(&self, idx: u32) -> (f64, f64) {
        let i = idx as usize;
        (self.xs[i].load(Ordering::Relaxed) as f64, self.zs[i].load(Ordering::Relaxed) as f64)
    }
}

// -- World-state read loop (SpacetimeDB SQL polling with spatial filter) ----

async fn read_loop_spacetimedb(
    client: reqwest::Client,
    sql_url: String,
    read_rate: f64,
    read_metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
    player_idx: u32,
    positions: Arc<SharedPositions>,
) {
    if read_rate <= 0.0 { return; }
    let interval_us = (1_000_000.0 / read_rate) as u64;
    let mut interval = time::interval(Duration::from_micros(interval_us));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        let (px, pz) = positions.get(player_idx);
        let query = format!(
            "SELECT * FROM entity WHERE x >= {} AND x <= {} AND z >= {} AND z <= {}",
            (px - VISIBILITY_RADIUS) as i64,
            (px + VISIBILITY_RADIUS) as i64,
            (pz - VISIBILITY_RADIUS) as i64,
            (pz + VISIBILITY_RADIUS) as i64,
        );
        let t0 = Instant::now();
        match client.post(&sql_url)
            .header("Content-Type", "text/plain")
            .body(query)
            .send().await
        {
            Ok(resp) if resp.status().is_success() => {
                let bytes = resp.bytes().await.map(|b| b.len() as u64).unwrap_or(0);
                read_metrics.record_ok_bytes(t0.elapsed(), bytes);
            }
            Ok(resp) => {
                read_metrics.record_err();
                if player_idx == 0 {
                    let s = resp.status();
                    let t = resp.text().await.unwrap_or_default();
                    eprintln!("[player 0 read] HTTP {}: {}", s, &t[..t.len().min(200)]);
                }
            }
            Err(_) => { read_metrics.record_err(); }
        }
    }
}

// -- SpacetimeDB player task -----------------------------------------------

async fn player_loop_spacetimedb(
    client: reqwest::Client,
    url_update_player: String,
    url_update_player_input: String,
    url_remove: String,
    idx: u32, total: u32,
    tick_interval: Duration,
    metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
    cluster_flag: Arc<AtomicBool>,
    server_physics: bool,
    positions: Arc<SharedPositions>,
) {
    let clustered = cluster_flag.load(Ordering::Relaxed);
    let mut player = Player::new(idx, total, clustered);
    positions.set(idx, player.x, player.z);
    let tick_dt = tick_interval.as_secs_f64();
    let mut interval = time::interval(tick_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let mut first_tick = true;
    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        player.tick(tick_dt, cluster_flag.load(Ordering::Relaxed));
        positions.set(idx, player.x, player.z);
        let t0 = Instant::now();
        if server_physics {
            // Matches SpacetimeDB module's server_physics mode:
            // - First tick: spawn/update initial position via update_player
            // - Subsequent ticks: advance via direction-only inputs (update_player_input)
            if first_tick {
                let body = entity_json(&player.id, player.x, player.y, player.z, player.vx, player.vy, player.vz);
                match client.post(&url_update_player).header("Content-Type", "application/json").body(body).send().await {
                    Ok(resp) if resp.status().is_success() => { metrics.record_ok(t0.elapsed()); }
                    Ok(resp) => {
                        metrics.record_err();
                        if idx == 0 {
                            let s = resp.status();
                            let t = resp.text().await.unwrap_or_default();
                            eprintln!("[player 0] HTTP {}: {}", s, &t[..t.len().min(200)]);
                        }
                    }
                    Err(e) => { metrics.record_err(); if idx == 0 { eprintln!("[player 0] error: {}", e); } }
                }
                first_tick = false;
            } else {
                let body = player_input_json(&player.id, player.dir_x, player.dir_z);
                match client.post(&url_update_player_input).header("Content-Type", "application/json").body(body).send().await {
                    Ok(resp) if resp.status().is_success() => { metrics.record_ok(t0.elapsed()); }
                    Ok(resp) => {
                        metrics.record_err();
                        if idx == 0 {
                            let s = resp.status();
                            let t = resp.text().await.unwrap_or_default();
                            eprintln!("[player 0] HTTP {}: {}", s, &t[..t.len().min(200)]);
                        }
                    }
                    Err(e) => { metrics.record_err(); if idx == 0 { eprintln!("[player 0] error: {}", e); } }
                }
            }
        } else {
            let body = entity_json(&player.id, player.x, player.y, player.z, player.vx, player.vy, player.vz);
            match client
                .post(&url_update_player)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => { metrics.record_ok(t0.elapsed()); }
                Ok(resp) => {
                    metrics.record_err();
                    if idx == 0 {
                        let s = resp.status();
                        let t = resp.text().await.unwrap_or_default();
                        eprintln!("[player 0] HTTP {}: {}", s, &t[..t.len().min(200)]);
                    }
                }
                Err(e) => { metrics.record_err(); if idx == 0 { eprintln!("[player 0] error: {}", e); } }
            }
        }
    }
    let body = entity_json(&player.id, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let _ = client.post(&url_remove).header("Content-Type", "application/json").body(body).send().await;
}

// -- Arcane (WebSocket) player task ----------------------------------------

async fn player_loop_arcane(
    endpoint: ArcaneEndpoint,
    client: reqwest::Client,
    idx: u32, total: u32,
    tick_interval: Duration,
    metrics: Arc<Metrics>,
    read_metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
    cluster_flag: Arc<AtomicBool>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let ws_url = resolve_arcane_ws(&endpoint, &client, idx).await;
    let clustered = cluster_flag.load(Ordering::Relaxed);
    let mut player = Player::new(idx, total, clustered);
    let tick_dt = tick_interval.as_secs_f64();

    let ws_stream = match tokio_tungstenite::connect_async(&ws_url).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            if idx == 0 { eprintln!("[player 0] WebSocket connect failed: {}", e); }
            metrics.record_err();
            return;
        }
    };
    let (mut sink, mut stream) = ws_stream.split();

    let stop_drain = stop.clone();
    let rm = read_metrics.clone();
    tokio::spawn(async move {
        while !stop_drain.load(Ordering::Relaxed) {
            match stream.next().await {
                Some(Ok(Message::Text(txt))) => {
                    rm.ok.fetch_add(1, Ordering::Relaxed);
                    rm.bytes.fetch_add(txt.len() as u64, Ordering::Relaxed);
                }
                Some(Ok(Message::Binary(bin))) => {
                    rm.ok.fetch_add(1, Ordering::Relaxed);
                    rm.bytes.fetch_add(bin.len() as u64, Ordering::Relaxed);
                }
                Some(Ok(_)) => {}
                _ => break,
            }
        }
    });

    let mut interval = time::interval(tick_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        player.tick(tick_dt, cluster_flag.load(Ordering::Relaxed));
        let msg = player_state_json(&player.id, player.x, player.y, player.z, player.vx, player.vy, player.vz);
        let t0 = Instant::now();
        match sink.send(Message::Text(msg.into())).await {
            Ok(_) => { metrics.record_ok(t0.elapsed()); }
            Err(e) => {
                metrics.record_err();
                if idx == 0 { eprintln!("[player 0] ws send error: {}", e); }
                break;
            }
        }
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

        let a = if has_actions { action_metrics.snapshot_and_reset() } else {
            MetricsSnapshot { ok: 0, err: 0, avg_latency_us: 0, max_latency_us: 0, latency_sum_us: 0, latency_samples: 0, bytes: 0 }
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

// -- Main ------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cfg = parse_args();
    let tick_interval = Duration::from_micros(1_000_000 / cfg.tick_rate as u64);
    let backend_name = if cfg.backend == Backend::Arcane { "arcane" } else { "spacetimedb" };

    eprintln!("arcane-swarm: {} players, {} Hz, mode={}, backend={}, server_physics={}, duration={}s, actions/s={:.1}, read_rate={:.1}Hz",
        cfg.players, cfg.tick_rate,
        if cfg.mode == SwarmMode::Clustered { "clustered" } else { "spread" },
        backend_name, cfg.server_physics, cfg.duration_secs, cfg.actions_per_sec, cfg.read_rate,
    );

    let metrics = Arc::new(Metrics::new());
    let action_metrics = Arc::new(Metrics::new());
    let read_metrics = Arc::new(Metrics::new());
    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(cfg.players as usize * 3);

    let all_ids: Arc<Vec<uuid::Uuid>> = Arc::new((0..cfg.players).map(|_| uuid::Uuid::new_v4()).collect());

    let stdb_base = cfg.spacetimedb_uri.trim_end_matches('/').to_string();
    let base = format!("{}/v1/database/{}/call", stdb_base, cfg.database);
    let sql_url = format!("{}/v1/database/{}/sql", stdb_base, cfg.database);
    let action_urls = ActionUrls {
        pickup: format!("{}/pickup_item", base),
        use_item: format!("{}/use_item", base),
        interact: format!("{}/player_interact", base),
    };

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(cfg.players as usize * 2)
        .build()
        .expect("HTTP client");

    let positions = Arc::new(SharedPositions::new(cfg.players));

    match cfg.backend {
        Backend::SpacetimeDb => {
            let url_update_player = format!("{}/update_player", base);
            let url_update_player_input = format!("{}/update_player_input", base);
            let url_remove = format!("{}/remove_player", base);
            eprintln!("  SpacetimeDB: {}/database/{}", cfg.spacetimedb_uri, cfg.database);

            for i in 0..cfg.players {
                handles.push(tokio::spawn(player_loop_spacetimedb(
                    http_client.clone(),
                    url_update_player.clone(),
                    url_update_player_input.clone(),
                    url_remove.clone(),
                    i, cfg.players, tick_interval,
                    metrics.clone(), stop.clone(), cfg.cluster_command.clone(),
                    cfg.server_physics,
                    positions.clone(),
                )));
            }

            if cfg.read_rate > 0.0 {
                eprintln!("  Read simulation: spatial queries (radius={}) at {} Hz per player ({} total queries/s)",
                    VISIBILITY_RADIUS, cfg.read_rate, cfg.read_rate as u64 * cfg.players as u64);
                for i in 0..cfg.players {
                    handles.push(tokio::spawn(read_loop_spacetimedb(
                        http_client.clone(), sql_url.clone(),
                        cfg.read_rate, read_metrics.clone(), stop.clone(), i,
                        positions.clone(),
                    )));
                }
            }
        }
        Backend::Arcane => {
            let arcane_endpoint = match &cfg.arcane_manager {
                Some(base) => {
                    eprintln!("  Arcane: manager join at {} (round-robin clusters)", base);
                    ArcaneEndpoint::ManagerJoin { base_url: base.clone() }
                }
                None => {
                    eprintln!("  Arcane WS: {} (single cluster)", cfg.arcane_ws);
                    ArcaneEndpoint::SingleUrl(cfg.arcane_ws.clone())
                }
            };
            for i in 0..cfg.players {
                handles.push(tokio::spawn(player_loop_arcane(
                    arcane_endpoint.clone(),
                    http_client.clone(),
                    i, cfg.players, tick_interval,
                    metrics.clone(), read_metrics.clone(), stop.clone(), cfg.cluster_command.clone(),
                )));
            }
        }
    }

    if cfg.actions_per_sec > 0.0 {
        for i in 0..cfg.players {
            let player_id = all_ids[i as usize];
            handles.push(tokio::spawn(action_loop(
                http_client.clone(),
                ActionUrls {
                    pickup: action_urls.pickup.clone(),
                    use_item: action_urls.use_item.clone(),
                    interact: action_urls.interact.clone(),
                },
                player_id,
                i,
                cfg.players,
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
