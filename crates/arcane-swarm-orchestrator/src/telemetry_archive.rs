//! Telemetry archive — orchestrator component C6.
//!
//! Subscribes to a `TelemetrySource` and persists each snapshot as a
//! standalone JSON file on local disk plus an optional pluggable uploader
//! (for S3 in production; mocked in tests).
//!
//! Files are named `snapshot_<unix_ms>_<seq>.json`; the unix timestamp +
//! per-archive sequence number keep filenames monotonic and avoid
//! overwriting prior snapshots when the orchestrator restarts.
//!
//! This is **operator-facing operational data**, not benchmark results.
//! Per-phase / per-tier benchmark output belongs to the benchmark
//! controller in `arcane-scaling-benchmarks`.

use crate::telemetry::TelemetrySnapshot;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::broadcast;

/// Trait abstracting "upload one snapshot blob to remote storage."
/// Production: backed by an S3 client. Tests: backed by a `MockUploader`
/// that records calls in memory.
pub trait Uploader: Send + Sync {
    fn upload(&self, key: String, body: Vec<u8>)
        -> impl Future<Output = Result<(), String>> + Send;
}

/// No-op uploader for deployments that only want local archiving.
pub struct NoopUploader;
impl Uploader for NoopUploader {
    async fn upload(&self, _key: String, _body: Vec<u8>) -> Result<(), String> {
        Ok(())
    }
}

/// The archive itself.
///
/// Generic over the uploader so production wires real S3 and tests inject a
/// mock recorder.
pub struct TelemetryArchive<U: Uploader + 'static> {
    dir: PathBuf,
    uploader: Arc<U>,
    /// Per-instance monotonic counter appended to filenames so two snapshots
    /// recorded in the same wall-clock millisecond don't collide.
    next_seq: AtomicU64,
}

impl<U: Uploader + 'static> TelemetryArchive<U> {
    pub fn new(dir: impl Into<PathBuf>, uploader: Arc<U>) -> Self {
        Self {
            dir: dir.into(),
            uploader,
            next_seq: AtomicU64::new(0),
        }
    }

    /// Ensure the archive directory exists. Idempotent.
    pub async fn ensure_dir(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir).await
    }

    /// Persist one snapshot: write to disk under `dir`, then upload via the
    /// configured uploader. Failures on either side propagate so the caller
    /// can decide retry policy.
    pub async fn write_snapshot(&self, snap: &TelemetrySnapshot) -> Result<PathBuf, String> {
        let body = serde_json::to_vec_pretty(snap).map_err(|e| e.to_string())?;
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let filename = format!("snapshot_{}_{:06}.json", snap.snapshot_at_unix_ms, seq);
        let path = self.dir.join(&filename);
        fs::write(&path, &body).await.map_err(|e| e.to_string())?;
        self.uploader.upload(filename, body).await?;
        Ok(path)
    }

    /// Drive the archive: subscribe to the source's broadcast and persist
    /// every snapshot until the channel closes. Logs (silently) on failures.
    /// Production calls this from `tokio::spawn`.
    pub async fn run(&self, mut rx: broadcast::Receiver<TelemetrySnapshot>) -> std::io::Result<()> {
        self.ensure_dir().await?;
        loop {
            match rx.recv().await {
                Ok(snap) => {
                    let _ = self.write_snapshot(&snap).await;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }
}

/// List existing snapshot files in `dir`, sorted by filename. Used by the
/// restart-resume test to verify prior snapshots are still on disk.
pub async fn list_snapshots(dir: impl AsRef<Path>) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("snapshot_") && n.ends_with(".json"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}
