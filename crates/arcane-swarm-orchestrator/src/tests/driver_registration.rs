#[test]
#[ignore]
fn registration_round_trip_succeeds() {
    // Acceptance: Registration round-trip succeeds; pool size grows.
    todo!()
}

#[test]
#[ignore]
fn heartbeat_keeps_driver_active() {
    // Acceptance: Heartbeat keeps driver in `Active`; missed heartbeats transition to `Stale`.
    todo!()
}

#[test]
#[ignore]
fn graceful_deregister_removes_driver() {
    // Acceptance: Graceful deregister removes driver immediately.
    todo!()
}

#[test]
#[ignore]
fn pool_cap_enforced() {
    // Acceptance: Pool cap enforced (orchestrator rejects registration past `--max-drivers`).
    todo!()
}
