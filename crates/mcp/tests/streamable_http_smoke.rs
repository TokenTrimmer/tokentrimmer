//! Integration smoke tests for the Streamable HTTP transport (MCP 2025-03-26).
//!
//! The Streamable HTTP transport exposes a SINGLE MCP endpoint (`/mcp`) that
//! supports POST (client→server JSON-RPC), GET (server→client SSE stream), and
//! DELETE (session termination). These tests exercise the wire shape against
//! the spec:
//!   - POST `initialize` returns the InitializeResult plus an `Mcp-Session-Id`
//!     response header.
//!   - A subsequent POST `tools/call` over the same endpoint, carrying the
//!     session id, returns the tool result.
//!   - A POST with `Accept: text/event-stream` upgrades the response to an SSE
//!     stream carrying the JSON-RPC response as a single `message` event.
//!   - Session-id handling: missing id on a non-initialize request → 400; an
//!     unknown/expired id → 404; DELETE terminates the session.
//!   - A JSON-RPC notification (no `id`) → 202 Accepted with no body.
//!
//! Hermetic: spins the server on 127.0.0.1:0, no network, no upstreams.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::task::JoinHandle;
use tt_mcp::tools::find_route_for::FindRouteForTool;
use tt_mcp::Server;

const TOKEN: &str = "tt_test_streamable_token";

// ---------------------------------------------------------------------------
// Spawn the Streamable HTTP server on an ephemeral port with one tool
// registered (find_route_for, which is hermetic — pure routing logic).
// ---------------------------------------------------------------------------

async fn spawn_http_server() -> (SocketAddr, JoinHandle<()>) {
    let mut server = Server::new();
    server.tools.register(Box::new(FindRouteForTool));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener); // release so run() can re-bind

    let handle = tokio::spawn(async move {
        server
            .run_http(addr, TOKEN.to_string())
            .await
            .expect("Streamable HTTP server exited with error");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, handle)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

fn init_body() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" }
        },
        "id": 1
    })
}

// ---------------------------------------------------------------------------
// initialize → JSON response carrying Mcp-Session-Id header; a tool call over
// the same endpoint with that session id returns the tool result.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn initialize_then_tool_call_single_endpoint() {
    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let url = format!("http://{addr}/mcp");

    // 1. initialize over POST → application/json + Mcp-Session-Id header.
    let resp = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json, text/event-stream")
        .json(&init_body())
        .send()
        .await
        .expect("POST initialize");

    assert_eq!(resp.status(), 200, "initialize must return 200");
    let session_id = resp
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .expect("initialize response must carry Mcp-Session-Id");
    assert!(!session_id.is_empty(), "session id must be non-empty");
    // session id must be visible ASCII (0x21..=0x7E)
    assert!(
        session_id.bytes().all(|b| (0x21..=0x7e).contains(&b)),
        "session id must be visible ASCII"
    );

    let ct = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("application/json"),
        "initialize without SSE Accept preference returns JSON, got {ct}"
    );
    let init_result: serde_json::Value = resp.json().await.expect("initialize json");
    assert_eq!(init_result["jsonrpc"], "2.0");
    assert_eq!(init_result["id"], 1);
    assert_eq!(init_result["result"]["serverInfo"]["name"], "tt-mcp");

    // 2. tools/call over the same endpoint, carrying the session id.
    let call_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "find_route_for",
            "arguments": { "task_description": "classify this email as spam" }
        },
        "id": 2
    });
    let resp = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .header("Mcp-Session-Id", &session_id)
        .json(&call_body)
        .send()
        .await
        .expect("POST tools/call");
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.expect("tools/call json");
    assert_eq!(v["id"], 2);
    assert_eq!(v["result"]["model"], "claude-haiku-4-5");

    handle.abort();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// SSE upgrade: a POST request whose Accept prefers text/event-stream gets the
// JSON-RPC response streamed as a single SSE `message` event.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_with_sse_accept_streams_response() {
    use futures::StreamExt;

    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let url = format!("http://{addr}/mcp");

    // initialize to get a session id (use JSON for simplicity).
    let init = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .json(&init_body())
        .send()
        .await
        .expect("POST initialize");
    let session_id = init
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();

    // tools/list with Accept: text/event-stream → SSE stream.
    let resp = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 7
        }))
        .send()
        .await
        .expect("POST tools/list (SSE)");

    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "Accept: text/event-stream must upgrade to SSE, got {ct}"
    );

    // Read the stream until the JSON-RPC response arrives as an SSE event.
    let mut stream = resp.bytes_stream();
    let mut acc = String::new();
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);
    let rpc: serde_json::Value = loop {
        tokio::select! {
            _ = &mut deadline => panic!("timed out waiting for SSE response event"),
            chunk = stream.next() => match chunk {
                None => panic!("SSE stream ended before response"),
                Some(Err(e)) => panic!("SSE error: {e}"),
                Some(Ok(bytes)) => {
                    acc.push_str(&String::from_utf8_lossy(&bytes));
                    if let Some(data) = sse_data_line(&acc) {
                        break serde_json::from_str(&data).expect("SSE data is JSON");
                    }
                }
            }
        }
    };
    assert_eq!(rpc["id"], 7);
    let tools = rpc["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1, "find_route_for registered");
    assert_eq!(tools[0]["name"], "find_route_for");

    handle.abort();
    let _ = handle.await;
}

/// Extract the first complete `data:` payload from accumulated SSE text.
fn sse_data_line(acc: &str) -> Option<String> {
    for block in acc.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        // an event block is complete only if it ended with the blank-line
        // delimiter, i.e. it is not the trailing partial block.
        if !acc.contains(&format!("{block}\n\n")) {
            continue;
        }
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// GET opens a server→client SSE stream (text/event-stream).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_opens_sse_stream() {
    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let url = format!("http://{addr}/mcp");

    let init = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .json(&init_body())
        .send()
        .await
        .expect("POST initialize");
    let session_id = init
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();

    let resp = c
        .get(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .send()
        .await
        .expect("GET /mcp");
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "GET must open an SSE stream, got {ct}"
    );

    handle.abort();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// Session lifecycle: missing id on a non-initialize request → 400; unknown id
// → 404; DELETE terminates so the id is afterward 404.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_session_id_on_non_initialize_is_400() {
    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let url = format!("http://{addr}/mcp");

    let resp = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 3
        }))
        .send()
        .await
        .expect("POST without session id");
    assert_eq!(
        resp.status(),
        400,
        "non-initialize request without Mcp-Session-Id must be 400"
    );

    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn unknown_session_id_is_404() {
    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let url = format!("http://{addr}/mcp");

    let resp = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .header("Mcp-Session-Id", "00000000-0000-0000-0000-000000000000")
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 4
        }))
        .send()
        .await
        .expect("POST with unknown session id");
    assert_eq!(resp.status(), 404, "unknown session id must be 404");

    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn delete_terminates_session() {
    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let url = format!("http://{addr}/mcp");

    let init = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .json(&init_body())
        .send()
        .await
        .expect("POST initialize");
    let session_id = init
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();

    // DELETE terminates the session.
    let del = c
        .delete(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Mcp-Session-Id", &session_id)
        .send()
        .await
        .expect("DELETE /mcp");
    assert!(
        del.status().is_success(),
        "DELETE of a live session should succeed, got {}",
        del.status()
    );

    // The same id is now unknown → 404.
    let resp = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .header("Mcp-Session-Id", &session_id)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "method": "tools/list", "params": {}, "id": 5
        }))
        .send()
        .await
        .expect("POST after DELETE");
    assert_eq!(resp.status(), 404, "terminated session id must be 404");

    handle.abort();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// A JSON-RPC notification (no `id`) → 202 Accepted, no body.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notification_returns_202() {
    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let url = format!("http://{addr}/mcp");

    let init = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .json(&init_body())
        .send()
        .await
        .expect("POST initialize");
    let session_id = init
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();

    // `notifications/initialized` has no id → notification.
    let resp = c
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json, text/event-stream")
        .header("Mcp-Session-Id", &session_id)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }))
        .send()
        .await
        .expect("POST notification");
    assert_eq!(resp.status(), 202, "notification must return 202 Accepted");
    let body = resp.bytes().await.expect("body");
    assert!(
        body.is_empty(),
        "202 notification response must have no body"
    );

    handle.abort();
    let _ = handle.await;
}

// ---------------------------------------------------------------------------
// Auth + loopback guard still apply on the single endpoint.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_without_bearer_is_401() {
    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let resp = c
        .post(format!("http://{addr}/mcp"))
        .header("Accept", "application/json")
        .json(&init_body())
        .send()
        .await
        .expect("POST without bearer");
    assert_eq!(resp.status(), 401, "no bearer → 401");
    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn non_local_host_is_403() {
    let (addr, handle) = spawn_http_server().await;
    let c = client();
    let resp = c
        .post(format!("http://{addr}/mcp"))
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Accept", "application/json")
        .header("host", "evil.example.com")
        .json(&init_body())
        .send()
        .await
        .expect("POST non-local host");
    assert_eq!(resp.status(), 403, "non-local Host → 403");
    handle.abort();
    let _ = handle.await;
}
