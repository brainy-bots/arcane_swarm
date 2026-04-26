//! Arcane backend loops (manager join + cluster WebSocket write/read).
//!
//! Implements binary-internal runtime behavior for `--backend arcane`.
//!
//! ## What the "lat" column means here
//!
//! The client's outbound writes are fire-and-forget WebSocket frames —
//! `sink.send(Message::Binary(_))` returns in nanoseconds once the bytes are
//! queued in the local send buffer, regardless of whether the cluster is
//! healthy, lagging, or dead. Timing the call site is therefore meaningless.
//!
//! Instead the swarm measures **client-perceived latency**: the wall-clock
//! gap between "I wrote state at T0" and "the cluster's next broadcast frame
//! carrying my own entity landed in my receive buffer at T1". That's what a
//! real game client experiences — action → world-reflection. Under server
//! load the cluster's tick slides later, the broadcast arrives late, and the
//! number rises. Under catastrophic load the broadcast stops, and the
//! `NotDelivered` / `ConnectionDrop` counters pick up.
//!
//! The swarm decodes each incoming binary frame via
//! [`arcane_wire::decode_server`] and walks `DeltaPayload::updated`; when it
//! finds its own `entity_id` it computes `now - last_send` and records the
//! sample. Every outbound write (movement or action) updates `last_send`.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arcane_wire::{decode_server, ServerFrame};
use futures_util::{SinkExt, StreamExt};
use tokio::time;
use tokio_tungstenite::tungstenite::Message;

use arcane_swarm::{
    encode_game_action, encode_player_state, is_zone_event_active, ArcaneEndpoint, BurstConfig,
    ErrorKind, Metrics, Player,
};

#[derive(serde::Deserialize)]
struct ManagerJoinResponse {
    server_host: String,
    server_port: u16,
}

/// Resolve WebSocket URL for one player. If using manager, GET base/join and build ws://host:port.
pub(crate) async fn resolve_arcane_ws(
    endpoint: &ArcaneEndpoint,
    client: &reqwest::Client,
    player_idx: u32,
) -> String {
    match endpoint {
        ArcaneEndpoint::SingleUrl(url) => url.clone(),
        ArcaneEndpoint::ManagerJoin { base_url } => {
            let join_url = format!("{}/join", base_url.trim_end_matches('/'));
            const RETRIES: u32 = 3;
            for attempt in 0..RETRIES {
                match client.get(&join_url).send().await {
                    Ok(resp) if resp.status().is_success() => {
                        if let Ok(join) = resp.json::<ManagerJoinResponse>().await {
                            return format!("ws://{}:{}", join.server_host, join.server_port);
                        }
                    }
                    Ok(resp) => {
                        if player_idx == 0 && attempt == RETRIES - 1 {
                            let status = resp.status();
                            let t = resp.text().await.unwrap_or_default();
                            eprintln!(
                                "[player 0] manager join HTTP {}: {}",
                                status,
                                &t[..t.len().min(200)]
                            );
                        }
                    }
                    Err(e) => {
                        if player_idx == 0 && attempt == RETRIES - 1 {
                            eprintln!(
                                "[player 0] manager join error (after {} attempts): {}",
                                RETRIES, e
                            );
                        }
                    }
                }
                if attempt < RETRIES - 1 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
            // Do not fall back to 8080: our clusters use 8090+. Falling back would send all traffic to one wrong process.
            if player_idx == 0 {
                eprintln!("[player 0] manager join failed after {} attempts; using invalid URL so this player fails (fix manager/ports).", RETRIES);
            }
            "ws://127.0.0.1:1".to_string()
        }
    }
}

/// Arguments for [`player_loop_arcane`].
pub(crate) struct ArcanePlayerLoop {
    pub endpoint: ArcaneEndpoint,
    pub client: reqwest::Client,
    pub idx: u32,
    pub entity_id: uuid::Uuid,
    pub total: u32,
    pub tick_interval: Duration,
    pub metrics: Arc<Metrics>,
    pub read_metrics: Arc<Metrics>,
    pub action_metrics: Arc<Metrics>,
    pub stop: Arc<AtomicBool>,
    pub cluster_flag: Arc<AtomicBool>,
    pub actions_per_sec: f64,
    pub burst: BurstConfig,
    pub run_started: std::time::Instant,
}

/// Pick a random game action for this player at this tick.
fn random_arcane_action(player_idx: u32, tick: u64) -> (&'static str, String) {
    let seed = (player_idx as u64).wrapping_mul(31) ^ tick.wrapping_mul(7);
    match seed % 5 {
        0 => {
            let item_type = (seed % 20) as u32;
            let quantity = 1 + (seed % 5) as u32;
            (
                "pickup_item",
                format!(r#"{{"item_type":{},"quantity":{}}}"#, item_type, quantity),
            )
        }
        1 => {
            let item_type = (seed % 20) as u32;
            ("use_item", format!(r#"{{"item_type":{}}}"#, item_type))
        }
        _ => {
            let event_type = (seed % 4) as u32;
            (
                "interact",
                format!(
                    r#"{{"target_id":"{}","event_type":{}}}"#,
                    uuid::Uuid::nil(),
                    event_type
                ),
            )
        }
    }
}

pub(crate) async fn player_loop_arcane(ctx: ArcanePlayerLoop) {
    let ArcanePlayerLoop {
        endpoint,
        client,
        idx,
        entity_id,
        total,
        tick_interval,
        metrics,
        read_metrics,
        action_metrics,
        stop,
        cluster_flag,
        actions_per_sec,
        burst,
        run_started,
    } = ctx;

    let ws_url = resolve_arcane_ws(&endpoint, &client, idx).await;
    let clustered = cluster_flag.load(Ordering::Relaxed);
    let mut player = Player::new(entity_id, idx, total, clustered);
    let tick_dt = tick_interval.as_secs_f64();

    let ws_stream = match tokio_tungstenite::connect_async(&ws_url).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            if idx == 0 {
                eprintln!("[player 0] WebSocket connect failed: {}", e);
            }
            metrics.record_err_kind(ErrorKind::NotDelivered);
            return;
        }
    };
    let (mut sink, mut stream) = ws_stream.split();

    // Shared micros-since-run-start of the player's most recent outbound write
    // (movement or action). The drain task reads this when it spots the
    // player's own entity_id in an incoming broadcast frame and computes
    // latency = now - last_send. Using a shared atomic rather than passing
    // through a channel keeps the hot paths lock-free.
    let last_send_micros = Arc::new(AtomicI64::new(-1));
    // Same instant in UNIX micros — used for the cross-clock T2 - T1 wire
    // portion of the latency decomposition. Updated alongside
    // `last_send_micros` at every outbound write so they refer to the same
    // moment.
    let last_send_unix_us = Arc::new(AtomicI64::new(-1));

    let stop_drain = stop.clone();
    let rm = read_metrics.clone();
    let latency_metrics = metrics.clone();
    let last_send_drain = last_send_micros.clone();
    let last_send_unix_drain = last_send_unix_us.clone();
    let my_id = player.id;
    let drain_run_started = run_started;
    tokio::spawn(async move {
        while !stop_drain.load(Ordering::Relaxed) {
            match stream.next().await {
                Some(Ok(Message::Binary(bin))) => {
                    // T_arrival: capture immediately, before any decode work,
                    // so `drain_us = T3 - T_arrival` is purely on-driver
                    // post-receive cost (decode + linear scan + record). This
                    // is clock-sync free.
                    let arrival_us = drain_run_started.elapsed().as_micros() as i64;
                    rm.record_inbound_ok(bin.len() as u64);
                    // Decode the cluster's broadcast delta and look for our
                    // own entity_id. The first hit produces a real latency
                    // sample; extra hits in the same frame (shouldn't happen,
                    // but defensive) are ignored to avoid double-counting.
                    if let Ok(ServerFrame::Delta(payload)) = decode_server(&bin) {
                        if payload.updated.iter().any(|e| e.entity_id == my_id) {
                            let sent = last_send_drain.load(Ordering::Relaxed);
                            if sent >= 0 {
                                let now = drain_run_started.elapsed().as_micros() as i64;
                                let total_us = (now - sent).max(0) as u64;
                                let drain_us = (now - arrival_us).max(0) as u64;
                                // Wire portion: T2 (server stamp, UNIX
                                // seconds f64) − T1 (driver's send moment in
                                // UNIX micros). Both endpoints chrony-synced
                                // to ~1ms on AWS; clock-skew bias is folded
                                // here. `payload.timestamp <= 0.0` means the
                                // server didn't stamp this frame (e.g. older
                                // image), so we skip the wire sample.
                                let wire_lat = if payload.timestamp > 0.0 {
                                    let sent_unix = last_send_unix_drain.load(Ordering::Relaxed);
                                    if sent_unix > 0 {
                                        let server_us = (payload.timestamp * 1_000_000.0) as i64;
                                        let wire_us = (server_us - sent_unix).max(0) as u64;
                                        Some(Duration::from_micros(wire_us))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };
                                latency_metrics.record_ok_decomposed(
                                    Duration::from_micros(total_us),
                                    wire_lat,
                                    Duration::from_micros(drain_us),
                                );
                            }
                        }
                    }
                }
                Some(Ok(Message::Text(txt))) => {
                    // Cluster no longer speaks text frames after Shape B, but
                    // we still accept any bytes we receive as inbound traffic.
                    rm.record_inbound_ok(txt.len() as u64);
                }
                Some(Ok(_)) => {}
                _ => {
                    rm.record_err_kind(ErrorKind::ConnectionDrop);
                    break;
                }
            }
        }
    });

    let mut interval = time::interval(tick_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    // Action timing: send game actions at configured rate via the same WebSocket
    let action_interval_us = if actions_per_sec > 0.0 {
        Some((1_000_000.0 / actions_per_sec) as u64)
    } else {
        None
    };
    let mut last_action = std::time::Instant::now();
    let mut action_tick: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        if is_zone_event_active(run_started.elapsed().as_millis() as u64, burst) {
            player.steer_to_point(2500.0, 2500.0);
        }
        player.tick(tick_dt, cluster_flag.load(Ordering::Relaxed));

        // Send movement — binary postcard frame for fairness with SpacetimeDB's
        // BSATN. See brainy-bots/arcane#28 for motivation.
        let msg = encode_player_state(
            &player.id, player.x, player.y, player.z, player.vx, player.vy, player.vz,
        );
        match sink.send(Message::Binary(msg)).await {
            Ok(_) => {
                // Count the successful enqueue; latency is recorded by the
                // drain task when it sees the cluster echo this write back.
                metrics.record_ok_count();
                last_send_micros.store(run_started.elapsed().as_micros() as i64, Ordering::Relaxed);
                // UNIX wall-clock copy of the same moment for the wire
                // (T2 - T1) portion of the latency decomposition.
                if let Ok(d) = SystemTime::now().duration_since(UNIX_EPOCH) {
                    last_send_unix_us
                        .store(d.as_micros() as i64, Ordering::Relaxed);
                }
            }
            Err(e) => {
                metrics.record_err_kind(ErrorKind::NotDelivered);
                if idx == 0 {
                    eprintln!("[player 0] ws send error: {}", e);
                }
                break;
            }
        }

        // Send game action if it's time. Action payload is already a JSON
        // string (from random_arcane_action); we pass its bytes through
        // opaquely — the cluster deserializes on the other side.
        if let Some(interval_us) = action_interval_us {
            if last_action.elapsed() >= Duration::from_micros(interval_us) {
                action_tick += 1;
                let (action_type, payload) = random_arcane_action(idx, action_tick);
                let action_msg = encode_game_action(&player.id, action_type, payload.as_bytes());
                match sink.send(Message::Binary(action_msg)).await {
                    Ok(_) => {
                        // Same pattern as movement: count the enqueue; the
                        // next echo carrying our entity provides the latency
                        // sample for the combined write stream.
                        action_metrics.record_ok_count();
                        last_send_micros
                            .store(run_started.elapsed().as_micros() as i64, Ordering::Relaxed);
                        if let Ok(d) = SystemTime::now().duration_since(UNIX_EPOCH) {
                            last_send_unix_us
                                .store(d.as_micros() as i64, Ordering::Relaxed);
                        }
                    }
                    Err(e) => {
                        action_metrics.record_err();
                        if idx == 0 {
                            eprintln!("[player 0] ws action send error: {}", e);
                        }
                        break;
                    }
                }
                last_action = std::time::Instant::now();
            }
        }
    }
}
