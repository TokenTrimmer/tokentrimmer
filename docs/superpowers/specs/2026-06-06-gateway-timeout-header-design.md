# Honor `X-TokenTrimmer-Timeout-Ms` request header (F10) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** F10. Honors the `X-TokenTrimmer-Timeout-Ms` request header (documented "Planned"): a per-request upstream timeout, enforced gateway-side, returning `408` on expiry.

## Goal

Let a caller bound a single request's upstream time via `X-TokenTrimmer-Timeout-Ms: 30000` (max `600000`). On expiry the gateway returns **408 Request Timeout** (distinct from the `504` an upstream provider timeout yields), on both `/v1/chat/completions` and `/v1/embeddings`.

## Background (current state)

- `RequestContext.deadline: Option<Duration>` (`crates/shared/src/context.rs:57`) exists but is **dormant** — written only to `None`, never read or enforced.
- A global tower `TimeoutLayer` (`server.rs:71`) wraps the whole HTTP request at 600s → 504. No per-request timeout.
- `ProviderError::Timeout` → `504` (`error.rs:191`). Docs reserve `408` for "per-request (`X-TokenTrimmer-Timeout-Ms`) timeout exceeded".
- Provider dispatch is awaited via `with_retry` (single) / `dispatch_with_failover` / `dispatch_stream_with_failover` (chat.rs streaming ~821 + non-streaming ~1163; embeddings single `provider.embeddings(req, &ctx).await` at `embeddings.rs:226`).
- Both handlers build `let mut ctx = RequestContext { …, deadline: None };`.

## Architecture

`crates/core` (`routes/chat.rs`, `routes/embeddings.rs`, `error.rs`) + docs.

### 1. Header parser (`chat.rs`, `pub(crate)`)
```rust
/// `X-TokenTrimmer-Timeout-Ms` — per-request upstream timeout in ms (1..=600000).
/// Invalid / non-positive / over-max → None (no per-request timeout; the global
/// 600s limit still applies).
pub(crate) fn timeout_ms_from_header(headers: &HeaderMap) -> Option<u64> {
    const MAX_TIMEOUT_MS: u64 = 600_000;
    headers
        .get("x-tokentrimmer-timeout-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0 && *ms <= MAX_TIMEOUT_MS)
}
```

### 2. New error variant (`error.rs`)
```rust
/// A caller-set per-request deadline (`X-TokenTrimmer-Timeout-Ms`) elapsed.
#[error("request timed out after {ms} ms")]
RequestTimeout { ms: u64 },
```
maps to:
```rust
ApiError::RequestTimeout { ms } => (
    StatusCode::REQUEST_TIMEOUT, // 408
    "timeout_error",
    "request_timeout",
    format!("Request exceeded the {ms} ms X-TokenTrimmer-Timeout-Ms deadline."),
),
```
`ApiError` is not a TS-exported type, so this adds no bindings drift.

### 3. Timeout helper (`chat.rs`)
```rust
/// Run `fut` under an optional per-request deadline; on expiry return 408.
async fn with_request_timeout<T>(
    timeout: Option<std::time::Duration>,
    fut: impl std::future::Future<Output = ApiResult<T>>,
) -> ApiResult<T> {
    match timeout {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(r) => r,
            Err(_) => Err(ApiError::RequestTimeout {
                ms: d.as_millis().min(u128::from(u64::MAX)) as u64,
            }),
        },
        None => fut.await,
    }
}
```

### 4. Chat handler wiring
- Near the other header reads:
  ```rust
  let request_timeout = timeout_ms_from_header(&headers).map(std::time::Duration::from_millis);
  ```
- Populate the dormant field at `ctx` construction: `deadline: request_timeout` (instead of `None`) — documents intent and is available to adapters later (enforcement is gateway-side here).
- Wrap each dispatch (the whole single-vs-failover `if/else`, covering the retry + failover budget) in `with_request_timeout`, with the block returning `ApiResult<T>`:
  - **Non-streaming** (~1163): wrap the block producing `ApiResult<(provider, resp)>`.
  - **Streaming** (~821): wrap the block producing `ApiResult<(provider, served_model, stream)>` (each arm returns `Ok(...)` / maps its error), then `?`. For streaming the timeout bounds **establishment** (the first response/handle) — consistent with how failover already only retries establishment; the global 600s still caps the full token stream.

### 5. Embeddings handler wiring
- `let request_timeout = timeout_ms_from_header(&headers).map(Duration::from_millis);` → `deadline: request_timeout` in `ctx`.
- Wrap the single dispatch:
  ```rust
  let resp = with_request_timeout(request_timeout, async { provider.embeddings(req, &ctx).await.map_err(ApiError::from) }).await?;
  ```
  Import `with_request_timeout` from the chat module (same path embeddings uses for the other shared helpers).

## Behavior
- A per-request timeout always tightens (≤ the 600s global cap).
- The timeout covers the full retry + failover budget for that dispatch (one ceiling for the whole attempt chain), not per-attempt.
- Expiry → 408 (caller deadline). An upstream provider's own timeout is unchanged → 504.
- `tt_test_*` sandbox short-circuits before dispatch — unaffected.

## Testing

Integration (`crates/core/tests/timeout_header.rs`): a `SleepyProvider` whose `chat_completion`/`embeddings` `tokio::time::sleep` longer than the deadline.
- **`timeout_header_returns_408`**: `X-TokenTrimmer-Timeout-Ms: 50` against a provider that sleeps 10s → `408` (returns fast; the sleep future is dropped), error code `request_timeout`.
- **`no_timeout_header_completes`**: no header, fast provider → `200`.
- **`embeddings_timeout_returns_408`**: same on `/v1/embeddings`.
- Unit (`chat.rs`): `timeout_ms_from_header` — `"30000"`→Some(30000); `"0"`/`"700000"`/`"abc"`/absent → None.

Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p tt-core`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-core --no-deps`.

## Docs
- Flip `docs/04-gateway-api-reference.md` `X-TokenTrimmer-Timeout-Ms` row (§6.1) → "Honored"; note: per-request upstream timeout in ms (max 600000); `408` on expiry; invalid/over-max ignored (global 600s still applies). The §status-codes table already lists `408` (per-request) and `504` (upstream) — leave as-is.

## Out of scope
- Mid-stream token-by-token deadline (streaming = establishment-bound; the global limit caps the full stream).
- Per-adapter enforcement (the `deadline` field is now populated, but enforced gateway-side via `tokio::time::timeout`).
- Changing the global 600s `TimeoutLayer`.
