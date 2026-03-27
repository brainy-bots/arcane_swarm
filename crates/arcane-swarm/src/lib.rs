//! Shared library for the `arcane-swarm` binary: metrics, simulated clients, CLI config, reporting.
//!
//! **Stable public interface:** the `arcane-swarm` executable’s CLI flags and behavior (including log
//! lines such as `FINAL:` / `FINAL_SPACETIMEDB:` and HTTP/WebSocket payloads) are the contract for
//! benchmark harnesses and operators. Backend wiring (`BackendRuntime`, spawn helpers) lives only under
//! `src/bin/arcane_swarm/` and is not part of the SemVer API of this crate.

pub mod config;
pub mod metrics;
pub mod player;
pub mod protocol;
pub mod reporter;

pub use config::{parse_args, ArcaneEndpoint, Backend, Config, SwarmMode};
pub use metrics::{Metrics, MetricsSnapshot};
pub use player::Player;
pub use protocol::{player_state_json, VISIBILITY_RADIUS};
pub use reporter::{fmt_bytes, run_reporter, ReporterConfig};
