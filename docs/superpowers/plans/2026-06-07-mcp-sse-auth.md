# MCP SSE transport auth + hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Authenticate the MCP SSE transport (bearer `TT_API_KEY` on both routes), validate Origin/Host against DNS-rebinding, and cap the request body — closing the "any local process can drive every tool" + "no body limit" findings.

**Architecture:** `transport::sse::run` gains an `auth_token` param and applies an axum `from_fn_with_state` guard layer (constant-time bearer + loopback Origin/Host) plus `DefaultBodyLimit`. `Server::run_sse` threads the token; the CLI passes its already-validated `TT_API_KEY`.

**Tech Stack:** Rust (`crates/mcp` = `tt-mcp`, `crates/cli`), axum 0.7, tokio, reqwest (dev).

Spec: `docs/superpowers/specs/2026-06-07-mcp-sse-auth-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`.** `run_sse`/`sse::run` are PUBLIC signature changes → workspace-ripple risk (lesson [[ci-verify-all-targets]]): before pushing, `grep -rn "run_sse\|sse::run" crates` (known callers: `cli/src/main.rs`, `crates/mcp/tests/sse_transport_smoke.rs`) and run `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace --no-run`.
>
> **TDD note:** under `-D warnings` an `auth_token` param can't be added unused, so the signature change + guard land together (Step 1–3); the guard's negative tests (Step 5) are therefore authored alongside the impl, not strictly failing-first. They still deterministically verify the guard (401/403/413). Stated, not hidden.

---

### Task 1: SSE bearer auth + Origin/Host + body limit

**Files:**
- Modify: `crates/mcp/src/transport/sse.rs` (guard layer + helpers + body limit + `run` signature)
- Modify: `crates/mcp/src/lib.rs` (`run_sse` signature)
- Modify: `crates/cli/src/main.rs` (pass `api_key`)
- Modify: `crates/mcp/tests/sse_transport_smoke.rs` (token + bearer; new negative tests)
- Modify: docs (MCP SSE usage section)

- [ ] **Step 1: Add the guard + helpers + body limit to `sse.rs`**

In `crates/mcp/src/transport/sse.rs`, change `run` to take the token and apply the layers. Replace the `run` function's router/serve construction:
```rust
/// 1 MiB cap on POST /messages bodies (GET /sse has none).
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Boot the SSE MCP server on `addr`. Requires `Authorization: Bearer <auth_token>`
/// and a loopback Host/Origin on every request. Runs until SIGINT/SIGTERM.
pub async fn run(server: Server, addr: SocketAddr, auth_token: String) -> Result<(), McpError> {
    let state = AppState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        server: Arc::new(server),
    };

    let app = axum::Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .with_state(state)
        // Inner: cap request bodies.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        // Outer (runs first): bearer + loopback Origin/Host guard.
        .layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::<str>::from(auth_token.as_str()),
            guard,
        ));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| McpError::Internal(format!("bind {addr}: {e}")))?;

    tracing::info!(addr = %addr, "MCP SSE server listening (bearer-auth, loopback-only)");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| McpError::Internal(format!("SSE server: {e}")))?;

    Ok(())
}
```

Add the guard + helpers (place them after `run`, before `sse_handler`):
```rust
/// Reject any request lacking a valid bearer token or arriving with a non-loopback
/// Host/Origin (DNS-rebind defense). Runs before body read + dispatch.
async fn guard(
    axum::extract::State(token): axum::extract::State<std::sync::Arc<str>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    let headers = req.headers();

    if !host_is_local(headers.get(header::HOST)) {
        return (StatusCode::FORBIDDEN, "non-local Host").into_response();
    }
    if !origin_is_local_or_absent(headers.get(header::ORIGIN)) {
        return (StatusCode::FORBIDDEN, "cross-origin").into_response();
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if !presented.is_some_and(|p| ct_eq(p.as_bytes(), token.as_bytes())) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    next.run(req).await
}

/// Constant-time byte compare (length mismatch returns early — token length is
/// not sensitive). Mirrors the cloud `constant_time_eq`.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Host header host-part is loopback (ignoring port). Missing Host → false.
fn host_is_local(h: Option<&axum::http::HeaderValue>) -> bool {
    h.and_then(|v| v.to_str().ok())
        .is_some_and(is_local_authority)
}

/// Origin absent (non-browser MCP clients) or its host is loopback.
fn origin_is_local_or_absent(h: Option<&axum::http::HeaderValue>) -> bool {
    match h.and_then(|v| v.to_str().ok()) {
        None => true,
        Some("null") => true,
        Some(s) => s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("https://"))
            .is_some_and(is_local_authority),
    }
}

/// Host-part (strip a trailing `:port`) is 127.0.0.1 / localhost / ::1.
fn is_local_authority(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("") // IPv6 "[::1]:port" → "::1"
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}
```
Ensure `use axum::response::IntoResponse;` is in scope (it already is — `messages_handler` uses it).

- [ ] **Step 2: Thread the token through `run_sse` (lib.rs)**

In `crates/mcp/src/lib.rs`, change `run_sse`:
```rust
    pub async fn run_sse(
        self,
        addr: std::net::SocketAddr,
        auth_token: String,
    ) -> Result<(), McpError> {
        crate::transport::sse::run(self, addr, auth_token).await
    }
```

- [ ] **Step 3: Pass the validated key from the CLI (main.rs)**

In `crates/cli/src/main.rs`, the `"sse"` transport arm calls `server.run_sse(addr)` (~line 514). Change it to pass the `api_key` validated at ~line 461:
```rust
                        .block_on(server.run_sse(addr, api_key))?;
```
(`api_key: String` is in scope; this is its last use, so a move is fine. If a borrow-check error arises because `api_key` is used earlier in the arm, clone it: `server.run_sse(addr, api_key.clone())`.)

- [ ] **Step 4: Update the existing smoke tests (token + bearer)**

In `crates/mcp/tests/sse_transport_smoke.rs`:

(a) Add a module-level token const (top of file, after imports):
```rust
const TOKEN: &str = "tt_test_smoke_token";
```
(b) In `spawn_sse_server`, change the `run_sse` call:
```rust
        server
            .run_sse(addr, TOKEN.to_string())
            .await
            .expect("SSE server exited with error");
```
(c) In `sse_transport_tools_list_round_trip`, add the bearer header to BOTH requests:
- the `client.get(format!("{base}/sse"))` builder: add `.header("Authorization", format!("Bearer {TOKEN}"))`
- the `client.post(&post_url)` builder: add `.header("Authorization", format!("Bearer {TOKEN}"))`
(d) In `post_unknown_session_returns_404`, add `.header("Authorization", format!("Bearer {TOKEN}"))` to the `client.post(&url)` builder (so it passes auth and reaches the 404 session check).

- [ ] **Step 5: Add the negative-path tests**

Append to `crates/mcp/tests/sse_transport_smoke.rs`:
```rust
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
    // Valid bearer but a spoofed (non-loopback) Host → DNS-rebind defense fires.
    let resp = client
        .get(format!("http://{addr}/sse"))
        .header("Accept", "text/event-stream")
        .header("Authorization", format!("Bearer {TOKEN}"))
        .header("Host", "evil.example.com")
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
    // Valid bearer + well-formed sessionId in the URL; body > 1 MiB → 413 at
    // extraction (before the handler reads the session).
    let url = format!("http://{addr}/messages?sessionId={}", uuid::Uuid::new_v4());
    let big = "A".repeat(2 * 1024 * 1024);
    let body = serde_json::json!({
        "jsonrpc":"2.0","method":"tools/list","params":{"pad": big},"id":1
    });
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
```

- [ ] **Step 6: Run the mcp tests**

Run: `cargo test -p tt-mcp 2>&1 | tail -25`
Expected: PASS — `sse_transport_tools_list_round_trip`, `post_unknown_session_returns_404`, `sse_without_bearer_is_401`, `messages_with_wrong_bearer_is_401`, `non_local_host_is_403`, `oversized_body_is_413`, plus the auth/dispatcher unit tests.
(If `non_local_host_is_403` fails because reqwest refused to send a custom `Host`, the implementer should construct the request with a low-level header insert that reqwest honors — reqwest does allow an explicit `Host` header override via `.header("host", …)`; confirm it reaches the server.)

- [ ] **Step 7: Workspace-ripple gates + fmt + clippy**

Run: `grep -rn "run_sse\|sse::run" crates` → confirm only `cli/src/main.rs`, `crates/mcp/src/lib.rs`, `crates/mcp/src/transport/sse.rs`, `crates/mcp/tests/sse_transport_smoke.rs` reference them (no other caller needs updating).
Run: `cargo test --workspace --no-run 2>&1 | grep -E "^error|error\[" | head` → empty (everything compiles, incl. all test targets).
Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E "^error|error\[|warning:" | grep -v auto-clean | head` → empty.
Run: `cargo fmt --check -p tt-mcp -p tt-cli 2>&1 | tail -5` → no diff (if drift: `cargo fmt -p tt-mcp -p tt-cli`, re-check).

- [ ] **Step 8: Docs**

Find the MCP SSE usage docs: `grep -rln "sse" docs | head` (likely `docs/tt-mcp-usage.md`). In the SSE-transport section, add a short note: the SSE transport now requires `Authorization: Bearer $TT_API_KEY` on every request, binds localhost-only, validates Origin/Host (DNS-rebind defense), and caps request bodies at 1 MiB; configure your MCP client's SSE connection with that header. Note stdio (the default) needs no token (parent-process pipe, no network). If no SSE doc section exists, add the note wherever the `tt mcp --transport sse` command is documented; report where you placed it.

- [ ] **Step 9: Commit (stage only the changed files)**

```bash
git add crates/mcp/src/transport/sse.rs crates/mcp/src/lib.rs crates/cli/src/main.rs crates/mcp/tests/sse_transport_smoke.rs <docs-file>
git commit -m "fix(mcp): authenticate + harden the SSE transport

Require Authorization: Bearer <TT_API_KEY> on /sse and /messages (constant-time),
validate loopback Origin/Host (DNS-rebind defense), cap request bodies at 1 MiB.
Thread the operator's validated key from the CLI. Closes the no-wire-auth + no-
body-limit findings.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```
(Replace `<docs-file>` with the actual doc path you edited.)

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-mcp 2>&1 | tail -15
cargo test --workspace --no-run 2>&1 | grep -E "^error|error\[" | head
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -E "^error|error\[|warning:" | grep -v auto-clean | head
cargo fmt --check -p tt-mcp -p tt-cli
```
All green / empty output. **Stage only the changed files** (the working tree also carries an unrelated stale `docs/reviews/...audit-checklist.md` edit + a `rust_out` junk file — do NOT stage them).

## Notes for the implementer
- The guard layer is the OUTERMOST `.layer()` so it runs first (auth/Origin/Host before body read + dispatch); `DefaultBodyLimit` is inner. In axum, the last `.layer()` added is outermost.
- `from_fn_with_state` carries the token as its OWN middleware state (`Arc<str>`), independent of the router's `.with_state(AppState)` — both coexist.
- Real MCP SSE clients can set an `Authorization` header (this is not a browser `EventSource`); requiring the bearer is compatible. Browsers (which can't set SSE headers and DO send Origin) are intentionally blocked.
- Do NOT add per-session tokens, TLS, or touch the stdio transport — out of scope.
- 401 = bad/missing bearer; 403 = non-local Host/Origin; 413 = oversized body. The guard returns these as plain status codes (no new McpError variant).
