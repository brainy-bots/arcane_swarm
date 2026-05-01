//! Tier ramp coordinator — orchestrator component C2.
//!
//! Owns the test plan (a sequence of tiers `[(player_count, hold_seconds), ...]`),
//! drives all `Active` drivers through it in parallel, and surfaces per-tier
//! progress as a stream of `TierEvent`s.
//!
//! Decoupled from the wire transport via `DriverDispatch`. In production this
//! is wired to the WebSocket server that already handles driver registration
//! (C1); in tests it is a `MockDispatch` that records every send.

use crate::driver_pool::DriverPool;
use crate::protocol::{DriverId, OrchestratorCommand, StartTierCommand};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// One tier in a test plan: a steady-state load with a hold duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier {
    pub player_count: u32,
    pub hold_seconds: u32,
}

/// An ordered sequence of tiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestPlan {
    pub tiers: Vec<Tier>,
}

impl TestPlan {
    pub fn new(tiers: Vec<Tier>) -> Self {
        Self { tiers }
    }

    pub fn len(&self) -> usize {
        self.tiers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }
}

/// Per-tier outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierState {
    Pending,
    Running,
    Pass,
    Fail,
    Invalid,
}

/// Reason a ramp aborted before completing all tiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbortReason {
    Manual,
    DriverStale(DriverId),
    TierFailed(u32),
}

/// Events emitted by the coordinator while a ramp is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierEvent {
    /// A tier just started; carries the index and the underlying tier data.
    TierStarted { index: u32, tier: Tier },
    /// A tier just ended. `state` reflects the outcome (Pass/Fail/Invalid).
    TierEnded { index: u32, state: TierState },
    /// The whole ramp aborted; subsequent tiers will not run.
    RampAborted { reason: AbortReason },
    /// All tiers completed without abort.
    RampCompleted,
}

/// Trait abstraction over "send a command to one driver." Production wires
/// this to the WebSocket server; tests use a mock that records the call.
///
/// Uses Rust 1.75+ async fn in traits.
pub trait DriverDispatch: Send + Sync {
    /// Send a command to a single driver. Returns Ok when the driver has
    /// at least received the command (not necessarily processed it).
    fn send(
        &self,
        driver_id: DriverId,
        command: OrchestratorCommand,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// Coordinator state visible to observers.
#[derive(Debug, Clone)]
pub struct RampStatus {
    pub current_tier_index: Option<u32>,
    pub tier_states: Vec<TierState>,
    pub aborted: bool,
}

/// The tier ramp coordinator itself.
///
/// Run order is purely sequential: start tier 0, hold for `hold_seconds`,
/// end tier 0, start tier 1, ... abort early on driver Stale or manual abort.
pub struct TierRampCoordinator<D: DriverDispatch + 'static> {
    plan: TestPlan,
    pool: Arc<DriverPool>,
    dispatch: Arc<D>,
    abort_signal: Arc<AtomicBool>,
    events_tx: broadcast::Sender<TierEvent>,
    /// Wall-clock deadline for fan-out completion. The coordinator sends
    /// `SetPlayers` to all `Active` drivers concurrently; this bounds how
    /// long it will wait for all sends to complete before declaring the
    /// fan-out failed.
    fan_out_deadline: Duration,
}

impl<D: DriverDispatch + 'static> TierRampCoordinator<D> {
    /// Construct a new coordinator. The events broadcast channel is created
    /// with a sane default capacity; subscribe before calling `run()` to
    /// avoid missing the first event.
    pub fn new(plan: TestPlan, pool: Arc<DriverPool>, dispatch: D) -> Self {
        let (events_tx, _) = broadcast::channel(64);
        Self {
            plan,
            pool,
            dispatch: Arc::new(dispatch),
            abort_signal: Arc::new(AtomicBool::new(false)),
            events_tx,
            fan_out_deadline: Duration::from_millis(100),
        }
    }

    /// Override the default fan-out deadline (tests may want a tighter one).
    pub fn with_fan_out_deadline(mut self, deadline: Duration) -> Self {
        self.fan_out_deadline = deadline;
        self
    }

    /// Subscribe to `TierEvent`s. Call this before `run()` to receive the
    /// first event (broadcast channels drop messages with no live receivers).
    pub fn subscribe(&self) -> broadcast::Receiver<TierEvent> {
        self.events_tx.subscribe()
    }

    /// Trigger an abort. The coordinator will end the current tier with
    /// `Invalid` state, emit `RampAborted{Manual}`, and stop.
    pub fn abort(&self) {
        self.abort_signal.store(true, Ordering::SeqCst);
    }

    /// Drive the entire plan. Returns a final `RampStatus` reflecting per-tier
    /// outcomes. Returns early on abort or driver-Stale.
    ///
    /// Implementation lands in C2 PR (this issue's agent task). The full
    /// behavior contract is encoded in the test file at
    /// `tests/tier_ramp_coordinator.rs`; tests are the spec.
    pub async fn run(&self) -> Result<RampStatus, String> {
        // Concrete behavior:
        //   for each tier in self.plan:
        //     - check abort signal; if set, mark current tier Invalid + emit RampAborted{Manual}
        //     - check pool: any non-Active drivers? if yes, mark Invalid + emit RampAborted{DriverStale}
        //     - emit TierStarted
        //     - fan out SetPlayers + StartTier to all Active drivers concurrently;
        //       all must complete within fan_out_deadline
        //     - sleep for hold_seconds (or abort early if signal/Stale)
        //     - fan out EndTier
        //     - emit TierEnded{state=Pass}
        //   on completion: emit RampCompleted, return RampStatus
        let _ = (&self.plan, &self.pool, &self.dispatch, &self.events_tx);
        unimplemented!("C2: tier ramp coordinator run loop — see tests/tier_ramp_coordinator.rs")
    }

    /// Send a command concurrently to every driver currently in `Active`.
    /// Returns Ok once all sends have completed (or fan_out_deadline exceeded).
    /// Public so tests can exercise the fan-out independent of the run loop.
    pub async fn send_command_to_active_drivers(
        &self,
        _command: OrchestratorCommand,
    ) -> Result<Vec<DriverId>, String> {
        // Concrete behavior:
        //   - snapshot pool, filter to Active
        //   - spawn one tokio task per driver, calling self.dispatch.send(...)
        //   - join all with self.fan_out_deadline as the wall-clock cap
        //   - return the list of driver IDs that completed Ok
        unimplemented!("C2: fan-out — see tests/tier_ramp_coordinator.rs")
    }
}

/// Construct an `OrchestratorCommand::StartTier` from a `Tier` and index.
/// Pure helper; trivial enough to not need its own test.
pub fn start_tier_command(index: u32, tier: Tier) -> OrchestratorCommand {
    OrchestratorCommand::StartTier(StartTierCommand {
        tier_index: index,
        player_count: tier.player_count,
        hold_seconds: tier.hold_seconds,
    })
}
