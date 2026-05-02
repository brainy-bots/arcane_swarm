//! Orchestrated mode for the swarm driver.
//!
//! When the operator passes `--orchestrator-url <url>`, the driver connects
//! to a swarm orchestrator over WebSocket, registers, sends periodic
//! heartbeats, and applies real-time commands (`SetPlayers`, `SetSpawnDelayMs`,
//! `Stop`) by updating shared atomics. Each command is acknowledged with a
//! `CommandAck` carrying the orchestrator-assigned sequence number.
//!
//! **Scope of this module.** This is the wire-protocol half of the driver
//! protocol extension (#27). It maintains the `target_players` and
//! `spawn_delay_ms` state mutated by orchestrator commands, but does **not**
//! drive the existing per-player spawning machinery — wiring that state into
//! the live spawn loop is a follow-up. The contract this PR satisfies:
//! "registration succeeds, commands received, telemetry pushed."
//!
//! Standalone mode (no `--orchestrator-url`) is unchanged.

use crate::{Backend, Config};
use arcane_swarm_orchestrator::protocol::{
    CommandAck, CommandEnvelope, DeregisterRequest, DriverMessage, HeartbeatRequest,
    OrchestratorCommand, OrchestratorResponse, RegisterRequest,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Shared state mutated by orchestrator commands. Public so a future PR
/// (driving the existing spawn loop from these atomics) can read them.
#[derive(Clone)]
pub struct OrchestratedState {
    pub target_players: Arc<AtomicU32>,
    pub spawn_delay_ms: Arc<AtomicU32>,
    pub stop: Arc<AtomicBool>,
}

impl OrchestratedState {
    pub fn new(initial_players: u32, initial_spawn_delay_ms: u32) -> Self {
        Self {
            target_players: Arc::new(AtomicU32::new(initial_players)),
            spawn_delay_ms: Arc::new(AtomicU32::new(initial_spawn_delay_ms)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Connect, register, run the heartbeat + command loop until either the
/// orchestrator disconnects or a `Stop` command is received.
pub async fn run_orchestrated_mode(cfg: Config) -> Result<(), String> {
    let url = cfg
        .orchestrator_url
        .clone()
        .expect("run_orchestrated_mode requires --orchestrator-url");

    eprintln!(
        "arcane-swarm(orchestrated): connecting to {} (initial players={}, spawn_delay_ms={})",
        url, cfg.players, cfg.inter_spawn_delay_ms
    );

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

    let state = OrchestratedState::new(cfg.players, cfg.inter_spawn_delay_ms);

    // Heartbeat task. Independent of the read loop so missed heartbeats are
    // determined by wall-clock cadence, not message arrival rate.
    let (hb_tx, mut hb_rx) = tokio::sync::mpsc::channel::<DriverMessage>(8);
    let hb_state_stop = state.stop.clone();
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
        }
    });

    // Outbound multiplexer: writes Heartbeat (from hb_rx) + CommandAck (from
    // ack_tx) onto the single WS sink.
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<DriverMessage>(64);
    let writer_stop = state.stop.clone();
    let writer = tokio::spawn(async move {
        loop {
            if writer_stop.load(Ordering::Relaxed) {
                return;
            }
            tokio::select! {
                Some(msg) = hb_rx.recv() => {
                    if sender
                        .send(Message::Text(serde_json::to_string(&msg).unwrap()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Some(msg) = ack_rx.recv() => {
                    if sender
                        .send(Message::Text(serde_json::to_string(&msg).unwrap()))
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

    // Read loop: process orchestrator-pushed messages.
    while let Some(msg_result) = receiver.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(_) => break,
        };
        if msg.is_close() {
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
                apply_command(&state, &env);
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
                // Heartbeat ack — nothing to do beyond confirming the loop is alive.
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

fn apply_command(state: &OrchestratedState, env: &CommandEnvelope) {
    match &env.command {
        OrchestratorCommand::SetPlayers(c) => {
            state
                .target_players
                .store(c.player_count, Ordering::Relaxed);
        }
        OrchestratorCommand::SetSpawnDelayMs(c) => {
            state
                .spawn_delay_ms
                .store(c.spawn_delay_ms, Ordering::Relaxed);
        }
        OrchestratorCommand::Stop => {
            // stop bit is set by caller after the ack is enqueued
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcane_swarm_orchestrator::protocol::{SetPlayersCommand, SetSpawnDelayMsCommand};

    #[test]
    fn apply_set_players_updates_atomic() {
        let s = OrchestratedState::new(0, 0);
        apply_command(
            &s,
            &CommandEnvelope {
                seq: 1,
                command: OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 250 }),
            },
        );
        assert_eq!(s.target_players.load(Ordering::Relaxed), 250);
        assert_eq!(s.spawn_delay_ms.load(Ordering::Relaxed), 0);
        assert!(!s.stop.load(Ordering::Relaxed));
    }

    #[test]
    fn apply_set_spawn_delay_updates_atomic() {
        let s = OrchestratedState::new(100, 0);
        apply_command(
            &s,
            &CommandEnvelope {
                seq: 2,
                command: OrchestratorCommand::SetSpawnDelayMs(SetSpawnDelayMsCommand {
                    spawn_delay_ms: 250,
                }),
            },
        );
        assert_eq!(s.spawn_delay_ms.load(Ordering::Relaxed), 250);
        assert_eq!(s.target_players.load(Ordering::Relaxed), 100);
    }
}
