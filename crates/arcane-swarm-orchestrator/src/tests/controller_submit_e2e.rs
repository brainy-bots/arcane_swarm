//! End-to-end test for the controller-submission HTTP endpoint.
//! Exercises a real TCP client → POST /commands/submit → DispatchResult.
//! Companion to `tests/ws_command_e2e.rs` (driver-side wire) and
//! `tests/dashboard_sse.rs` (SSE wire).

use crate::command_dispatcher::CommandDispatcher;
use crate::driver_pool::DriverPool;
use crate::protocol::{
    CommandAck, CommandEnvelope, DriverMessage, OrchestratorCommand, OrchestratorResponse,
    RegisterRequest, SetPlayersCommand,
};
use crate::server::handle_connection as driver_handle_connection;
use crate::sse_server::{serve_bound, SubmitRequest, SubmitResponse};
use crate::stats_collector::{ClusterEndpoint, ClusterStats, StatsCollector};
use crate::telemetry::TelemetrySource;
use crate::ws_driver_channel::WsDriverChannel;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_tungstenite::{client_async, tungstenite::Message};

struct IdleEndpoint;
impl ClusterEndpoint for IdleEndpoint {
    fn url(&self) -> &str {
        "https://test/stats"
    }
    async fn fetch(&self) -> Result<ClusterStats, String> {
        Ok(ClusterStats {
            bytes_in: 0,
            bytes_out: 0,
            last_tick_us: 33_000,
            broadcast_lagged_events: 0,
            entities_current: 0,
            sampled_at: Instant::now(),
        })
    }
}

async fn spawn_full_orchestrator() -> (
    String, // driver WS url
    String, // HTTP/SSE/Submit URL prefix (http://...)
    Arc<DriverPool>,
    Arc<CommandDispatcher<WsDriverChannel>>,
) {
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(100),
        Duration::from_secs(2),
        16,
    ));
    let dispatcher = Arc::new(CommandDispatcher::<WsDriverChannel>::new(pool.clone()));

    // Driver WS server
    let driver_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let driver_addr = driver_listener.local_addr().unwrap();
    let driver_url = format!("ws://{}", driver_addr);
    let pool_for_driver = pool.clone();
    let dispatcher_for_driver = dispatcher.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = driver_listener.accept().await else {
                return;
            };
            let pool = pool_for_driver.clone();
            let dispatcher = dispatcher_for_driver.clone();
            tokio::spawn(async move {
                let _ = driver_handle_connection(stream, pool, Some(dispatcher)).await;
            });
        }
    });

    // HTTP API server (telemetry SSE + command submit) on a separate port.
    let endpoint = Arc::new(IdleEndpoint);
    let collector = Arc::new(StatsCollector::new(vec![endpoint]));
    collector.poll_once().await.unwrap();
    let source = Arc::new(TelemetrySource::new(
        pool.clone(),
        dispatcher.clone(),
        collector.clone(),
    ));
    let (http_addr, _http_handle) = serve_bound(
        "127.0.0.1:0".parse().unwrap(),
        source.clone(),
        Some(dispatcher.clone()),
    )
    .await
    .unwrap();
    let http_url = format!("http://{}", http_addr);

    (driver_url, http_url, pool, dispatcher)
}

async fn connect_synthetic_driver(
    driver_url: &str,
) -> (
    impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    uuid::Uuid,
) {
    let stream = TcpStream::connect(driver_url.trim_start_matches("ws://"))
        .await
        .unwrap();
    let (ws, _) = client_async(driver_url, stream).await.unwrap();
    let (mut sender, mut receiver) = ws.split();

    let req = DriverMessage::Register(RegisterRequest {
        capabilities: json!({"role": "synthetic-test-driver"}),
    });
    sender
        .send(Message::Text(serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    let msg = receiver.next().await.unwrap().unwrap();
    let resp: OrchestratorResponse = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    let driver_id = match resp {
        OrchestratorResponse::Ack(ack) => ack.driver_id.unwrap(),
        other => panic!("expected register Ack, got {:?}", other),
    };

    (sender, receiver, driver_id)
}

/// Tiny HTTP/1.1 client: POST a JSON body to `<base>/commands/submit`,
/// return the parsed `SubmitResponse` (status code 200 → Ok, else Err
/// with body).
async fn http_post_submit(base: &str, req: &SubmitRequest) -> Result<SubmitResponse, String> {
    let host_port = base.trim_start_matches("http://");
    let body = serde_json::to_string(req).map_err(|e| e.to_string())?;
    let request = format!(
        "POST /commands/submit HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        host_port,
        body.len(),
        body,
    );
    let mut stream = TcpStream::connect(host_port)
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .await
        .map_err(|e| e.to_string())?;

    let s = String::from_utf8_lossy(&buf).to_string();
    let (status_line, _rest) = s.split_once("\r\n").ok_or("no status line")?;
    let status_code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .ok_or("no status code")?
        .parse()
        .map_err(|e: std::num::ParseIntError| e.to_string())?;
    let body_start = s.find("\r\n\r\n").ok_or("no header terminator")? + 4;
    let body = &s[body_start..];

    if status_code != 200 {
        return Err(format!("status {}: {}", status_code, body));
    }
    serde_json::from_str(body).map_err(|e| e.to_string())
}

#[tokio::test]
async fn http_post_submit_routes_command_to_active_driver() {
    let (driver_url, http_url, pool, _dispatcher) = spawn_full_orchestrator().await;

    // Register a synthetic driver and have it ack any inbound commands.
    let (mut sender, mut receiver, driver_id) = connect_synthetic_driver(&driver_url).await;
    let driver_task = tokio::spawn(async move {
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
        }
    });

    // Wait for the driver to register.
    for _ in 0..50 {
        if pool.contains(driver_id).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(pool.contains(driver_id).await);

    // POST a SetPlayers command via the HTTP endpoint.
    let req = SubmitRequest {
        submitter: "test-controller".to_string(),
        command: OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 75 }),
    };
    let resp = http_post_submit(&http_url, &req)
        .await
        .expect("submit should succeed");
    assert_eq!(resp.acks.len(), 1, "driver should ack via the wire");
    assert_eq!(resp.acks[0].driver_id, driver_id);
    assert_eq!(resp.acks[0].command_seq, resp.seq);
    assert!(resp.missing.is_empty());

    driver_task.abort();
}

#[tokio::test]
async fn http_post_submit_returns_400_when_no_active_drivers() {
    let (_driver_url, http_url, _pool, _dispatcher) = spawn_full_orchestrator().await;

    let req = SubmitRequest {
        submitter: "test-controller".to_string(),
        command: OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 }),
    };
    let err = http_post_submit(&http_url, &req)
        .await
        .expect_err("no drivers → 400");
    assert!(err.contains("400"));
    assert!(err.contains("no_active_drivers"));
}

#[tokio::test]
async fn http_post_submit_returns_503_when_no_dispatcher_configured() {
    // Stand up the HTTP server WITHOUT a dispatcher.
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(100),
        Duration::from_secs(2),
        16,
    ));
    let dispatcher_unused = Arc::new(CommandDispatcher::<WsDriverChannel>::new(pool.clone()));
    let endpoint = Arc::new(IdleEndpoint);
    let collector = Arc::new(StatsCollector::new(vec![endpoint]));
    let source = Arc::new(TelemetrySource::new(
        pool.clone(),
        dispatcher_unused.clone(),
        collector.clone(),
    ));
    let (addr, _h) = serve_bound("127.0.0.1:0".parse().unwrap(), source, None)
        .await
        .unwrap();
    let url = format!("http://{}", addr);

    let req = SubmitRequest {
        submitter: "test".into(),
        command: OrchestratorCommand::Stop,
    };
    let err = http_post_submit(&url, &req)
        .await
        .expect_err("no dispatcher wired = 503");
    assert!(err.contains("503"));

    // Suppress the unused-Mutex import warning.
    let _ = Mutex::new(());
}
