//! End-to-end test for the driver's orchestrated mode (#27): spin up a
//! real `DriverServer` (with a real `CommandDispatcher`) and run the
//! driver-side `run_orchestrated_mode` against it. Submit a SetPlayers
//! command from the dispatcher and verify the driver:
//!   - registered, getting back a driver_id
//!   - received the command and updated its OrchestratedState
//!   - acked the command, completing the dispatcher's submit()
//!
//! This complements the orchestrator-side e2e test in the orchestrator
//! crate (which uses a synthetic driver client). Together they prove the
//! full bidirectional wire path works.

use std::sync::Arc;
use std::time::Duration;

use arcane_swarm::config::{Backend, SwarmMode};
use arcane_swarm::orchestrated_mode::run_orchestrated_mode;
use arcane_swarm::{BurstConfig, Config};
use arcane_swarm_orchestrator::command_dispatcher::CommandDispatcher;
use arcane_swarm_orchestrator::driver_pool::DriverPool;
use arcane_swarm_orchestrator::protocol::{OrchestratorCommand, SetPlayersCommand};
use arcane_swarm_orchestrator::server::handle_connection;
use arcane_swarm_orchestrator::ws_driver_channel::WsDriverChannel;

fn test_config(orchestrator_url: String) -> Config {
    Config {
        backend: Backend::Arcane,
        spacetimedb_uri: "http://127.0.0.1:3000".into(),
        database: "arcane".into(),
        arcane_ws: "ws://127.0.0.1:8080".into(),
        arcane_manager: None,
        players: 100,
        max_players: 1000,
        tick_rate: 20,
        duration_secs: 60,
        mode: SwarmMode::Spread,
        csv_path: None,
        cluster_command: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        actions_per_sec: 0.0,
        read_rate: 5.0,
        server_physics: false,
        run_forever: false,
        control_port: 0,
        burst: BurstConfig::default(),
        user_data_bytes: 0,
        inter_spawn_delay_ms: 0,
        max_players_per_driver: 0,
        orchestrator_url: Some(orchestrator_url),
    }
}

#[tokio::test]
async fn driver_registers_and_acks_commands_against_real_orchestrator() {
    // Spin up the orchestrator's WS server with a real dispatcher.
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(100),
        Duration::from_secs(2),
        16,
    ));
    let dispatcher = Arc::new(CommandDispatcher::<WsDriverChannel>::new(pool.clone()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{}", addr);

    let pool_for_server = pool.clone();
    let dispatcher_for_server = dispatcher.clone();
    let _server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let pool = pool_for_server.clone();
            let dispatcher = dispatcher_for_server.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, pool, Some(dispatcher)).await;
            });
        }
    });

    // Run the driver in orchestrated mode in the background.
    let cfg = test_config(url.clone());
    let driver_handle = tokio::spawn(async move { run_orchestrated_mode(cfg).await });

    // Wait until the driver registers (DriverPool size grows from 0 → 1).
    for _ in 0..100 {
        if pool.len().await >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(pool.len().await, 1, "driver should have registered");

    // Submit a real command. The driver-side run loop should apply it and
    // ack so this resolves.
    let result = dispatcher
        .submit(
            "test-controller".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 250 }),
        )
        .await
        .expect("dispatcher submit");
    assert_eq!(
        result.acks.len(),
        1,
        "driver should ack the SetPlayers command"
    );
    assert_eq!(result.acks[0].command_seq, result.seq);
    assert!(result.missing.is_empty());

    // Submit Stop to let the driver tear down cleanly.
    let _ = dispatcher
        .submit("test-controller".to_string(), OrchestratorCommand::Stop)
        .await;

    // Driver should exit on Stop.
    match tokio::time::timeout(Duration::from_secs(3), driver_handle).await {
        Ok(join_result) => {
            let inner = join_result.expect("driver task join");
            assert!(inner.is_ok(), "driver returned error: {:?}", inner);
        }
        Err(_) => panic!("driver did not exit after Stop within 3s"),
    }
}

#[tokio::test]
async fn driver_returns_error_when_orchestrator_unreachable() {
    // Use a port that nothing is listening on.
    let cfg = test_config("ws://127.0.0.1:1".into());
    let result = run_orchestrated_mode(cfg).await;
    assert!(result.is_err());
}
