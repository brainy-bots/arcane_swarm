//! Acceptance tests for the telemetry archive (C6).
//!
//! Replaces the old "results writer" tests. The orchestrator does NOT write
//! per-tier benchmark results — that's the controller's job. The orchestrator
//! periodically snapshots its operational state (telemetry + command log) to
//! local disk and S3 for operator reference and post-hoc inspection.
//!
//! Tests are gated with `#[ignore]` until the implementation lands.

#[test]
#[ignore]
fn snapshots_written_locally_during_run() {
    // Acceptance: After a 60s mock run, periodic snapshots exist locally on
    // the orchestrator's disk under the configured archive directory.
    todo!()
}

#[test]
#[ignore]
fn snapshots_uploaded_to_s3() {
    // Acceptance: Snapshots also land in the S3 artifact bucket configured by
    // Terraform; uploaded contents match the local copies byte-for-byte.
    todo!()
}

#[test]
#[ignore]
fn snapshot_schema_includes_required_fields() {
    // Acceptance: Each snapshot includes:
    //   - timestamp
    //   - command-log slice (commands sent + per-driver acks since last snapshot)
    //   - fleet state (per-driver state + last-heartbeat)
    //   - per-cluster /stats summary (latest sample per cluster)
    todo!()
}

#[test]
#[ignore]
fn restart_resumes_without_overwriting_prior_snapshots() {
    // Acceptance: An orchestrator restart picks up where it left off and writes
    // new snapshots without overwriting earlier ones. (No notion of a "run" —
    // archives are continuous operator data.)
    todo!()
}
