# MCP SSE transport auth + hardening — Design

**Status:** approved (design)
**Date:** 2026-06-07
**Slice:** Audit-remediation Wave 4 (public repo, `crates/mcp` + `crates/cli`). Closes two `pub-inspect-mcp-preview` findings: *"MCP SSE transport applies no authentication on the wire"* (security/medium) and *"SSE /messages has no request body size limit"* (security/low).

## Background (verified against current code)
- `crates/mcp/src/transport/sse.rs::run(server, addr)` builds an axum router (`GET /sse`, `POST /messages`) with **no auth, no Origin/Host check, no body limit**. `sse_handler` mints a fresh session UUID and returns it to any caller via the `endpoint` event, so the 404-on-unknown-session check is not access control — any local caller gets a valid session free.
- The tools carry real privilege: `lookup_semantic_cache`/`simulate_plan`/`plan_history` call `.bearer_auth(&self.api_key)` with the operator's cloud key; `inspect_diff`/`inspect_baseline`/`cost_ledger` read the local filesystem. Binding is `127.0.0.1` (cli/src/main.rs:508), so the gap is local confused-deputy, not remote.
- `crates/cli/src/main.rs:461` already computes `let api_key = auth::validate_api_key(ctx.api_key_string())?;` (a `tt_live_`/`tt_test_` key) before the transport branch; main.rs:514 calls `server.run_sse(addr)` (no key threaded). `Server::run_sse(self, addr)` (mcp/src/lib.rs:26) delegates to `transport::sse::run`.
- axum 0.7 (`from_fn_with_state`, `extract::DefaultBodyLimit`, `middleware::Next` available). `McpError` has `Unauthorized` (JSON-RPC −32001) + `Internal`. mcp deps: axum (macros), tokio; dev-deps include reqwest (used by the existing `tests/sse_transport_smoke.rs`, which spins a real server on `127.0.0.1:0`).

## Decision (user-approved)
Require `Authorization: Bearer <TT_API_KEY>` on the whole SSE router (constant-time compare), validate Origin/Host (DNS-rebind defense), and cap the request body. Token = the operator's already-validated `TT_API_KEY` (the MCP client already holds it; a local process without it can't drive any tool).

## Architecture

### `crates/mcp/src/transport/sse.rs`
Change the entry point to take the token and apply a guard layer + body limit:
```rust
pub async fn run(server: Server, addr: SocketAddr, auth_token: String) -> Result<(), McpError> {
    let state = AppState { /* sessions, server */ };

    let app = axum::Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .with_state(state)
        // Inner: cap request bodies (GET /sse has none; POST /messages is capped).
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        // Outer (runs first): bearer + Origin/Host guard. Rejects before the body
        // is read or `dispatch` runs.
        .layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::<str>::from(auth_token.as_str()),
            guard,
        ));
    // …existing bind + serve unchanged…
}

const MAX_BODY_BYTES: usize = 1024 * 1024; // 1 MiB
```

The guard middleware (axum 0.7 shape):
```rust
async fn guard(
    axum::extract::State(token): axum::extract::State<std::sync::Arc<str>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::StatusCode;
    let headers = req.headers();

    // 1. Host must be loopback (blocks DNS-rebinding: a rebound browser sends a
    //    non-local Host even though the socket is 127.0.0.1).
    if !host_is_local(headers.get(axum::http::header::HOST)) {
        return (StatusCode::FORBIDDEN, "non-local Host").into_response();
    }
    // 2. Origin, when present, must be loopback (real MCP clients send none).
    if !origin_is_local_or_absent(headers.get(axum::http::header::ORIGIN)) {
        return (StatusCode::FORBIDDEN, "cross-origin").into_response();
    }
    // 3. Bearer token, constant-time compared.
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let ok = presented.is_some_and(|p| ct_eq(p.as_bytes(), token.as_bytes()));
    if !ok {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    next.run(req).await
}
```

Helpers (in the same file):
```rust
/// Constant-time byte compare (length difference returns early — token length is
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

/// True if the Host header's host-part is loopback (127.0.0.1 / localhost / ::1),
/// ignoring the port. A missing Host is rejected (HTTP/1.1 requires it).
fn host_is_local(h: Option<&axum::http::HeaderValue>) -> bool {
    match h.and_then(|v| v.to_str().ok()) {
        Some(s) => is_local_authority(s),
        None => false,
    }
}

/// True if Origin is absent (non-browser clients) or its host is loopback.
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
        // IPv6 literal: "[::1]:port" or "[::1]"
        rest.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}
```
(`IntoResponse` is already imported; add the `header`/`StatusCode`/`Request`/`Next` paths as needed — keep imports tidy.)

### `crates/mcp/src/lib.rs`
```rust
pub async fn run_sse(self, addr: std::net::SocketAddr, auth_token: String) -> Result<(), McpError> {
    crate::transport::sse::run(self, addr, auth_token).await
}
```

### `crates/cli/src/main.rs`
At the `"sse"` transport arm (~main.rs:508-514), pass the already-validated `api_key`:
```rust
        .block_on(server.run_sse(addr, api_key))?;
```
(`api_key` is in scope from main.rs:461. It's a `String`; clone if the borrow checker requires it, but it's the last use, so a move is fine.)

## Error handling
- Missing/invalid bearer → `401`. Non-local Host or cross-origin → `403`. Body over 1 MiB → `413` (axum `DefaultBodyLimit`). All before `dispatch`.
- Constant-time bearer compare avoids a content-timing side channel; the length-difference early return leaks only the token length (non-sensitive).
- A missing `Host` header → 403 (HTTP/1.1 mandates Host; absence is anomalous).

## Testing (`crates/mcp/tests/sse_transport_smoke.rs`, mirroring the existing ephemeral-server + reqwest pattern)
- **Update the helper + 2 existing tests:** `spawn_sse_server` passes a known token to `run_sse`; the happy-path `GET /sse` and `POST /messages` requests add `.header("Authorization", format!("Bearer {token}"))`. `sse_transport_tools_list_round_trip` + `post_unknown_session_returns_404` stay green with the header.
- **New tests:**
  - `GET /sse` with no `Authorization` → `401`.
  - `POST /messages` with no `Authorization` → `401`.
  - wrong bearer (`Bearer wrong`) → `401`.
  - request with `Host: evil.com` (override the header) + valid bearer → `403`.
  - `POST /messages` with a body > 1 MiB + valid bearer + valid session → `413`.
- The CLI wiring change (passing `api_key`) is covered by compilation; no CLI integration test exists for `tt mcp sse` (it binds a server), matching the current test strategy.

Gates (public repo, scoped per ADR-012): `cargo test -p tt-mcp`; **`cargo fmt --check -p tt-mcp -p tt-cli`** (public CI gates fmt); `cargo clippy -p tt-mcp -p tt-cli --all-targets -- -D warnings` clean. **Workspace-ripple guard (lesson [[ci-verify-all-targets]]):** `run_sse`/`sse::run` are public signature changes — grep the workspace for all callers (`grep -rn "run_sse\|sse::run" crates`) and run `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace --no-run` before pushing (known callers: cli/src/main.rs + tests/sse_transport_smoke.rs; verify no others).

## Docs
Update the MCP SSE usage docs (`docs/` — the `tt-mcp-usage.md` SSE section, or wherever the SSE transport is documented; search `docs/` for `sse`): the SSE transport requires `Authorization: Bearer $TT_API_KEY`, binds localhost-only, validates Origin/Host, and caps the request body. Note stdio (the default) is unauthenticated by nature (parent-process pipe, no network).

## Out of scope
- stdio transport (no network surface; unchanged).
- Per-session tokens (the single server bearer fully gates both routes — adds nothing here).
- TLS (loopback only).
- Generating a separate token (rejected in favor of reusing `TT_API_KEY`).
- Verifying the `TT_API_KEY` against the cloud at SSE-startup (already validated for shape at main.rs:461; full cloud verification is a separate concern).
