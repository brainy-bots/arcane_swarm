//! Arcane backend loops (manager join + cluster WebSocket write/read).
//!
//! Implements binary-internal runtime behavior for `--backend arcane`.
//!
//! ## What the "lat" column means here
//!
//! The swarm measures **sequence-tagged round-trip latency**: each outbound
//! `PlayerState` frame carries a monotonic `client_seq` and records the send
//! `Instant` in a per-player flight table. The cluster echoes `client_seq`
//! through the entity state back in the broadcast. When the drain task sees
//! its own entity in a broadcast, it reads the echoed `client_seq`, looks up
//! the flight table, and computes RTT = `Instant::now() - sent_at`. This
//! measures the true end-to-end round trip for each specific send, immune to
//! overwrite artifacts from the previous `last_send` atomic approach.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arcane_wire::{decode_server, ServerFrame};
use futures_util::{SinkExt, StreamExt};
use tokio::time;
use tokio_tungstenite::tungstenite::Message;

use arcane_swarm::{
    encode_game_action, encode_player_state, fill_pseudo_user_data, is_zone_event_active,
    ArcaneEndpoint, BurstConfig, CachedDelta, DeltaCache, ErrorKind, Metrics, Player,
};
use std::collections::HashSet;

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
    /// Per-driver shared decode cache. The drain task consults this before
    /// running a full `decode_server`; on hit it skips the FlatBuffer decode
    /// entirely and reads the cached entity-id set. See `delta_cache.rs`
    /// for the architecture rationale.
    pub delta_cache: Arc<DeltaCache>,
    /// Bytes per `PlayerStatePayload.user_data` payload. 0 = lean baseline
    /// (the historical behavior). Set > 0 by the `--user-data-bytes` CLI
    /// flag to measure the realistic-state ceiling.
    pub user_data_bytes: usize,
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
        delta_cache,
        user_data_bytes,
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

    let flight_table: Arc<tokio::sync::Mutex<BTreeMap<u64, std::time::Instant>>> =
        Arc::new(tokio::sync::Mutex::new(BTreeMap::new()));
    let mut next_seq: u64 = 0;

    let stop_drain = stop.clone();
    let rm = read_metrics.clone();
    let latency_metrics = metrics.clone();
    let my_id = player.id;
    let cache_drain = delta_cache.clone();
    let flight_drain = flight_table.clone();
    tokio::spawn(async move {
        while !stop_drain.load(Ordering::Relaxed) {
            match stream.next().await {
                Some(Ok(Message::Binary(bin))) => {
                    rm.record_inbound_ok(bin.len() as u64);
                    // Cache lookup first. The cluster encodes each broadcast
                    // once and sends the same bytes to every connected
                    // player, so 1437 drain tasks per cluster all see
                    // identical payloads — decoding 1437× is wasted work.
                    // The first drain to see this frame decodes it and
                    // populates the cache; the rest hit the cache and skip
                    // the FlatBuffer decode. See `delta_cache.rs`.
                    let cached: Arc<CachedDelta> = match cache_drain.lookup(&bin) {
                        Some(e) => e,
                        None => match decode_server(&bin) {
                            Ok(ServerFrame::Delta(payload)) => {
                                let entity_ids: HashSet<_> =
                                    payload.updated.iter().map(|e| e.entity_id).collect();
                                let client_seqs: HashMap<_, _> = payload
                                    .updated
                                    .iter()
                                    .filter(|e| e.client_seq != 0)
                                    .map(|e| (e.entity_id, e.client_seq))
                                    .collect();
                                let entry = Arc::new(CachedDelta {
                                    entity_ids,
                                    client_seqs,
                                });
                                cache_drain.insert(&bin, entry.clone());
                                entry
                            }
                            Err(_) => continue,
                        },
                    };
                    if let Some(&echoed_seq) = cached.client_seqs.get(&my_id) {
                        if echoed_seq > 0 {
                            let mut ft = flight_drain.lock().await;
                            if let Some(sent_at) = ft.remove(&echoed_seq) {
                                let rtt = sent_at.elapsed();
                                latency_metrics.record_ok_decomposed(rtt, None, Duration::ZERO);
                                *ft = ft.split_off(&(echoed_seq + 1));
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

    // Reusable buffer for the per-tick pseudo-user-data payload. Refilled in
    // place each tick so the realistic-state benchmark (UserDataBytes > 0)
    // doesn't allocate a fresh Vec on every send. Empty when user_data_bytes=0.
    let mut user_data_buf: Vec<u8> = Vec::with_capacity(user_data_bytes);
    // Stable per-player seed for the deterministic PRNG. Lower 64 bits of the
    // entity UUID — varies across players, stable across ticks for the same
    // player.
    let user_data_seed = (player.id.as_u128() as u64) ^ (player.id.as_u128() >> 64) as u64;
    let mut send_tick: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        if is_zone_event_active(run_started.elapsed().as_millis() as u64, burst) {
            player.steer_to_point(2500.0, 2500.0);
        }
        player.tick(tick_dt, cluster_flag.load(Ordering::Relaxed));

        send_tick = send_tick.wrapping_add(1);
        if user_data_bytes > 0 {
            fill_pseudo_user_data(
                &mut user_data_buf,
                user_data_bytes,
                user_data_seed,
                send_tick,
            );
        }

        next_seq += 1;
        let msg = encode_player_state(
            &player.id,
            player.x,
            player.y,
            player.z,
            player.vx,
            player.vy,
            player.vz,
            &user_data_buf,
            next_seq,
        );
        match sink.send(Message::Binary(msg)).await {
            Ok(_) => {
                metrics.record_ok_count();
                let mut ft = flight_table.lock().await;
                ft.insert(next_seq, std::time::Instant::now());
                if ft.len() > 1024 {
                    ft.pop_first();
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
                        action_metrics.record_ok_count();
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
