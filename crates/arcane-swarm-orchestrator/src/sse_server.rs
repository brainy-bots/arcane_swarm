//! Minimal HTTP/SSE server wrapping `TelemetrySource`.
//!
//! Single endpoint (`GET /telemetry/stream`) returning a long-lived
//! `text/event-stream` response. Each broadcast snapshot becomes one
//! `data: <json>\n\n` event. Multiple subscribers receive the same stream.
//!
//! Intentionally minimal — no routing framework, no TLS termination, no
//! request validation beyond a method+path sniff. The orchestrator runs
//! inside a private VPC; aggressive parsing is unnecessary and adds risk.

use crate::command_dispatcher::DriverChannel;
use crate::stats_collector::ClusterEndpoint;
use crate::telemetry::TelemetrySource;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const SSE_HEADERS: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
Access-Control-Allow-Origin: *\r\n\r\n";

const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Bind to `addr`, accept connections, dispatch each to `handle_connection`.
/// Never returns under normal operation; spawn from `tokio::spawn`.
pub async fn serve_sse<C, E>(
    addr: SocketAddr,
    source: Arc<TelemetrySource<C, E>>,
) -> std::io::Result<()>
where
    C: DriverChannel + 'static,
    E: ClusterEndpoint + 'static,
{
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let source = source.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, source).await;
        });
    }
}

/// Same as `serve_sse` but binds to a kernel-assigned port and returns the
/// bound address before entering the accept loop. Used by tests.
pub async fn serve_sse_bound<C, E>(
    addr: SocketAddr,
    source: Arc<TelemetrySource<C, E>>,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)>
where
    C: DriverChannel + 'static,
    E: ClusterEndpoint + 'static,
{
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let source = source.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, source).await;
            });
        }
    });
    Ok((local, handle))
}

async fn handle_connection<C, E>(
    mut stream: TcpStream,
    source: Arc<TelemetrySource<C, E>>,
) -> std::io::Result<()>
where
    C: DriverChannel + 'static,
    E: ClusterEndpoint + 'static,
{
    // Read until end of HTTP headers (`\r\n\r\n`). Cap at 8 KB to avoid
    // unbounded growth from a malformed client.
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Ok(());
        }
    }

    // Sniff request line: GET /telemetry/stream HTTP/1.1
    let head = std::str::from_utf8(&buf).unwrap_or("");
    let request_line = head.lines().next().unwrap_or("");
    if !request_line.starts_with("GET /telemetry/stream") {
        let _ = stream.write_all(NOT_FOUND).await;
        return Ok(());
    }

    stream.write_all(SSE_HEADERS).await?;

    let mut rx = source.subscribe();
    loop {
        match rx.recv().await {
            Ok(snapshot) => {
                let json = match serde_json::to_string(&snapshot) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut event = String::with_capacity(json.len() + 16);
                event.push_str("data: ");
                event.push_str(&json);
                event.push_str("\n\n");
                if stream.write_all(event.as_bytes()).await.is_err() {
                    // Subscriber gone.
                    return Ok(());
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}
