//! Shared handles for spawning per-player tasks from the binary orchestrator.
//!
//! This isolates orchestration-time wiring so backend modules focus only on loop logic.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arcane_swarm::{BurstConfig, Metrics};

use super::backends_spacetimedb;

/// HTTP client, metric sinks, and flags shared by every player loop on a run.
///
/// Action-loop config (rate, metrics, target pool) lives here too because both
/// backends carry movement and actions on the same WebSocket — so both
/// `spawn_player` implementations need to read it from one place.
pub(crate) struct PlayerLoopShared {
    pub http_client: reqwest::Client,
    pub metrics: Arc<Metrics>,
    pub read_metrics: Arc<Metrics>,
    pub action_metrics: Arc<Metrics>,
    pub cluster_flag: Arc<AtomicBool>,
    pub positions: Arc<backends_spacetimedb::SharedPositions>,
    pub all_ids: Arc<Vec<uuid::Uuid>>,
    pub total_players: Arc<AtomicU32>,
    pub actions_per_sec: f64,
    pub burst: BurstConfig,
    pub run_started: std::time::Instant,
}

/// Values that differ per player (or change when control port adjusts player count).
#[derive(Clone)]
pub(crate) struct PlayerSpawnParams {
    pub idx: u32,
    pub entity_id: uuid::Uuid,
    pub desired_total: u32,
    pub tick_interval: Duration,
    pub stop: Arc<AtomicBool>,
}

/// Bundles mutable handles and config for incremental control-mode spawns.
pub(crate) struct ControlSpawnKit<'a> {
    pub handles: &'a mut Vec<Option<tokio::task::JoinHandle<()>>>,
    pub player_stop_flags: &'a Arc<Vec<Arc<AtomicBool>>>,
    pub loop_shared: &'a PlayerLoopShared,
    pub backend_runtime: &'a Arc<dyn crate::BackendRuntime>,
    pub tick_interval: Duration,
    pub read_rate: f64,
}

pub(crate) fn spawn_control_mode_player(
    kit: &mut ControlSpawnKit<'_>,
    idx: usize,
    desired_total: u32,
) {
    kit.player_stop_flags[idx].store(false, Ordering::Relaxed);
    let stop = kit.player_stop_flags[idx].clone();
    let params = PlayerSpawnParams {
        idx: idx as u32,
        entity_id: kit.loop_shared.all_ids[idx],
        desired_total,
        tick_interval: kit.tick_interval,
        stop,
    };
    kit.handles[idx] = Some(
        kit.backend_runtime
            .spawn_player(kit.loop_shared, params.clone()),
    );
    let _ = kit
        .backend_runtime
        .spawn_read(kit.loop_shared, &params, kit.read_rate);
}
