//! SpacetimeDB backend loops — native SDK over a persistent WebSocket per
//! simulated player.
//!
//! ## Why SDK + WebSocket (not HTTP POST)
//!
//! SpacetimeDB's own docs steer real clients toward the SDK over WebSocket:
//! fire-and-forget reducer calls, no ~1–5 ms HTTP overhead per request, no JSON
//! round-trip per reducer arg, persistent connection lifecycle. The previous
//! revision of this file used `reqwest::Client::post(...)` per reducer call —
//! which is the "CLI / tests" pipe, not the pipe a game client would use.
//!
//! ## One WebSocket per simulated player
//!
//! The benchmark's premise is "N independent game clients drive N players."
//! We honour that by opening one `DbConnection` per player and wrapping it in
//! an `Arc` so the per-tick position loop, the action loop, and the read loop
//! share a single socket. SpacetimeDB's module is anonymous (no per-identity
//! auth), so sharing the connection across these loops doesn't change what the
//! module sees: reducer calls carry their own `entity_id` argument.
//!
//! ## What still uses HTTP
//!
//! Spatial reads (`SELECT * FROM entity WHERE x BETWEEN … AND z BETWEEN …`)
//! stay on the HTTP SQL endpoint. After the module-side `btree(x, z)` index
//! landed, these are cheap — SQL subscriptions would be equivalent here for a
//! worse-DX tradeoff, so the HTTP path survives.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time;

use arcane_swarm::{
    burst_actions_to_emit, is_zone_event_active, BurstConfig, ErrorKind, Metrics, Player,
    VISIBILITY_RADIUS,
};

use crate::spacetimedb_bindings::{
    pickup_item_reducer::pickup_item, player_interact_reducer::player_interact,
    remove_player_by_id_reducer::remove_player_by_id,
    update_player_input_reducer::update_player_input, update_player_reducer::update_player,
    use_item_reducer::use_item, DbConnection, Entity,
};

use spacetimedb_sdk::{DbContext, Uuid as StdbUuid};

/// Convert a runtime `uuid::Uuid` into the SpacetimeDB-native `Uuid`. These
/// are two different types (SpacetimeDB's is `{ __uuid__: u128 }`) even though
/// they carry the same 128-bit value.
fn to_stdb_uuid(id: &uuid::Uuid) -> StdbUuid {
    StdbUuid::from_u128(u128::from_be_bytes(*id.as_bytes()))
}

/// Connection parameters the swarm needs to open one `DbConnection` per player.
///
/// `ws_uri` is the SpacetimeDB host as a WebSocket URL (e.g. `ws://host:3000`
/// or `wss://host:3000`). `database_name` is the published module's database
/// name — same value passed to `spacetime publish`.
#[derive(Clone)]
pub(crate) struct SpacetimeConnectParams {
    pub ws_uri: String,
    pub database_name: String,
}

/// Build a `DbConnection` that runs its message loop in a background task.
/// Returns an `Arc<DbConnection>` so the caller can clone it into every loop
/// (position tick, actions, shutdown) that needs to fire reducers.
///
/// Anonymous auth: the benchmark module accepts unauthenticated reducer calls
/// and carries `entity_id` explicitly in each payload, so we don't need to
/// persist or reuse an identity token across runs.
pub(crate) fn connect_spacetimedb(
    params: &SpacetimeConnectParams,
) -> Result<Arc<DbConnection>, String> {
    let conn = DbConnection::builder()
        .with_uri(&*params.ws_uri)
        .with_database_name(&*params.database_name)
        .with_token(Option::<String>::None)
        .on_connect_error(|_ctx, err| {
            eprintln!("[stdb] connect error: {err}");
        })
        .build()
        .map_err(|e| format!("DbConnection::build failed: {e}"))?;
    let conn = Arc::new(conn);
    let bg = Arc::clone(&conn);
    tokio::spawn(async move {
        if let Err(e) = bg.run_async().await {
            eprintln!("[stdb] run_async terminated: {e}");
        }
    });
    Ok(conn)
}

#[derive(Clone, Copy)]
enum GameAction {
    PickupItem { item_type: u32, quantity: u32 },
    UseItem { item_type: u32 },
    Interact { target_idx: u32, event_type: u32 },
}

fn random_action(player_idx: u32, total_players: u32, tick: u64) -> GameAction {
    let seed = (player_idx as u64).wrapping_mul(31) ^ tick.wrapping_mul(7);
    match seed % 5 {
        0 => GameAction::PickupItem {
            item_type: (seed % 20) as u32,
            quantity: 1 + (seed % 5) as u32,
        },
        1 => GameAction::UseItem {
            item_type: (seed % 20) as u32,
        },
        _ => GameAction::Interact {
            target_idx: (player_idx + 1 + (seed % total_players.max(2) as u64) as u32)
                % total_players.max(1),
            event_type: (seed % 4) as u32,
        },
    }
}

pub(crate) struct SharedPositions {
    xs: Vec<AtomicI64>,
    zs: Vec<AtomicI64>,
}

impl SharedPositions {
    pub(crate) fn new(count: u32) -> Self {
        let n = count as usize;
        Self {
            xs: (0..n).map(|_| AtomicI64::new(0)).collect(),
            zs: (0..n).map(|_| AtomicI64::new(0)).collect(),
        }
    }

    pub(crate) fn set(&self, idx: u32, x: f64, z: f64) {
        let i = idx as usize;
        self.xs[i].store(x as i64, Ordering::Relaxed);
        self.zs[i].store(z as i64, Ordering::Relaxed);
    }

    pub(crate) fn get(&self, idx: u32) -> (f64, f64) {
        let i = idx as usize;
        (
            self.xs[i].load(Ordering::Relaxed) as f64,
            self.zs[i].load(Ordering::Relaxed) as f64,
        )
    }
}

pub(crate) struct SpacetimeActionLoop {
    pub conn: Arc<DbConnection>,
    pub player_id: uuid::Uuid,
    pub player_idx: u32,
    pub total_players: Arc<AtomicU32>,
    pub all_ids: Arc<Vec<uuid::Uuid>>,
    pub actions_per_sec: f64,
    pub action_metrics: Arc<Metrics>,
    pub stop: Arc<AtomicBool>,
    pub burst: BurstConfig,
    pub run_started: Instant,
}

pub(crate) async fn action_loop(ctx: SpacetimeActionLoop) {
    let SpacetimeActionLoop {
        conn,
        player_id,
        player_idx,
        total_players,
        all_ids,
        actions_per_sec,
        action_metrics,
        stop,
        burst,
        run_started,
    } = ctx;

    if actions_per_sec <= 0.0 {
        return;
    }
    let interval_us = (1_000_000.0 / actions_per_sec) as u64;
    let mut interval = time::interval(Duration::from_micros(interval_us));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut tick: u64 = 0;
    let player_stdb = to_stdb_uuid(&player_id);

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        tick += 1;
        let total_now = total_players.load(Ordering::Relaxed);
        let action = random_action(player_idx, total_now, tick);
        let mut burst_remaining =
            burst_actions_to_emit(player_idx, run_started.elapsed().as_millis() as u64, burst);
        if burst_remaining == 0 {
            burst_remaining = 1;
        }
        for _ in 0..burst_remaining {
            let t0 = Instant::now();
            // SDK reducer calls are fire-and-forget over the already-open WebSocket:
            // they return immediately after enqueueing the message on the socket,
            // no server round-trip on this thread. An error from the SDK means the
            // message wasn't even sent (channel closed, connection lost), not that
            // the reducer failed.
            let result: Result<(), String> = match action {
                GameAction::PickupItem {
                    item_type,
                    quantity,
                } => conn
                    .reducers
                    .pickup_item(player_stdb, item_type, quantity)
                    .map_err(|e| e.to_string()),
                GameAction::UseItem { item_type } => conn
                    .reducers
                    .use_item(player_stdb, item_type)
                    .map_err(|e| e.to_string()),
                GameAction::Interact {
                    target_idx,
                    event_type,
                } => {
                    let target = all_ids
                        .get(target_idx as usize)
                        .copied()
                        .unwrap_or(player_id);
                    let target_stdb = to_stdb_uuid(&target);
                    conn.reducers
                        .player_interact(player_stdb, target_stdb, event_type)
                        .map_err(|e| e.to_string())
                }
            };
            match result {
                Ok(()) => action_metrics.record_ok(t0.elapsed()),
                Err(_) => action_metrics.record_err_kind(ErrorKind::Transport),
            }
        }
    }
}

pub(crate) struct SpacetimeReadLoop {
    pub client: reqwest::Client,
    pub sql_url: String,
    pub read_rate: f64,
    pub read_metrics: Arc<Metrics>,
    pub stop: Arc<AtomicBool>,
    pub player_idx: u32,
    pub positions: Arc<SharedPositions>,
}

pub(crate) async fn read_loop_spacetimedb(ctx: SpacetimeReadLoop) {
    let SpacetimeReadLoop {
        client,
        sql_url,
        read_rate,
        read_metrics,
        stop,
        player_idx,
        positions,
    } = ctx;

    if read_rate <= 0.0 {
        return;
    }
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
        match client
            .post(&sql_url)
            .header("Content-Type", "text/plain")
            .body(query)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let bytes = resp.bytes().await.map(|b| b.len() as u64).unwrap_or(0);
                read_metrics.record_ok_bytes(t0.elapsed(), bytes);
            }
            Ok(resp) => {
                read_metrics.record_err_kind(ErrorKind::HttpStatus);
                if player_idx == 0 {
                    let s = resp.status();
                    let t = resp.text().await.unwrap_or_default();
                    eprintln!("[player 0 read] HTTP {}: {}", s, &t[..t.len().min(200)]);
                }
            }
            Err(e) => {
                let kind = if e.is_timeout() {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::Transport
                };
                read_metrics.record_err_kind(kind);
            }
        }
    }
}

pub(crate) struct SpacetimePlayerLoop {
    pub conn: Arc<DbConnection>,
    pub idx: u32,
    pub entity_id: uuid::Uuid,
    pub total: u32,
    pub tick_interval: Duration,
    pub metrics: Arc<Metrics>,
    pub stop: Arc<AtomicBool>,
    pub cluster_flag: Arc<AtomicBool>,
    pub server_physics: bool,
    pub positions: Arc<SharedPositions>,
    pub burst: BurstConfig,
    pub run_started: Instant,
}

pub(crate) async fn player_loop_spacetimedb(ctx: SpacetimePlayerLoop) {
    let SpacetimePlayerLoop {
        conn,
        idx,
        entity_id,
        total,
        tick_interval,
        metrics,
        stop,
        cluster_flag,
        server_physics: _server_physics,
        positions,
        burst,
        run_started,
    } = ctx;

    let clustered = cluster_flag.load(Ordering::Relaxed);
    let mut player = Player::new(entity_id, idx, total, clustered);
    positions.set(idx, player.x, player.z);
    let tick_dt = tick_interval.as_secs_f64();
    let mut interval = time::interval(tick_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let player_stdb = to_stdb_uuid(&player.id);
    let mut first_tick = true;

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        if is_zone_event_active(run_started.elapsed().as_millis() as u64, burst) {
            player.steer_to_point(2500.0, 2500.0);
        }
        player.tick(tick_dt, cluster_flag.load(Ordering::Relaxed));
        positions.set(idx, player.x, player.z);

        let t0 = Instant::now();
        let result = if first_tick {
            // Insert the Entity row so subsequent input updates have something to move.
            let entity = Entity {
                entity_id: player_stdb,
                x: player.x,
                y: player.y,
                z: player.z,
            };
            let r = conn
                .reducers
                .update_player(entity)
                .map_err(|e| e.to_string());
            first_tick = false;
            r
        } else {
            conn.reducers
                .update_player_input(player_stdb, player.dir_x, player.dir_z)
                .map_err(|e| e.to_string())
        };
        match result {
            Ok(()) => metrics.record_ok(t0.elapsed()),
            Err(e) => {
                metrics.record_err_kind(ErrorKind::Transport);
                if idx == 0 {
                    eprintln!("[player 0] SDK reducer error: {e}");
                }
            }
        }
    }

    // Best-effort removal on shutdown. If the socket is already closed the SDK
    // returns an error that we just swallow — the benchmark is tearing down.
    let _ = conn.reducers.remove_player_by_id(player_stdb);
    let _ = conn.disconnect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_stdb_uuid_roundtrips_u128() {
        let id = uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1122_3344_5566_7788);
        let stdb = to_stdb_uuid(&id);
        assert_eq!(stdb.as_u128(), id.as_u128());
    }

    #[test]
    fn random_action_handles_zero_players_without_panic() {
        for tick in 0..20 {
            match random_action(0, 0, tick) {
                GameAction::Interact {
                    target_idx,
                    event_type,
                } => {
                    assert_eq!(target_idx, 0);
                    assert!(event_type < 4);
                }
                GameAction::PickupItem {
                    item_type,
                    quantity,
                } => {
                    assert!(item_type < 20);
                    assert!((1..=5).contains(&quantity));
                }
                GameAction::UseItem { item_type } => {
                    assert!(item_type < 20);
                }
            }
        }
    }

    #[test]
    fn shared_positions_roundtrip() {
        let positions = SharedPositions::new(2);
        positions.set(1, 42.0, -9.0);
        assert_eq!(positions.get(1), (42.0, -9.0));
    }
}
