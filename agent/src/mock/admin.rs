//! Plaintext HTTP admin surface of the mock control plane.
//!
//! The gRPC service only speaks the agent-facing contract: publication is a
//! control-plane-side action with no RPC, so a process outside the mock cannot
//! make it publish. This module closes that gap for humans and for
//! `verification/run_control_demo.sh`: a loopback-only, unauthenticated HTTP/1.1
//! listener with four endpoints.
//!
//! * `GET /health` — is it up, is `Watch` supported, what is published;
//! * `POST /publish` — `{"version": N, "document_hex": "..."}`, answers with the
//!   control-plane timestamp of the publish;
//! * `GET /acks` — every ack with its receive time, `applied_at`, degraded
//!   features and whether a `Watch` push preceded it;
//! * `GET /state` — cursor, call counts, watch subscriptions and the window ids
//!   of every `ReportWindows` (what makes "resumed at cursor + 1" checkable).
//!
//! **Test scaffolding, not an API.** No authentication, no limits, no TLS; it
//! binds whatever the caller asked for, and the contract's real control plane
//! has no such surface at all.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use serde_json::{Value, json};

use super::{AckRecord, MockShared, to_hex};
use crate::client::Rpc;

/// Where the admin listener ended up. Keeping the handle alive is not required:
/// the thread runs until the process exits (which is what the mock does too).
pub(super) struct AdminHandle {
    /// The address actually bound; port 0 is resolved here.
    pub addr: SocketAddr,
}

/// Starts the admin listener on `listen` and answers requests on its own thread.
pub(super) fn spawn(shared: MockShared, listen: SocketAddr) -> anyhow::Result<AdminHandle> {
    let listener = TcpListener::bind(listen)?;
    let addr = listener.local_addr()?;
    std::thread::Builder::new()
        .name("mock-admin".to_owned())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(error) = handle(stream, &shared) {
                            log::warn!("mock admin request failed: {error:#}");
                        }
                    }
                    Err(error) => log::warn!("mock admin accept failed: {error}"),
                }
            }
        })?;
    Ok(AdminHandle { addr })
}

/// One parsed request: enough of HTTP/1.1 to route four endpoints.
struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

/// Serves one connection: read, route, answer, close.
fn handle(mut stream: TcpStream, shared: &MockShared) -> anyhow::Result<()> {
    let request = read_request(&mut stream)?;
    let (status, body) = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => (200, health(shared)),
        ("GET", "/acks") => (200, acks(shared)),
        ("GET", "/state") => (200, state(shared)),
        ("POST", "/publish") => publish(shared, &request.body),
        ("GET", other) | ("POST", other) => {
            (404, json!({"error": format!("no such endpoint: {other}")}))
        }
        (method, _) => (
            405,
            json!({"error": format!("method {method} is not supported")}),
        ),
    };
    respond(&mut stream, status, &body)
}

fn health(shared: &MockShared) -> Value {
    json!({
        "ok": true,
        "watch_supported": shared.config().watch_supported,
        "announced_version": shared.announced_version(),
        "acks": shared.acks().len(),
    })
}

fn acks(shared: &MockShared) -> Value {
    let records: Vec<Value> = shared.ack_records().iter().map(ack_json).collect();
    json!({
        "acks": records,
        "watch_pushes": shared.watch_pushes(),
    })
}

fn state(shared: &MockShared) -> Value {
    let calls = {
        let state = shared.lock();
        Value::Object(
            Rpc::ALL
                .iter()
                .map(|rpc| (rpc.name().to_owned(), json!(state.calls[rpc.index()])))
                .collect(),
        )
    };
    let reports: Vec<Value> = shared
        .report_records()
        .iter()
        .map(|(received_at_ms, window_ids)| {
            json!({"received_at_ms": received_at_ms, "window_ids": window_ids})
        })
        .collect();
    json!({
        "announced_version": shared.announced_version(),
        "cursor": shared.cursor(),
        "watch_subscriptions": shared.watch_subscriptions(),
        "watch_pushes": shared.watch_pushes(),
        "heartbeats": shared.heartbeats().len(),
        "acks": shared.acks().len(),
        "calls": calls,
        "reports": reports,
    })
}

/// `POST /publish`: the one mutating endpoint. The document is hex so the bytes
/// that get served (and hashed) are exactly the bytes the caller encoded — the
/// contract's "hash the received bytes, never re-encode" rule is what the mock
/// must not violate either.
fn publish(shared: &MockShared, body: &[u8]) -> (u16, Value) {
    let request: Value = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return (400, json!({"error": format!("body is not JSON: {error}")})),
    };
    let Some(version) = request["version"].as_u64() else {
        return (400, json!({"error": "missing or non-numeric `version`"}));
    };
    let Some(hex) = request["document_hex"].as_str() else {
        return (
            400,
            json!({"error": "missing or non-string `document_hex`"}),
        );
    };
    match shared.publish_hex(version, hex) {
        Ok(outcome) => (
            200,
            json!({
                "version": outcome.version,
                "published_at_ms": outcome.published_at_ms,
                "sha256": to_hex(&outcome.sha256),
                "watchers": outcome.watchers,
            }),
        ),
        Err(error) => (400, json!({"error": error})),
    }
}

fn ack_json(record: &AckRecord) -> Value {
    json!({
        "version": record.version,
        "result": result_name(record.result),
        "result_code": record.result,
        "apply_ms": record.apply_ms,
        "degraded_features": record.degraded_features,
        "error_code": record.error_code,
        "content_sha256": record.content_sha256_hex,
        "applied_at_ms": record.applied_at_ms,
        "received_at_ms": record.received_at_ms,
        "watch_used": record.after_watch_push,
    })
}

fn result_name(result: i32) -> &'static str {
    match result {
        1 => "APPLIED",
        2 => "REJECTED",
        3 => "FAILED",
        _ => "UNSPECIFIED",
    }
}

/// Reads one HTTP/1.1 request; `Content-Length` is honoured, everything else is
/// ignored. A request that never ends is cut at 64 KiB of headers.
fn read_request(stream: &mut TcpStream) -> anyhow::Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("connection closed before the request headers ended");
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 64 * 1024 {
            anyhow::bail!("request headers exceed 64 KiB");
        }
    };

    let head = std::str::from_utf8(&buffer[..header_end])?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let mut path = parts.next().unwrap_or_default().to_owned();
    if let Some(position) = path.find('?') {
        path.truncate(position);
    }

    let mut content_length = 0_usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(Request { method, path, body })
}

fn respond(stream: &mut TcpStream, status: u16, body: &Value) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        404 => "Not Found",
        _ => "Error",
    };
    let text = serde_json::to_vec(body)?;
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        text.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&text)?;
    stream.flush()?;
    Ok(())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
