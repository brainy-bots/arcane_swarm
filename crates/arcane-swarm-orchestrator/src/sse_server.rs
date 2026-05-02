//! Minimal HTTP server: telemetry SSE + controller command submission.
//!
//! Two endpoints on a single port:
//! - `GET /telemetry/stream` — long-lived `text/event-stream`; one
//!   `data: <json>\n\n` event per `TelemetrySnapshot`. Multiple subscribers
//!   share the same broadcast.
//! - `POST /commands/submit` — JSON body `{"submitter": "...",
//!   "command": <OrchestratorCommand>}`; calls `dispatcher.submit` and
//!   responds with the `DispatchResult` as JSON. Optional — if no
//!   dispatcher was wired in, the endpoint returns `503 Service
//!   Unavailable`.
//!
//! Intentionally minimal — no routing framework, no TLS termination, no
//! request validation beyond a method+path sniff. The orchestrator runs
//! inside a private VPC; aggressive parsing is unnecessary and adds risk.

use crate::command_dispatcher::{CommandDispatcher, DispatchError, DispatchResult, DriverChannel};
use crate::protocol::{CommandAck, DriverId, OrchestratorCommand};
use crate::stats_collector::ClusterEndpoint;
use crate::telemetry::TelemetrySource;
use serde::{Deserialize, Serialize};
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

const SERVICE_UNAVAILABLE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
Content-Type: application/json\r\n\
Content-Length: 47\r\n\
Connection: close\r\n\r\n\
{\"error\":\"command dispatcher not configured\"}\n";

const BAD_REQUEST_PREFIX: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\
Content-Type: application/json\r\n\
Connection: close\r\n";

/// Wire body of a `POST /commands/submit` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitRequest {
    pub submitter: String,
    pub command: OrchestratorCommand,
}

/// Wire body of the response to `POST /commands/submit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub seq: u64,
    pub acks: Vec<CommandAck>,
    pub missing: Vec<DriverId>,
}

impl From<DispatchResult> for SubmitResponse {
    fn from(d: DispatchResult) -> Self {
        Self {
            seq: d.seq,
            acks: d.acks,
            missing: d.missing,
        }
    }
}

/// Bind to `addr`, accept connections, dispatch each. Never returns under
/// normal operation; spawn from `tokio::spawn`.
pub async fn serve<C, E>(
    addr: SocketAddr,
    source: Arc<TelemetrySource<C, E>>,
    dispatcher: Option<Arc<CommandDispatcher<C>>>,
) -> std::io::Result<()>
where
    C: DriverChannel + 'static,
    E: ClusterEndpoint + 'static,
{
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let source = source.clone();
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, source, dispatcher).await;
        });
    }
}

/// Bind variant that returns the bound address before accepting (test helper).
pub async fn serve_bound<C, E>(
    addr: SocketAddr,
    source: Arc<TelemetrySource<C, E>>,
    dispatcher: Option<Arc<CommandDispatcher<C>>>,
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
            let dispatcher = dispatcher.clone();
            tokio::spawn(async move {
                let _ = handle_connection(stream, source, dispatcher).await;
            });
        }
    });
    Ok((local, handle))
}

/// Backward-compatible aliases: the prior `serve_sse` / `serve_sse_bound`
/// functions kept their signatures so existing callers still compile.
pub async fn serve_sse<C, E>(
    addr: SocketAddr,
    source: Arc<TelemetrySource<C, E>>,
) -> std::io::Result<()>
where
    C: DriverChannel + 'static,
    E: ClusterEndpoint + 'static,
{
    serve(addr, source, None).await
}

pub async fn serve_sse_bound<C, E>(
    addr: SocketAddr,
    source: Arc<TelemetrySource<C, E>>,
) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)>
where
    C: DriverChannel + 'static,
    E: ClusterEndpoint + 'static,
{
    serve_bound(addr, source, None).await
}

async fn handle_connection<C, E>(
    mut stream: TcpStream,
    source: Arc<TelemetrySource<C, E>>,
    dispatcher: Option<Arc<CommandDispatcher<C>>>,
) -> std::io::Result<()>
where
    C: DriverChannel + 'static,
    E: ClusterEndpoint + 'static,
{
    // Read until end of HTTP headers (`\r\n\r\n`). Cap at 64 KB so a POST
    // body up to 64 KB also fits in the initial read; we don't try to
    // re-read for streaming bodies.
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let (header_end, content_length) = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(idx) = (0..buf.len().saturating_sub(3)).find(|&i| &buf[i..i + 4] == b"\r\n\r\n")
        {
            let cl = parse_content_length(&buf[..idx]);
            break (idx + 4, cl);
        }
        if buf.len() > 65536 {
            return Ok(());
        }
    };

    let head = std::str::from_utf8(&buf[..header_end]).unwrap_or("");
    let request_line = head.lines().next().unwrap_or("");

    if request_line.starts_with("GET /telemetry/stream") {
        return handle_sse(stream, source).await;
    }

    if request_line.starts_with("POST /commands/submit") {
        return handle_submit(stream, &mut buf, header_end, content_length, dispatcher).await;
    }

    let _ = stream.write_all(NOT_FOUND).await;
    Ok(())
}

fn parse_content_length(headers: &[u8]) -> usize {
    let s = std::str::from_utf8(headers).unwrap_or("");
    for line in s.lines() {
        let mut parts = line.splitn(2, ':');
        let k = parts.next().unwrap_or("").trim();
        let v = parts.next().unwrap_or("").trim();
        if k.eq_ignore_ascii_case("content-length") {
            return v.parse().unwrap_or(0);
        }
    }
    0
}

async fn handle_sse<C, E>(
    mut stream: TcpStream,
    source: Arc<TelemetrySource<C, E>>,
) -> std::io::Result<()>
where
    C: DriverChannel + 'static,
    E: ClusterEndpoint + 'static,
{
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
                    return Ok(());
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

async fn handle_submit<C: DriverChannel + 'static>(
    mut stream: TcpStream,
    buf: &mut Vec<u8>,
    header_end: usize,
    content_length: usize,
    dispatcher: Option<Arc<CommandDispatcher<C>>>,
) -> std::io::Result<()> {
    let dispatcher = match dispatcher {
        Some(d) => d,
        None => {
            let _ = stream.write_all(SERVICE_UNAVAILABLE).await;
            return Ok(());
        }
    };

    // Make sure we've read the entire body.
    let body_already = buf.len() - header_end;
    if body_already < content_length {
        let mut more = vec![0u8; content_length - body_already];
        let mut read = 0;
        while read < more.len() {
            let n = stream.read(&mut more[read..]).await?;
            if n == 0 {
                break;
            }
            read += n;
        }
        buf.extend_from_slice(&more[..read]);
    }
    let body = &buf[header_end..header_end + content_length.min(buf.len() - header_end)];

    let req: SubmitRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            let body = serde_json::json!({"error": format!("invalid request body: {}", e)});
            return write_json(stream, BAD_REQUEST_PREFIX, &body).await;
        }
    };

    match dispatcher.submit(req.submitter, req.command).await {
        Ok(result) => {
            let resp: SubmitResponse = result.into();
            let body = serde_json::to_value(&resp).unwrap_or_else(|_| serde_json::json!({}));
            write_json(stream, ok_prefix(), &body).await
        }
        Err(DispatchError::NoActiveDrivers) => {
            let body = serde_json::json!({"error": "no_active_drivers"});
            write_json(stream, BAD_REQUEST_PREFIX, &body).await
        }
    }
}

fn ok_prefix() -> &'static [u8] {
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n"
}

async fn write_json(
    mut stream: TcpStream,
    prefix: &[u8],
    body: &serde_json::Value,
) -> std::io::Result<()> {
    let body_bytes = serde_json::to_vec(body).unwrap_or_default();
    let header = format!(
        "Content-Length: {}\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        body_bytes.len()
    );
    stream.write_all(prefix).await?;
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body_bytes).await?;
    Ok(())
}
