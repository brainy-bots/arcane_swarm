//! Real-time validity gate — orchestrator component C4.
//!
//! Per-tier acceptance criteria evaluated every 5 seconds during a tier;
//! if a gate fails for ≥3 consecutive evaluations during steady-state, the
//! tier is marked `FAIL` and the ramp aborts.
//!
//! The gate is pure logic — no I/O — so tests inject scripted evaluation
//! samples directly.

/// Per-axis acceptance thresholds. Each axis is optional; `None` means the
/// gate ignores that axis.
#[derive(Debug, Clone, Copy, Default)]
pub struct GateConfig {
    /// Per-tier maximum p99 latency in milliseconds.
    pub max_p99_latency_ms: Option<u32>,
    /// Per-tier maximum error rate (0.0–1.0). 0.05 = 5%.
    pub max_error_rate: Option<f64>,
    /// Minimum entities the cluster should be reporting; tier fails if it
    /// drops below this for the breach window.
    pub min_entities: Option<u64>,
    /// Number of consecutive failed evaluations required to flip the tier
    /// from `Running` to `Failing`. Defaults to 3 per the design doc.
    pub breach_window: u32,
}

impl GateConfig {
    /// Sensible defaults: no axis enabled, breach window 3.
    pub fn new() -> Self {
        Self {
            max_p99_latency_ms: None,
            max_error_rate: None,
            min_entities: None,
            breach_window: 3,
        }
    }

    pub fn with_max_p99_latency(mut self, ms: u32) -> Self {
        self.max_p99_latency_ms = Some(ms);
        self
    }

    pub fn with_max_error_rate(mut self, rate: f64) -> Self {
        self.max_error_rate = Some(rate);
        self
    }

    pub fn with_min_entities(mut self, n: u64) -> Self {
        self.min_entities = Some(n);
        self
    }

    pub fn with_breach_window(mut self, n: u32) -> Self {
        self.breach_window = n;
        self
    }
}

/// One evaluation sample fed into the gate. Composed from the stats
/// collector + driver telemetry; the gate doesn't care where it came from.
#[derive(Debug, Clone, Copy)]
pub struct EvaluationSample {
    pub p99_latency_ms: u32,
    pub error_rate: f64,
    pub entities_current: u64,
}

/// Per-tier outcome surfaced by the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierOutcome {
    /// Tier is in flight; not enough evaluations to decide yet.
    Pending,
    /// All evaluations to date have passed; tier currently passing.
    Pass,
    /// `breach_window` consecutive failed evaluations seen; tier is failing
    /// and the ramp should abort.
    Fail,
}

/// The ValidityGate itself. Stateful: tracks consecutive breach count per
/// tier so it knows when the breach window has been hit.
pub struct ValidityGate {
    config: GateConfig,
    /// Index of the tier currently being evaluated (None if no tier active).
    current_tier: Option<u32>,
    /// Outcome of each completed tier, indexed by tier index.
    tier_outcomes: Vec<TierOutcome>,
    /// Consecutive failed evaluations on the current tier.
    consecutive_breaches: u32,
    /// Has any tier failed? Used by `should_short_circuit_subsequent_tiers()`.
    any_tier_failed: bool,
}

impl ValidityGate {
    pub fn new(config: GateConfig) -> Self {
        Self {
            config,
            current_tier: None,
            tier_outcomes: Vec::new(),
            consecutive_breaches: 0,
            any_tier_failed: false,
        }
    }

    /// Mark the start of a new tier. Resets per-tier breach counter.
    pub fn start_tier(&mut self, _tier_index: u32) {
        unimplemented!("C4: tier start tracking — see tests/validity_gate.rs")
    }

    /// Feed one evaluation sample. Returns the current outcome (Pending /
    /// Pass / Fail). When this returns `Fail`, the orchestrator must abort
    /// the ramp.
    pub fn evaluate(&mut self, _sample: EvaluationSample) -> TierOutcome {
        // Concrete behavior:
        //   - if any axis breaches its threshold, increment consecutive_breaches
        //   - else reset consecutive_breaches to 0
        //   - if consecutive_breaches >= self.config.breach_window:
        //       return Fail (and remember any_tier_failed)
        //   - if no breach this sample, return Pass; else Pending
        let _ = (&self.config, &self.current_tier);
        unimplemented!("C4: per-sample evaluation — see tests/validity_gate.rs")
    }

    /// Mark the current tier as complete (called by coordinator at tier end).
    /// Records the final outcome in `tier_outcomes`.
    pub fn complete_tier(&mut self, _outcome: TierOutcome) {
        unimplemented!("C4: tier completion — see tests/validity_gate.rs")
    }

    /// True when any prior tier has been marked `Fail`. The coordinator
    /// queries this before starting subsequent tiers; if true, those tiers
    /// must NOT run.
    pub fn should_short_circuit_subsequent_tiers(&self) -> bool {
        self.any_tier_failed
    }

    /// Read-only access to the per-tier outcomes recorded so far.
    pub fn tier_outcomes(&self) -> &[TierOutcome] {
        &self.tier_outcomes
    }

    /// Number of consecutive failed evaluations on the current tier.
    /// Test-visible; production callers don't need this.
    pub fn consecutive_breaches(&self) -> u32 {
        self.consecutive_breaches
    }
}
