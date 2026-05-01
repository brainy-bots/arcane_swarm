//! Acceptance tests for the tier ramp coordinator (C2).
//!
//! Tests are gated with `#[ignore]` until the implementation in
//! `src/tier_ramp_coordinator.rs` lands. To run them locally:
//!
//!   cargo test -p arcane-swarm-orchestrator -- --ignored
//!
//! The tests are the spec — implementation must make them pass without
//! being modified.

use crate::driver_pool::DriverPool;
use crate::protocol::{DriverId, OrchestratorCommand, SetPlayersCommand};
use crate::tier_ramp_coordinator::{
    AbortReason, DriverDispatch, TestPlan, Tier, TierEvent, TierRampCoordinator, TierState,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Mock dispatch: records every (driver_id, command, timestamp) tuple so
/// tests can assert on fan-out behavior without real WebSockets.
#[derive(Clone, Default)]
struct MockDispatch {
    sends: Arc<Mutex<Vec<(DriverId, OrchestratorCommand, Instant)>>>,
    /// Optional artificial latency injected per send.
    per_send_delay: Duration,
    /// If non-empty, fail the next send for this driver_id (used to
    /// simulate a driver gone Stale during fan-out).
    fail_for: Arc<Mutex<Vec<DriverId>>>,
}

impl MockDispatch {
    fn new() -> Self {
        Self::default()
    }

    fn recorded(&self) -> Vec<(DriverId, OrchestratorCommand, Instant)> {
        self.sends.lock().unwrap().clone()
    }

    fn fail_next_for(&self, driver_id: DriverId) {
        self.fail_for.lock().unwrap().push(driver_id);
    }
}

impl DriverDispatch for MockDispatch {
    async fn send(&self, driver_id: DriverId, command: OrchestratorCommand) -> Result<(), String> {
        if !self.per_send_delay.is_zero() {
            tokio::time::sleep(self.per_send_delay).await;
        }

        // Optional injected failure (used to simulate a Stale driver mid-fan-out).
        {
            let mut fail_for = self.fail_for.lock().unwrap();
            if let Some(pos) = fail_for.iter().position(|d| *d == driver_id) {
                fail_for.remove(pos);
                return Err(format!("simulated send failure for driver {}", driver_id));
            }
        }

        self.sends
            .lock()
            .unwrap()
            .push((driver_id, command, Instant::now()));
        Ok(())
    }
}

/// Helper: build a pool, register N drivers, return the pool + the registered
/// driver IDs in registration order.
async fn make_pool_with_drivers(n: usize) -> (Arc<DriverPool>, Vec<DriverId>) {
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(50),
        Duration::from_millis(150),
        n.max(1) * 2,
    ));
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = pool
            .register(json!({"platform": "test", "idx": i}))
            .await
            .expect("register should succeed");
        ids.push(id);
    }
    (pool, ids)
}

/// Helper: drain a broadcast receiver into a Vec<TierEvent> within a window.
async fn drain_events(
    rx: &mut tokio::sync::broadcast::Receiver<TierEvent>,
    window: Duration,
) -> Vec<TierEvent> {
    let mut events = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) => events.push(ev),
            Ok(Err(_)) => break, // channel closed
            Err(_) => break,     // deadline reached
        }
    }
    events
}

#[tokio::test]
#[ignore]
async fn all_drivers_receive_commands_simultaneously() {
    // Acceptance: 12-driver mock fleet (in-process test): all receive
    // `SetPlayers(125)` simultaneously (within 100 ms wall-clock).
    let (pool, ids) = make_pool_with_drivers(12).await;
    let dispatch = MockDispatch::new();

    let plan = TestPlan::new(vec![Tier {
        player_count: 125,
        hold_seconds: 1,
    }]);
    let coordinator = TierRampCoordinator::new(plan, pool.clone(), dispatch.clone())
        .with_fan_out_deadline(Duration::from_millis(100));

    let cmd = OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 125 });
    let started_at = Instant::now();
    let recipients = coordinator
        .send_command_to_active_drivers(cmd.clone())
        .await
        .expect("fan-out should succeed");

    let recorded = dispatch.recorded();

    assert_eq!(
        recipients.len(),
        12,
        "all 12 active drivers should have been recipients"
    );
    assert_eq!(recorded.len(), 12, "12 sends should have been recorded");

    // Every recorded driver_id must be in the registered set.
    for (driver_id, sent_command, _ts) in &recorded {
        assert!(ids.contains(driver_id), "unexpected driver_id in fan-out");
        assert_eq!(sent_command, &cmd, "command payload preserved");
    }

    // Wall-clock fan-out window: from coordinator start to last recorded send
    // must be within 100ms (the fan_out_deadline).
    let last_ts = recorded
        .iter()
        .map(|(_, _, ts)| *ts)
        .max()
        .expect("at least one send recorded");
    let elapsed = last_ts.saturating_duration_since(started_at);
    assert!(
        elapsed <= Duration::from_millis(100),
        "fan-out exceeded 100ms deadline: took {:?}",
        elapsed
    );
}

#[tokio::test]
#[ignore]
async fn tier_ramp_progresses_through_sequence() {
    // Acceptance: Tier ramp progresses through the configured sequence.
    let (pool, _ids) = make_pool_with_drivers(3).await;
    let dispatch = MockDispatch::new();

    let plan = TestPlan::new(vec![
        Tier {
            player_count: 10,
            hold_seconds: 0,
        },
        Tier {
            player_count: 50,
            hold_seconds: 0,
        },
        Tier {
            player_count: 100,
            hold_seconds: 0,
        },
    ]);
    let coordinator = TierRampCoordinator::new(plan, pool.clone(), dispatch.clone());
    let mut events_rx = coordinator.subscribe();

    let coordinator = Arc::new(coordinator);
    let coord_for_run = coordinator.clone();
    let run_handle = tokio::spawn(async move { coord_for_run.run().await });

    // Wait up to 5s for completion (tiers have 0s hold so should be fast).
    let events = drain_events(&mut events_rx, Duration::from_secs(5)).await;
    let _ = run_handle.await.expect("run task did not panic");

    // Expected event sequence: TierStarted(0), TierEnded(0), TierStarted(1),
    // TierEnded(1), TierStarted(2), TierEnded(2), RampCompleted.
    let started_indices: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            TierEvent::TierStarted { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(
        started_indices,
        vec![0, 1, 2],
        "tiers must start in order 0, 1, 2"
    );

    let ended: Vec<(u32, TierState)> = events
        .iter()
        .filter_map(|e| match e {
            TierEvent::TierEnded { index, state } => Some((*index, *state)),
            _ => None,
        })
        .collect();
    assert_eq!(ended.len(), 3, "all three tiers must end");
    for (_idx, state) in &ended {
        assert!(
            matches!(state, TierState::Pass),
            "all-clean tiers must end as Pass, got {:?}",
            state
        );
    }

    let completed = events.iter().any(|e| matches!(e, TierEvent::RampCompleted));
    assert!(completed, "RampCompleted must be emitted at end");
}

#[tokio::test]
#[ignore]
async fn stale_driver_marks_tier_invalid() {
    // Acceptance: A driver going `Stale` mid-tier marks the tier `INVALID`
    // and the run aborts.
    let (pool, ids) = make_pool_with_drivers(3).await;
    let dispatch = MockDispatch::new();
    // Cause the second driver's send to fail; that is the signal to the
    // coordinator that this driver is not reachable.
    dispatch.fail_next_for(ids[1]);

    let plan = TestPlan::new(vec![
        Tier {
            player_count: 10,
            hold_seconds: 1,
        },
        Tier {
            player_count: 50,
            hold_seconds: 1,
        },
    ]);
    let coordinator = TierRampCoordinator::new(plan, pool.clone(), dispatch.clone());
    let mut events_rx = coordinator.subscribe();

    let coordinator = Arc::new(coordinator);
    let coord_for_run = coordinator.clone();
    let run_handle = tokio::spawn(async move { coord_for_run.run().await });

    let events = drain_events(&mut events_rx, Duration::from_secs(5)).await;
    let _ = run_handle.await.expect("run task did not panic");

    // The first tier must end with Invalid state and the ramp must abort.
    let first_tier_invalid = events.iter().any(|e| {
        matches!(
            e,
            TierEvent::TierEnded {
                index: 0,
                state: TierState::Invalid,
            }
        )
    });
    assert!(
        first_tier_invalid,
        "tier 0 must be marked Invalid when a driver goes Stale during fan-out"
    );

    let aborted = events.iter().find_map(|e| match e {
        TierEvent::RampAborted { reason } => Some(reason.clone()),
        _ => None,
    });
    assert!(
        matches!(aborted, Some(AbortReason::DriverStale(_))),
        "ramp must abort with DriverStale reason; got {:?}",
        aborted
    );

    // Tier 1 must NOT have been started.
    let tier_one_started = events
        .iter()
        .any(|e| matches!(e, TierEvent::TierStarted { index: 1, .. }));
    assert!(!tier_one_started, "tier 1 must not start after abort");
}

#[tokio::test]
#[ignore]
async fn manual_abort_stops_tier_cleanly() {
    // Acceptance: Manual abort signal stops the current tier cleanly.
    let (pool, _ids) = make_pool_with_drivers(3).await;
    let dispatch = MockDispatch::new();

    // Tier 0 holds for 30s, long enough to reliably abort during it.
    let plan = TestPlan::new(vec![
        Tier {
            player_count: 10,
            hold_seconds: 30,
        },
        Tier {
            player_count: 50,
            hold_seconds: 30,
        },
    ]);
    let coordinator = Arc::new(TierRampCoordinator::new(
        plan,
        pool.clone(),
        dispatch.clone(),
    ));
    let mut events_rx = coordinator.subscribe();

    let coord_for_run = coordinator.clone();
    let run_handle = tokio::spawn(async move { coord_for_run.run().await });

    // Wait for tier 0 to actually start before aborting.
    let mut tier_zero_started = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !tier_zero_started {
        match tokio::time::timeout(Duration::from_millis(100), events_rx.recv()).await {
            Ok(Ok(TierEvent::TierStarted { index: 0, .. })) => tier_zero_started = true,
            _ => continue,
        }
    }
    assert!(tier_zero_started, "tier 0 should start within 2s");

    coordinator.abort();

    // Drain events and verify abort sequence.
    let events = drain_events(&mut events_rx, Duration::from_secs(5)).await;
    let _ = run_handle.await.expect("run task did not panic");

    // Tier 0 should be marked Invalid (current tier when abort fired).
    let tier_zero_invalid = events.iter().any(|e| {
        matches!(
            e,
            TierEvent::TierEnded {
                index: 0,
                state: TierState::Invalid,
            }
        )
    });
    assert!(
        tier_zero_invalid,
        "tier 0 must be marked Invalid on manual abort"
    );

    // RampAborted with reason Manual.
    let aborted = events.iter().find_map(|e| match e {
        TierEvent::RampAborted { reason } => Some(reason.clone()),
        _ => None,
    });
    assert_eq!(
        aborted,
        Some(AbortReason::Manual),
        "ramp must abort with Manual reason; got {:?}",
        aborted
    );

    // Tier 1 must NOT have started.
    let tier_one_started = events
        .iter()
        .any(|e| matches!(e, TierEvent::TierStarted { index: 1, .. }));
    assert!(
        !tier_one_started,
        "tier 1 must not start after manual abort"
    );
}
