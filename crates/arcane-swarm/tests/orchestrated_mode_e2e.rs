//! End-to-end test for the driver's orchestrated mode (#27).
//!
//! Spins up a real orchestrator (DriverServer + CommandDispatcher) AND a
//! stub TCP listener (standing in for the local control-mode server the
//! real driver would run alongside the bridge), then runs
//! `run_ws_to_tcp_bridge` and verifies that SetPlayers commands round-trip
//! end-to-end:
//!
//!   controller → orchestrator → WS → driver bridge → local TCP → "SET_PLAYERS N"
//!   driver bridge → CommandAck → orchestrator → controller
//!
//! The stub TCP listener captures every line the bridge writes so the test
//! can assert the per-driver player count actually reached the control
//! protocol.

use arcane_swarm::config::{Backend, SwarmMode};
use arcane_swarm::orchestrated_mode::{run_ws_to_tcp_bridge, OrchestratedState};
use arcane_swarm::{BurstConfig, Config};
use arcane_swarm_orchestrator::command_dispatcher::CommandDispatcher;
use arcane_swarm_orchestrator::driver_pool::DriverPool;
use arcane_swarm_orchestrator::protocol::{OrchestratorCommand, SetPlayersCommand};
use arcane_swarm_orchestrator::server::handle_connection;
use arcane_swarm_orchestrator::ws_driver_channel::WsDriverChannel;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

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
async fn driver_bridge_relays_set_players_into_local_control_protocol() {
    // Spin up the orchestrator's WS server with a real dispatcher.
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(100),
        Duration::from_secs(2),
        16,
    ));
    let dispatcher = Arc::new(CommandDispatcher::<WsDriverChannel>::new(pool.clone()));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{}", addr);

    let pool_for_server = pool.clone();
    let dispatcher_for_server = dispatcher.clone();
    tokio::spawn(async move {
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

    // Stub local control listener — this is what run_control_mode would be
    // doing in production. Accepts every connection (the bridge does a
    // wait-for-listener probe before its real connection, so we need at
    // least 2 accepts).
    let stub_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stub_port = stub_listener.local_addr().unwrap().port();
    let captured_lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_stub = captured_lines.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = stub_listener.accept().await else {
                return;
            };
            let captured = captured_for_stub.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    captured.lock().await.push(line);
                }
            });
        }
    });

    // Run the driver bridge against the orchestrator + stub listener.
    let cfg = test_config(url.clone());
    let state = OrchestratedState::new(cfg.players, cfg.inter_spawn_delay_ms);
    let bridge_state = state.clone();
    let bridge_cfg = cfg.clone();
    let bridge_handle =
        tokio::spawn(
            async move { run_ws_to_tcp_bridge(bridge_cfg, stub_port, bridge_state).await },
        );

    // Wait for driver registration.
    for _ in 0..100 {
        if pool.len().await >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(pool.len().await, 1, "driver should have registered");

    // Initial SET_PLAYERS from cfg.players (100) should land within a few hundred ms.
    for _ in 0..50 {
        if !captured_lines.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    {
        let lines = captured_lines.lock().await;
        assert!(
            lines.iter().any(|l| l == "SET_PLAYERS 100"),
            "initial SET_PLAYERS 100 should be written; got {:?}",
            *lines
        );
    }

    // Submit a SetPlayers from the controller side. The orchestrator
    // distributes per-driver — with 1 driver, the per-driver count equals
    // the aggregate.
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

    // Wait for the SET_PLAYERS 250 line to arrive on the stub listener.
    for _ in 0..50 {
        let lines = captured_lines.lock().await;
        if lines.iter().any(|l| l == "SET_PLAYERS 250") {
            break;
        }
        drop(lines);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    {
        let lines = captured_lines.lock().await;
        assert!(
            lines.iter().any(|l| l == "SET_PLAYERS 250"),
            "SET_PLAYERS 250 should be written by the bridge; got {:?}",
            *lines
        );
    }

    // Submit Stop to drain the bridge cleanly.
    let _ = dispatcher
        .submit("test-controller".to_string(), OrchestratorCommand::Stop)
        .await;

    // Bridge should exit on Stop.
    match tokio::time::timeout(Duration::from_secs(3), bridge_handle).await {
        Ok(join_result) => {
            let inner = join_result.expect("bridge join");
            assert!(inner.is_ok(), "bridge returned error: {:?}", inner);
        }
        Err(_) => panic!("bridge did not exit after Stop within 3s"),
    }

    // The QUIT line should also have been written.
    let lines = captured_lines.lock().await;
    assert!(
        lines.iter().any(|l| l == "QUIT"),
        "QUIT should have been written on Stop; got {:?}",
        *lines
    );
}

#[tokio::test]
async fn driver_bridge_returns_error_when_local_control_port_unreachable() {
    // Use a real orchestrator URL but a port that nothing's listening on
    // for the local control bridge.
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(100),
        Duration::from_secs(2),
        16,
    ));
    let dispatcher = Arc::new(CommandDispatcher::<WsDriverChannel>::new(pool.clone()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{}", addr);
    let pool_for_server = pool.clone();
    let dispatcher_for_server = dispatcher.clone();
    tokio::spawn(async move {
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

    let cfg = test_config(url.clone());
    let state = OrchestratedState::new(cfg.players, cfg.inter_spawn_delay_ms);
    // Port 1 is reserved + nothing's listening.
    let result = run_ws_to_tcp_bridge(cfg, 1, state).await;
    assert!(
        result.is_err(),
        "bridge should error on unreachable local port"
    );
}
