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
//! ## One WebSocket per simulated player — shared between movement and actions
//!
//! The benchmark's premise is "N independent game clients drive N players."
//! Each simulated player opens exactly one `DbConnection` that carries BOTH
//! the movement reducer calls (20 Hz) and the game-action reducer calls
//! (~2 Hz). This mirrors `backends_arcane.rs`, where the same WebSocket sends
//! movement and actions, and it matches how a real game client would talk to
//! a SpacetimeDB backend.
//!
//! An earlier revision multiplexed every player through one shared WebSocket —
//! the client reported 0 errors, but SpacetimeDB's per-client
//! `incoming_queue_length` limit (16384 messages) silently overflowed once
//! aggregate traffic passed that rate. Every call beyond the limit was dropped
//! server-side while the SDK's fire-and-forget API happily returned Ok. We
//! fixed that by moving to one WS per player (22 msg/sec per connection, 750×
//! under the limit).
//!
//! A later revision opened two WebSockets per player (one for movement, one
//! for actions) because the loops were structured as independent tasks. That
//! doubled socket count with no scaling benefit: the Arcane comparison already
//! uses one WS per player, so the extra sockets were pure benchmark-fairness
//! noise. This revision collapses them back to one.
//!
//! ## What the "lat" column means here
//!
//! The SDK's `conn.reducers.update_player_input(...)` is fire-and-forget —
//! the call returns in microseconds once the message is queued in the SDK's
//! send buffer, regardless of whether SpacetimeDB has processed it. Timing
//! the call site would always read ~0 ms and tell us nothing about server
//! load. Instead the swarm measures **client-perceived latency**:
//!
//! 1. Every outbound write updates a shared `last_send_micros` for this
//!    player.
//! 2. The same connection is subscribed to the `entity` table (restricted to
//!    a spatial region around the player's starting position, matching what
//!    a real game client's area-of-interest subscription would look like).
//! 3. When the SDK fires `on_update(Entity)` for the player's own entity_id,
//!    we compute `now - last_send` and record that as a latency sample.
//!
//! That number represents "time from my action to seeing my world reflect
//! it" — the same quantity a real game client experiences. Under server
//! load, tick processing slides later, the subscription push lags, and the
//! number rises. Under catastrophic load the subscription stalls and the
//! `NotDelivered` / `Transport` error counters pick up.
//!
//! The previous HTTP SQL polling path (`reqwest::Client.post(sql_url)`
//! against `/v1/database/<db>/sql`) has been removed — it was the CLI-shape
//! read, not the pipe a game client uses. Subscriptions fill the same role
//! and are what SpacetimeDB's docs recommend for real clients.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time;

use arcane_swarm::{
    burst_actions_to_emit, is_zone_event_active, BurstConfig, ErrorKind, Metrics, Player,
    VISIBILITY_RADIUS,
};

use crate::spacetimedb_bindings::{
    entity_table::EntityTableAccess, pickup_item_reducer::pickup_item,
    player_interact_reducer::player_interact, remove_player_by_id_reducer::remove_player_by_id,
    update_player_input_reducer::update_player_input, update_player_reducer::update_player,
    use_item_reducer::use_item, DbConnection, Entity,
};

use spacetimedb_sdk::{DbContext, TableWithPrimaryKey, Uuid as StdbUuid};

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
        .on_disconnect(|_ctx, err| {
            // One shared socket carries every simulated player's reducer calls.
            // If it drops mid-run, every player's writes stop at the same tick
            // — the metrics will show a cliff. Log so it's attributable.
            eprintln!("[stdb] socket disconnected: {err:?}");
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

pub(crate) struct SpacetimePlayerLoop {
    /// One dedicated WebSocket per simulated player — shared between movement
    /// and action reducer calls. Opened inside `player_loop_spacetimedb` so
    /// every task drives its own connection.
    pub connect_params: SpacetimeConnectParams,
    pub idx: u32,
    pub entity_id: uuid::Uuid,
    pub total: u32,
    pub tick_interval: Duration,
    pub metrics: Arc<Metrics>,
    pub read_metrics: Arc<Metrics>,
    pub action_metrics: Arc<Metrics>,
    pub stop: Arc<AtomicBool>,
    pub cluster_flag: Arc<AtomicBool>,
    pub server_physics: bool,
    pub all_ids: Arc<Vec<uuid::Uuid>>,
    pub total_players: Arc<AtomicU32>,
    pub actions_per_sec: f64,
    pub burst: BurstConfig,
    pub run_started: Instant,
}

pub(crate) async fn player_loop_spacetimedb(ctx: SpacetimePlayerLoop) {
    let SpacetimePlayerLoop {
        connect_params,
        idx,
        entity_id,
        total,
        tick_interval,
        metrics,
        read_metrics,
        action_metrics,
        stop,
        cluster_flag,
        server_physics: _server_physics,
        all_ids,
        total_players,
        actions_per_sec,
        burst,
        run_started,
    } = ctx;

    let conn = match connect_spacetimedb(&connect_params) {
        Ok(c) => c,
        Err(e) => {
            if idx == 0 {
                eprintln!("[stdb player 0] connect failed: {e}");
            }
            metrics.record_err_kind(ErrorKind::NotDelivered);
            return;
        }
    };

    let clustered = cluster_flag.load(Ordering::Relaxed);
    let mut player = Player::new(entity_id, idx, total, clustered);
    let tick_dt = tick_interval.as_secs_f64();
    let mut interval = time::interval(tick_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    let player_stdb = to_stdb_uuid(&player.id);

    // Shared micros-since-run-start of this player's most recent outbound
    // reducer call. The `on_update` callback reads this to compute
    // client-perceived latency when SpacetimeDB delivers the subscribed
    // `entity` row back. See the module-level docstring for the full
    // rationale — it's the same mechanism used by `backends_arcane.rs`.
    let last_send_micros = Arc::new(AtomicI64::new(-1));

    // Register the latency hook before we subscribe, so any update that
    // arrives during the initial applied() is counted. The SDK invokes the
    // callback on its internal task, so the captures must be Send + 'static.
    {
        let last_send_hook = last_send_micros.clone();
        let metrics_hook = metrics.clone();
        let read_metrics_hook = read_metrics.clone();
        let run_started_hook = run_started;
        conn.db.entity().on_update(move |_ectx, _old, new| {
            // Count every inbound row update as "read workload" — this is the
            // equivalent of the byte-counting in the Arcane WS drain task.
            read_metrics_hook.record_inbound_ok(std::mem::size_of::<Entity>() as u64);
            if new.entity_id == player_stdb {
                let sent = last_send_hook.load(Ordering::Relaxed);
                if sent >= 0 {
                    let now = run_started_hook.elapsed().as_micros() as i64;
                    let lat_us = (now - sent).max(0) as u64;
                    metrics_hook.record_ok(Duration::from_micros(lat_us));
                }
            }
        });
    }

    // Subscribe to the player's spatial area-of-interest. Static box around
    // the starting position — matches what a real game client's subscription
    // would look like (entities near me) and keeps per-connection fan-out
    // bounded. Players don't wander far during a 30 s tier at the benchmark's
    // movement velocities, so the starting-position box is a faithful proxy.
    //
    // We also subscribe to the player's own row unconditionally so the
    // latency signal stays intact even if a player drifts out of the AOI box
    // (which shouldn't happen over 30 s but the defensive subscription is
    // essentially free).
    let (px0, pz0) = (player.x as i64, player.z as i64);
    let r = VISIBILITY_RADIUS as i64;
    let aoi_query = format!(
        "SELECT * FROM entity WHERE x >= {} AND x <= {} AND z >= {} AND z <= {}",
        px0 - r,
        px0 + r,
        pz0 - r,
        pz0 + r,
    );
    let self_query = format!(
        "SELECT * FROM entity WHERE entity_id = 0x{:032x}",
        player_stdb.as_u128()
    );
    let _sub = conn
        .subscription_builder()
        .on_error(|_err_ctx, err| {
            eprintln!("[stdb] subscription error: {err}");
        })
        .subscribe([aoi_query.as_str(), self_query.as_str()]);

    let mut first_tick = true;

    // Action timing: emit game actions at configured rate via the same
    // WebSocket as movement, driven off the movement tick (mirrors
    // `backends_arcane.rs`).
    let action_interval_us = if actions_per_sec > 0.0 {
        Some((1_000_000.0 / actions_per_sec) as u64)
    } else {
        None
    };
    let mut last_action = Instant::now();
    let mut action_tick: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        if is_zone_event_active(run_started.elapsed().as_millis() as u64, burst) {
            player.steer_to_point(2500.0, 2500.0);
        }
        player.tick(tick_dt, cluster_flag.load(Ordering::Relaxed));

        // Movement reducer call (fire-and-forget — latency is measured by the
        // on_update handler above).
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
            Ok(()) => {
                // Count the successful enqueue; latency lands via on_update.
                metrics.record_ok_count();
                last_send_micros.store(run_started.elapsed().as_micros() as i64, Ordering::Relaxed);
            }
            Err(e) => {
                metrics.record_err_kind(ErrorKind::Transport);
                if idx == 0 {
                    eprintln!("[player 0] SDK reducer error: {e}");
                }
            }
        }

        // Action reducer calls (same connection, time-gated)
        if let Some(interval_us) = action_interval_us {
            if last_action.elapsed() >= Duration::from_micros(interval_us) {
                action_tick += 1;
                let total_now = total_players.load(Ordering::Relaxed);
                let action = random_action(idx, total_now, action_tick);
                let mut burst_remaining =
                    burst_actions_to_emit(idx, run_started.elapsed().as_millis() as u64, burst);
                if burst_remaining == 0 {
                    burst_remaining = 1;
                }
                for _ in 0..burst_remaining {
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
                                .unwrap_or(entity_id);
                            let target_stdb = to_stdb_uuid(&target);
                            conn.reducers
                                .player_interact(player_stdb, target_stdb, event_type)
                                .map_err(|e| e.to_string())
                        }
                    };
                    match result {
                        Ok(()) => {
                            action_metrics.record_ok_count();
                            last_send_micros
                                .store(run_started.elapsed().as_micros() as i64, Ordering::Relaxed);
                        }
                        Err(_) => action_metrics.record_err_kind(ErrorKind::Transport),
                    }
                }
                last_action = Instant::now();
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
}
