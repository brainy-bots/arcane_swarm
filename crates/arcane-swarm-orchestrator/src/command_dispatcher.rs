//! Real-time command dispatch — orchestrator component C2.
//!
//! Relays domain-neutral commands (`SetPlayers`, `SetSpawnDelayMs`, `Stop`)
//! from any submitting controller to all `Active` drivers in parallel, with
//! a wall-clock fan-out deadline. Records per-driver acknowledgments and
//! every (command, submitter, acks) tuple in an append-only command log.
//!
//! Decoupled from the wire transport via `DriverChannel`. Production wires
//! this to per-driver mpsc channels populated by the WS server when a
//! driver registers; tests inject a `MockDriverChannel` that records sends
//! and replies with scripted acks.

use crate::driver_pool::{DriverPool, DriverState};
use crate::protocol::{CommandAck, DriverId, OrchestratorCommand};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, RwLock};
use tokio::time::sleep;

/// Identifier of the controller that submitted a command. Recorded in the
/// command log so multiple concurrent controllers can be told apart.
pub type SubmitterId = String;

/// One entry in the append-only command log.
#[derive(Debug, Clone)]
pub struct CommandLogEntry {
    pub seq: u64,
    pub submitter: SubmitterId,
    pub command: OrchestratorCommand,
    pub submitted_at: Instant,
    pub acks: Vec<CommandAck>,
    /// Drivers that were `Active` at submit time but did not ack within the
    /// fan-out deadline (or whose channel send returned an error).
    pub missing: Vec<DriverId>,
}

/// Result of a successful submit. Mirrors the per-entry log fields the
/// submitter needs to react to.
#[derive(Debug, Clone)]
pub struct DispatchResult {
    pub seq: u64,
    pub acks: Vec<CommandAck>,
    pub missing: Vec<DriverId>,
}

/// Reasons a submit can fail before broadcast is even attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// No drivers were `Active` at submit time. The caller can retry later.
    NoActiveDrivers,
}

/// Trait abstracting "send one command to one driver and wait for its ack."
/// The dispatcher fans out by calling this once per Active driver.
///
/// Production: backed by an mpsc::Sender<OrchestratorCommand> populated by
/// the WS server when a driver registers; the receiver side is the per-
/// connection task that pushes the command on the WS wire and awaits ack.
///
/// Tests: backed by `MockDriverChannel` (in `tests/command_dispatch.rs`).
pub trait DriverChannel: Send + Sync {
    /// Push one command (with its dispatcher-assigned `seq`) to the driver
    /// and resolve to its `CommandAck` (carrying the same `seq`) when it
    /// arrives. Production wires this to a per-connection mpsc + oneshot
    /// pair driven by the WS server.
    fn send(
        &self,
        seq: u64,
        command: OrchestratorCommand,
    ) -> impl Future<Output = Result<CommandAck, String>> + Send;
}

/// The dispatcher.
///
/// Generic over a `DriverChannel` implementation so production and tests
/// can share the same fan-out / deadline / logging logic.
pub struct CommandDispatcher<C: DriverChannel + 'static> {
    pool: Arc<DriverPool>,
    channels: Arc<RwLock<HashMap<DriverId, Arc<C>>>>,
    next_seq: AtomicU64,
    log: Arc<RwLock<Vec<CommandLogEntry>>>,
    fan_out_deadline: Duration,
}

impl<C: DriverChannel + 'static> CommandDispatcher<C> {
    pub fn new(pool: Arc<DriverPool>) -> Self {
        Self {
            pool,
            channels: Arc::new(RwLock::new(HashMap::new())),
            next_seq: AtomicU64::new(0),
            log: Arc::new(RwLock::new(Vec::new())),
            fan_out_deadline: Duration::from_millis(100),
        }
    }

    pub fn with_fan_out_deadline(mut self, d: Duration) -> Self {
        self.fan_out_deadline = d;
        self
    }

    /// Register a per-driver channel. Called by the WS server on `Register`.
    pub async fn register_channel(&self, driver_id: DriverId, channel: Arc<C>) {
        self.channels.write().await.insert(driver_id, channel);
    }

    /// Forget a per-driver channel. Called by the WS server on `Deregister`
    /// or when a connection drops.
    pub async fn deregister_channel(&self, driver_id: DriverId) {
        self.channels.write().await.remove(&driver_id);
    }

    /// Fan out a command to every currently-Active driver, wait up to the
    /// fan-out deadline for acks, log the result, return a per-call summary.
    pub async fn submit(
        &self,
        submitter: SubmitterId,
        command: OrchestratorCommand,
    ) -> Result<DispatchResult, DispatchError> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let submitted_at = Instant::now();

        let active = self.snapshot_active().await;
        if active.is_empty() {
            return Err(DispatchError::NoActiveDrivers);
        }

        // Snapshot per-driver channels under the read lock, then drop it.
        let channels: HashMap<DriverId, Arc<C>> = {
            let map = self.channels.read().await;
            active
                .iter()
                .filter_map(|id| map.get(id).map(|ch| (*id, Arc::clone(ch))))
                .collect()
        };

        // Fan out. Each task pushes its (driver_id, result) onto a shared
        // channel; the collect loop below drains the channel until either
        // every task reports or the fan-out deadline elapses. Cancellation
        // of slow tasks is implicit — they remain spawned but their results
        // are ignored once we move past the deadline.
        let n = channels.len();
        let (tx, mut rx) = mpsc::channel(n.max(1));
        for (driver_id, ch) in channels {
            let cmd = command.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let result = ch.send(seq, cmd).await;
                let _ = tx.send((driver_id, result)).await;
            });
        }
        drop(tx); // ensure rx ends after the last spawned task replies

        let mut acks: Vec<CommandAck> = Vec::new();
        let mut delivered: std::collections::HashSet<DriverId> = std::collections::HashSet::new();
        let deadline = sleep(self.fan_out_deadline);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                _ = &mut deadline => break,
                msg = rx.recv() => match msg {
                    Some((driver_id, Ok(ack))) => {
                        acks.push(ack);
                        delivered.insert(driver_id);
                    }
                    Some((_, Err(_))) => continue,
                    None => break,
                },
            }
        }

        // Anyone who was Active at submit but didn't deliver = missing.
        let missing: Vec<DriverId> = active
            .iter()
            .filter(|id| !delivered.contains(id))
            .copied()
            .collect();

        // Append to log.
        self.log.write().await.push(CommandLogEntry {
            seq,
            submitter,
            command,
            submitted_at,
            acks: acks.clone(),
            missing: missing.clone(),
        });

        Ok(DispatchResult { seq, acks, missing })
    }

    /// Snapshot the command log. Cheap clone of an in-memory Vec; the log is
    /// expected to stay small (telemetry archive trims it on snapshot).
    pub async fn command_log(&self) -> Vec<CommandLogEntry> {
        self.log.read().await.clone()
    }

    async fn snapshot_active(&self) -> Vec<DriverId> {
        // We can't iterate the pool directly (no `iter` API) so we ask the
        // channels map first and cross-reference state. In practice every
        // registered channel corresponds to a driver in the pool.
        let channel_ids: Vec<DriverId> = {
            let map = self.channels.read().await;
            map.keys().copied().collect()
        };
        let mut active = Vec::new();
        for id in channel_ids {
            if matches!(self.pool.get_state(id).await, Some(DriverState::Active)) {
                active.push(id);
            }
        }
        active
    }
}
