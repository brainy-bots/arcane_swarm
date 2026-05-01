use crate::driver_pool::DriverPool;
use crate::protocol::{
    AckResponse, DriverMessage, ErrorResponse, OrchestratorResponse, RegisterRejectedResponse,
};
use futures::stream::StreamExt;
use futures::SinkExt;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;

pub struct DriverServer {
    pool: Arc<DriverPool>,
}

impl DriverServer {
    pub fn new(pool: Arc<DriverPool>) -> Self {
        Self { pool }
    }

    pub async fn listen(self, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind(addr).await?;
        println!("Driver server listening on: {}", addr);

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            let pool = self.pool.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, pool).await {
                    eprintln!("Error handling connection from {}: {}", peer_addr, e);
                }
            });
        }
    }
}

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
    }
}

pub async fn handle_connection(
    stream: TcpStream,
    pool: Arc<DriverPool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ws_stream = accept_async(stream).await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    while let Some(msg_result) = ws_receiver.next().await {
        let msg = msg_result?;

        if msg.is_close() {
            break;
        }

        if !msg.is_text() {
            continue;
        }

        let text = msg.to_text()?;
        let response = match serde_json::from_str::<DriverMessage>(text) {
            Ok(msg) => handle_message(&pool, msg).await,
            Err(e) => OrchestratorResponse::Error(ErrorResponse {
                message: format!("Invalid message: {}", e),
            }),
        };

        let response_text = serde_json::to_string(&response)?;
        ws_sender
            .send(tokio_tungstenite::tungstenite::Message::Text(response_text))
            .await?;
    }

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
            OrchestratorResponse::Ack(ack) => {
                assert!(ack.driver_id.is_some());
            }
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

        match response {
            OrchestratorResponse::RegisterRejected(_) => (),
            _ => panic!("Expected RegisterRejected response"),
        }
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
            OrchestratorResponse::Ack(ack) => {
                assert_eq!(ack.driver_id, None);
            }
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

        let unknown_id = uuid::Uuid::new_v4();

        let msg = DriverMessage::Heartbeat(crate::protocol::HeartbeatRequest {
            driver_id: unknown_id,
        });

        let response = handle_message(&pool, msg).await;

        match response {
            OrchestratorResponse::Error(err) => {
                assert!(err.message.contains("not found"));
            }
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
            OrchestratorResponse::Ack(ack) => {
                assert_eq!(ack.driver_id, None);
            }
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

        let unknown_id = uuid::Uuid::new_v4();

        let msg = DriverMessage::Deregister(crate::protocol::DeregisterRequest {
            driver_id: unknown_id,
        });

        let response = handle_message(&pool, msg).await;

        match response {
            OrchestratorResponse::Error(err) => {
                assert!(err.message.contains("not found"));
            }
            _ => panic!("Expected Error response"),
        }
    }
}
