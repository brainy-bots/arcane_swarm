//! SpacetimeDB backend loops (HTTP reducers + optional SQL reads + action traffic).
//!
//! Implements binary-internal runtime behavior for `--backend spacetimedb`.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time;

use arcane_swarm::{
    burst_actions_to_emit, is_zone_event_active, BurstConfig, ErrorKind, Metrics, Player,
    VISIBILITY_RADIUS,
};

fn uuid_json(id: &uuid::Uuid) -> u128 {
    u128::from_be_bytes(*id.as_bytes())
}

pub(crate) fn entity_json(
    id: &uuid::Uuid,
    x: f64,
    y: f64,
    z: f64,
    _vx: f64,
    _vy: f64,
    _vz: f64,
) -> String {
    format!(
        r#"[{{"entity_id":{{"__uuid__":{}}},"x":{},"y":{},"z":{}}}]"#,
        uuid_json(id),
        x,
        y,
        z
    )
}

fn player_input_json(id: &uuid::Uuid, dir_x: f64, dir_z: f64) -> String {
    format!(r#"[{{"__uuid__":{}}},{},{}]"#, uuid_json(id), dir_x, dir_z)
}

fn pickup_item_json(owner_id: &uuid::Uuid, item_type: u32, quantity: u32) -> String {
    format!(
        r#"[{{"__uuid__":{}}},{},{}]"#,
        uuid_json(owner_id),
        item_type,
        quantity
    )
}

fn use_item_json(owner_id: &uuid::Uuid, item_type: u32) -> String {
    format!(r#"[{{"__uuid__":{}}},{}]"#, uuid_json(owner_id), item_type)
}

fn interact_json(actor_id: &uuid::Uuid, target_id: &uuid::Uuid, event_type: u32) -> String {
    format!(
        r#"[{{"__uuid__":{}}},{{"__uuid__":{}}},{}]"#,
        uuid_json(actor_id),
        uuid_json(target_id),
        event_type
    )
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

pub(crate) struct ActionUrls {
    pub(crate) pickup: String,
    pub(crate) use_item: String,
    pub(crate) interact: String,
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

/// Arguments for [`action_loop`].
pub(crate) struct SpacetimeActionLoop {
    pub client: reqwest::Client,
    pub urls: ActionUrls,
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
        client,
        urls,
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
            let (url, body) = match action {
                GameAction::PickupItem {
                    item_type,
                    quantity,
                } => (
                    &urls.pickup,
                    pickup_item_json(&player_id, item_type, quantity),
                ),
                GameAction::UseItem { item_type } => {
                    (&urls.use_item, use_item_json(&player_id, item_type))
                }
                GameAction::Interact {
                    target_idx,
                    event_type,
                } => {
                    let target = all_ids
                        .get(target_idx as usize)
                        .copied()
                        .unwrap_or(player_id);
                    (
                        &urls.interact,
                        interact_json(&player_id, &target, event_type),
                    )
                }
            };

            let t0 = Instant::now();
            match client
                .post(url.as_str())
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    action_metrics.record_ok(t0.elapsed());
                }
                Ok(resp) => {
                    action_metrics.record_err_kind(ErrorKind::HttpStatus);
                    if player_idx == 0 {
                        let s = resp.status();
                        let t = resp.text().await.unwrap_or_default();
                        eprintln!("[player 0 action] HTTP {}: {}", s, &t[..t.len().min(200)]);
                    }
                }
                Err(e) => {
                    let kind = if e.is_timeout() {
                        ErrorKind::Timeout
                    } else {
                        ErrorKind::Transport
                    };
                    action_metrics.record_err_kind(kind);
                }
            }
        }
    }
}

/// Arguments for [`read_loop_spacetimedb`].
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

/// Arguments for [`player_loop_spacetimedb`].
pub(crate) struct SpacetimePlayerLoop {
    pub client: reqwest::Client,
    pub url_update_player: String,
    pub url_update_player_input: String,
    pub url_remove: String,
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
        client,
        url_update_player,
        url_update_player_input,
        url_remove,
        idx,
        entity_id,
        total,
        tick_interval,
        metrics,
        stop,
        cluster_flag,
        server_physics,
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

    let mut first_tick = true;
    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        if is_zone_event_active(run_started.elapsed().as_millis() as u64, burst) {
            player.steer_to_point(2500.0, 2500.0);
        }
        player.tick(tick_dt, cluster_flag.load(Ordering::Relaxed));
        positions.set(idx, player.x, player.z);
        let t0 = Instant::now();
        if server_physics {
            if first_tick {
                let body = entity_json(
                    &player.id, player.x, player.y, player.z, player.vx, player.vy, player.vz,
                );
                match client
                    .post(&url_update_player)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        metrics.record_ok(t0.elapsed());
                    }
                    Ok(resp) => {
                        metrics.record_err_kind(ErrorKind::HttpStatus);
                        if idx == 0 {
                            let s = resp.status();
                            let t = resp.text().await.unwrap_or_default();
                            eprintln!("[player 0] HTTP {}: {}", s, &t[..t.len().min(200)]);
                        }
                    }
                    Err(e) => {
                        let kind = if e.is_timeout() {
                            ErrorKind::Timeout
                        } else {
                            ErrorKind::Transport
                        };
                        metrics.record_err_kind(kind);
                        if idx == 0 {
                            eprintln!("[player 0] error: {}", e);
                        }
                    }
                }
                first_tick = false;
            } else {
                let body = player_input_json(&player.id, player.dir_x, player.dir_z);
                match client
                    .post(&url_update_player_input)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        metrics.record_ok(t0.elapsed());
                    }
                    Ok(resp) => {
                        metrics.record_err_kind(ErrorKind::HttpStatus);
                        if idx == 0 {
                            let s = resp.status();
                            let t = resp.text().await.unwrap_or_default();
                            eprintln!("[player 0] HTTP {}: {}", s, &t[..t.len().min(200)]);
                        }
                    }
                    Err(e) => {
                        let kind = if e.is_timeout() {
                            ErrorKind::Timeout
                        } else {
                            ErrorKind::Transport
                        };
                        metrics.record_err_kind(kind);
                        if idx == 0 {
                            eprintln!("[player 0] error: {}", e);
                        }
                    }
                }
            }
        } else {
            let body = entity_json(
                &player.id, player.x, player.y, player.z, player.vx, player.vy, player.vz,
            );
            match client
                .post(&url_update_player)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    metrics.record_ok(t0.elapsed());
                }
                Ok(resp) => {
                    metrics.record_err_kind(ErrorKind::HttpStatus);
                    if idx == 0 {
                        let s = resp.status();
                        let t = resp.text().await.unwrap_or_default();
                        eprintln!("[player 0] HTTP {}: {}", s, &t[..t.len().min(200)]);
                    }
                }
                Err(e) => {
                    let kind = if e.is_timeout() {
                        ErrorKind::Timeout
                    } else {
                        ErrorKind::Transport
                    };
                    metrics.record_err_kind(kind);
                    if idx == 0 {
                        eprintln!("[player 0] error: {}", e);
                    }
                }
            }
        }
    }
    let body = entity_json(&player.id, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    let _ = client
        .post(&url_remove)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse_json_array(body: &str) -> Value {
        serde_json::from_str(body).expect("valid json payload")
    }

    #[test]
    fn entity_json_emits_expected_shape() {
        let id = uuid::Uuid::nil();
        let body = entity_json(&id, 10.0, 20.0, 30.0, 1.0, 2.0, 3.0);
        let parsed = parse_json_array(&body);
        let obj = &parsed[0];

        assert_eq!(obj["entity_id"]["__uuid__"], Value::from(0u64));
        assert_eq!(obj["x"].as_f64(), Some(10.0));
        assert_eq!(obj["y"].as_f64(), Some(20.0));
        assert_eq!(obj["z"].as_f64(), Some(30.0));
    }

    #[test]
    fn player_input_json_emits_expected_args() {
        let id = uuid::Uuid::nil();
        let body = player_input_json(&id, -1.0, 0.5);
        let parsed = parse_json_array(&body);

        assert_eq!(parsed[0]["__uuid__"], Value::from(0u64));
        assert_eq!(parsed[1].as_f64(), Some(-1.0));
        assert_eq!(parsed[2].as_f64(), Some(0.5));
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
