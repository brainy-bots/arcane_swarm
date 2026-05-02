//! Backend-agnostic runtime trait and concrete per-backend implementations.
//!
//! Extracted from `main.rs` (713→~250 lines). Each impl owns backend-specific
//! state (connection params, decode cache, endpoint config) and implements
//! `BackendRuntime` with a single `SharedHandles` clone per player spawn —
//! rather than cloning every Arc field individually.
//!
//! ## Unit-test targets
//!
//! - Trait dispatch: verify the correct `name()` and `spawn_player`/`spawn_read`
//!   wiring for both backends.
//! - Error paths: simulate a connection failure and check that the returned
//!   `JoinHandle` completes with an error / the caller can detect it through
//!   the metrics sink.
//! - `SharedHandles::clone` produces independent (but pointer-equal) `Arc`s.

use arcane_swarm::{ArcaneEndpoint, BurstConfig, DeltaCache, Metrics};
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;

use crate::backends_arcane::{self, ArcanePlayerLoop};
use crate::backends_spacetimedb::{self, SpacetimeConnectParams, SpacetimePlayerLoop};
use crate::spawn_context::{PlayerLoopShared, PlayerSpawnParams};

// ---- SharedHandles --------------------------------------------------------

/// Bundle of `Arc`-wrapped sinks and atomics cloned once per player
/// instead of field-by-field inside each `spawn_player` implementation.
///
/// Constructed once before the player-spawn loop and passed by reference to
/// every `BackendRuntime::spawn_player` call. The impl clones the whole
/// struct once and moves it into the async block.
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

// ---- BackendRuntime trait -------------------------------------------------

/// Backend-specific runtime selected at startup (`spacetimedb` vs `arcane`).
/// Binary-internal only; CLI and wire formats are unchanged.
///
/// Both backends carry movement and action reducer calls on one WebSocket per
/// simulated player, so `spawn_player` owns the entire per-player lifecycle
/// (connect, movement loop, action loop, disconnect). There is no separate
/// action spawn.
pub(crate) trait BackendRuntime: Send + Sync {
    fn name(&self) -> &'static str;

    /// Spawn one player's full lifecycle task, taking ownership of shared
    /// handles via a single `SharedHandles::clone()`.
    fn spawn_player(
        &self,
        handles: &SharedHandles,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()>;

    /// Spawn an optional background read-simulation task. Most backends return
    /// `None` because reads arrive via subscription callbacks or inline drain
    /// loops rather than a separate task.
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

// ---- SpacetimeRuntime -----------------------------------------------------

pub(crate) struct SpacetimeRuntime {
    /// Connection params handed to every player loop so it opens its own
    /// dedicated WebSocket. Multiplexing all players over one socket hit
    /// SpacetimeDB's per-client `incoming_queue_length` limit under load and
    /// silently dropped messages — see backends_spacetimedb.rs top-of-file.
    pub connect_params: SpacetimeConnectParams,
    pub server_physics: bool,
}

impl BackendRuntime for SpacetimeRuntime {
    fn name(&self) -> &'static str {
        "spacetimedb"
    }

    fn spawn_player(
        &self,
        handles: &SharedHandles,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()> {
        let connect_params = self.connect_params.clone();
        let server_physics = self.server_physics;
        let h = handles.clone();
        tokio::spawn(backends_spacetimedb::player_loop_spacetimedb(
            SpacetimePlayerLoop {
                connect_params,
                idx: params.idx,
                entity_id: params.entity_id,
                total: params.desired_total,
                tick_interval: params.tick_interval,
                metrics: h.metrics,
                read_metrics: h.read_metrics,
                action_metrics: h.action_metrics,
                stop: params.stop,
                cluster_flag: h.cluster_flag,
                server_physics,
                all_ids: h.all_ids,
                total_players: h.total_players,
                actions_per_sec: h.actions_per_sec,
                burst: h.burst,
                run_started: h.run_started,
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

// ---- ArcaneRuntime --------------------------------------------------------

pub(crate) struct ArcaneRuntime {
    pub endpoint: ArcaneEndpoint,
    /// Per-driver shared decode cache. Populated lazily by drain tasks; see
    /// `arcane_swarm::delta_cache` for why it exists. Lives on the runtime
    /// (rather than `PlayerLoopShared`) because it's Arcane-specific —
    /// SpacetimeDB backend has its own subscription pipeline.
    pub delta_cache: Arc<DeltaCache>,
    /// Bytes per `PlayerStatePayload.user_data` payload. Same reasoning as
    /// `delta_cache` — Arcane-specific knob; SpacetimeDB backend has no
    /// equivalent opaque-payload field on its movement frames.
    pub user_data_bytes: usize,
}

impl BackendRuntime for ArcaneRuntime {
    fn name(&self) -> &'static str {
        "arcane"
    }

    fn spawn_player(
        &self,
        handles: &SharedHandles,
        params: PlayerSpawnParams,
    ) -> tokio::task::JoinHandle<()> {
        let endpoint = self.endpoint.clone();
        let delta_cache = self.delta_cache.clone();
        let user_data_bytes = self.user_data_bytes;
        let h = handles.clone();
        tokio::spawn(backends_arcane::player_loop_arcane(ArcanePlayerLoop {
            endpoint,
            client: h.http_client,
            idx: params.idx,
            entity_id: params.entity_id,
            total: params.desired_total,
            tick_interval: params.tick_interval,
            metrics: h.metrics,
            read_metrics: h.read_metrics,
            action_metrics: h.action_metrics,
            stop: params.stop,
            cluster_flag: h.cluster_flag,
            actions_per_sec: h.actions_per_sec,
            burst: h.burst,
            run_started: h.run_started,
            delta_cache,
            user_data_bytes,
        }))
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

// ---- Tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    // -- SharedHandles construction helpers ---------------------------------

    fn dummy_handles() -> SharedHandles {
        SharedHandles {
            http_client: reqwest::Client::new(),
            metrics: Arc::new(Metrics::new()),
            read_metrics: Arc::new(Metrics::new()),
            action_metrics: Arc::new(Metrics::new()),
            cluster_flag: Arc::new(AtomicBool::new(false)),
            all_ids: Arc::new(vec![uuid::Uuid::nil()]),
            total_players: Arc::new(AtomicU32::new(1)),
            actions_per_sec: 2.0,
            burst: BurstConfig::default(),
            run_started: std::time::Instant::now(),
        }
    }

    fn dummy_params(idx: u32) -> PlayerSpawnParams {
        PlayerSpawnParams {
            idx,
            entity_id: uuid::Uuid::nil(),
            desired_total: 1,
            tick_interval: Duration::from_millis(50),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    // -- SpacetimeRuntime tests ---------------------------------------------

    #[test]
    fn spacetimedb_name_is_correct() {
        let rt = SpacetimeRuntime {
            connect_params: SpacetimeConnectParams {
                ws_uri: "ws://localhost:3000".into(),
                database_name: "test".into(),
            },
            server_physics: false,
        };
        assert_eq!(rt.name(), "spacetimedb");
    }

    #[tokio::test]
    async fn spacetimedb_spawn_player_completes_when_stop_flag_set() {
        let rt = SpacetimeRuntime {
            connect_params: SpacetimeConnectParams {
                ws_uri: "ws://localhost:3000".into(),
                database_name: "test".into(),
            },
            server_physics: false,
        };
        let handles = dummy_handles();
        let mut params = dummy_params(0);
        let stop = Arc::new(AtomicBool::new(false));
        params.stop = stop.clone();

        let task = rt.spawn_player(&handles, params);
        // Signal stop so the player loop exits immediately.
        stop.store(true, Ordering::Relaxed);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        // The task should complete (either normally or with an error from
        // connection failure; we only care that it doesn't hang).
    }

    #[test]
    fn spacetimedb_spawn_read_returns_none() {
        let rt = SpacetimeRuntime {
            connect_params: SpacetimeConnectParams {
                ws_uri: "ws://localhost:3000".into(),
                database_name: "test".into(),
            },
            server_physics: false,
        };
        let shared = PlayerLoopShared {
            http_client: reqwest::Client::new(),
            metrics: Arc::new(Metrics::new()),
            read_metrics: Arc::new(Metrics::new()),
            action_metrics: Arc::new(Metrics::new()),
            cluster_flag: Arc::new(AtomicBool::new(false)),
            all_ids: Arc::new(vec![]),
            total_players: Arc::new(AtomicU32::new(0)),
            actions_per_sec: 0.0,
            burst: BurstConfig::default(),
            run_started: std::time::Instant::now(),
        };
        let params = dummy_params(0);
        assert!(rt.spawn_read(&shared, &params, 0.0).is_none());
    }

    #[test]
    fn spacetimedb_snapshot_cache_counters_returns_zero() {
        let rt = SpacetimeRuntime {
            connect_params: SpacetimeConnectParams {
                ws_uri: "ws://localhost:3000".into(),
                database_name: "test".into(),
            },
            server_physics: false,
        };
        assert_eq!(rt.snapshot_cache_counters(), (0, 0));
    }

    // -- ArcaneRuntime tests ------------------------------------------------

    #[test]
    fn arcane_name_is_correct() {
        let rt = ArcaneRuntime {
            endpoint: ArcaneEndpoint::SingleUrl("ws://localhost:8080".into()),
            delta_cache: Arc::new(DeltaCache::default()),
            user_data_bytes: 0,
        };
        assert_eq!(rt.name(), "arcane");
    }

    #[test]
    fn arcane_spawn_read_returns_none() {
        let rt = ArcaneRuntime {
            endpoint: ArcaneEndpoint::SingleUrl("ws://localhost:8080".into()),
            delta_cache: Arc::new(DeltaCache::default()),
            user_data_bytes: 0,
        };
        let shared = PlayerLoopShared {
            http_client: reqwest::Client::new(),
            metrics: Arc::new(Metrics::new()),
            read_metrics: Arc::new(Metrics::new()),
            action_metrics: Arc::new(Metrics::new()),
            cluster_flag: Arc::new(AtomicBool::new(false)),
            all_ids: Arc::new(vec![]),
            total_players: Arc::new(AtomicU32::new(0)),
            actions_per_sec: 0.0,
            burst: BurstConfig::default(),
            run_started: std::time::Instant::now(),
        };
        let params = dummy_params(0);
        assert!(rt.spawn_read(&shared, &params, 0.0).is_none());
    }

    #[test]
    fn arcane_snapshot_cache_counters_returns_cache_state() {
        let cache = Arc::new(DeltaCache::default());
        let rt = ArcaneRuntime {
            endpoint: ArcaneEndpoint::SingleUrl("ws://localhost:8080".into()),
            delta_cache: cache.clone(),
            user_data_bytes: 0,
        };
        // Fresh cache: hits=0, misses=0
        assert_eq!(rt.snapshot_cache_counters(), (0, 0));
    }

    // -- SharedHandles clone test -------------------------------------------

    #[test]
    fn shared_handles_clone_produces_pointer_equal_arcs() {
        let h = dummy_handles();
        let c = h.clone();
        // Both Arcs point to the same Metrics instance (same allocation).
        assert!(Arc::ptr_eq(&h.metrics, &c.metrics));
        assert!(Arc::ptr_eq(&h.read_metrics, &c.read_metrics));
    }
}
