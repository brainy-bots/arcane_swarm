#[test]
#[ignore]
fn all_gates_passing_marks_tier_pass() {
    // Acceptance: Synthetic tier with all gates passing → tier marked `PASS`.
    todo!()
}

#[test]
#[ignore]
fn gate_failure_marks_tier_fail_and_aborts() {
    // Acceptance: Synthetic tier with latency exceeding gate for 3 consecutive evaluations → tier marked `FAIL`, ramp aborts within 15s of the breach.
    todo!()
}

#[test]
#[ignore]
fn failed_tier_short_circuits_subsequent_tiers() {
    // Acceptance: Tier marked `FAIL` short-circuits subsequent tier execution.
    todo!()
}
