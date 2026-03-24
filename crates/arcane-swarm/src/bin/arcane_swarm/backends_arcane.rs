use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::time;
use tokio_tungstenite::tungstenite::Message;

use crate::{ArcaneEndpoint, Metrics, Player};

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
                            eprintln!("[player 0] manager join HTTP {}: {}", status, &t[..t.len().min(200)]);
                        }
                    }
                    Err(e) => {
                        if player_idx == 0 && attempt == RETRIES - 1 {
                            eprintln!("[player 0] manager join error (after {} attempts): {}", RETRIES, e);
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

pub(crate) async fn player_loop_arcane(
    endpoint: ArcaneEndpoint,
    client: reqwest::Client,
    idx: u32,
    total: u32,
    tick_interval: Duration,
    metrics: Arc<Metrics>,
    read_metrics: Arc<Metrics>,
    stop: Arc<AtomicBool>,
    cluster_flag: Arc<AtomicBool>,
) {
    let ws_url = resolve_arcane_ws(&endpoint, &client, idx).await;
    let clustered = cluster_flag.load(Ordering::Relaxed);
    let mut player = Player::new(idx, total, clustered);
    let tick_dt = tick_interval.as_secs_f64();

    let ws_stream = match tokio_tungstenite::connect_async(&ws_url).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            if idx == 0 {
                eprintln!("[player 0] WebSocket connect failed: {}", e);
            }
            metrics.record_err();
            return;
        }
    };
    let (mut sink, mut stream) = ws_stream.split();

    let stop_drain = stop.clone();
    let rm = read_metrics.clone();
    tokio::spawn(async move {
        while !stop_drain.load(Ordering::Relaxed) {
            match stream.next().await {
                Some(Ok(Message::Text(txt))) => {
                    rm.ok.fetch_add(1, Ordering::Relaxed);
                    rm.bytes.fetch_add(txt.len() as u64, Ordering::Relaxed);
                }
                Some(Ok(Message::Binary(bin))) => {
                    rm.ok.fetch_add(1, Ordering::Relaxed);
                    rm.bytes.fetch_add(bin.len() as u64, Ordering::Relaxed);
                }
                Some(Ok(_)) => {}
                _ => break,
            }
        }
    });

    let mut interval = time::interval(tick_interval);
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    while !stop.load(Ordering::Relaxed) {
        interval.tick().await;
        player.tick(tick_dt, cluster_flag.load(Ordering::Relaxed));
        let msg = crate::player_state_json(
            &player.id, player.x, player.y, player.z, player.vx, player.vy, player.vz,
        );
        let t0 = std::time::Instant::now();
        match sink.send(Message::Text(msg.into())).await {
            Ok(_) => {
                metrics.record_ok(t0.elapsed());
            }
            Err(e) => {
                metrics.record_err();
                if idx == 0 {
                    eprintln!("[player 0] ws send error: {}", e);
                }
                break;
            }
        }
    }
}
