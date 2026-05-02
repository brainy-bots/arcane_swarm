use crate::command_dispatcher::CommandDispatcher;
use crate::driver_pool::DriverPool;
use crate::protocol::{
    AckResponse, CommandAck, CommandEnvelope, DriverId, DriverMessage, ErrorResponse,
    OrchestratorCommand, OrchestratorResponse, RegisterRejectedResponse,
};
use crate::ws_driver_channel::WsDriverChannel;
use futures::stream::StreamExt;
use futures::SinkExt;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::accept_async;

pub struct DriverServer {
    pool: Arc<DriverPool>,
    /// Optional dispatcher. When present, the server wires per-connection
    /// `WsDriverChannel`s into it on `Register`, enabling C2 command dispatch
    /// over the wire. When absent, the server still accepts driver lifecycle
    /// messages but cannot push commands.
    dispatcher: Option<Arc<CommandDispatcher<WsDriverChannel>>>,
}

impl DriverServer {
    pub fn new(pool: Arc<DriverPool>) -> Self {
        Self {
            pool,
            dispatcher: None,
        }
    }

    pub fn with_dispatcher(
        pool: Arc<DriverPool>,
        dispatcher: Arc<CommandDispatcher<WsDriverChannel>>,
    ) -> Self {
        Self {
            pool,
            dispatcher: Some(dispatcher),
        }
    }

    pub async fn listen(self, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(addr).await?;
        println!("Driver server listening on: {}", addr);

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            let pool = self.pool.clone();
            let dispatcher = self.dispatcher.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, pool, dispatcher).await {
                    eprintln!("Error handling connection from {}: {}", peer_addr, e);
                }
            });
        }
    }
}

/// Synchronous response builder for driver lifecycle messages. Used by the
/// in-module unit tests that exercise the response shape without going
/// through a real WS connection.
#[cfg(test)]
pub(crate) async fn handle_message(
    pool: &Arc<DriverPool>,
    msg: DriverMessage,
) -> OrchestratorResponse {
    match msg {
        DriverMessage::Register(req) => match pool.register(req.capabilities).await {
            Ok(driver_id) => OrchestratorResponse::Ack(AckResponse {
                driver_id: Some(driver_id),
            }),
            Err(reason) => {
                OrchestratorResponse::RegisterRejected(RegisterRejectedResponse { reason })
            }
        },
        DriverMessage::Heartbeat(req) => match pool.heartbeat(req.driver_id).await {
            Ok(_) => OrchestratorResponse::Ack(AckResponse { driver_id: None }),
            Err(err) => OrchestratorResponse::Error(ErrorResponse { message: err }),
        },
        DriverMessage::Deregister(req) => match pool.deregister(req.driver_id).await {
            Ok(_) => OrchestratorResponse::Ack(AckResponse { driver_id: None }),
            Err(err) => OrchestratorResponse::Error(ErrorResponse { message: err }),
        },
        DriverMessage::CommandAck(_) => OrchestratorResponse::Error(ErrorResponse {
            message: "CommandAck handled at the connection layer".to_string(),
        }),
    }
}

pub async fn handle_connection(
    stream: TcpStream,
    pool: Arc<DriverPool>,
    dispatcher: Option<Arc<CommandDispatcher<WsDriverChannel>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws_stream = accept_async(stream).await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Outbound multiplexing channels: one for server-pushed commands (with
    // their ack-routing oneshot), one for handshake/heartbeat responses.
    let (cmd_tx, mut cmd_rx) =
        mpsc::channel::<(u64, OrchestratorCommand, oneshot::Sender<CommandAck>)>(16);
    let (resp_tx, mut resp_rx) = mpsc::channel::<OrchestratorResponse>(16);

    // Pending command-ack routing table. Reader resolves; writer populates.
    let pending_acks: Arc<Mutex<HashMap<u64, oneshot::Sender<CommandAck>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let pending_for_writer = pending_acks.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                Some((seq, cmd, ack_tx)) = cmd_rx.recv() => {
                    pending_for_writer.lock().await.insert(seq, ack_tx);
                    let envelope = OrchestratorResponse::Command(CommandEnvelope {
                        seq,
                        command: cmd,
                    });
                    let Ok(text) = serde_json::to_string(&envelope) else { continue };
                    if ws_sender
                        .send(tokio_tungstenite::tungstenite::Message::Text(text))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Some(resp) = resp_rx.recv() => {
                    let Ok(text) = serde_json::to_string(&resp) else { continue };
                    if ws_sender
                        .send(tokio_tungstenite::tungstenite::Message::Text(text))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                else => return,
            }
        }
    });

    let mut driver_id: Option<DriverId> = None;

    while let Some(msg_result) = ws_receiver.next().await {
        let msg = msg_result?;
        if msg.is_close() {
            break;
        }
        if !msg.is_text() {
            continue;
        }
        let text = msg.to_text()?;

        match serde_json::from_str::<DriverMessage>(text) {
            Ok(DriverMessage::Register(req)) => {
                let response = match pool.register(req.capabilities).await {
                    Ok(id) => {
                        driver_id = Some(id);
                        if let Some(dispatcher) = &dispatcher {
                            let channel = Arc::new(WsDriverChannel::new(cmd_tx.clone()));
                            dispatcher.register_channel(id, channel).await;
                        }
                        OrchestratorResponse::Ack(AckResponse {
                            driver_id: Some(id),
                        })
                    }
                    Err(reason) => {
                        OrchestratorResponse::RegisterRejected(RegisterRejectedResponse { reason })
                    }
                };
                let _ = resp_tx.send(response).await;
            }
            Ok(DriverMessage::Heartbeat(req)) => {
                let response = match pool.heartbeat(req.driver_id).await {
                    Ok(_) => OrchestratorResponse::Ack(AckResponse { driver_id: None }),
                    Err(err) => OrchestratorResponse::Error(ErrorResponse { message: err }),
                };
                let _ = resp_tx.send(response).await;
            }
            Ok(DriverMessage::Deregister(req)) => {
                let response = match pool.deregister(req.driver_id).await {
                    Ok(_) => OrchestratorResponse::Ack(AckResponse { driver_id: None }),
                    Err(err) => OrchestratorResponse::Error(ErrorResponse { message: err }),
                };
                if let (Some(dispatcher), Some(id)) = (&dispatcher, driver_id) {
                    dispatcher.deregister_channel(id).await;
                }
                let _ = resp_tx.send(response).await;
            }
            Ok(DriverMessage::CommandAck(ack)) => {
                let mut map = pending_acks.lock().await;
                if let Some(tx) = map.remove(&ack.command_seq) {
                    let _ = tx.send(ack);
                }
            }
            Err(e) => {
                let _ = resp_tx
                    .send(OrchestratorResponse::Error(ErrorResponse {
                        message: format!("Invalid message: {}", e),
                    }))
                    .await;
            }
        }
    }

    if let (Some(dispatcher), Some(id)) = (&dispatcher, driver_id) {
        dispatcher.deregister_channel(id).await;
    }
    drop(cmd_tx);
    drop(resp_tx);
    let _ = writer.await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn handle_message_register_succeeds() {
        let pool = Arc::new(DriverPool::new(
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(150),
            100,
        ));
        let msg = DriverMessage::Register(crate::protocol::RegisterRequest {
            capabilities: json!({"platform": "linux"}),
        });
        let response = handle_message(&pool, msg).await;
        match response {
            OrchestratorResponse::Ack(ack) => assert!(ack.driver_id.is_some()),
            _ => panic!("Expected Ack response"),
        }
    }

    #[tokio::test]
    async fn handle_message_register_rejected_when_full() {
        let pool = Arc::new(DriverPool::new(
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(150),
            1,
        ));
        pool.register(json!({"platform": "linux"})).await.unwrap();
        let msg = DriverMessage::Register(crate::protocol::RegisterRequest {
            capabilities: json!({"platform": "windows"}),
        });
        let response = handle_message(&pool, msg).await;
        assert!(matches!(
            response,
            OrchestratorResponse::RegisterRejected(_)
        ));
    }

    #[tokio::test]
    async fn handle_message_heartbeat_succeeds() {
        let pool = Arc::new(DriverPool::new(
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(150),
            100,
        ));
        let driver_id = pool.register(json!({"platform": "linux"})).await.unwrap();
        let msg = DriverMessage::Heartbeat(crate::protocol::HeartbeatRequest { driver_id });
        let response = handle_message(&pool, msg).await;
        match response {
            OrchestratorResponse::Ack(ack) => assert_eq!(ack.driver_id, None),
            _ => panic!("Expected Ack response"),
        }
    }

    #[tokio::test]
    async fn handle_message_heartbeat_unknown_driver_errors() {
        let pool = Arc::new(DriverPool::new(
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(150),
            100,
        ));
        let msg = DriverMessage::Heartbeat(crate::protocol::HeartbeatRequest {
            driver_id: uuid::Uuid::new_v4(),
        });
        let response = handle_message(&pool, msg).await;
        match response {
            OrchestratorResponse::Error(err) => assert!(err.message.contains("not found")),
            _ => panic!("Expected Error response"),
        }
    }

    #[tokio::test]
    async fn handle_message_deregister_succeeds() {
        let pool = Arc::new(DriverPool::new(
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(150),
            100,
        ));
        let driver_id = pool.register(json!({"platform": "linux"})).await.unwrap();
        let msg = DriverMessage::Deregister(crate::protocol::DeregisterRequest { driver_id });
        let response = handle_message(&pool, msg).await;
        match response {
            OrchestratorResponse::Ack(ack) => assert_eq!(ack.driver_id, None),
            _ => panic!("Expected Ack response"),
        }
    }

    #[tokio::test]
    async fn handle_message_deregister_unknown_driver_errors() {
        let pool = Arc::new(DriverPool::new(
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(150),
            100,
        ));
        let msg = DriverMessage::Deregister(crate::protocol::DeregisterRequest {
            driver_id: uuid::Uuid::new_v4(),
        });
        let response = handle_message(&pool, msg).await;
        match response {
            OrchestratorResponse::Error(err) => assert!(err.message.contains("not found")),
            _ => panic!("Expected Error response"),
        }
    }
}
