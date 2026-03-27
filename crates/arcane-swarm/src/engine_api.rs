//! Reusable engine-facing API boundary for embedding swarm orchestration.
//!
//! The binary remains the default runtime entrypoint, but these types define the
//! stable shape for external tooling that wants to drive swarm runs without
//! depending on binary-internal modules.

use crate::{Backend, SwarmMode};

/// Backend choice exposed to embedding callers.
pub type SwarmBackend = Backend;

/// Mode choice exposed to embedding callers.
pub type SwarmDistribution = SwarmMode;

/// Config used by an embedding orchestrator to start a run.
#[derive(Clone)]
pub struct EngineRunConfig {
    pub players: u32,
    pub tick_rate: u32,
    pub duration_secs: u64,
    pub backend: SwarmBackend,
    pub mode: SwarmDistribution,
}

/// Snapshot of summary metrics expected by benchmark harnesses.
#[derive(Debug, Clone, Default)]
pub struct EngineSummary {
    pub total_calls: u64,
    pub total_oks: u64,
    pub total_errs: u64,
    pub avg_latency_ms: f64,
}

/// Handle for an in-flight swarm run controlled by an embedding caller.
pub trait EngineRunHandle: Send + Sync {
    fn set_players(&self, desired_players: u32) -> Result<(), String>;
    fn request_summary(&self) -> Result<EngineSummary, String>;
    fn stop(&self) -> Result<(), String>;
}

/// Engine contract for reusable orchestration independent of binary internals.
pub trait SwarmEngine: Send + Sync {
    type Handle: EngineRunHandle;

    fn start_run(&self, config: EngineRunConfig) -> Result<Self::Handle, String>;
}
