//! Acceptance tests for real-time command dispatch (C2).
//!
//! Replaces the old "tier ramp coordinator" tests. The orchestrator owns
//! command broadcast plumbing only — it knows nothing about phases, tiers,
//! ramps, or validity. The benchmark controller (in arcane-scaling-benchmarks)
//! schedules commands; the orchestrator relays them to drivers and records
//! per-driver acknowledgments.
//!
//! Initial command vocabulary (extensible):
//!   - SetPlayers(N)
//!   - SetSpawnDelayMs(ms)
//!   - Stop
//!
//! Tests are gated with `#[ignore]` until the implementation lands.

#[test]
#[ignore]
fn set_players_broadcasts_to_all_active_drivers_within_100ms() {
    // Acceptance: 12-driver mock fleet receives a `SetPlayers(125)` broadcast
    // within 100 ms wall-clock; all drivers acknowledge inside the window.
    todo!()
}

#[test]
#[ignore]
fn per_driver_acknowledgment_recorded() {
    // Acceptance: Per-driver delivery acknowledgment is recorded; orchestrator
    // surfaces which drivers acked and which did not.
    todo!()
}

#[test]
#[ignore]
fn stale_driver_does_not_block_broadcast_to_others() {
    // Acceptance: A driver going `Stale` mid-broadcast is logged but does not
    // block delivery to the rest of the fleet.
    todo!()
}

#[test]
#[ignore]
fn unknown_command_returns_typed_error_no_broadcast() {
    // Acceptance: Unknown command type → orchestrator returns a typed error to
    // the submitter; no broadcast attempted.
    todo!()
}

#[test]
#[ignore]
fn multiple_controllers_can_submit_concurrently() {
    // Acceptance: Multiple controllers submit commands concurrently; each
    // command + ack pair is logged with the submitter's identity.
    todo!()
}

#[test]
#[ignore]
fn set_spawn_delay_ms_propagates_to_drivers() {
    // Acceptance: SetSpawnDelayMs(250) reaches every Active driver; each
    // driver's ack reports it accepted the new pacing value.
    todo!()
}

#[test]
#[ignore]
fn stop_command_drains_drivers_and_flushes_telemetry() {
    // Acceptance: A `Stop` command causes drivers to tear down their fleet
    // slices, deregister, and the orchestrator flushes the final telemetry
    // snapshot before returning to idle.
    todo!()
}
