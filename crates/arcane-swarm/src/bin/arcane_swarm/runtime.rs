//! Backend-specific runtime selected at startup (`spacetimedb` vs `arcane`).
//!
//! Both backends carry movement and action reducer calls on one WebSocket per
//! simulated player, so `spawn_player` owns the entire per-player lifecycle
//! (connect, movement loop, action loop, disconnect). There is no separate
//! action spawn.

use std::sync::Arc;

use arcane_swarm::ArcaneEndpoint;

use crate::spawn_context::{PlayerLoopShared, PlayerSpawnParams};
use crate::{backends_arcane, backends_spacetimedb};

/// Backend-specific runtime behavior for spawning players.
pub(crate) trait BackendRuntime: Send + Sync {
    fn name(&self) -> &'static str;

    fn spawn_player(
        &self,
        shared: &PlayerLoopShared,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()>;

    fn spawn_read(
        &self,
        shared: &PlayerLoopShared,
        params: &PlayerSpawnParams,
        read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>>;

    /// Snapshot+reset cache hit/miss counters if this backend has a per-frame
    /// decode cache. Default returns `(0, 0)` so backends without one (e.g.
    /// SpacetimeDB) report zero in the FINAL line without special-casing.
    fn snapshot_cache_counters(&self) -> (u64, u64) {
        (0, 0)
    }
}

pub(crate) struct SpacetimeRuntime {
    /// Connection params handed to every player loop so it opens its own
    /// dedicated WebSocket. Multiplexing all players over one socket hit
    /// SpacetimeDB's per-client `incoming_queue_length` limit under load and
    /// silently dropped messages — see backends_spacetimedb.rs top-of-file.
    connect_params: backends_spacetimedb::SpacetimeConnectParams,
    server_physics: bool,
}

impl SpacetimeRuntime {
    pub(crate) fn new(ws_uri: String, database_name: String, server_physics: bool) -> Self {
        Self {
            connect_params: backends_spacetimedb::SpacetimeConnectParams {
                ws_uri,
                database_name,
            },
            server_physics,
        }
    }
}

impl BackendRuntime for SpacetimeRuntime {
    fn name(&self) -> &'static str {
        "spacetimedb"
    }

    fn spawn_player(
        &self,
        shared: &PlayerLoopShared,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(backends_spacetimedb::player_loop_spacetimedb(
            backends_spacetimedb::SpacetimePlayerLoop {
                connect_params: self.connect_params.clone(),
                idx: params.idx,
                entity_id: params.entity_id,
                total: params.desired_total,
                tick_interval: params.tick_interval,
                metrics: shared.metrics.clone(),
                read_metrics: shared.read_metrics.clone(),
                action_metrics: shared.action_metrics.clone(),
                stop: params.stop,
                cluster_flag: shared.cluster_flag.clone(),
                server_physics: self.server_physics,
                all_ids: shared.all_ids.clone(),
                total_players: shared.total_players.clone(),
                actions_per_sec: shared.actions_per_sec,
                burst: shared.burst,
                run_started: shared.run_started,
            },
        ))
    }

    fn spawn_read(
        &self,
        _shared: &PlayerLoopShared,
        _params: &PlayerSpawnParams,
        _read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        // SpacetimeDB reads arrive as subscription updates via the SDK — the
        // player loop registers an `on_update(Entity)` handler inline and
        // counts inbound bytes there. No separate read task to spawn.
        None
    }
}

pub(crate) struct ArcaneRuntime {
    endpoint: ArcaneEndpoint,
    /// Per-driver shared decode cache. Populated lazily by drain tasks; see
    /// `arcane_swarm::delta_cache` for why it exists. Lives on the runtime
    /// (rather than `PlayerLoopShared`) because it's Arcane-specific —
    /// SpacetimeDB backend has its own subscription pipeline.
    delta_cache: Arc<arcane_swarm::DeltaCache>,
    /// Bytes per `PlayerStatePayload.user_data` payload. Same reasoning as
    /// `delta_cache` — Arcane-specific knob; SpacetimeDB backend has no
    /// equivalent opaque-payload field on its movement frames.
    user_data_bytes: usize,
}

impl ArcaneRuntime {
    pub(crate) fn new(endpoint: ArcaneEndpoint, user_data_bytes: usize) -> Self {
        Self {
            endpoint,
            delta_cache: Arc::new(arcane_swarm::DeltaCache::default()),
            user_data_bytes,
        }
    }
}

impl BackendRuntime for ArcaneRuntime {
    fn name(&self) -> &'static str {
        "arcane"
    }

    fn spawn_player(
        &self,
        shared: &PlayerLoopShared,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(backends_arcane::player_loop_arcane(
            backends_arcane::ArcanePlayerLoop {
                endpoint: self.endpoint.clone(),
                client: shared.http_client.clone(),
                idx: params.idx,
                entity_id: params.entity_id,
                total: params.desired_total,
                tick_interval: params.tick_interval,
                metrics: shared.metrics.clone(),
                read_metrics: shared.read_metrics.clone(),
                action_metrics: shared.action_metrics.clone(),
                stop: params.stop,
                cluster_flag: shared.cluster_flag.clone(),
                actions_per_sec: shared.actions_per_sec,
                burst: shared.burst,
                run_started: shared.run_started,
                delta_cache: self.delta_cache.clone(),
                user_data_bytes: self.user_data_bytes,
            },
        ))
    }

    fn spawn_read(
        &self,
        _shared: &PlayerLoopShared,
        _params: &PlayerSpawnParams,
        _read_rate: f64,
    ) -> Option<tokio::task::JoinHandle<()>> {
        None
    }

    fn snapshot_cache_counters(&self) -> (u64, u64) {
        self.delta_cache.snapshot_and_reset_counters()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacetime_runtime_returns_correct_name() {
        let rt = SpacetimeRuntime::new(
            "ws://localhost:3000".to_string(),
            "test_db".to_string(),
            false,
        );
        assert_eq!(rt.name(), "spacetimedb");
    }

    #[test]
    fn arcane_runtime_returns_correct_name() {
        let endpoint = ArcaneEndpoint::SingleUrl("ws://localhost:8080".to_string());
        let rt = ArcaneRuntime::new(endpoint, 256);
        assert_eq!(rt.name(), "arcane");
    }

    #[test]
    fn spacetime_runtime_spawn_read_returns_none() {
        let rt = SpacetimeRuntime::new(
            "ws://localhost:3000".to_string(),
            "test_db".to_string(),
            false,
        );
        let shared = PlayerLoopShared {
            http_client: reqwest::Client::new(),
            metrics: Arc::new(arcane_swarm::Metrics::new()),
            read_metrics: Arc::new(arcane_swarm::Metrics::new()),
            action_metrics: Arc::new(arcane_swarm::Metrics::new()),
            cluster_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            all_ids: Arc::new(vec![uuid::Uuid::new_v4()]),
            total_players: Arc::new(std::sync::atomic::AtomicU32::new(1)),
            actions_per_sec: 10.0,
            burst: arcane_swarm::BurstConfig::default(),
            run_started: std::time::Instant::now(),
        };
        let params = PlayerSpawnParams {
            idx: 0,
            entity_id: uuid::Uuid::new_v4(),
            desired_total: 1,
            tick_interval: std::time::Duration::from_millis(50),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let result = rt.spawn_read(&shared, &params, 1.0);
        assert!(result.is_none());
    }

    #[test]
    fn arcane_runtime_spawn_read_returns_none() {
        let endpoint = ArcaneEndpoint::SingleUrl("ws://localhost:8080".to_string());
        let rt = ArcaneRuntime::new(endpoint, 256);
        let shared = PlayerLoopShared {
            http_client: reqwest::Client::new(),
            metrics: Arc::new(arcane_swarm::Metrics::new()),
            read_metrics: Arc::new(arcane_swarm::Metrics::new()),
            action_metrics: Arc::new(arcane_swarm::Metrics::new()),
            cluster_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            all_ids: Arc::new(vec![uuid::Uuid::new_v4()]),
            total_players: Arc::new(std::sync::atomic::AtomicU32::new(1)),
            actions_per_sec: 10.0,
            burst: arcane_swarm::BurstConfig::default(),
            run_started: std::time::Instant::now(),
        };
        let params = PlayerSpawnParams {
            idx: 0,
            entity_id: uuid::Uuid::new_v4(),
            desired_total: 1,
            tick_interval: std::time::Duration::from_millis(50),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let result = rt.spawn_read(&shared, &params, 1.0);
        assert!(result.is_none());
    }

    #[test]
    fn spacetime_runtime_snapshot_cache_counters_returns_zero() {
        let rt = SpacetimeRuntime::new(
            "ws://localhost:3000".to_string(),
            "test_db".to_string(),
            false,
        );
        let (hits, misses) = rt.snapshot_cache_counters();
        assert_eq!(hits, 0);
        assert_eq!(misses, 0);
    }

    #[test]
    fn arcane_runtime_snapshot_cache_counters() {
        let endpoint = ArcaneEndpoint::SingleUrl("ws://localhost:8080".to_string());
        let rt = ArcaneRuntime::new(endpoint, 256);
        let (hits, misses) = rt.snapshot_cache_counters();
        assert_eq!(hits + misses, 0);
    }
}
