# Per-provider timeout counter (/metrics) — Design

**Status:** approved (design)
**Date:** 2026-06-07
**Slice:** Audit-remediation Wave 4 (public repo, `crates/core`). Closes the #72 (`gw-metrics-endpoint`) deferred follow-up: *"per-provider latency sample dropped on provider timeout."*

## Background (verified against current code)
At the three dispatch sites, `with_request_timeout(deadline, async { …; record_provider_latency(provider.id(), op, started.elapsed()); … })` records the per-provider latency **after** the dispatch await. When the request deadline fires, `tokio::time::timeout` cancels the inner future, so `record_provider_latency` never runs — the timed-out attempt is invisible in `provider_request_duration_seconds`:
- `crates/core/src/routes/chat.rs:1214` (non-stream, op `"chat"`) — result bound to `dispatch_result`, consumed later (`if let Err(ref err) = dispatch_result`).
- `crates/core/src/routes/chat.rs:843` (stream, op `"chat_stream"`) — result consumed with `?` (`let (provider, served_model, stream) = …await?;`).
- `crates/core/src/routes/embeddings.rs:226` (op `"embeddings"`) — result consumed with `?` (`let resp = …await?;`).

`with_request_timeout` (chat.rs:1893) returns `Err(ApiError::RequestTimeout { ms })` on deadline. The request-level RED middleware still records the 408 — only the per-provider signal is missing. `metrics.rs` already has the counter pattern (PR #72 added `cache_lookups_total` etc.) and `record_provider_latency(provider: &'static str, operation: &'static str, …)`; `provider.id()` returns `&'static str`.

## Decision (user-approved)
Add a dedicated `provider_timeouts_total{provider, operation}` **counter** (not a histogram sample) — keeps the latency histogram uncensored (accurate p50/p99) and sidesteps the failover double-count the original PR cited. Attribute a timeout to the **primary** provider at each call site.

## Architecture

### `crates/core/src/metrics.rs`
Add after `record_provider_latency`:
```rust
/// Count a provider dispatch that hit the request deadline (timed out). Kept
/// separate from `provider_request_duration_seconds` so right-censored-at-
/// deadline values don't skew the latency percentiles.
pub fn record_provider_timeout(provider: &'static str, operation: &'static str) {
    metrics::counter!(
        "provider_timeouts_total",
        "provider" => provider,
        "operation" => operation,
    )
    .increment(1);
}
```

### Call sites — capture the primary provider id, then inspect the outcome
The pattern at each site: capture `let __primary = provider.id();` immediately before the `with_request_timeout(...)` (needed because `provider` is moved into the async on the chat paths), bind the outcome to a local, increment the counter on a `RequestTimeout`, then consume the outcome exactly as before.

**`chat.rs:1214` (non-stream):** the result already binds to `dispatch_result`. After the `.await;` (chat.rs:~1250), before the negative-cache block, add:
```rust
        if matches!(dispatch_result, Err(ApiError::RequestTimeout { .. })) {
            crate::metrics::record_provider_timeout(__primary, "chat");
        }
```
and add `let __primary = provider.id();` immediately before `let dispatch_result: ApiResult<_> = with_request_timeout(`. (`matches!` borrows, doesn't consume `dispatch_result` — the later `if let Err(ref err) = dispatch_result` still works.)

**`chat.rs:843` (stream):** replace `let (provider, served_model, stream) = with_request_timeout(request_timeout, async { … }).await?;` with:
```rust
        let __primary = provider.id();
        let __stream_outcome = with_request_timeout(request_timeout, async { /* unchanged body */ }).await;
        if matches!(__stream_outcome, Err(ApiError::RequestTimeout { .. })) {
            crate::metrics::record_provider_timeout(__primary, "chat_stream");
        }
        let (provider, served_model, stream) = __stream_outcome?;
```

**`embeddings.rs:226`:** replace `let resp = with_request_timeout(ctx.deadline, async { … }).await?;` with:
```rust
    let __primary = provider.id();
    let __emb_outcome = with_request_timeout(ctx.deadline, async { /* unchanged body */ }).await;
    if matches!(__emb_outcome, Err(ApiError::RequestTimeout { .. })) {
        crate::metrics::record_provider_timeout(__primary, "embeddings");
    }
    let resp = __emb_outcome?;
```
(`ApiError` is already in scope at all three sites. `provider.id()` is `&'static str`, so capturing it before the move is free.)

## Failover attribution
The request deadline spans the whole dispatch, including the failover loop, so a timeout cannot be attributed to a specific failed-over candidate at the outer boundary. It is attributed to the **primary** provider (`__primary`, the entering provider). The failover path's internal per-candidate `record_provider_latency` calls (failover.rs:212/331) are unchanged. Documented imprecision, acceptable for a timeout-rate signal.

## Error handling
- Only `ApiError::RequestTimeout` increments the counter; all other errors (provider 4xx/5xx, network) are unaffected and still flow through unchanged.
- The counter helper is a no-op until `install()` has run (the `metrics` facade), same as the existing helpers — safe in any context.
- No behavior change: the timeout still returns 408; the latency histogram is untouched.

## Testing (`crates/core/tests/timeout_header.rs`, reusing `SleepyProvider` + `build_router`)
`build_router` installs the metrics recorder and registers the auth-exempt `GET /metrics`. Add a test:
```rust
#[tokio::test]
async fn timeout_increments_provider_timeouts_total() {
    let router = app(1_000); // provider sleeps 1s
    // Trigger a deadline timeout (caller allows 50ms).
    let resp = router.clone().oneshot(chat(Some("50"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
    // Scrape /metrics and confirm the counter series exists for sleepy/chat.
    let m = router
        .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(m.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("provider_timeouts_total"), "series missing:\n{text}");
    assert!(text.contains("provider=\"sleepy\""));
    assert!(text.contains("operation=\"chat\""));
}
```
Notes: the Prometheus recorder is process-global (`OnceLock`) — counters accumulate across tests in the binary, so assert series/label **existence** (contains), not an exact value (exact counts would be flaky under parallel tests). `axum::Router` is `Clone`, so `router.clone()` for the first call and `router` for the `/metrics` scrape. The existing 408 tests stay green.

Gates (public repo, scoped per ADR-012): `cargo test -p tt-core --test timeout_header` + `cargo test -p tt-core` (no regressions); **`cargo fmt --check -p tt-core`**; `cargo clippy -p tt-core --all-targets -- -D warnings` clean. No public signature change (new fn is additive; call-site edits are internal), so no workspace ripple — but run `cargo test -p tt-core` (the metrics + timeout integration tests live there).

## Out of scope
- Folding timeout-elapsed into the latency histogram (rejected — censors the distribution).
- Precise per-candidate timeout attribution in the failover loop (attributed to the primary; the request-level RED 408 already captures the event).
- Any change to timeout behavior, the latency histogram, or the RED request metrics.
