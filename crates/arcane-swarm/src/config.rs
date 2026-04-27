//! CLI and environment defaults for the swarm binary.
//!
//! This module is the entry contract between operators/benchmark scripts and runtime behavior:
//! every flag/env var eventually maps into [`Config`], which is consumed by binary orchestration.

use crate::BurstConfig;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// How each player resolves the Arcane cluster WebSocket URL.
#[derive(Clone)]
pub enum ArcaneEndpoint {
    /// All players connect to this single URL (one cluster server).
    SingleUrl(String),
    /// Each player does GET base/join; manager returns server_host:port (round-robin across clusters).
    ManagerJoin { base_url: String },
}

#[derive(Clone, Copy, PartialEq)]
pub enum SwarmMode {
    Spread,
    Clustered,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Backend {
    SpacetimeDb,
    Arcane,
}

#[derive(Clone)]
pub struct Config {
    pub backend: Backend,
    pub spacetimedb_uri: String,
    pub database: String,
    pub arcane_ws: String,
    pub arcane_manager: Option<String>,
    pub players: u32,
    /// Max players the swarm is allowed to spawn (used for incremental SET_PLAYERS without reallocating).
    pub max_players: u32,
    pub tick_rate: u32,
    pub duration_secs: u64,
    pub mode: SwarmMode,
    pub csv_path: Option<String>,
    pub cluster_command: Arc<AtomicBool>,
    pub actions_per_sec: f64,
    pub read_rate: f64,
    /// If true for the `spacetimedb` backend:
    /// - first tick uses `update_player` (initial position spawn)
    /// - subsequent ticks use `update_player_input` (direction only)
    pub server_physics: bool,
    pub run_forever: bool,
    pub control_port: u16,
    pub burst: BurstConfig,
    /// Bytes per `PlayerStatePayload.user_data` payload sent in the per-tick
    /// PLAYER_STATE frame. Default 0 (lean baseline). Set > 0 to measure the
    /// realistic-state ceiling — the Arcane backend fills the bytes per
    /// `(player, tick)` via `protocol::fill_pseudo_user_data`. SpacetimeDB
    /// backend ignores this knob; its `update_player` path doesn't carry an
    /// equivalent opaque payload field.
    pub user_data_bytes: usize,
    /// Milliseconds to sleep between consecutive player spawns. Default 0 =
    /// burst-spawn (historical behavior). Set N > 0 to pace the per-driver
    /// join rate when running multiple drivers against one manager — the
    /// harness keeps aggregate manager join rate constant by scaling this
    /// with driver count.
    pub inter_spawn_delay_ms: u32,
    /// Hard safety cap on simultaneously-active players per driver process.
    /// Default 0 = no cap (historical behavior; max_players is the only
    /// limit). Set > 0 to refuse SET_PLAYERS values above this number; the
    /// swarm clamps to the cap and emits `[cap] desired=X cap=Y refusing` to
    /// stderr. Used by multi-driver runs so a single driver can't be pushed
    /// into the soft-saturation zone where measurements become unreliable —
    /// the orchestrator must provision more drivers instead.
    pub max_players_per_driver: u32,
}

pub fn parse_args() -> Config {
    let mut players: u32 = 100;
    let mut max_players: u32 = 0;
    let mut tick_rate: u32 = 20;
    let mut duration_secs: u64 = 60;
    let mut mode = SwarmMode::Spread;
    let mut csv_path: Option<String> = None;
    let mut uri =
        std::env::var("SPACETIMEDB_URI").unwrap_or_else(|_| "http://127.0.0.1:3000".into());
    let mut database = std::env::var("DATABASE_NAME").unwrap_or_else(|_| "arcane".into());
    let mut arcane_ws = std::env::var("ARCANE_WS").unwrap_or_else(|_| "ws://127.0.0.1:8080".into());
    let mut arcane_manager: Option<String> = std::env::var("ARCANE_MANAGER").ok();
    let mut backend = Backend::SpacetimeDb;
    let mut actions_per_sec: f64 = 0.0;
    let mut read_rate: f64 = 5.0;
    let mut server_physics: bool = false;
    let mut run_forever: bool = false;
    let mut control_port: u16 = 0;
    let mut burst = BurstConfig::default();
    let mut user_data_bytes: usize = 0;
    let mut inter_spawn_delay_ms: u32 = 0;
    let mut max_players_per_driver: u32 = 0;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--players" | "-n" => {
                i += 1;
                players = args[i].parse().unwrap_or(players);
            }
            "--tick-rate" | "-t" => {
                i += 1;
                tick_rate = args[i].parse().unwrap_or(tick_rate);
            }
            "--duration" | "-d" => {
                i += 1;
                duration_secs = args[i].parse().unwrap_or(duration_secs);
            }
            "--mode" | "-m" => {
                i += 1;
                mode = if args[i] == "clustered" {
                    SwarmMode::Clustered
                } else {
                    SwarmMode::Spread
                };
            }
            "--csv" => {
                i += 1;
                csv_path = Some(args[i].clone());
            }
            "--uri" => {
                i += 1;
                uri = args[i].clone();
            }
            "--database" | "--db" => {
                i += 1;
                database = args[i].clone();
            }
            "--arcane-ws" => {
                i += 1;
                arcane_ws = args[i].clone();
            }
            "--arcane-manager" => {
                i += 1;
                arcane_manager = Some(args[i].clone());
            }
            "--backend" | "-b" => {
                i += 1;
                backend = if args[i] == "arcane" {
                    Backend::Arcane
                } else {
                    Backend::SpacetimeDb
                };
            }
            "--actions-per-sec" | "--aps" => {
                i += 1;
                actions_per_sec = args[i].parse().unwrap_or(0.0);
            }
            "--read-rate" => {
                i += 1;
                read_rate = args[i].parse().unwrap_or(5.0);
            }
            "--server-physics" => {
                server_physics = true;
            }
            "--max-players" => {
                i += 1;
                max_players = args[i].parse().unwrap_or(players);
            }
            "--run-forever" => {
                run_forever = true;
            }
            "--control-port" => {
                i += 1;
                control_port = args[i].parse().unwrap_or(0);
            }
            "--burst-enabled" => {
                burst.enabled = true;
            }
            "--burst-disabled" => {
                burst.enabled = false;
            }
            "--burst-period-secs" => {
                i += 1;
                burst.burst_period_secs = args[i].parse().unwrap_or(burst.burst_period_secs);
            }
            "--burst-cohort-percent" => {
                i += 1;
                burst.burst_cohort_percent = args[i].parse().unwrap_or(burst.burst_cohort_percent);
            }
            "--burst-actions-per-player" => {
                i += 1;
                burst.burst_actions_per_player =
                    args[i].parse().unwrap_or(burst.burst_actions_per_player);
            }
            "--burst-window-ms" => {
                i += 1;
                burst.burst_window_ms = args[i].parse().unwrap_or(burst.burst_window_ms);
            }
            "--zone-event-period-secs" => {
                i += 1;
                burst.zone_event_period_secs =
                    args[i].parse().unwrap_or(burst.zone_event_period_secs);
            }
            "--zone-event-window-ms" => {
                i += 1;
                burst.zone_event_window_ms = args[i].parse().unwrap_or(burst.zone_event_window_ms);
            }
            "--user-data-bytes" => {
                i += 1;
                user_data_bytes = args[i].parse().unwrap_or(0);
            }
            "--inter-spawn-delay-ms" => {
                i += 1;
                inter_spawn_delay_ms = args[i].parse().unwrap_or(0);
            }
            "--max-players-per-driver" => {
                i += 1;
                max_players_per_driver = args[i].parse().unwrap_or(0);
            }
            "--help" | "-h" => {
                eprintln!("arcane-swarm: headless client swarm\n");
                eprintln!("  --backend MODE        spacetimedb | arcane (default spacetimedb)");
                eprintln!("  --players N            number of simulated players (default 100)");
                eprintln!("  --max-players N       max players for incremental mode (default = --players)");
                eprintln!("  --tick-rate HZ         ticks per second per player (default 20)");
                eprintln!("  --duration SECS        how long to run (default 60)");
                eprintln!("  --mode MODE            spread | clustered (default spread)");
                eprintln!(
                    "  --actions-per-sec N    persistent actions per player per second (default 0)"
                );
                eprintln!(
                    "  --read-rate HZ         world-state reads per player per second (default 5)"
                );
                eprintln!("  --server-physics      for spacetimedb backend: use update_player_input for movement");
                eprintln!("  --run-forever          keep running until QUIT");
                eprintln!("  --control-port PORT   enable TCP control server at 127.0.0.1:PORT");
                eprintln!("  --burst-enabled      enable deterministic burst profile (default on)");
                eprintln!("  --burst-disabled     disable deterministic burst profile");
                eprintln!("  --burst-period-secs N    seconds between bursts (default 30)");
                eprintln!(
                    "  --burst-cohort-percent N percentage of players in each burst (default 20)"
                );
                eprintln!("  --burst-actions-per-player N extra actions for selected players during burst (default 10)");
                eprintln!(
                    "  --burst-window-ms N     burst window length in milliseconds (default 500)"
                );
                eprintln!("  --zone-event-period-secs N seconds between all-player convergence events (default 30)");
                eprintln!("  --zone-event-window-ms N zone event steering window in milliseconds (default 500)");
                eprintln!("  --user-data-bytes N    bytes per PLAYER_STATE.user_data payload (default 0; Arcane backend only)");
                eprintln!("  --inter-spawn-delay-ms N  ms between consecutive player spawns (default 0; multi-driver join-rate pacing)");
                eprintln!("  --max-players-per-driver N  hard safety cap on simultaneously-active players (default 0 = no cap; multi-driver runs set this conservatively)");
                eprintln!("  --csv PATH             write metrics CSV to this file");
                eprintln!(
                    "  --uri URL              SpacetimeDB URI (default http://127.0.0.1:3000)"
                );
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
        max_players: if max_players == 0 {
            players
        } else {
            max_players
        },
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
        burst,
        user_data_bytes,
        inter_spawn_delay_ms,
        max_players_per_driver,
    }
}
