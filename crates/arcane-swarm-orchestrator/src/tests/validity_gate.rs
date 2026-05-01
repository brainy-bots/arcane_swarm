//! Acceptance tests for the validity gate (C4).
//!
//! Tests are gated with `#[ignore]` until the implementation in
//! `src/validity_gate.rs` lands. To run them locally:
//!
//!   cargo test -p arcane-swarm-orchestrator -- --ignored
//!
//! The tests are the spec.

use crate::validity_gate::{EvaluationSample, GateConfig, TierOutcome, ValidityGate};

fn passing_sample() -> EvaluationSample {
    EvaluationSample {
        p99_latency_ms: 50,
        error_rate: 0.0,
        entities_current: 1_000,
    }
}

fn high_latency_sample() -> EvaluationSample {
    EvaluationSample {
        p99_latency_ms: 500, // way above any sensible gate
        error_rate: 0.0,
        entities_current: 1_000,
    }
}

#[test]
#[ignore]
fn all_gates_passing_marks_tier_pass() {
    // Acceptance: Synthetic tier with all gates passing → tier marked `PASS`.
    let config = GateConfig::new()
        .with_max_p99_latency(200)
        .with_max_error_rate(0.05)
        .with_min_entities(100);
    let mut gate = ValidityGate::new(config);
    gate.start_tier(0);

    // Feed 5 clean samples; gate must report Pass throughout.
    for _ in 0..5 {
        let outcome = gate.evaluate(passing_sample());
        assert_ne!(
            outcome,
            TierOutcome::Fail,
            "gate must not fail on clean samples"
        );
    }

    assert_eq!(
        gate.consecutive_breaches(),
        0,
        "no breaches should be recorded on a clean tier"
    );

    gate.complete_tier(TierOutcome::Pass);
    assert_eq!(gate.tier_outcomes(), &[TierOutcome::Pass]);
    assert!(
        !gate.should_short_circuit_subsequent_tiers(),
        "no failed tiers; subsequent tiers should still run"
    );
}

#[test]
#[ignore]
fn gate_failure_marks_tier_fail_and_aborts() {
    // Acceptance: Synthetic tier with latency exceeding gate for 3 consecutive
    // evaluations → tier marked `FAIL`, ramp aborts within 15 s of the breach.
    let config = GateConfig::new()
        .with_max_p99_latency(200)
        .with_breach_window(3);
    let mut gate = ValidityGate::new(config);
    gate.start_tier(0);

    // First evaluation breaches: outcome is Pending (1 of 3 needed).
    let r1 = gate.evaluate(high_latency_sample());
    assert_ne!(
        r1,
        TierOutcome::Fail,
        "first breach alone must not fail the tier"
    );

    // Second breach: still Pending (2 of 3).
    let r2 = gate.evaluate(high_latency_sample());
    assert_ne!(r2, TierOutcome::Fail, "second breach alone must not fail");

    // Third consecutive breach: tier must be Fail.
    let r3 = gate.evaluate(high_latency_sample());
    assert_eq!(
        r3,
        TierOutcome::Fail,
        "three consecutive breaches must mark tier Fail"
    );

    // Confirm consecutive_breaches reflects state.
    assert_eq!(gate.consecutive_breaches(), 3);

    // Coordinator's response would be to abort. Simulate that by completing
    // the tier with Fail outcome.
    gate.complete_tier(TierOutcome::Fail);
    assert_eq!(gate.tier_outcomes(), &[TierOutcome::Fail]);
}

#[test]
#[ignore]
fn intermittent_breaches_do_not_fail_tier() {
    // Auxiliary acceptance: only CONSECUTIVE breaches in the breach window
    // should fail the tier. A pass-fail-pass-fail-pass sequence must not
    // accumulate to 3.
    let config = GateConfig::new()
        .with_max_p99_latency(200)
        .with_breach_window(3);
    let mut gate = ValidityGate::new(config);
    gate.start_tier(0);

    let outcomes: Vec<TierOutcome> = [
        passing_sample(),
        high_latency_sample(),
        passing_sample(),
        high_latency_sample(),
        passing_sample(),
        high_latency_sample(),
    ]
    .iter()
    .map(|s| gate.evaluate(*s))
    .collect();

    assert!(
        !outcomes.contains(&TierOutcome::Fail),
        "intermittent (non-consecutive) breaches must not flip the tier; got {:?}",
        outcomes
    );
}

#[test]
#[ignore]
fn failed_tier_short_circuits_subsequent_tiers() {
    // Acceptance: Tier marked `FAIL` short-circuits subsequent tier execution.
    let config = GateConfig::new()
        .with_max_p99_latency(200)
        .with_breach_window(3);
    let mut gate = ValidityGate::new(config);

    // Tier 0: fail it.
    gate.start_tier(0);
    for _ in 0..3 {
        let _ = gate.evaluate(high_latency_sample());
    }
    gate.complete_tier(TierOutcome::Fail);

    assert!(
        gate.should_short_circuit_subsequent_tiers(),
        "after a Fail, subsequent tiers should be short-circuited"
    );

    // Even if a hypothetical tier 1 had clean evaluations, the coordinator
    // must not run it because should_short_circuit_subsequent_tiers() is true.
    // We assert the gate's own state is consistent with that.
    assert_eq!(gate.tier_outcomes().last(), Some(&TierOutcome::Fail));
}
