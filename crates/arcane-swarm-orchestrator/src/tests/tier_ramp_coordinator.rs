#[test]
#[ignore]
fn all_drivers_receive_commands_simultaneously() {
    // Acceptance: 12-driver mock fleet (in-process test): all receive `SetPlayers(125)` simultaneously (within 100ms wall-clock).
    todo!()
}

#[test]
#[ignore]
fn tier_ramp_progresses_through_sequence() {
    // Acceptance: Tier ramp progresses through the configured sequence.
    todo!()
}

#[test]
#[ignore]
fn stale_driver_marks_tier_invalid() {
    // Acceptance: A driver going `Stale` mid-tier marks the tier `INVALID` and the run aborts.
    todo!()
}

#[test]
#[ignore]
fn manual_abort_stops_tier_cleanly() {
    // Acceptance: Manual abort signal stops the current tier cleanly.
    todo!()
}
