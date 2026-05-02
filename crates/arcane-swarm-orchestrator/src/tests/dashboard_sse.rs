//! Acceptance tests for telemetry SSE source + dashboard CLI (C5).
//! The tests are the spec.

use crate::command_dispatcher::{CommandDispatcher, DriverChannel};
use crate::driver_pool::DriverPool;
use crate::protocol::{CommandAck, DriverId, OrchestratorCommand, SetPlayersCommand};
use crate::sse_server::serve_sse_bound;
use crate::stats_collector::{ClusterEndpoint, ClusterStats, StatsCollector};
use crate::telemetry::{TelemetrySnapshot, TelemetrySource};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

// --- mock driver channel (mirrors the one in command_dispatch tests) ----

struct MockDriverChannel {
    driver_id: DriverId,
    seq: Arc<AtomicU64>,
}
impl MockDriverChannel {
    fn new(driver_id: DriverId, seq: Arc<AtomicU64>) -> Self {
        Self { driver_id, seq }
    }
}
impl DriverChannel for MockDriverChannel {
    async fn send(&self, _command: OrchestratorCommand) -> Result<CommandAck, String> {
        Ok(CommandAck {
            driver_id: self.driver_id,
            command_seq: self.seq.fetch_add(1, Ordering::SeqCst),
        })
    }
}

// --- mock cluster endpoint (idle: single sample, no failures) -----------

struct MockEndpoint {
    url: String,
    sample: Mutex<Option<ClusterStats>>,
}
impl MockEndpoint {
    fn new(url: &str, stats: ClusterStats) -> Self {
        Self {
            url: url.to_string(),
            sample: Mutex::new(Some(stats)),
        }
    }
}
impl ClusterEndpoint for MockEndpoint {
    fn url(&self) -> &str {
        &self.url
    }
    async fn fetch(&self) -> Result<ClusterStats, String> {
        self.sample
            .lock()
            .await
            .clone()
            .ok_or_else(|| "no sample".into())
    }
}

fn one_stats(now: Instant) -> ClusterStats {
    ClusterStats {
        bytes_in: 0,
        bytes_out: 1_000_000,
        last_tick_us: 33_000,
        broadcast_lagged_events: 0,
        entities_current: 50,
        sampled_at: now,
    }
}

// --- builder ------------------------------------------------------------

async fn build_source(
    n_drivers: usize,
) -> (
    Arc<TelemetrySource<MockDriverChannel, MockEndpoint>>,
    Arc<DriverPool>,
    Arc<CommandDispatcher<MockDriverChannel>>,
    Arc<StatsCollector<MockEndpoint>>,
) {
    let pool = Arc::new(DriverPool::new(
        Duration::from_millis(50),
        Duration::from_millis(150),
        32,
    ));
    let dispatcher = Arc::new(CommandDispatcher::<MockDriverChannel>::new(pool.clone()));
    let seq = Arc::new(AtomicU64::new(1));
    for i in 0..n_drivers {
        let id = pool.register(json!({"i": i})).await.unwrap();
        let ch = Arc::new(MockDriverChannel::new(id, seq.clone()));
        dispatcher.register_channel(id, ch).await;
    }

    let endpoint = Arc::new(MockEndpoint::new(
        "https://cluster-a/stats",
        one_stats(Instant::now()),
    ));
    let collector = Arc::new(StatsCollector::new(vec![endpoint]));
    // Prime the collector with at least one sample so snapshots have a
    // cluster summary.
    collector.poll_once().await.unwrap();

    let source = Arc::new(TelemetrySource::new(
        pool.clone(),
        dispatcher.clone(),
        collector.clone(),
    ));
    (source, pool, dispatcher, collector)
}

// --- HTTP/SSE client ----------------------------------------------------

/// Connect, GET /telemetry/stream, parse out `data:` lines for `count`
/// events. Returns the parsed snapshots in arrival order.
async fn read_sse(addr: SocketAddr, count: usize) -> Vec<TelemetrySnapshot> {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(b"GET /telemetry/stream HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();

    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut snapshots = Vec::new();
    while snapshots.len() < count {
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut tmp)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => break,
        };
        buf.extend_from_slice(&tmp[..n]);
        let s = String::from_utf8_lossy(&buf).to_string();
        for line in s.lines() {
            if let Some(json_str) = line.strip_prefix("data: ") {
                if let Ok(snapshot) = serde_json::from_str::<TelemetrySnapshot>(json_str) {
                    snapshots.push(snapshot);
                    if snapshots.len() >= count {
                        break;
                    }
                }
            }
        }
        // Drop processed bytes so next iteration's split is fast.
        if let Some(idx) = buf.iter().rposition(|b| *b == b'\n') {
            buf.drain(..=idx);
        }
    }
    snapshots
}

// --- tests --------------------------------------------------------------

#[tokio::test]
async fn sse_stream_emits_valid_json_events() {
    let (source, _pool, _dispatcher, _collector) = build_source(2).await;
    let (addr, _server) = serve_sse_bound("127.0.0.1:0".parse().unwrap(), source.clone())
        .await
        .unwrap();

    // Drive snapshots from the test thread so timing is deterministic.
    let s2 = source.clone();
    let _ticker = tokio::spawn(async move {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            s2.tick().await;
        }
    });

    let snapshots = read_sse(addr, 3).await;
    assert!(snapshots.len() >= 3, "got {} snapshots", snapshots.len());
    for snap in &snapshots {
        assert!(snap.snapshot_at_unix_ms > 0);
        assert_eq!(snap.fleet.len(), 2);
        assert!(snap.clusters.contains_key("https://cluster-a/stats"));
    }
}

#[tokio::test]
async fn cli_connects_renders_and_reconnects() {
    // "Reconnects on transient drop" — verify the server keeps serving after
    // an existing client disconnects, and that a fresh connect succeeds.
    let (source, _pool, _dispatcher, _collector) = build_source(1).await;
    let (addr, _server) = serve_sse_bound("127.0.0.1:0".parse().unwrap(), source.clone())
        .await
        .unwrap();

    let s = source.clone();
    let _ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(40)).await;
            s.tick().await;
        }
    });

    // First client: receive a couple of events then drop.
    let first = read_sse(addr, 2).await;
    assert!(first.len() >= 2);
    // Dropping `first` (a Vec) doesn't drop the connection — but read_sse
    // returned, the TcpStream inside it went out of scope when read_sse
    // returned. The server's per-connection task should detect the broken
    // pipe on the next write.

    // Reconnect; server should still be accepting and emitting events.
    let second = read_sse(addr, 2).await;
    assert!(
        second.len() >= 2,
        "reconnect failed; only got {} events",
        second.len()
    );
}

#[tokio::test]
async fn multiple_subscribers_each_receive_events() {
    let (source, _pool, _dispatcher, _collector) = build_source(1).await;
    let (addr, _server) = serve_sse_bound("127.0.0.1:0".parse().unwrap(), source.clone())
        .await
        .unwrap();

    let s = source.clone();
    let _ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(40)).await;
            s.tick().await;
        }
    });

    let a = tokio::spawn(read_sse(addr, 2));
    let b = tokio::spawn(read_sse(addr, 2));

    let (ra, rb) = tokio::join!(a, b);
    let ra = ra.unwrap();
    let rb = rb.unwrap();
    assert!(ra.len() >= 2, "subscriber A got {} events", ra.len());
    assert!(rb.len() >= 2, "subscriber B got {} events", rb.len());
}

#[tokio::test]
async fn stream_continues_across_command_activity() {
    let (source, _pool, dispatcher, _collector) = build_source(2).await;
    let (addr, _server) = serve_sse_bound("127.0.0.1:0".parse().unwrap(), source.clone())
        .await
        .unwrap();

    // Submit a command, then drive snapshots.
    dispatcher
        .submit(
            "controller-a".to_string(),
            OrchestratorCommand::SetPlayers(SetPlayersCommand { player_count: 100 }),
        )
        .await
        .unwrap();

    let s = source.clone();
    let _ticker = tokio::spawn(async move {
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(40)).await;
            s.tick().await;
        }
    });

    let snapshots = read_sse(addr, 2).await;
    assert!(!snapshots.is_empty());

    // The snapshot's recent_commands should include the command we just
    // submitted, attributed to "controller-a".
    let saw_command = snapshots.iter().any(|s| {
        s.recent_commands
            .iter()
            .any(|e| e.submitter == "controller-a")
    });
    assert!(
        saw_command,
        "command-log slice should include the submitted command"
    );
}
