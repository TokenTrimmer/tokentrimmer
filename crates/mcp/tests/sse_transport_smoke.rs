//! Integration smoke test for the SSE transport.
//!
//! Spins up the Axum SSE server on 127.0.0.1:0 (ephemeral port), opens
//! GET /sse, asserts the first event is `endpoint` and captures the sessionId,
//! then POSTs a `tools/list` request and receives the response via the SSE
//! stream.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::task::JoinHandle;
use tt_mcp::Server;

const TOKEN: &str = "tt_test_smoke_token";

// ---------------------------------------------------------------------------
// Helper: spawn the SSE server in a background task on an ephemeral port.
// Returns (bound_addr, task_handle).  Cancel by aborting the handle.
// ---------------------------------------------------------------------------

async fn spawn_sse_server() -> (SocketAddr, JoinHandle<()>) {
    let server = Server::new(); // no tools registered — tools/list returns []

    // Bind on port 0 to get an OS-assigned ephemeral port, then pass the addr
    // to `run`.  We need to know the port before the server starts, so we
    // bind a listener first and pass its local addr.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener); // release so run() can bind it — tiny TOCTOU window is acceptable in tests

    let handle = tokio::spawn(async move {
        server
            .run_sse(addr, TOKEN.to_string())
            .await
            .expect("SSE server exited with error");
    });

    // Brief yield so the server has time to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, handle)
}

// ---------------------------------------------------------------------------
// Parse a raw SSE chunk into (event_name, data) pairs.
// SSE format:  "event: <name>\ndata: <value>\n\n"
// ---------------------------------------------------------------------------

fn parse_sse_event(chunk: &str) -> Vec<(String, String)> {
    let mut events = Vec::new();
    // Split on double-newline to get individual event blocks
    for block in chunk.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let mut event_name = String::new();
        let mut data_value = String::new();
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event_name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_value = rest.trim().to_string();
            }
        }
        if !event_name.is_empty() {
            events.push((event_name, data_value));
        }
    }
    events
}

// ---------------------------------------------------------------------------
// The actual test
// ---------------------------------------------------------------------------

#[tokio::test]
// `session_id`/`messages_path`/`rpc_resp` are declared before their `select!`
// loops and only assigned on the break arm; the initial `None` is structurally
// required for the pre-loop declaration but never read — allow that here.
#[allow(unused_assignments)]
async fn sse_transport_tools_list_round_trip() {
    let (addr, server_handle) = spawn_sse_server().await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client");

    let base = format!("http://{addr}");

    // -----------------------------------------------------------------------
    // 1. Open SSE stream.  We'll read just enough bytes to get the endpoint
    //    event + the response event, then cancel the server task.
    // -----------------------------------------------------------------------
    let mut sse_stream = client
        .get(format!("{base}/sse"))
        .header("Accept", "text/event-stream")
        .header("Authorization", format!("Bearer {TOKEN}"))
        .send()
        .await
        .expect("GET /sse failed")
        .bytes_stream();

    // -----------------------------------------------------------------------
    // 2. Read until we get the `endpoint` event.
    // -----------------------------------------------------------------------
    use futures::StreamExt;

    let mut accumulated = String::new();
    let mut session_id: Option<String> = None;
    let mut messages_path: Option<String> = None;

    // Read chunks until we find the endpoint event.
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                panic!("timed out waiting for endpoint event");
            }
            chunk = sse_stream.next() => {
                match chunk {
                    None => panic!("SSE stream ended before endpoint event"),
                    Some(Err(e)) => panic!("SSE stream error: {e}"),
                    Some(Ok(bytes)) => {
                        accumulated.push_str(&String::from_utf8_lossy(&bytes));
                        let events = parse_sse_event(&accumulated);
                        if let Some((name, data)) = events.first() {
                            assert_eq!(name, "endpoint", "first SSE event must be 'endpoint'");
                            // data is e.g. "/messages?sessionId=<uuid>"
                            assert!(data.starts_with("/messages?sessionId="), "data = {data}");
                            // Extract sessionId
                            let sid = data
                                .split("sessionId=")
                                .nth(1)
                                .expect("sessionId present")
                                .to_string();
                            session_id = Some(sid);
                            messages_path = Some(data.clone());
                            break;
                        }
                    }
                }
            }
        }
    }

    let session_id = session_id.expect("session_id captured");
    let messages_path = messages_path.expect("messages_path captured");

    // Validate it's a valid UUID
    let _: uuid::Uuid = session_id.parse().expect("sessionId must be a valid UUID");

    // -----------------------------------------------------------------------
    // 3. POST a tools/list request.
    // -----------------------------------------------------------------------
    let post_url = format!("{base}{messages_path}");
    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {},
        "id": 1
    });

    let post_resp = client
        .post(&post_url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .json(&rpc_body)
        .send()
        .await
        .expect("POST /messages failed");

    assert_eq!(
        post_resp.status(),
        202,
        "POST /messages must return 202 Accepted"
    );

    // -----------------------------------------------------------------------
    // 4. Read the `message` event from the SSE stream.
    // -----------------------------------------------------------------------
    accumulated.clear(); // start fresh for the message event
    let mut rpc_resp: Option<serde_json::Value> = None;

    let deadline2 = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline2);

    loop {
        tokio::select! {
            _ = &mut deadline2 => {
                panic!("timed out waiting for message event");
            }
            chunk = sse_stream.next() => {
                match chunk {
                    None => panic!("SSE stream ended before message event"),
                    Some(Err(e)) => panic!("SSE stream error: {e}"),
                    Some(Ok(bytes)) => {
                        accumulated.push_str(&String::from_utf8_lossy(&bytes));
                        let events = parse_sse_event(&accumulated);
                        if let Some((name, data)) = events.iter().find(|(n, _)| n == "message") {
                            assert_eq!(name, "message");
                            let v: serde_json::Value =
                                serde_json::from_str(data).expect("message data is valid JSON");
                            rpc_resp = Some(v);
                            break;
                        }
                    }
                }
            }
        }
    }

    let rpc_resp = rpc_resp.expect("received message event");

    // -----------------------------------------------------------------------
    // 5. Assert the response contains an empty tools array.
    // -----------------------------------------------------------------------
    assert_eq!(rpc_resp["jsonrpc"], "2.0");
    assert_eq!(rpc_resp["id"], 1);
    let tools = rpc_resp["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.is_empty(),
        "no tools registered, expected empty array"
    );

    // -----------------------------------------------------------------------
    // 6. Cancel the server task and wait for it to stop.
    // -----------------------------------------------------------------------
    server_handle.abort();
    let _ = server_handle.await; // ignore JoinError from abort
}

#[tokio::test]
async fn post_unknown_session_returns_404() {
    let (addr, server_handle) = spawn_sse_server().await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");

    let unknown_id = uuid::Uuid::new_v4();
    let url = format!("http://{addr}/messages?sessionId={unknown_id}");
    let rpc_body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {},
        "id": 99
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .json(&rpc_body)
        .send()
        .await
        .expect("POST failed");

    assert_eq!(resp.status(), 404, "unknown sessionId must return 404");

    server_handle.abort();
    let _ = server_handle.await;
}

#[tokio::test]
async fn sse_without_bearer_is_401() {
    let (addr, server_handle) = spawn_sse_server().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://{addr}/sse"))
        .header("Accept", "text/event-stream")
        .send()
        .await
        .expect("GET /sse");
    assert_eq!(resp.status(), 401, "no bearer → 401");
    server_handle.abort();
    let _ = server_handle.await;
}

#[tokio::test]
async fn messages_with_wrong_bearer_is_401() {
    let (addr, server_handle) = spawn_sse_server().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let url = format!("http://{addr}/messages?sessionId={}", uuid::Uuid::new_v4());
    let resp = client
        .post(&url)
        .header("Authorization", "Bearer wrong")
        .json(&serde_json::json!({"jsonrpc":"2.0","method":"tools/list","params":{},"id":1}))
        .send()
        .await
        .expect("POST /messages");
    assert_eq!(resp.status(), 401, "wrong bearer → 401");
    server_handle.abort();
    let _ = server_handle.await;
}

#[tokio::test]
async fn non_local_host_is_403() {
    let (addr, server_handle) = spawn_sse_server().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let resp = client
        .get(format!("http://{addr}/sse"))
        .header("Accept", "text/event-stream")
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("host", "evil.example.com")
        .send()
        .await
        .expect("GET /sse");
    assert_eq!(resp.status(), 403, "non-local Host → 403");
    server_handle.abort();
    let _ = server_handle.await;
}

#[tokio::test]
async fn oversized_body_is_413() {
    let (addr, server_handle) = spawn_sse_server().await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let url = format!("http://{addr}/messages?sessionId={}", uuid::Uuid::new_v4());
    let big = "A".repeat(2 * 1024 * 1024);
    let body =
        serde_json::json!({"jsonrpc":"2.0","method":"tools/list","params":{"pad": big},"id":1});
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {TOKEN}"))
        .json(&body)
        .send()
        .await
        .expect("POST /messages");
    assert_eq!(resp.status(), 413, "oversized body → 413");
    server_handle.abort();
    let _ = server_handle.await;
}
