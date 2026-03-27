//! Shared handles for spawning per-player tasks from the binary orchestrator.
//!
//! This isolates orchestration-time wiring so backend modules focus only on loop logic.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arcane_swarm::Metrics;

use super::backends_spacetimedb;

/// HTTP client, metric sinks, and flags shared by every player loop on a run.
pub(crate) struct PlayerLoopShared {
    pub http_client: reqwest::Client,
    pub metrics: Arc<Metrics>,
    pub read_metrics: Arc<Metrics>,
    pub cluster_flag: Arc<AtomicBool>,
    pub positions: Arc<backends_spacetimedb::SharedPositions>,
}

/// Values that differ per player (or change when control port adjusts player count).
#[derive(Clone)]
pub(crate) struct PlayerSpawnParams {
    pub idx: u32,
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
    pub actions_per_sec: f64,
    pub max_players: u32,
    pub action_urls: &'a backends_spacetimedb::ActionUrls,
    pub all_ids: Arc<Vec<uuid::Uuid>>,
    pub total_players_atomic: Arc<AtomicU32>,
    pub action_metrics: Arc<Metrics>,
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
        desired_total,
        tick_interval: kit.tick_interval,
        stop: stop.clone(),
    };
    kit.handles[idx] = Some(
        kit.backend_runtime
            .spawn_player(kit.loop_shared, params.clone()),
    );
    let _ = kit
        .backend_runtime
        .spawn_read(kit.loop_shared, &params, kit.read_rate);

    if kit.actions_per_sec > 0.0 {
        let action_idx = kit.max_players as usize + idx;
        let player_id = kit.all_ids[idx];
        kit.handles[action_idx] = Some(tokio::spawn(backends_spacetimedb::action_loop(
            backends_spacetimedb::SpacetimeActionLoop {
                client: kit.loop_shared.http_client.clone(),
                urls: backends_spacetimedb::ActionUrls {
                    pickup: kit.action_urls.pickup.clone(),
                    use_item: kit.action_urls.use_item.clone(),
                    interact: kit.action_urls.interact.clone(),
                },
                player_id,
                player_idx: idx as u32,
                total_players: kit.total_players_atomic.clone(),
                all_ids: kit.all_ids.clone(),
                actions_per_sec: kit.actions_per_sec,
                action_metrics: kit.action_metrics.clone(),
                stop,
            },
        )));
    }
}
