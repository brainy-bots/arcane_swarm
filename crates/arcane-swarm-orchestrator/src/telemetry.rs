//! Telemetry source — orchestrator component C5 data plane.
//!
//! Periodically (or on-demand) builds a `TelemetrySnapshot` from the live
//! fleet state, command log, and per-cluster stats, then broadcasts it to
//! every subscribed consumer (operator-cli over SSE, benchmark controller
//! over SSE, in-process tests via `subscribe`).
//!
//! Wire format is a serializable subset of the internal types — the
//! orchestrator's internal `Instant` time axis is converted to millisecond
//! ages relative to snapshot time so that subscribers don't have to care
//! about the orchestrator process's monotonic clock.

use crate::command_dispatcher::{CommandDispatcher, CommandLogEntry, DriverChannel};
use crate::driver_pool::{DriverPool, DriverState};
use crate::protocol::{DriverId, OrchestratorCommand};
use crate::stats_collector::{ClusterEndpoint, ClusterStats, StatsCollector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// One telemetry snapshot — the JSON payload of every SSE event.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TelemetrySnapshot {
    /// Wall-clock time of the snapshot (unix ms). Subscribers use this for
    /// ordering and for cross-referencing with their own clocks.
    pub snapshot_at_unix_ms: u128,
    pub fleet: Vec<DriverWireState>,
    /// Recent command log entries (most-recent first), capped at
    /// `recent_command_window` entries by the source.
    pub recent_commands: Vec<CommandWireEntry>,
    pub clusters: HashMap<String, ClusterWireStats>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DriverWireState {
    pub driver_id: DriverId,
    pub state: String,
    pub last_heartbeat_age_ms: u128,
    pub capabilities: Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CommandWireEntry {
    pub seq: u64,
    pub submitter: String,
    pub command: OrchestratorCommand,
    pub age_ms: u128,
    pub acked_drivers: Vec<DriverId>,
    pub missing_drivers: Vec<DriverId>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClusterWireStats {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub last_tick_us: u64,
    pub broadcast_lagged_events: u64,
    pub entities_current: u64,
}

/// The telemetry source.
///
/// Generic over the driver-channel and cluster-endpoint types so it can be
/// constructed against either production or mock implementations.
pub struct TelemetrySource<C: DriverChannel + 'static, E: ClusterEndpoint + 'static> {
    pool: Arc<DriverPool>,
    dispatcher: Arc<CommandDispatcher<C>>,
    collector: Arc<StatsCollector<E>>,
    tx: broadcast::Sender<TelemetrySnapshot>,
    /// Maximum number of recent command-log entries embedded in each snapshot.
    recent_command_window: usize,
}

impl<C: DriverChannel + 'static, E: ClusterEndpoint + 'static> TelemetrySource<C, E> {
    pub fn new(
        pool: Arc<DriverPool>,
        dispatcher: Arc<CommandDispatcher<C>>,
        collector: Arc<StatsCollector<E>>,
    ) -> Self {
        let (tx, _rx) = broadcast::channel(64);
        Self {
            pool,
            dispatcher,
            collector,
            tx,
            recent_command_window: 32,
        }
    }

    pub fn with_recent_command_window(mut self, n: usize) -> Self {
        self.recent_command_window = n;
        self
    }

    /// Subscribe to the live snapshot stream. Late subscribers miss earlier
    /// snapshots (broadcast channel — no replay).
    pub fn subscribe(&self) -> broadcast::Receiver<TelemetrySnapshot> {
        self.tx.subscribe()
    }

    /// Number of currently-connected subscribers. Useful for tests + ops.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Build one snapshot from current state and broadcast it. Returns the
    /// snapshot for the caller's own use (tests, archive component).
    pub async fn tick(&self) -> TelemetrySnapshot {
        let now = Instant::now();
        let snapshot_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        let fleet: Vec<DriverWireState> = self
            .pool
            .snapshot()
            .await
            .into_iter()
            .map(|entry| DriverWireState {
                driver_id: entry.id,
                state: state_name(entry.state).to_string(),
                last_heartbeat_age_ms: now
                    .saturating_duration_since(entry.last_heartbeat)
                    .as_millis(),
                capabilities: entry.capabilities,
            })
            .collect();

        let log = self.dispatcher.command_log().await;
        let recent_commands: Vec<CommandWireEntry> = log
            .iter()
            .rev()
            .take(self.recent_command_window)
            .map(|e| wire_entry(e, now))
            .collect();

        let clusters: HashMap<String, ClusterWireStats> = self
            .collector
            .latest_per_cluster()
            .await
            .into_iter()
            .map(|(url, stats)| (url, wire_cluster(stats)))
            .collect();

        let snapshot = TelemetrySnapshot {
            snapshot_at_unix_ms,
            fleet,
            recent_commands,
            clusters,
        };

        // Drop send-error: it just means no live subscribers.
        let _ = self.tx.send(snapshot.clone());
        snapshot
    }

    /// Spawn a periodic snapshot loop. Never returns under normal operation.
    /// Production calls this from `tokio::spawn`; tests prefer `tick()`.
    pub async fn run(&self, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            self.tick().await;
        }
    }
}

fn state_name(s: DriverState) -> &'static str {
    match s {
        DriverState::Active => "Active",
        DriverState::Stale => "Stale",
        DriverState::Failed => "Failed",
    }
}

fn wire_entry(e: &CommandLogEntry, now: Instant) -> CommandWireEntry {
    CommandWireEntry {
        seq: e.seq,
        submitter: e.submitter.clone(),
        command: e.command.clone(),
        age_ms: now.saturating_duration_since(e.submitted_at).as_millis(),
        acked_drivers: e.acks.iter().map(|a| a.driver_id).collect(),
        missing_drivers: e.missing.clone(),
    }
}

fn wire_cluster(s: ClusterStats) -> ClusterWireStats {
    ClusterWireStats {
        bytes_in: s.bytes_in,
        bytes_out: s.bytes_out,
        last_tick_us: s.last_tick_us,
        broadcast_lagged_events: s.broadcast_lagged_events,
        entities_current: s.entities_current,
    }
}
