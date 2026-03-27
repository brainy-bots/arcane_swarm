//! CLI and environment defaults for the swarm binary.
//!
//! This module is the entry contract between operators/benchmark scripts and runtime behavior:
//! every flag/env var eventually maps into [`Config`], which is consumed by binary orchestration.

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
    }
}
