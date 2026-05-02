//! End-to-end test for the C2 wire layer: real WebSocket between a
//! synthetic driver client and `DriverServer::with_dispatcher`. Exercises:
//!
//!   - Register: pool gains the driver + dispatcher acquires its channel
//!   - Submit: dispatcher fans out a Command envelope on the wire
//!   - Ack: client echoes CommandAck, dispatcher resolves the submit
//!   - Deregister: dispatcher drops the channel cleanly
//!
//! These complete the C2 contract that `tests/command_dispatch.rs` covers
//! at the in-process layer.

use crate::command_dispatcher::CommandDispatcher;
use crate::driver_pool::DriverPool;
use crate::protocol::{
    CommandAck, CommandEnvelope, DeregisterRequest, DriverMessage, OrchestratorCommand,
    OrchestratorResponse, RegisterRequest, SetPlayersCommand,
};
use crate::server::handle_connection;
use crate::ws_driver_channel::WsDriverChannel;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;

async fn spawn_server() -> (
    String,
    Arc<DriverPool>,
    Arc<CommandDispatcher<WsDriverChannel>>,
) {
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(50),
        Duration::from_millis(150),
        16,
    ));
    let dispatcher = Arc::new(CommandDispatcher::<WsDriverChannel>::new(pool.clone()));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{}", addr);

    let pool_for_task = pool.clone();
    let dispatcher_for_task = dispatcher.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let pool = pool_for_task.clone();
            let dispatcher = dispatcher_for_task.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, pool, Some(dispatcher)).await;
            });
        }
    });

    (url, pool, dispatcher)
}

async fn connect_client(
    url: &str,
) -> (
    impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
) {
    let stream = TcpStream::connect(url.trim_start_matches("ws://"))
        .await
        .unwrap();
    let (ws, _) = client_async(url, stream).await.unwrap();
    ws.split()
}

async fn send_register(
    sender: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    receiver: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
              + Unpin),
) -> uuid::Uuid {
    let req = DriverMessage::Register(RegisterRequest {
        capabilities: json!({"platform": "test"}),
    });
    sender
        .send(Message::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    let msg = receiver.next().await.unwrap().unwrap();
    let resp: OrchestratorResponse = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    match resp {
        OrchestratorResponse::Ack(ack) => ack.driver_id.unwrap(),
        other => panic!("unexpected register response: {:?}", other),
    }
}

#[tokio::test]
async fn dispatcher_round_trips_command_to_real_driver_via_ws() {
    let (url, pool, dispatcher) = spawn_server().await;

    let (mut sender, mut receiver) = connect_client(&url).await;
    let driver_id = send_register(&mut sender, &mut receiver).await;

    // Wait briefly for the dispatcher to see the channel registration; the
    // server's Register handler races slightly with the dispatcher::submit.
    for _ in 0..50 {
        if pool.contains(driver_id).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(pool.contains(driver_id).await);

    // Driver-side task: read incoming Command, echo back CommandAck.
    let driver_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            let Ok(msg) = msg else {
                return;
            };
            if !msg.is_text() {
                continue;
            }
            let resp: OrchestratorResponse = match serde_json::from_str(msg.to_text().unwrap()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if let OrchestratorResponse::Command(CommandEnvelope { seq, .. }) = resp {
                let ack = DriverMessage::CommandAck(CommandAck {
                    driver_id,
                    command_seq: seq,
                });
                if sender
                    .send(Message::Text(serde_json::to_string(&ack).unwrap()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    });

    let result = dispatcher
        .submit(
            "test-controller".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 }),
        )
        .await
        .expect("submit should succeed");

    assert_eq!(result.acks.len(), 1, "driver should ack via the wire");
    assert_eq!(result.acks[0].driver_id, driver_id);
    assert_eq!(result.acks[0].command_seq, result.seq);
    assert!(result.missing.is_empty());

    driver_task.abort();
}

#[tokio::test]
async fn dispatcher_drops_channel_when_driver_deregisters() {
    let (url, pool, dispatcher) = spawn_server().await;

    let (mut sender, mut receiver) = connect_client(&url).await;
    let driver_id = send_register(&mut sender, &mut receiver).await;
    for _ in 0..50 {
        if pool.contains(driver_id).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Submit one command first to confirm the channel is wired.
    let _bg = tokio::spawn({
        let mut sender = sender;
        let mut receiver = receiver;
        async move {
            while let Some(Ok(msg)) = receiver.next().await {
                if !msg.is_text() {
                    continue;
                }
                if let Ok(OrchestratorResponse::Command(CommandEnvelope { seq, .. })) =
                    serde_json::from_str::<OrchestratorResponse>(msg.to_text().unwrap())
                {
                    let ack = DriverMessage::CommandAck(CommandAck {
                        driver_id,
                        command_seq: seq,
                    });
                    let _ = sender
                        .send(Message::Text(serde_json::to_string(&ack).unwrap()))
                        .await;
                }
                if matches!(
                    serde_json::from_str::<OrchestratorResponse>(msg.to_text().unwrap()),
                    Ok(OrchestratorResponse::Ack(_))
                ) {
                    // Deregister-ack arrived; client task done.
                    return;
                }
            }
        }
    });

    let _ = dispatcher
        .submit(
            "test-controller".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 50 }),
        )
        .await
        .expect("submit-1 should succeed");

    // Now have the client deregister via a fresh connection (the prior task
    // owns the original sender). Simpler: send deregister through a brand-new
    // connection using the same driver_id.
    let (mut s2, mut r2) = connect_client(&url).await;
    let dereg = DriverMessage::Deregister(DeregisterRequest { driver_id });
    s2.send(Message::Text(serde_json::to_string(&dereg).unwrap()))
        .await
        .unwrap();
    let _ = r2.next().await.unwrap().unwrap();

    // Pool no longer has the driver.
    for _ in 0..50 {
        if !pool.contains(driver_id).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!pool.contains(driver_id).await);

    // Submit again: dispatcher reports no active drivers (channel dropped
    // when the deregister landed).
    let result = dispatcher
        .submit(
            "test-controller".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 }),
        )
        .await;
    assert!(result.is_err(), "submit after deregister should fail");
}
