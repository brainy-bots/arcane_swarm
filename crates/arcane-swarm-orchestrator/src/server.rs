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
            Ok(DriverMessage::Register(req)) => match pool.register(req.capabilities).await {
                Ok(driver_id) => OrchestratorResponse::Ack(AckResponse {
                    driver_id: Some(driver_id),
                }),
                Err(reason) => {
                    OrchestratorResponse::RegisterRejected(RegisterRejectedResponse { reason })
                }
            },
            Ok(DriverMessage::Heartbeat(req)) => match pool.heartbeat(req.driver_id).await {
                Ok(_) => OrchestratorResponse::Ack(AckResponse { driver_id: None }),
                Err(err) => OrchestratorResponse::Error(ErrorResponse { message: err }),
            },
            Ok(DriverMessage::Deregister(req)) => match pool.deregister(req.driver_id).await {
                Ok(_) => OrchestratorResponse::Ack(AckResponse { driver_id: None }),
                Err(err) => OrchestratorResponse::Error(ErrorResponse { message: err }),
            },
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
