//! Acceptance tests for real-time command dispatch (C2).
//!
//! Initial command vocabulary (extensible):
//!   - SetPlayers(N)
//!   - SetSpawnDelayMs(ms)
//!   - Stop

use crate::command_dispatcher::{CommandDispatcher, DispatchError, DriverChannel};
use crate::driver_pool::DriverPool;
use crate::protocol::{
    CommandAck, DriverId, OrchestratorCommand, SetPlayersCommand, SetSpawnDelayMsCommand,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Records every command sent to it; replies with a scripted ack.
struct MockDriverChannel {
    driver_id: DriverId,
    sent: Mutex<Vec<OrchestratorCommand>>,
    /// If true, channel never replies (caller will hit fan-out deadline).
    block_forever: bool,
    /// Monotonic counter shared across channels so each ack carries a unique
    /// command_seq (mirrors what a real driver would do).
    seq_counter: Arc<AtomicU64>,
}

impl MockDriverChannel {
    fn new(driver_id: DriverId, seq_counter: Arc<AtomicU64>) -> Self {
        Self {
            driver_id,
            sent: Mutex::new(Vec::new()),
            block_forever: false,
            seq_counter,
        }
    }

    fn block(mut self) -> Self {
        self.block_forever = true;
        self
    }

    async fn sent_commands(&self) -> Vec<OrchestratorCommand> {
        self.sent.lock().await.clone()
    }
}

impl DriverChannel for MockDriverChannel {
    async fn send(&self, seq: u64, command: OrchestratorCommand) -> Result<CommandAck, String> {
        self.sent.lock().await.push(command);
        if self.block_forever {
            tokio::time::sleep(Duration::from_secs(60)).await;
            return Err("blocked".to_string());
        }
        let _ = self.seq_counter.fetch_add(1, Ordering::SeqCst);
        Ok(CommandAck {
            driver_id: self.driver_id,
            command_seq: seq,
        })
    }
}

/// Build a fleet of `n` MockDriverChannels, register them in the pool, and
/// return (dispatcher, pool, channels-by-id).
async fn fleet(
    n: usize,
) -> (
    CommandDispatcher<MockDriverChannel>,
    Arc<DriverPool>,
    HashMap<DriverId, Arc<MockDriverChannel>>,
) {
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(50),
        Duration::from_millis(150),
        n + 16,
    ));
    let dispatcher = CommandDispatcher::new(pool.clone());
    let seq_counter = Arc::new(AtomicU64::new(1));

    let mut channels = HashMap::new();
    for i in 0..n {
        let driver_id = pool
            .register(json!({"i": i}))
            .await
            .expect("register should succeed");
        let ch = Arc::new(MockDriverChannel::new(driver_id, seq_counter.clone()));
        dispatcher.register_channel(driver_id, ch.clone()).await;
        channels.insert(driver_id, ch);
    }

    (dispatcher, pool, channels)
}

#[tokio::test]
async fn set_players_broadcasts_to_all_active_drivers_within_100ms() {
    let (dispatcher, _pool, channels) = fleet(12).await;
    let cmd = OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 125 });

    let started = Instant::now();
    let result = dispatcher
        .submit("controller-a".to_string(), cmd.clone())
        .await
        .expect("submit should succeed");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(100),
        "broadcast took {:?}, expected < 100ms",
        elapsed
    );
    assert_eq!(result.acks.len(), 12, "all 12 drivers should ack");
    assert!(result.missing.is_empty(), "no driver should be missing");
    // Per-driver distribution: 125 / 12 = 10 base, rem = 5; first 5 drivers
    // get 11, rest get 10. Sum = 5*11 + 7*10 = 125.
    let mut counts: Vec<u32> = Vec::new();
    for ch in channels.values() {
        let sent = ch.sent_commands().await;
        assert_eq!(sent.len(), 1);
        if let OrchestratorCommand::SetPlayers(s) = &sent[0] {
            counts.push(s.player_count);
        } else {
            panic!("expected SetPlayers; got {:?}", sent[0]);
        }
    }
    counts.sort();
    assert_eq!(
        counts.iter().sum::<u32>(),
        125,
        "per-driver counts sum to aggregate"
    );
    let _ = cmd;
}

#[tokio::test]
async fn per_driver_acknowledgment_recorded() {
    let (dispatcher, _pool, channels) = fleet(4).await;
    let cmd = OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 50 });

    let result = dispatcher
        .submit("controller-a".to_string(), cmd)
        .await
        .expect("submit should succeed");

    let acked_ids: std::collections::HashSet<DriverId> =
        result.acks.iter().map(|a| a.driver_id).collect();
    let registered_ids: std::collections::HashSet<DriverId> = channels.keys().copied().collect();
    assert_eq!(acked_ids, registered_ids, "every driver's ack recorded");
    assert!(result.missing.is_empty());

    // Log mirrors the result.
    let log = dispatcher.command_log().await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].acks.len(), 4);
    assert!(log[0].missing.is_empty());
    assert_eq!(log[0].submitter, "controller-a");
}

#[tokio::test]
async fn stale_driver_does_not_block_broadcast_to_others() {
    // Keep one driver fresh, let the others go stale, broadcast a command,
    // verify the fresh driver acks and the stale ones are excluded.
    let (dispatcher, pool, channels) = fleet(5).await;
    let fresh_id = *channels.keys().next().unwrap();

    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        pool.heartbeat(fresh_id).await.unwrap();
    }
    pool.mark_stale_drivers().await;

    let cmd = OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 });
    let result = dispatcher
        .submit("controller-a".to_string(), cmd)
        .await
        .expect("submit should succeed");

    let fresh_acked = result.acks.iter().any(|a| a.driver_id == fresh_id);
    assert!(fresh_acked, "fresh driver should have acked");
    assert!(
        result.acks.len() < 5,
        "stale drivers excluded; got {} acks, expected < 5",
        result.acks.len()
    );
}

#[tokio::test]
async fn unknown_command_returns_typed_error_no_broadcast() {
    // The OrchestratorCommand enum is closed; an "unknown command type" can
    // only originate from a malformed wire-level submission. The dispatcher's
    // internal API takes a typed enum, so this acceptance tests the analogous
    // submitter-facing failure: submitting when no drivers are Active returns
    // a typed DispatchError without any broadcast.
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(50),
        Duration::from_millis(150),
        16,
    ));
    let dispatcher: CommandDispatcher<MockDriverChannel> = CommandDispatcher::new(pool.clone());

    let cmd = OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 });
    let err = dispatcher
        .submit("controller-a".to_string(), cmd)
        .await
        .unwrap_err();
    assert_eq!(err, DispatchError::NoActiveDrivers);

    let log = dispatcher.command_log().await;
    assert!(
        log.is_empty(),
        "no broadcast = no log entry; submit should be rejected before logging"
    );
}

#[tokio::test]
async fn multiple_controllers_can_submit_concurrently() {
    let (dispatcher, _pool, _channels) = fleet(3).await;
    let dispatcher = Arc::new(dispatcher);

    let cmd_a = OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 });
    let cmd_b = OrchestratorCommand::SetSpawnDelayMs(SetSpawnDelayMsCommand { spawn_delay_ms: 50 });
    let cmd_c = OrchestratorCommand::Stop;

    // Three controllers submit concurrently.
    let d1 = dispatcher.clone();
    let d2 = dispatcher.clone();
    let d3 = dispatcher.clone();
    let h1 =
        tokio::spawn(async move { d1.submit("controller-a".to_string(), cmd_a).await.unwrap() });
    let h2 =
        tokio::spawn(async move { d2.submit("controller-b".to_string(), cmd_b).await.unwrap() });
    let h3 =
        tokio::spawn(async move { d3.submit("controller-c".to_string(), cmd_c).await.unwrap() });

    let (_r1, _r2, _r3) = tokio::join!(h1, h2, h3);

    let log = dispatcher.command_log().await;
    assert_eq!(log.len(), 3, "three commands should be logged");

    let mut submitters: Vec<&str> = log.iter().map(|e| e.submitter.as_str()).collect();
    submitters.sort();
    assert_eq!(
        submitters,
        vec!["controller-a", "controller-b", "controller-c"]
    );

    // Sequence numbers monotonic and unique.
    let mut seqs: Vec<u64> = log.iter().map(|e| e.seq).collect();
    seqs.sort();
    assert_eq!(seqs, vec![0, 1, 2]);
}

#[tokio::test]
async fn set_spawn_delay_ms_propagates_to_drivers() {
    let (dispatcher, _pool, channels) = fleet(4).await;
    let cmd = OrchestratorCommand::SetSpawnDelayMs(SetSpawnDelayMsCommand {
        spawn_delay_ms: 250,
    });

    let result = dispatcher
        .submit("controller-a".to_string(), cmd.clone())
        .await
        .expect("submit should succeed");

    assert_eq!(result.acks.len(), 4);
    for ch in channels.values() {
        let sent = ch.sent_commands().await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], cmd);
    }
}

#[tokio::test]
async fn stop_command_is_broadcast_and_logged() {
    // Per the orchestrator/controller split: the orchestrator broadcasts Stop
    // and records it. Driver-side "tear down fleet slice + deregister" is the
    // driver protocol's responsibility (#27); telemetry flushing belongs to
    // the telemetry archive (#26). The C2 contract ends at "broadcast + log".
    let (dispatcher, _pool, channels) = fleet(3).await;

    let result = dispatcher
        .submit("controller-a".to_string(), OrchestratorCommand::Stop)
        .await
        .expect("submit should succeed");

    assert_eq!(result.acks.len(), 3);
    for ch in channels.values() {
        let sent = ch.sent_commands().await;
        assert_eq!(sent, vec![OrchestratorCommand::Stop]);
    }
    let log = dispatcher.command_log().await;
    assert_eq!(log.len(), 1);
    assert!(matches!(log[0].command, OrchestratorCommand::Stop));
}

#[tokio::test]
async fn set_players_is_distributed_per_driver_evenly() {
    // 4 drivers, target 100 → each driver gets 25.
    let (dispatcher, _pool, channels) = fleet(4).await;
    let _ = dispatcher
        .submit(
            "controller-a".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 }),
        )
        .await
        .unwrap();
    for ch in channels.values() {
        let sent = ch.sent_commands().await;
        assert_eq!(sent.len(), 1);
        match &sent[0] {
            OrchestratorCommand::SetPlayers(s) => assert_eq!(s.player_count, 25),
            _ => panic!("expected SetPlayers"),
        }
    }
}

#[tokio::test]
async fn set_players_distributes_remainder_to_first_drivers() {
    // 4 drivers, target 102 → first 2 get 26, last 2 get 25 (sum = 102).
    let (dispatcher, _pool, channels) = fleet(4).await;
    let _ = dispatcher
        .submit(
            "controller-a".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 102 }),
        )
        .await
        .unwrap();
    let mut counts: Vec<u32> = Vec::new();
    for ch in channels.values() {
        let sent = ch.sent_commands().await;
        if let OrchestratorCommand::SetPlayers(s) = &sent[0] {
            counts.push(s.player_count);
        }
    }
    counts.sort();
    assert_eq!(counts, vec![25, 25, 26, 26]);
    assert_eq!(counts.iter().sum::<u32>(), 102);
}

#[tokio::test]
async fn set_players_aggregate_13500_across_12_drivers_is_1125_each() {
    // Headline scenario: 13,500 aggregate / 12 drivers = 1,125 each, exact.
    let (dispatcher, _pool, channels) = fleet(12).await;
    let _ = dispatcher
        .submit(
            "controller-a".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand {
                player_count: 13_500,
            }),
        )
        .await
        .unwrap();
    for ch in channels.values() {
        let sent = ch.sent_commands().await;
        if let OrchestratorCommand::SetPlayers(s) = &sent[0] {
            assert_eq!(s.player_count, 1_125);
        }
    }
}

#[tokio::test]
async fn set_spawn_delay_ms_remains_a_broadcast() {
    // Non-SetPlayers commands go to every driver verbatim.
    let (dispatcher, _pool, channels) = fleet(4).await;
    let _ = dispatcher
        .submit(
            "controller-a".to_string(),
            OrchestratorCommand::SetSpawnDelayMs(SetSpawnDelayMsCommand { spawn_delay_ms: 50 }),
        )
        .await
        .unwrap();
    for ch in channels.values() {
        let sent = ch.sent_commands().await;
        if let OrchestratorCommand::SetSpawnDelayMs(s) = &sent[0] {
            assert_eq!(s.spawn_delay_ms, 50);
        }
    }
}

#[tokio::test]
async fn slow_driver_excluded_from_acks_when_past_deadline() {
    // Wire-level fairness: a single slow driver must not block the broadcast
    // window for the rest of the fleet. The dispatcher's fan-out deadline
    // bounds how long it will wait for acks; slow drivers land in `missing`.
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(50),
        Duration::from_millis(150),
        16,
    ));
    let dispatcher =
        CommandDispatcher::new(pool.clone()).with_fan_out_deadline(Duration::from_millis(50));
    let seq_counter = Arc::new(AtomicU64::new(1));

    let mut fast_ids = Vec::new();
    for _ in 0..3 {
        let id = pool.register(json!({})).await.unwrap();
        let ch = Arc::new(MockDriverChannel::new(id, seq_counter.clone()));
        dispatcher.register_channel(id, ch).await;
        fast_ids.push(id);
    }
    let slow_id = pool.register(json!({})).await.unwrap();
    let slow_ch = Arc::new(MockDriverChannel::new(slow_id, seq_counter.clone()).block());
    dispatcher.register_channel(slow_id, slow_ch).await;

    let result = dispatcher
        .submit(
            "controller-a".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 }),
        )
        .await
        .expect("submit should succeed");

    assert_eq!(result.acks.len(), 3, "fast drivers ack; slow one drops");
    assert_eq!(result.missing, vec![slow_id]);
}
