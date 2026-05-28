//! Acceptance tests for the telemetry archive (C6).
//! The tests are the spec.

use crate::telemetry::{ClusterWireStats, TelemetrySnapshot};
use crate::telemetry_archive::{list_snapshots, TelemetryArchive, Uploader};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Mock uploader: records every (key, body) pair it receives.
#[derive(Default)]
struct MockUploader {
    calls: Mutex<Vec<(String, Vec<u8>)>>,
}
impl MockUploader {
    async fn calls(&self) -> Vec<(String, Vec<u8>)> {
        self.calls.lock().await.clone()
    }
}
impl Uploader for MockUploader {
    async fn upload(&self, key: String, body: Vec<u8>) -> Result<(), String> {
        self.calls.lock().await.push((key, body));
        Ok(())
    }
}

fn synth_snapshot(unix_ms: u128) -> TelemetrySnapshot {
    let mut clusters = HashMap::new();
    clusters.insert(
        "https://cluster-a/stats".to_string(),
        ClusterWireStats {
            bytes_in: 0,
            bytes_out: 1_000_000,
            last_tick_us: 33_000,
            broadcast_lagged_events: 0,
            entities_current: 50,
        },
    );
    TelemetrySnapshot {
        snapshot_at_unix_ms: unix_ms,
        fleet: Vec::new(),
        recent_commands: Vec::new(),
        clusters,
        driver_metrics: HashMap::new(),
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

#[tokio::test]
async fn snapshots_written_locally_during_run() {
    let dir = tempdir();
    let archive = TelemetryArchive::new(&dir, Arc::new(MockUploader::default()));
    archive.ensure_dir().await.unwrap();

    let base = now_unix_ms();
    for i in 0..5 {
        let snap = synth_snapshot(base + i);
        archive.write_snapshot(&snap).await.unwrap();
    }

    let files = list_snapshots(&dir).await.unwrap();
    assert_eq!(files.len(), 5);
    for f in &files {
        let bytes = tokio::fs::read(f).await.unwrap();
        let parsed: TelemetrySnapshot = serde_json::from_slice(&bytes).unwrap();
        assert!(parsed.clusters.contains_key("https://cluster-a/stats"));
    }
}

#[tokio::test]
async fn snapshots_uploaded_to_s3() {
    let dir = tempdir();
    let uploader = Arc::new(MockUploader::default());
    let archive = TelemetryArchive::new(&dir, uploader.clone());
    archive.ensure_dir().await.unwrap();

    let base = now_unix_ms();
    for i in 0..3 {
        archive
            .write_snapshot(&synth_snapshot(base + i))
            .await
            .unwrap();
    }

    let calls = uploader.calls().await;
    assert_eq!(calls.len(), 3, "all 3 snapshots should be uploaded");

    // Uploaded body matches local file byte-for-byte.
    let files = list_snapshots(&dir).await.unwrap();
    assert_eq!(calls.len(), files.len());
    for (i, file) in files.iter().enumerate() {
        let local = tokio::fs::read(file).await.unwrap();
        assert_eq!(local, calls[i].1, "uploaded body must match local file");
    }
}

#[tokio::test]
async fn snapshot_schema_includes_required_fields() {
    let dir = tempdir();
    let archive = TelemetryArchive::new(&dir, Arc::new(MockUploader::default()));
    archive.ensure_dir().await.unwrap();

    let snap = synth_snapshot(now_unix_ms());
    let path = archive.write_snapshot(&snap).await.unwrap();
    let bytes = tokio::fs::read(&path).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    // timestamp
    assert!(json["snapshot_at_unix_ms"].is_number());
    // command-log slice
    assert!(json["recent_commands"].is_array());
    // fleet state
    assert!(json["fleet"].is_array());
    // per-cluster /stats summary
    assert!(json["clusters"].is_object());
    let clusters = json["clusters"].as_object().unwrap();
    let cluster_a = &clusters["https://cluster-a/stats"];
    for field in [
        "bytes_in",
        "bytes_out",
        "last_tick_us",
        "broadcast_lagged_events",
        "entities_current",
    ] {
        assert!(
            cluster_a.get(field).is_some(),
            "cluster_a missing field {}",
            field
        );
    }
}

#[tokio::test]
async fn restart_resumes_without_overwriting_prior_snapshots() {
    let dir = tempdir();

    // First "process": write 3 snapshots.
    {
        let archive = TelemetryArchive::new(&dir, Arc::new(MockUploader::default()));
        archive.ensure_dir().await.unwrap();
        let base = now_unix_ms();
        for i in 0..3 {
            archive
                .write_snapshot(&synth_snapshot(base + i))
                .await
                .unwrap();
        }
    }
    let before = list_snapshots(&dir).await.unwrap();
    assert_eq!(before.len(), 3);

    // "Restart": brand-new archive against the same dir.
    {
        let archive = TelemetryArchive::new(&dir, Arc::new(MockUploader::default()));
        // Even with the fresh per-instance seq counter starting at 0, the
        // unix_ms timestamp keeps filenames distinct across restarts.
        let base = now_unix_ms() + 1_000;
        for i in 0..2 {
            archive
                .write_snapshot(&synth_snapshot(base + i))
                .await
                .unwrap();
        }
    }

    let after = list_snapshots(&dir).await.unwrap();
    assert_eq!(after.len(), 5, "prior snapshots preserved + new ones added");

    // Earlier files are still present (filename-equal to `before`).
    for f in &before {
        assert!(
            after.contains(f),
            "prior snapshot {} should still exist",
            f.display()
        );
    }
}

// --- helpers ------------------------------------------------------------

/// Allocate a fresh per-test directory under the platform tempdir. Avoids
/// pulling in the `tempfile` crate as a dependency.
fn tempdir() -> std::path::PathBuf {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("orchestrator-archive-test-{}-{}", pid, nanos));
    std::fs::create_dir_all(&p).unwrap();
    p
}
