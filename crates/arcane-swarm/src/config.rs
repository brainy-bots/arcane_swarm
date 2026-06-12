//! CLI and environment defaults for the swarm binary.
//!
//! This module is the entry contract between operators/benchmark scripts and runtime behavior:
//! every flag/env var eventually maps into [`Config`], which is consumed by binary orchestration.

use crate::BurstConfig;
use clap::Parser;
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
    /// When set, the driver runs in orchestrated mode: connects to the named
    /// swarm orchestrator over WebSocket, registers, listens for commands,
    /// and acks. Mutually exclusive with the standalone player-spawning path.
    /// When unset (default), the driver runs in standalone mode unchanged.
    pub orchestrator_url: Option<String>,
}

#[derive(Parser, Debug)]
#[command(name = "arcane-swarm")]
#[command(about = "headless client swarm", long_about = None)]
struct Args {
    #[arg(
        long,
        value_name = "MODE",
        default_value = "spacetimedb",
        env = "BACKEND",
        value_parser = ["spacetimedb", "arcane"],
        help = "spacetimedb | arcane"
    )]
    backend: String,

    #[arg(
        long,
        short = 'n',
        value_name = "N",
        default_value = "0",
        help = "number of simulated players"
    )]
    players: u32,

    #[arg(
        long,
        value_name = "N",
        default_value = "0",
        help = "max players for incremental mode (default = --players)"
    )]
    max_players: u32,

    #[arg(
        long,
        short = 't',
        value_name = "HZ",
        default_value = "20",
        help = "ticks per second per player"
    )]
    tick_rate: u32,

    #[arg(
        long,
        short = 'd',
        value_name = "SECS",
        default_value = "60",
        help = "how long to run"
    )]
    duration: u64,

    #[arg(
        long,
        short = 'm',
        value_name = "MODE",
        default_value = "spread",
        value_parser = ["spread", "clustered"],
        help = "spread | clustered"
    )]
    mode: String,

    #[arg(long, value_name = "PATH", help = "write metrics CSV to this file")]
    csv: Option<String>,

    #[arg(
        long,
        value_name = "URL",
        env = "SPACETIMEDB_URI",
        default_value = "http://127.0.0.1:3000",
        help = "SpacetimeDB URI"
    )]
    uri: String,

    #[arg(
        long,
        alias = "db",
        value_name = "NAME",
        env = "DATABASE_NAME",
        default_value = "arcane",
        help = "database name"
    )]
    database: String,

    #[arg(
        long,
        value_name = "URL",
        env = "ARCANE_WS",
        default_value = "ws://127.0.0.1:8080",
        help = "Arcane cluster WebSocket"
    )]
    arcane_ws: String,

    #[arg(
        long,
        value_name = "URL",
        env = "ARCANE_MANAGER",
        help = "Use manager /join for cluster assignment (round-robin)"
    )]
    arcane_manager: Option<String>,

    #[arg(
        long,
        alias = "aps",
        value_name = "N",
        help = "persistent actions per player per second"
    )]
    actions_per_sec: Option<f64>,

    #[arg(
        long,
        value_name = "HZ",
        help = "world-state reads per player per second"
    )]
    read_rate: Option<f64>,

    #[arg(
        long,
        help = "for spacetimedb backend: use update_player_input for movement"
    )]
    server_physics: bool,

    #[arg(long, help = "keep running until QUIT")]
    run_forever: bool,

    #[arg(
        long,
        value_name = "PORT",
        help = "enable TCP control server at 127.0.0.1:PORT"
    )]
    control_port: Option<u16>,

    #[arg(long, help = "enable deterministic burst profile")]
    burst_enabled: bool,

    #[arg(long, help = "disable deterministic burst profile")]
    burst_disabled: bool,

    #[arg(long, value_name = "N", help = "seconds between bursts")]
    burst_period_secs: Option<u64>,

    #[arg(long, value_name = "N", help = "percentage of players in each burst")]
    burst_cohort_percent: Option<u32>,

    #[arg(
        long,
        value_name = "N",
        help = "extra actions for selected players during burst"
    )]
    burst_actions_per_player: Option<u32>,

    #[arg(long, value_name = "N", help = "burst window length in milliseconds")]
    burst_window_ms: Option<u64>,

    #[arg(
        long,
        value_name = "N",
        help = "seconds between all-player convergence events"
    )]
    zone_event_period_secs: Option<u64>,

    #[arg(
        long,
        value_name = "N",
        help = "zone event steering window in milliseconds"
    )]
    zone_event_window_ms: Option<u64>,

    #[arg(
        long,
        value_name = "N",
        help = "bytes per PLAYER_STATE.user_data payload (Arcane backend only)"
    )]
    user_data_bytes: Option<usize>,

    #[arg(
        long,
        value_name = "N",
        help = "ms between consecutive player spawns (multi-driver join-rate pacing)"
    )]
    inter_spawn_delay_ms: Option<u32>,

    #[arg(
        long,
        value_name = "N",
        help = "hard safety cap on simultaneously-active players (multi-driver runs)"
    )]
    max_players_per_driver: Option<u32>,

    #[arg(
        long,
        value_name = "URL",
        env = "ORCHESTRATOR_URL",
        help = "connect to swarm orchestrator over WS and run in orchestrated mode"
    )]
    orchestrator_url: Option<String>,
}

pub fn parse_args() -> Config {
    let args = Args::parse();

    let backend = match args.backend.as_str() {
        "arcane" => Backend::Arcane,
        _ => Backend::SpacetimeDb,
    };

    let mode = match args.mode.as_str() {
        "clustered" => SwarmMode::Clustered,
        _ => SwarmMode::Spread,
    };

    let tick_rate = args.tick_rate.max(1);
    let max_players = if args.max_players == 0 {
        args.players
    } else {
        args.max_players
    };

    let burst = BurstConfig {
        enabled: if args.burst_disabled {
            false
        } else {
            args.burst_enabled || BurstConfig::default().enabled
        },
        burst_period_secs: args
            .burst_period_secs
            .unwrap_or(BurstConfig::default().burst_period_secs),
        burst_cohort_percent: args
            .burst_cohort_percent
            .unwrap_or(BurstConfig::default().burst_cohort_percent),
        burst_actions_per_player: args
            .burst_actions_per_player
            .unwrap_or(BurstConfig::default().burst_actions_per_player),
        burst_window_ms: args
            .burst_window_ms
            .unwrap_or(BurstConfig::default().burst_window_ms),
        zone_event_period_secs: args
            .zone_event_period_secs
            .unwrap_or(BurstConfig::default().zone_event_period_secs),
        zone_event_window_ms: args
            .zone_event_window_ms
            .unwrap_or(BurstConfig::default().zone_event_window_ms),
    };

    Config {
        backend,
        spacetimedb_uri: args.uri,
        database: args.database,
        arcane_ws: args.arcane_ws,
        arcane_manager: args.arcane_manager,
        players: args.players,
        max_players,
        tick_rate,
        duration_secs: args.duration,
        mode,
        csv_path: args.csv,
        cluster_command: Arc::new(AtomicBool::new(mode == SwarmMode::Clustered)),
        actions_per_sec: args.actions_per_sec.unwrap_or(0.0),
        read_rate: args.read_rate.unwrap_or(5.0),
        server_physics: args.server_physics,
        run_forever: args.run_forever,
        control_port: args.control_port.unwrap_or(0),
        burst,
        user_data_bytes: args.user_data_bytes.unwrap_or(0),
        inter_spawn_delay_ms: args.inter_spawn_delay_ms.unwrap_or(0),
        max_players_per_driver: args.max_players_per_driver.unwrap_or(0),
        orchestrator_url: args.orchestrator_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_backend_value_is_a_hard_error() {
        let err = Args::try_parse_from(["arcane-swarm", "--backend", "arcnae"]);
        assert!(
            err.is_err(),
            "typo'd --backend must fail loudly, not fall back"
        );
    }

    #[test]
    fn invalid_mode_value_is_a_hard_error() {
        let err = Args::try_parse_from(["arcane-swarm", "--mode", "clusterd"]);
        assert!(
            err.is_err(),
            "typo'd --mode must fail loudly, not fall back"
        );
    }

    #[test]
    fn invalid_numeric_value_is_a_hard_error() {
        let err = Args::try_parse_from(["arcane-swarm", "--players", "abc"]);
        assert!(err.is_err(), "non-numeric --players must fail loudly");
    }

    #[test]
    fn aps_alias_maps_to_actions_per_sec() {
        let args = Args::try_parse_from(["arcane-swarm", "--aps", "2.5"]).unwrap();
        assert_eq!(args.actions_per_sec, Some(2.5));
    }

    #[test]
    fn players_defaults_to_zero() {
        // Ghost-entity fix (arcane_swarm#58): orchestrated drivers must start at 0.
        let args = Args::try_parse_from(["arcane-swarm"]).unwrap();
        assert_eq!(args.players, 0);
    }

    #[test]
    fn valid_backend_and_mode_parse() {
        let args =
            Args::try_parse_from(["arcane-swarm", "--backend", "arcane", "--mode", "clustered"])
                .unwrap();
        assert_eq!(args.backend, "arcane");
        assert_eq!(args.mode, "clustered");
    }
}
