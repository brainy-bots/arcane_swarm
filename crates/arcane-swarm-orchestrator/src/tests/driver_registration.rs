use crate::driver_pool::DriverPool;
use crate::protocol::{
    DeregisterRequest, DriverMessage, HeartbeatRequest, OrchestratorResponse, RegisterRequest,
};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::connect_async;

async fn start_test_server(
    heartbeat_interval: Duration,
    stale_threshold: Duration,
    max_drivers: usize,
) -> (Arc<DriverPool>, SocketAddr) {
    let pool = Arc::new(DriverPool::new(
        heartbeat_interval,
        stale_threshold,
        max_drivers,
    ));
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let addr = listener.local_addr().unwrap();

    let pool_for_server = pool.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let pool = pool_for_server.clone();
            tokio::spawn(async move {
                let _ = crate::server::handle_connection(stream, pool).await;
            });
        }
    });

    (pool, addr)
}

#[tokio::test]
async fn registration_round_trip_succeeds() {
    // Acceptance: Registration round-trip succeeds; pool size grows.
    let (pool, addr) =
        start_test_server(Duration::from_millis(50), Duration::from_millis(150), 100).await;

    let ws_url = format!("ws://{}", addr);
    let (ws_stream, _) = connect_async(&ws_url).await.unwrap();
    let (mut sender, mut receiver) = ws_stream.split();

    let register_msg = DriverMessage::Register(RegisterRequest {
        capabilities: json!({"platform": "test"}),
    });
    let msg_text = serde_json::to_string(&register_msg).unwrap();
    sender
        .send(tokio_tungstenite::tungstenite::Message::Text(msg_text))
        .await
        .unwrap();

    let response_msg = receiver.next().await.unwrap().unwrap();
    let response_text = response_msg.to_text().unwrap();
    let response: OrchestratorResponse = serde_json::from_str(response_text).unwrap();

    match response {
        OrchestratorResponse::Ack(ack) => {
            assert!(ack.driver_id.is_some());
        }
        _ => panic!("Expected Ack response"),
    }

    assert_eq!(pool.len().await, 1);
}

#[tokio::test]
async fn heartbeat_keeps_driver_active() {
    // Acceptance: Heartbeat keeps driver in `Active`; missed heartbeats transition to `Stale`.
    let heartbeat_interval = Duration::from_millis(50);
    let stale_threshold = Duration::from_millis(150); // 3 missed heartbeats
    let (pool, addr) = start_test_server(heartbeat_interval, stale_threshold, 100).await;

    let ws_url = format!("ws://{}", addr);
    let (ws_stream, _) = connect_async(&ws_url).await.unwrap();
    let (mut sender, mut receiver) = ws_stream.split();

    let register_msg = DriverMessage::Register(RegisterRequest {
        capabilities: json!({"platform": "test"}),
    });
    let msg_text = serde_json::to_string(&register_msg).unwrap();
    sender
        .send(tokio_tungstenite::tungstenite::Message::Text(msg_text))
        .await
        .unwrap();

    let response_msg = receiver.next().await.unwrap().unwrap();
    let response_text = response_msg.to_text().unwrap();
    let response: OrchestratorResponse = serde_json::from_str(response_text).unwrap();

    let driver_id = match response {
        OrchestratorResponse::Ack(ack) => ack.driver_id.unwrap(),
        _ => panic!("Expected Ack response"),
    };

    // Send heartbeats every 50ms for 500ms - driver should stay Active
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(50)).await;

        let heartbeat_msg = DriverMessage::Heartbeat(HeartbeatRequest { driver_id });
        let msg_text = serde_json::to_string(&heartbeat_msg).unwrap();
        sender
            .send(tokio_tungstenite::tungstenite::Message::Text(msg_text))
            .await
            .unwrap();

        let _response_msg = receiver.next().await.unwrap().unwrap();

        let state = pool.get_state(driver_id).await;
        assert_eq!(state, Some(crate::driver_pool::DriverState::Active));
    }

    // Stop sending heartbeats and wait for stale transition (>150ms)
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Mark stale drivers
    pool.mark_stale_drivers().await;

    let state = pool.get_state(driver_id).await;
    assert_eq!(state, Some(crate::driver_pool::DriverState::Stale));
}

#[tokio::test]
async fn graceful_deregister_removes_driver() {
    // Acceptance: Graceful deregister removes driver immediately.
    let (pool, addr) =
        start_test_server(Duration::from_millis(50), Duration::from_millis(150), 100).await;

    let ws_url = format!("ws://{}", addr);
    let (ws_stream, _) = connect_async(&ws_url).await.unwrap();
    let (mut sender, mut receiver) = ws_stream.split();

    let register_msg = DriverMessage::Register(RegisterRequest {
        capabilities: json!({"platform": "test"}),
    });
    let msg_text = serde_json::to_string(&register_msg).unwrap();
    sender
        .send(tokio_tungstenite::tungstenite::Message::Text(msg_text))
        .await
        .unwrap();

    let response_msg = receiver.next().await.unwrap().unwrap();
    let response_text = response_msg.to_text().unwrap();
    let response: OrchestratorResponse = serde_json::from_str(response_text).unwrap();

    let driver_id = match response {
        OrchestratorResponse::Ack(ack) => ack.driver_id.unwrap(),
        _ => panic!("Expected Ack response"),
    };

    assert_eq!(pool.len().await, 1);

    let deregister_msg = DriverMessage::Deregister(DeregisterRequest { driver_id });
    let msg_text = serde_json::to_string(&deregister_msg).unwrap();
    sender
        .send(tokio_tungstenite::tungstenite::Message::Text(msg_text))
        .await
        .unwrap();

    let _response_msg = receiver.next().await.unwrap().unwrap();

    assert_eq!(pool.len().await, 0);
}

#[tokio::test]
async fn pool_cap_enforced() {
    // Acceptance: Pool cap enforced (orchestrator rejects registration past `--max-drivers`).
    let (pool, addr) =
        start_test_server(Duration::from_millis(50), Duration::from_millis(150), 3).await;

    let ws_url = format!("ws://{}", addr);

    // Register 3 drivers (should succeed)
    for _ in 0..3 {
        let (ws_stream, _) = connect_async(&ws_url).await.unwrap();
        let (mut sender, mut receiver) = ws_stream.split();

        let register_msg = DriverMessage::Register(RegisterRequest {
            capabilities: json!({"platform": "test"}),
        });
        let msg_text = serde_json::to_string(&register_msg).unwrap();
        sender
            .send(tokio_tungstenite::tungstenite::Message::Text(msg_text))
            .await
            .unwrap();

        let response_msg = receiver.next().await.unwrap().unwrap();
        let response_text = response_msg.to_text().unwrap();
        let response: OrchestratorResponse = serde_json::from_str(response_text).unwrap();

        match response {
            OrchestratorResponse::Ack(ack) => {
                assert!(ack.driver_id.is_some());
            }
            _ => panic!("Expected Ack response"),
        }
    }

    assert_eq!(pool.len().await, 3);

    // Try to register 4th driver (should be rejected)
    let (ws_stream, _) = connect_async(&ws_url).await.unwrap();
    let (mut sender, mut receiver) = ws_stream.split();

    let register_msg = DriverMessage::Register(RegisterRequest {
        capabilities: json!({"platform": "test"}),
    });
    let msg_text = serde_json::to_string(&register_msg).unwrap();
    sender
        .send(tokio_tungstenite::tungstenite::Message::Text(msg_text))
        .await
        .unwrap();

    let response_msg = receiver.next().await.unwrap().unwrap();
    let response_text = response_msg.to_text().unwrap();
    let response: OrchestratorResponse = serde_json::from_str(response_text).unwrap();

    match response {
        OrchestratorResponse::RegisterRejected(_) => {
            // Expected
        }
        _ => panic!("Expected RegisterRejected response"),
    }

    assert_eq!(pool.len().await, 3);
}
