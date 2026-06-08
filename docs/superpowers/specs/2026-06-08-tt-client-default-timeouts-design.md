# tt-client default request hardening (timeouts + User-Agent) — Design

**Status:** approved (design)
**Date:** 2026-06-08
**Slice:** Audit-remediation (public repo, `crates/client`). Closes the finding *"No client-side timeout, retry, or User-Agent — a hung gateway hangs the caller forever"* (opportunity/medium, `pub-sdks`).

## Background (verified against current code)
`crates/client/src/lib.rs`:
- `Client::new(base, key)` → `Self::with_http_client(reqwest::Client::new(), base, key)` (lib.rs:304-306). `reqwest::Client::new()` has **no** connect timeout, **no** read/total timeout, **no** User-Agent. A dead/unreachable host or a gateway that accepts the socket then never responds blocks `send()`/`stream()`/`run_tools` **indefinitely** for the zero-config caller (the common path).
- `with_http_client(http, base, key)` (lib.rs:310-320) is the advanced escape hatch — callers can already supply a fully-configured `reqwest::Client`.
- reqwest = `0.12` (lockfile `0.12.28`), features incl. `stream`. reqwest 0.12 `ClientBuilder` exposes `connect_timeout`, `read_timeout` (per-read **inactivity** timeout, stabilized 0.12.5), `timeout` (total), and `user_agent`.

Why not a total `.timeout()`: the streaming path (`ChatBuilder::stream`) holds a long-lived SSE connection that legitimately runs far longer than any fixed total. A client-level total timeout would kill healthy long streams. `read_timeout` (inactivity) bounds a *hung* connection without capping a *healthy* long one — each chunk resets the window — so it applies uniformly to stream + non-stream with no per-request branching.

## Decision (user-approved)
Bake safe defaults into `Client::new` (single place); leave `with_http_client` as the advanced override. Defaults: `connect_timeout(10s)` + `read_timeout(60s)` (inactivity) + `user_agent("tt-client/{version}")`. No total timeout. Keep `new` infallible.

## Architecture

### `crates/client/src/lib.rs` — `Client::new`
Add module constants and build a configured client:
```rust
use std::time::Duration;

/// Fail fast if the gateway host is unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-read INACTIVITY timeout (not a total cap): aborts a hung connection but
/// does not kill a healthy long-lived stream, since each chunk resets it.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
```
```rust
    /// New client for `base` (e.g. `https://api.tokentrimmer.com`) with `key`.
    ///
    /// Uses safe defaults: a 10s connect timeout, a 60s read-inactivity timeout
    /// (which bounds a hung gateway without capping a healthy long stream), and a
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
`with_http_client` is unchanged.

## Error handling
- `new` stays `-> Self` (infallible) — `build()` failure falls back to `reqwest::Client::new()`.
- A read/connect-timeout abort surfaces through the existing `Error::Request(reqwest::Error)` channel (the inner error's `.is_timeout()` / `.is_connect()` is true) — no new error variant, same path as any transport error. Callers already handle `Error::Request`.
- The streaming path is unaffected by a *total* cap (there is none); a stalled stream is bounded by `read_timeout` inactivity, an active one is not.

## Testing (`crates/client`, httpmock — mirror the existing httpmock tests)
- **read-timeout fires (no forever-hang):** build a `Client` via `with_http_client` with a deliberately **short** `read_timeout` (e.g. `reqwest::Client::builder().read_timeout(Duration::from_millis(200)).build().unwrap()`), point it at a mock whose response is **delayed** beyond that (`then....delay(Duration::from_secs(2))` or an httpmock delay), call `.chat().model(...).messages(...).send()`, and assert it returns `Err(Error::Request(e))` with `e.is_timeout()` — within ~1s, not hanging. (Using a custom-client short timeout keeps the test fast and deterministic; it exercises the same read_timeout mechanism `Client::new` configures.)
- **`Client::new` builds successfully:** `let _ = Client::new("http://127.0.0.1:0", "tt_test_k");` — asserts the default builder config (connect/read/user-agent) is valid and `new` doesn't panic / falls through.
- Existing tests stay green (they use `Client::new` against httpmock with immediate responses — unaffected by the new timeouts).

Gates (public repo, scoped per ADR-012): **FIRST** confirm `read_timeout` compiles on reqwest 0.12.28 (`cargo build -p tt-client`); then `cargo test -p tt-client`; **`cargo fmt --check -p tt-client`**; `cargo clippy -p tt-client --all-targets -- -D warnings` clean. Additive (no signature change — `new`/`with_http_client` signatures unchanged) — no workspace ripple; scope to `tt-client`.

## Out of scope
- **Retry / backoff / `Retry-After`** — the finding says "consider opt-in retry"; it adds idempotency + jitter complexity and is a separate, larger feature. Noted, not built.
- New public timeout-config knobs / a `Client::builder()` — `with_http_client` already covers advanced needs (YAGNI).
- Tuning the 10s/60s values per-call.
- The TS/Python SDKs (separate parity finding).
