//! Orchestrator binary entry point.
//!
//! Brings up the full orchestrator process:
//!   - DriverServer (WebSocket on --driver-port, default 8088)
//!   - HTTP API (telemetry SSE + command submission, on --http-port,
//!     default 8090)
//!   - StatsCollector (polls each --cluster-stats-url every 2s)
//!   - TelemetrySource (rolls a snapshot every --telemetry-interval-ms,
//!     default 2000)
//!   - TelemetryArchive (writes JSON snapshots under --archive-dir;
//!     S3 upload deferred until aws-sdk dep lands)
//!
//! All ports bind on 0.0.0.0 so EC2 security groups (not the binary)
//! gate access.
//!
//! Usage:
//!   arcane-swarm-orchestrator \
//!     --driver-port 8088 \
//!     --http-port 8090 \
//!     --cluster-stats-url http://cluster1:8091/stats \
//!     --cluster-stats-url http://cluster2:8091/stats \
//!     --archive-dir /var/orchestrator/snapshots

use arcane_swarm_orchestrator::command_dispatcher::CommandDispatcher;
use arcane_swarm_orchestrator::driver_pool::DriverPool;
use arcane_swarm_orchestrator::server::DriverServer;
use arcane_swarm_orchestrator::sse_server::serve;
use arcane_swarm_orchestrator::stats_collector::{ClusterEndpoint, ClusterStats, StatsCollector};
use arcane_swarm_orchestrator::telemetry::TelemetrySource;
use arcane_swarm_orchestrator::telemetry_archive::{NoopUploader, TelemetryArchive};
use arcane_swarm_orchestrator::ws_driver_channel::WsDriverChannel;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct HttpClusterEndpoint {
    url: String,
    http: reqwest::Client,
}

impl HttpClusterEndpoint {
    fn new(url: String) -> Self {
        Self {
            url,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("reqwest client"),
        }
    }
}

impl ClusterEndpoint for HttpClusterEndpoint {
    fn url(&self) -> &str {
        &self.url
    }
    async fn fetch(&self) -> Result<ClusterStats, String> {
        let resp = self
            .http
            .get(&self.url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{}: {}", status, body));
        }
        // Parse a permissive subset of the cluster's /stats JSON. Unknown
        // fields are ignored so the orchestrator stays compatible with
        // future cluster-side schema additions.
        let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        Ok(ClusterStats {
            bytes_in: v["bytes_in"].as_u64().unwrap_or(0),
            bytes_out: v["bytes_out"].as_u64().unwrap_or(0),
            last_tick_us: v["last_tick_us"].as_u64().unwrap_or(0),
            broadcast_lagged_events: v["broadcast_lagged_events"].as_u64().unwrap_or(0),
            entities_current: v["entities_current"].as_u64().unwrap_or(0),
            sampled_at: Instant::now(),
        })
    }
}

#[derive(Default)]
struct Args {
    driver_port: u16,
    http_port: u16,
    cluster_stats_urls: Vec<String>,
    archive_dir: Option<String>,
    telemetry_interval_ms: u64,
    max_drivers: usize,
}

fn parse_args() -> Args {
    let mut a = Args {
        driver_port: 8088,
        http_port: 8090,
        cluster_stats_urls: Vec::new(),
        archive_dir: None,
        telemetry_interval_ms: 2_000,
        max_drivers: 64,
    };
    let argv: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < argv.len() {
        match argv[i].as_str() {
            "--driver-port" => {
                i += 1;
                a.driver_port = argv[i].parse().expect("--driver-port");
            }
            "--http-port" => {
                i += 1;
                a.http_port = argv[i].parse().expect("--http-port");
            }
            "--cluster-stats-url" => {
                i += 1;
                a.cluster_stats_urls.push(argv[i].clone());
            }
            "--archive-dir" => {
                i += 1;
                a.archive_dir = Some(argv[i].clone());
            }
            "--telemetry-interval-ms" => {
                i += 1;
                a.telemetry_interval_ms = argv[i].parse().expect("--telemetry-interval-ms");
            }
            "--max-drivers" => {
                i += 1;
                a.max_drivers = argv[i].parse().expect("--max-drivers");
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: arcane-swarm-orchestrator [--driver-port N] [--http-port N] \
                     [--cluster-stats-url URL]... [--archive-dir DIR] \
                     [--telemetry-interval-ms N] [--max-drivers N]"
                );
                std::process::exit(0);
            }
            _ => {}
        }
        i += 1;
    }
    a
}

#[tokio::main]
async fn main() {
    let args = parse_args();

    eprintln!(
        "arcane-swarm-orchestrator: driver_port={} http_port={} clusters={} archive_dir={:?} telemetry_interval_ms={}",
        args.driver_port,
        args.http_port,
        args.cluster_stats_urls.len(),
        args.archive_dir,
        args.telemetry_interval_ms,
    );

    let pool = Arc::new(DriverPool::new(
        Duration::from_secs(5),
        Duration::from_secs(15),
        args.max_drivers,
    ));
    let dispatcher = Arc::new(CommandDispatcher::<WsDriverChannel>::new(pool.clone()));

    let endpoints: Vec<Arc<HttpClusterEndpoint>> = args
        .cluster_stats_urls
        .into_iter()
        .map(|u| Arc::new(HttpClusterEndpoint::new(u)))
        .collect();
    let collector = Arc::new(StatsCollector::new(endpoints));

    let source = Arc::new(TelemetrySource::new(
        pool.clone(),
        dispatcher.clone(),
        collector.clone(),
    ));

    // Spawn the stats collector loop.
    {
        let collector = collector.clone();
        tokio::spawn(async move {
            let _ = collector.run().await;
        });
    }

    // Spawn the telemetry tick loop.
    {
        let source = source.clone();
        let interval = Duration::from_millis(args.telemetry_interval_ms);
        tokio::spawn(async move {
            source.run(interval).await;
        });
    }

    // Spawn the telemetry archive (local-only for now; S3 follow-up).
    if let Some(dir) = args.archive_dir.clone() {
        let source = source.clone();
        tokio::spawn(async move {
            let archive = TelemetryArchive::new(dir, Arc::new(NoopUploader));
            let rx = source.subscribe();
            let _ = archive.run(rx).await;
        });
    }

    // Spawn the HTTP API server (telemetry SSE + command submission).
    let http_addr: SocketAddr = format!("0.0.0.0:{}", args.http_port)
        .parse()
        .expect("http addr");
    {
        let source = source.clone();
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            let _ = serve(http_addr, source, Some(dispatcher)).await;
        });
    }
    eprintln!(
        "arcane-swarm-orchestrator: HTTP API listening on {}",
        http_addr
    );

    // Run the driver WS server in the foreground (blocks until shutdown).
    let driver_addr: SocketAddr = format!("0.0.0.0:{}", args.driver_port)
        .parse()
        .expect("driver addr");
    eprintln!(
        "arcane-swarm-orchestrator: driver WS listening on {}",
        driver_addr
    );
    let server = DriverServer::with_dispatcher(pool, dispatcher);

    tokio::select! {
        res = server.listen(driver_addr) => {
            if let Err(e) = res {
                eprintln!("driver server error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("arcane-swarm-orchestrator: shutting down on SIGINT");
        }
    }
}
