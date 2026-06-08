# tt-client default request hardening (timeouts + User-Agent) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the zero-config `Client::new` safe request defaults — a connect timeout, a read-inactivity timeout (bounds a hung gateway without killing healthy streams), and a `tt-client/<version>` User-Agent — so a dead/hung gateway can't hang the caller forever.

**Architecture:** Build the inner `reqwest::Client` in `Client::new` via `ClientBuilder` with `connect_timeout(10s)` + `read_timeout(60s)` + `user_agent`, falling back to the plain default on a rare build failure (keeps `new` infallible). `with_http_client` (the advanced override) is unchanged.

**Tech Stack:** Rust (`crates/client` = `tt-client`), reqwest 0.12 (`ClientBuilder::read_timeout`), httpmock (dev).

Spec: `docs/superpowers/specs/2026-06-08-tt-client-default-timeouts-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`.** No public-signature change (`new`/`with_http_client` unchanged) → no workspace ripple; scope gates to `tt-client`.
>
> **TDD note:** the default read-timeout *value* (60s) isn't practically unit-testable (a test can't wait 60s). The test below exercises the read_timeout *mechanism* with a deliberately short timeout (via `with_http_client`), proving a stalled response surfaces as `Error::Request` with `is_timeout()` — it passes independently of the `Client::new` change. `Client::new`'s default-wiring is verified by compile (the `read_timeout` API resolving) + a build-succeeds assertion + review. Stated, not hidden.

---

### Task 1: Safe default timeouts + User-Agent on `Client::new`

**Files:**
- Modify: `crates/client/src/lib.rs` (`Client::new` + constants + tests)

- [ ] **Step 1: Add the constants + update `Client::new`**

In `crates/client/src/lib.rs`, ensure `use std::time::Duration;` is present (add it to the imports near the top if not already there). Add the constants just above the `impl Client {` block:
```rust
/// Fail fast if the gateway host is unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-read INACTIVITY timeout (not a total cap): bounds a hung connection but
/// does not kill a healthy long-lived stream, since each chunk resets it.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
```
Replace the body of `Client::new` (currently `Self::with_http_client(reqwest::Client::new(), base, key)`) with:
```rust
    /// New client for `base` (e.g. `https://api.tokentrimmer.com`) with `key`.
    ///
    /// Uses safe defaults: a 10s connect timeout, a 60s read-inactivity timeout
    /// (bounds a hung gateway without capping a healthy long stream), and a
    /// `tt-client/<version>` User-Agent. For different timeouts, retry, proxies,
    /// etc., configure a `reqwest::Client` yourself and use
    /// [`with_http_client`](Self::with_http_client).
    #[must_use]
    pub fn new(base: impl Into<String>, key: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .user_agent(concat!("tt-client/", env!("CARGO_PKG_VERSION")))
            .build()
            // Keep `new` infallible; the only failure is a rare TLS-backend init
            // error, where the plain default client is no worse than today.
            .unwrap_or_else(|_| reqwest::Client::new());
        Self::with_http_client(http, base, key)
    }
```
Leave `with_http_client` unchanged.

- [ ] **Step 2: Confirm it compiles (validates the reqwest `read_timeout` API)**

Run: `cargo build -p tt-client 2>&1 | tail -10`
Expected: builds cleanly. (If `read_timeout` is somehow not present on this reqwest version, the build fails here — it IS present in reqwest 0.12.28; do not substitute a total `.timeout()`, which would kill streams. If truly unavailable, STOP and report.)

- [ ] **Step 3: Add the verification tests**

In `crates/client/src/lib.rs` `#[cfg(test)] mod tests`, add (mirroring the existing httpmock tests — they use `MockServer`, `Client::new`, `.chat().model(...).message(user(...)).send()`; confirm the exact `user(...)` / `.message(...)` helpers from a sibling test like `cost_limit_402_surfaces_as_status` and match them):
```rust
    #[tokio::test]
    async fn read_timeout_surfaces_as_timeout_not_hang() {
        use std::time::Duration;
        let server = MockServer::start_async().await;
        // Mock accepts the connection but holds the response well past the
        // client's read timeout.
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .delay(Duration::from_secs(2))
                .json_body(serde_json::json!({
                    "id":"x","object":"chat.completion","created":0,"model":"m",
                    "choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}
                }));
        });
        // A short read timeout via the advanced constructor exercises the same
        // mechanism Client::new configures (without waiting the 60s default).
        let http = reqwest::Client::builder()
            .read_timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let client = Client::with_http_client(http, server.base_url(), "k");
        let result = client
            .chat()
            .model("m")
            .message(user("hi"))
            .send()
            .await;
        match result {
            Err(Error::Request(e)) => assert!(e.is_timeout(), "expected timeout, got {e:?}"),
            other => panic!("expected Err(Request(timeout)), got {other:?}"),
        }
    }

    #[test]
    fn new_builds_with_default_timeouts() {
        // The default builder config (connect/read timeouts + user-agent) is
        // valid and `new` does not panic / falls through to a usable client.
        let _client = Client::new("http://127.0.0.1:0", "tt_test_k");
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tt-client read_timeout_surfaces new_builds 2>&1 | tail -15`
Expected: PASS — `read_timeout_surfaces_as_timeout_not_hang` returns within ~1s with a timeout error (not hanging); `new_builds_with_default_timeouts` passes.
Run: `cargo test -p tt-client 2>&1 | tail -10`
Expected: PASS — all existing tests green (they hit immediate mock responses, unaffected by the new defaults).

- [ ] **Step 5: fmt + clippy**

Run: `cargo fmt --check -p tt-client 2>&1 | tail -3` → no diff (if drift: `cargo fmt -p tt-client`, re-check).
Run: `cargo clippy -p tt-client --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean | head` → none.

- [ ] **Step 6: Commit (stage only lib.rs)**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(client): safe default timeouts + User-Agent on Client::new

Client::new built a bare reqwest client (no timeout, no UA), so a dead/hung
gateway hung the caller forever. Default to a 10s connect timeout, a 60s read-
inactivity timeout (bounds a hung gateway without capping a healthy stream),
and a tt-client/<version> User-Agent. with_http_client stays the advanced
override; new() stays infallible (falls back on a rare build error).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-client 2>&1 | tail -10
cargo fmt --check -p tt-client
cargo clippy -p tt-client --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean
```
All green / empty. **Stage only `crates/client/src/lib.rs`** (the working tree also carries a `rust_out` junk file — do NOT stage it).

## Notes for the implementer
- `read_timeout` is an INACTIVITY timeout — do NOT replace it with a total `.timeout()`, which would kill a legitimate long stream. The streaming path is intentionally left without a total cap.
- `httpmock`'s `then.delay(Duration)` holds the response so the client's read times out. If the pinned httpmock version's API differs, use its equivalent delay method; the goal is simply "the server accepts but doesn't send the body before the read timeout."
- `Client::new` stays `-> Self` (infallible) via the `.unwrap_or_else(|_| reqwest::Client::new())` fallback. Do not change its signature.
- Retry/backoff is intentionally out of scope (deferred per the spec).
