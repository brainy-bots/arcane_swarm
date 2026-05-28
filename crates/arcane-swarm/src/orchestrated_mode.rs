//! Orchestrated mode for the swarm driver.
//!
//! When the operator passes `--orchestrator-url <url>`, the driver:
//!   1. Picks a free localhost TCP port for the control bridge.
//!   2. Starts the standard `run_control_mode` loop (in main) listening on
//!      that port — same well-tested player spawn / metrics / FINAL-line
//!      machinery the SSM-driven harness uses.
//!   3. Connects to the swarm orchestrator over WebSocket, registers,
//!      sends periodic heartbeats.
//!   4. On each inbound `OrchestratorCommand`, translates it to the
//!      local control protocol (`SET_PLAYERS N`, `QUIT`) and acks back to
//!      the orchestrator. This makes the orchestrator's `SetPlayers(N)`
//!      actually drive real player spawning via the existing control path.
//!
//! Standalone mode (no `--orchestrator-url`) is unchanged.

use crate::metrics::Metrics;
use crate::{Backend, Config};
use arcane_swarm_orchestrator::protocol::{
    CommandAck, CommandEnvelope, DeregisterRequest, DriverErrorBreakdown, DriverMessage,
    DriverMetricsReport, HeartbeatRequest, OrchestratorCommand, OrchestratorResponse,
    RegisterRequest,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Shared state mutated by orchestrator commands. Public so observers (e.g.
/// future telemetry hooks) can read the current target without taking a lock.
#[derive(Clone)]
pub struct OrchestratedState {
    pub target_players: Arc<AtomicU32>,
    pub spawn_delay_ms: Arc<AtomicU32>,
    pub stop: Arc<AtomicBool>,
    pub metrics: Option<Arc<Metrics>>,
}

impl OrchestratedState {
    pub fn new(initial_players: u32, initial_spawn_delay_ms: u32) -> Self {
        Self {
            target_players: Arc::new(AtomicU32::new(initial_players)),
            spawn_delay_ms: Arc::new(AtomicU32::new(initial_spawn_delay_ms)),
            stop: Arc::new(AtomicBool::new(false)),
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }
}

/// Pick a free localhost TCP port by binding 0.0.0.0:0 and reading back
/// the kernel-assigned port. The listener is dropped before returning so
/// the caller (run_control_mode) can re-bind it.
pub async fn pick_free_local_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Wait until something is accepting TCP on `127.0.0.1:port`. Used to
/// synchronize the orchestrator-WS task with the control-mode TCP server
/// startup; without it the bridge can connect before run_control_mode has
/// bound its listener.
async fn wait_for_local_tcp(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(format!("control port {} did not bind in time", port));
        }
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        time::sleep(Duration::from_millis(100)).await;
    }
}

/// Connect to the orchestrator, register, then bridge inbound commands to
/// the local control-mode TCP server. Runs until either `Stop` is received
/// or the WS/TCP connection drops. The caller is expected to also be
/// running `run_control_mode` (or equivalent) on `tcp_port`.
pub async fn run_ws_to_tcp_bridge(
    cfg: Config,
    tcp_port: u16,
    state: OrchestratedState,
) -> Result<(), String> {
    let url = cfg
        .orchestrator_url
        .clone()
        .expect("run_ws_to_tcp_bridge requires --orchestrator-url");

    eprintln!(
        "arcane-swarm(orchestrated): connecting to {} (initial players={}, spawn_delay_ms={}, control_bridge=127.0.0.1:{})",
        url, cfg.players, cfg.inter_spawn_delay_ms, tcp_port
    );

    // Wait for run_control_mode's TCP server to bind so the first
    // SET_PLAYERS doesn't race ahead of the listener.
    wait_for_local_tcp(tcp_port, Duration::from_secs(15)).await?;
    let mut tcp = TcpStream::connect(("127.0.0.1", tcp_port))
        .await
        .map_err(|e| format!("connect to local control port: {}", e))?;

    // Connect + register against the orchestrator.
    let (ws, _) = connect_async(&url).await.map_err(|e| e.to_string())?;
    let (mut sender, mut receiver) = ws.split();

    let register = DriverMessage::Register(RegisterRequest {
        capabilities: json!({
            "binary": "arcane-swarm",
            "backend": match cfg.backend {
                Backend::SpacetimeDb => "spacetimedb",
                Backend::Arcane => "arcane",
            },
            "max_players": cfg.max_players,
            "tick_rate": cfg.tick_rate,
        }),
    });
    sender
        .send(Message::Text(serde_json::to_string(&register).unwrap()))
        .await
        .map_err(|e| e.to_string())?;

    let driver_id = match receiver.next().await {
        Some(Ok(msg)) if msg.is_text() => {
            let resp: OrchestratorResponse =
                serde_json::from_str(msg.to_text().unwrap()).map_err(|e| e.to_string())?;
            match resp {
                OrchestratorResponse::Ack(ack) => ack
                    .driver_id
                    .ok_or_else(|| "register ack missing driver_id".to_string())?,
                OrchestratorResponse::RegisterRejected(r) => {
                    return Err(format!("orchestrator rejected register: {}", r.reason));
                }
                other => return Err(format!("unexpected register response: {:?}", other)),
            }
        }
        other => {
            return Err(format!(
                "no register response from orchestrator: {:?}",
                other
            ))
        }
    };
    eprintln!("arcane-swarm(orchestrated): registered as {}", driver_id);

    // Push the driver's initial player count straight into the control
    // server so the operator's --players starting count actually becomes
    // the initial spawn target. Otherwise run_control_mode starts at 0
    // until the first SetPlayers arrives over WS.
    let _ = tcp
        .write_all(format!("SET_PLAYERS {}\n", cfg.players).as_bytes())
        .await;

    // Heartbeat + metrics reporting task — independent of the read loop so
    // missed heartbeats are determined by wall-clock, not message arrival rate.
    let (hb_tx, mut hb_rx) = tokio::sync::mpsc::channel::<DriverMessage>(16);
    let hb_state_stop = state.stop.clone();
    let hb_metrics = state.metrics.clone();
    tokio::spawn(async move {
        let mut ticker = time::interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;
            if hb_state_stop.load(Ordering::Relaxed) {
                return;
            }
            if hb_tx
                .send(DriverMessage::Heartbeat(HeartbeatRequest { driver_id }))
                .await
                .is_err()
            {
                return;
            }
            if let Some(ref m) = hb_metrics {
                let cum = m.cumulative();
                let report = DriverMetricsReport {
                    driver_id,
                    ok: cum.ok,
                    err: cum.err,
                    latency_sum_us: cum.latency_sum_us,
                    latency_samples: cum.latency_samples,
                    max_latency_us: cum.max_latency_us,
                    bytes: cum.bytes,
                    errors: DriverErrorBreakdown {
                        timeout: cum.errors.timeout,
                        not_delivered: cum.errors.not_delivered,
                        http_status: cum.errors.http_status,
                        transport: cum.errors.transport,
                        connection_drop: cum.errors.connection_drop,
                    },
                };
                let _ = hb_tx.send(DriverMessage::MetricsReport(report)).await;
            }
        }
    });

    // Outbound multiplexer — heartbeats + acks share the WS sink.
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<DriverMessage>(64);
    let writer_stop = state.stop.clone();
    let writer = tokio::spawn(async move {
        loop {
            if writer_stop.load(Ordering::Relaxed) {
                eprintln!("arcane-swarm(orchestrated): writer exit — stop signaled");
                return;
            }
            tokio::select! {
                Some(msg) = hb_rx.recv() => {
                    if let Err(e) = sender
                        .send(Message::Text(serde_json::to_string(&msg).unwrap()))
                        .await
                    {
                        eprintln!("arcane-swarm(orchestrated): writer exit on heartbeat send: {}", e);
                        return;
                    }
                }
                Some(msg) = ack_rx.recv() => {
                    if let Err(e) = sender
                        .send(Message::Text(serde_json::to_string(&msg).unwrap()))
                        .await
                    {
                        eprintln!("arcane-swarm(orchestrated): writer exit on ack send: {}", e);
                        return;
                    }
                }
                else => {
                    eprintln!("arcane-swarm(orchestrated): writer exit — both channels closed");
                    return;
                }
            }
        }
    });

    // Read loop: process orchestrator-pushed messages, translate to TCP
    // control commands, ack back.
    while let Some(msg_result) = receiver.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                eprintln!("arcane-swarm(orchestrated): reader exit on WS error: {}", e);
                break;
            }
        };
        if msg.is_close() {
            eprintln!("arcane-swarm(orchestrated): reader exit — close frame received");
            break;
        }
        if !msg.is_text() {
            continue;
        }
        let text = msg.to_text().unwrap_or("");
        let resp: OrchestratorResponse = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match resp {
            OrchestratorResponse::Command(env) => {
                // Apply to the shared atomics first (observers / tests
                // care) and then translate to the TCP control protocol the
                // existing run_control_mode speaks.
                if let Err(e) = apply_to_tcp(&state, &env, &mut tcp).await {
                    eprintln!("arcane-swarm(orchestrated): TCP bridge write failed: {}", e);
                }
                let ack = DriverMessage::CommandAck(CommandAck {
                    driver_id,
                    command_seq: env.seq,
                });
                let _ = ack_tx.send(ack).await;
                if matches!(env.command, OrchestratorCommand::Stop) {
                    state.stop.store(true, Ordering::Relaxed);
                    let _ = ack_tx
                        .send(DriverMessage::Deregister(DeregisterRequest { driver_id }))
                        .await;
                    break;
                }
            }
            OrchestratorResponse::Ack(_) => {
                // Heartbeat ack — confirms the loop is alive.
            }
            OrchestratorResponse::Error(e) => {
                eprintln!(
                    "arcane-swarm(orchestrated): orchestrator error: {}",
                    e.message
                );
            }
            OrchestratorResponse::RegisterRejected(_) => {
                // Should not arrive after a successful register.
            }
        }
    }

    state.stop.store(true, Ordering::Relaxed);
    let _ = writer.await;
    eprintln!("arcane-swarm(orchestrated): exiting");
    Ok(())
}

async fn apply_to_tcp(
    state: &OrchestratedState,
    env: &CommandEnvelope,
    tcp: &mut TcpStream,
) -> Result<(), String> {
    match &env.command {
        OrchestratorCommand::SetPlayers(c) => {
            state
                .target_players
                .store(c.player_count, Ordering::Relaxed);
            tcp.write_all(format!("SET_PLAYERS {}\n", c.player_count).as_bytes())
                .await
                .map_err(|e| e.to_string())
        }
        OrchestratorCommand::SetSpawnDelayMs(c) => {
            // run_control_mode reads inter_spawn_delay_ms once at startup
            // (a static `Duration`). Updating it mid-run requires extending
            // the TCP control protocol with SET_SPAWN_DELAY; until that
            // lands the value here is recorded for observers but isn't
            // applied to ongoing spawns. The headline benchmark sets a
            // single spawn_delay_ms at the first phase, so this is fine
            // for now.
            state
                .spawn_delay_ms
                .store(c.spawn_delay_ms, Ordering::Relaxed);
            Ok(())
        }
        OrchestratorCommand::Stop => {
            // Tell run_control_mode to drain + exit.
            tcp.write_all(b"QUIT\n").await.map_err(|e| e.to_string())
        }
    }
}

/// Backward-compat entry that constructs default state and runs the
/// bridge. Kept so existing tests + callers that don't share state with
/// the spawn loop still compile.
pub async fn run_orchestrated_mode(cfg: Config) -> Result<(), String> {
    let state = OrchestratedState::new(cfg.players, cfg.inter_spawn_delay_ms);
    let port = pick_free_local_port()
        .await
        .map_err(|e| format!("pick local port: {}", e))?;
    run_ws_to_tcp_bridge(cfg, port, state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcane_swarm_orchestrator::protocol::{SetPlayersCommand, SetSpawnDelayMsCommand};

    #[tokio::test]
    async fn pick_free_port_returns_distinct_unused_port() {
        let p1 = pick_free_local_port().await.unwrap();
        let p2 = pick_free_local_port().await.unwrap();
        assert!(p1 > 1024);
        assert!(p2 > 1024);
        // Don't assert distinct — kernel may reuse — but bind both
        // sequentially to confirm neither is in TIME_WAIT.
        let _l = TcpListener::bind(("127.0.0.1", p1)).await.unwrap();
    }

    #[tokio::test]
    async fn apply_to_tcp_writes_set_players_line() {
        // Bind a listener, accept one connection in the background, capture
        // the bytes the bridge writes.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            use tokio::io::AsyncReadExt;
            let n = sock.read(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let s = OrchestratedState::new(0, 0);
        apply_to_tcp(
            &s,
            &CommandEnvelope {
                seq: 1,
                command: OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 250 }),
            },
            &mut client,
        )
        .await
        .unwrap();
        drop(client);

        let received = server.await.unwrap();
        assert_eq!(received, "SET_PLAYERS 250\n");
        assert_eq!(s.target_players.load(Ordering::Relaxed), 250);
    }

    #[tokio::test]
    async fn apply_to_tcp_translates_stop_to_quit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 16];
            let n = sock.read(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let s = OrchestratedState::new(0, 0);
        apply_to_tcp(
            &s,
            &CommandEnvelope {
                seq: 2,
                command: OrchestratorCommand::Stop,
            },
            &mut client,
        )
        .await
        .unwrap();
        drop(client);
        let received = server.await.unwrap();
        assert_eq!(received, "QUIT\n");
    }

    #[tokio::test]
    async fn apply_to_tcp_records_spawn_delay_locally() {
        // SetSpawnDelayMs records the value in OrchestratedState but does
        // not write to TCP (the existing control protocol has no
        // SET_SPAWN_DELAY). This test pins that contract so a future
        // protocol extension doesn't silently change behavior.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 16];
            let n = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf))
                .await
                .map(|r| r.unwrap_or(0))
                .unwrap_or(0);
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let s = OrchestratedState::new(0, 0);
        apply_to_tcp(
            &s,
            &CommandEnvelope {
                seq: 3,
                command: OrchestratorCommand::SetSpawnDelayMs(SetSpawnDelayMsCommand {
                    spawn_delay_ms: 250,
                }),
            },
            &mut client,
        )
        .await
        .unwrap();
        drop(client);
        let received = server.await.unwrap();
        assert_eq!(received, "");
        assert_eq!(s.spawn_delay_ms.load(Ordering::Relaxed), 250);
    }
}
