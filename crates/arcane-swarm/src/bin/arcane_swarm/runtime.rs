//! Backend-specific runtime trait + implementations (`spacetimedb` vs `arcane`).
//!
//! Extracted from `main.rs` to keep the entry point under 350 lines. Each
//! backend implements `BackendRuntime`, which owns the per-player lifecycle
//! (connect, movement loop, action loop, disconnect).
//!
//! # Shared handles
//!
//! [`SharedHandles`] bundles the frequently-cloned fields from
//! [`PlayerLoopShared`](crate::spawn_context::PlayerLoopShared) so each
//! `spawn_player` call clones a single struct instead of 8–10 individual
//! `Arc::clone()` calls.

use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;

use arcane_swarm::{ArcaneEndpoint, BurstConfig, DeltaCache, Metrics};

use crate::backends_arcane::{player_loop_arcane, ArcanePlayerLoop};
use crate::backends_spacetimedb::{
    player_loop_spacetimedb, SpacetimeConnectParams, SpacetimePlayerLoop,
};
use crate::spawn_context::{PlayerLoopShared, PlayerSpawnParams};

/// Frequently-cloned handles from `PlayerLoopShared`, bundled to reduce
/// repetitive `.clone()` calls inside each `BackendRuntime::spawn_player`.
///
/// Construct one via `SharedHandles::from_player_loop_shared(...)` and clone
/// it once per spawn instead of cloning individual fields.
#[derive(Clone)]
pub(crate) struct SharedHandles {
    pub http_client: reqwest::Client,
    pub metrics: Arc<Metrics>,
    pub read_metrics: Arc<Metrics>,
    pub action_metrics: Arc<Metrics>,
    pub cluster_flag: Arc<AtomicBool>,
    pub all_ids: Arc<Vec<uuid::Uuid>>,
    pub total_players: Arc<AtomicU32>,
    pub actions_per_sec: f64,
    pub burst: BurstConfig,
    pub run_started: std::time::Instant,
}

impl SharedHandles {
    /// Build a `SharedHandles` from a `PlayerLoopShared` reference, cloning
    /// each field exactly once.
    pub(crate) fn from_player_loop_shared(shared: &PlayerLoopShared) -> Self {
        Self {
            http_client: shared.http_client.clone(),
            metrics: shared.metrics.clone(),
            read_metrics: shared.read_metrics.clone(),
            action_metrics: shared.action_metrics.clone(),
            cluster_flag: shared.cluster_flag.clone(),
            all_ids: shared.all_ids.clone(),
            total_players: shared.total_players.clone(),
            actions_per_sec: shared.actions_per_sec,
            burst: shared.burst,
            run_started: shared.run_started,
        }
    }
}

/// Backend-specific runtime selected at startup (`spacetimedb` vs `arcane`).
/// Binary-internal only; CLI and wire formats are unchanged.
///
/// Both backends carry movement and action reducer calls on one WebSocket per
/// simulated player, so `spawn_player` owns the entire per-player lifecycle
/// (connect, movement loop, action loop, disconnect). There is no separate
/// action spawn.
pub(crate) trait BackendRuntime: Send + Sync {
    fn name(&self) -> &'static str;

    fn spawn_player(
        &self,
        handles: SharedHandles,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()>;

    fn spawn_read(
        &self,
        _handles: &SharedHandles,
        _params: &PlayerSpawnParams,
        _read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }

    /// Snapshot+reset cache hit/miss counters if this backend has a per-frame
    /// decode cache. Default returns `(0, 0)` so backends without one (e.g.
    /// SpacetimeDB) report zero in the FINAL line without special-casing.
    fn snapshot_cache_counters(&self) -> (u64, u64) {
        (0, 0)
    }
}

// ---------------------------------------------------------------------------
// SpacetimeDB runtime
// ---------------------------------------------------------------------------

pub(crate) struct SpacetimeRuntime {
    /// Connection params handed to every player loop so it opens its own
    /// dedicated WebSocket. Multiplexing all players over one socket hit
    /// SpacetimeDB's per-client `incoming_queue_length` limit under load and
    /// silently dropped messages — see `backends_spacetimedb.rs` top-of-file.
    pub connect_params: SpacetimeConnectParams,
    pub server_physics: bool,
}

impl BackendRuntime for SpacetimeRuntime {
    fn name(&self) -> &'static str {
        "spacetimedb"
    }

    fn spawn_player(
        &self,
        handles: SharedHandles,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(player_loop_spacetimedb(SpacetimePlayerLoop {
            connect_params: self.connect_params.clone(),
            idx: params.idx,
            entity_id: params.entity_id,
            total: params.desired_total,
            tick_interval: params.tick_interval,
            metrics: handles.metrics.clone(),
            read_metrics: handles.read_metrics.clone(),
            action_metrics: handles.action_metrics.clone(),
            stop: params.stop,
            cluster_flag: handles.cluster_flag.clone(),
            server_physics: self.server_physics,
            all_ids: handles.all_ids.clone(),
            total_players: handles.total_players.clone(),
            actions_per_sec: handles.actions_per_sec,
            burst: handles.burst,
            run_started: handles.run_started,
        }))
    }

    fn spawn_read(
        &self,
        _handles: &SharedHandles,
        _params: &PlayerSpawnParams,
        _read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }
}

// ---------------------------------------------------------------------------
// Arcane runtime
// ---------------------------------------------------------------------------

pub(crate) struct ArcaneRuntime {
    pub endpoint: ArcaneEndpoint,
    pub delta_cache: Arc<DeltaCache>,
    pub user_data_bytes: usize,
}

impl BackendRuntime for ArcaneRuntime {
    fn name(&self) -> &'static str {
        "arcane"
    }

    fn spawn_player(
        &self,
        handles: SharedHandles,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(player_loop_arcane(ArcanePlayerLoop {
            endpoint: self.endpoint.clone(),
            client: handles.http_client.clone(),
            idx: params.idx,
            entity_id: params.entity_id,
            total: params.desired_total,
            tick_interval: params.tick_interval,
            metrics: handles.metrics.clone(),
            read_metrics: handles.read_metrics.clone(),
            action_metrics: handles.action_metrics.clone(),
            stop: params.stop,
            cluster_flag: handles.cluster_flag.clone(),
            actions_per_sec: handles.actions_per_sec,
            burst: handles.burst,
            run_started: handles.run_started,
            delta_cache: self.delta_cache.clone(),
            user_data_bytes: self.user_data_bytes,
        }))
    }

    fn spawn_read(
        &self,
        _handles: &SharedHandles,
        _params: &PlayerSpawnParams,
        _read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }

    fn snapshot_cache_counters(&self) -> (u64, u64) {
        self.delta_cache.snapshot_and_reset_counters()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    fn dummy_params() -> PlayerSpawnParams {
        PlayerSpawnParams {
            idx: 0,
            entity_id: uuid::Uuid::nil(),
            desired_total: 1,
            tick_interval: Duration::from_millis(100),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    fn dummy_handles() -> SharedHandles {
        use arcane_swarm::Metrics;
        SharedHandles {
            http_client: reqwest::Client::new(),
            metrics: Arc::new(Metrics::new()),
            read_metrics: Arc::new(Metrics::new()),
            action_metrics: Arc::new(Metrics::new()),
            cluster_flag: Arc::new(AtomicBool::new(false)),
            all_ids: Arc::new(vec![]),
            total_players: Arc::new(AtomicU32::new(0)),
            actions_per_sec: 0.0,
            burst: BurstConfig {
                enabled: false,
                burst_period_secs: 0,
                burst_cohort_percent: 0,
                burst_actions_per_player: 0,
                burst_window_ms: 0,
                zone_event_period_secs: 0,
                zone_event_window_ms: 0,
            },
            run_started: std::time::Instant::now(),
        }
    }

    #[test]
    fn test_spacetime_name() {
        let rt = SpacetimeRuntime {
            connect_params: SpacetimeConnectParams {
                ws_uri: "ws://localhost:3000".into(),
                database_name: "test".into(),
            },
            server_physics: false,
        };
        assert_eq!(rt.name(), "spacetimedb");
    }

    #[test]
    fn test_spacetime_spawn_read_returns_none() {
        let rt = SpacetimeRuntime {
            connect_params: SpacetimeConnectParams {
                ws_uri: "ws://localhost:3000".into(),
                database_name: "test".into(),
            },
            server_physics: false,
        };
        let handles = dummy_handles();
        let params = dummy_params();
        assert!(rt.spawn_read(&handles, &params, 0.0).is_none());
    }

    #[test]
    fn test_spacetime_snapshot_cache_default() {
        let rt = SpacetimeRuntime {
            connect_params: SpacetimeConnectParams {
                ws_uri: "ws://localhost:3000".into(),
                database_name: "test".into(),
            },
            server_physics: false,
        };
        assert_eq!(rt.snapshot_cache_counters(), (0, 0));
    }

    #[test]
    fn test_arcane_name() {
        let rt = ArcaneRuntime {
            endpoint: ArcaneEndpoint::SingleUrl("ws://localhost:8080".into()),
            delta_cache: Arc::new(DeltaCache::default()),
            user_data_bytes: 0,
        };
        assert_eq!(rt.name(), "arcane");
    }

    #[test]
    fn test_arcane_spawn_read_returns_none() {
        let rt = ArcaneRuntime {
            endpoint: ArcaneEndpoint::SingleUrl("ws://localhost:8080".into()),
            delta_cache: Arc::new(DeltaCache::default()),
            user_data_bytes: 0,
        };
        let handles = dummy_handles();
        let params = dummy_params();
        assert!(rt.spawn_read(&handles, &params, 0.0).is_none());
    }

    #[test]
    fn test_arcane_snapshot_cache_initial() {
        let rt = ArcaneRuntime {
            endpoint: ArcaneEndpoint::SingleUrl("ws://localhost:8080".into()),
            delta_cache: Arc::new(DeltaCache::default()),
            user_data_bytes: 0,
        };
        assert_eq!(rt.snapshot_cache_counters(), (0, 0));
    }

    #[test]
    fn test_shared_handles_from_player_loop_shared() {
        use crate::spawn_context::PlayerLoopShared;

        let metrics = Arc::new(Metrics::new());
        let shared = PlayerLoopShared {
            http_client: reqwest::Client::new(),
            metrics: metrics.clone(),
            read_metrics: Arc::new(Metrics::new()),
            action_metrics: Arc::new(Metrics::new()),
            cluster_flag: Arc::new(AtomicBool::new(false)),
            all_ids: Arc::new(vec![]),
            total_players: Arc::new(AtomicU32::new(42)),
            actions_per_sec: 10.0,
            burst: BurstConfig {
                enabled: true,
                burst_period_secs: 5,
                burst_cohort_percent: 50,
                burst_actions_per_player: 3,
                burst_window_ms: 100,
                zone_event_period_secs: 10,
                zone_event_window_ms: 50,
            },
            run_started: std::time::Instant::now(),
        };

        let handles = SharedHandles::from_player_loop_shared(&shared);
        assert_eq!(handles.total_players.load(Ordering::Relaxed), 42);
        assert_eq!(handles.actions_per_sec, 10.0);
        assert!(handles.burst.enabled);
    }
}
