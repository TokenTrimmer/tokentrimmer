# Per-provider timeout counter (/metrics) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record a `provider_timeouts_total{provider,operation}` counter when a provider dispatch hits the request deadline, so per-provider timeouts are visible in `/metrics` (today the latency sample is silently dropped on timeout).

**Architecture:** A new `record_provider_timeout` counter helper in `metrics.rs`; at each of the three `with_request_timeout` dispatch sites, capture the primary provider id, bind the timeout outcome to a local, and increment the counter on `ApiError::RequestTimeout`. The latency histogram is untouched (stays uncensored).

**Tech Stack:** Rust (`crates/core` = `tt-core`), `metrics` facade, axum (test harness), tower `oneshot`.

Spec: `docs/superpowers/specs/2026-06-07-provider-timeout-metric-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`.** This is additive (`record_provider_timeout` is a new `pub fn`; the call-site edits are internal) — no public signature change, so no workspace ripple; scope gates to `tt-core`. Keep each existing `with_request_timeout` async body VERBATIM — only wrap it with the primary-id capture + outcome binding + the timeout check.

---

### Task 1: provider_timeouts_total counter + wiring

**Files:**
- Modify: `crates/core/src/metrics.rs` (new counter helper)
- Modify: `crates/core/src/routes/chat.rs` (non-stream site ~1214; stream site ~843)
- Modify: `crates/core/src/routes/embeddings.rs` (site ~226)
- Modify: `crates/core/tests/timeout_header.rs` (new test)

- [ ] **Step 1: Write the failing test**

In `crates/core/tests/timeout_header.rs`, add (after `embeddings_timeout_returns_408`):
```rust
#[tokio::test]
async fn timeout_increments_provider_timeouts_total() {
    let router = app(1_000); // provider sleeps 1s
    // Trigger a deadline timeout (caller allows only 50ms).
    let resp = router.clone().oneshot(chat(Some("50"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);

    // Scrape /metrics and confirm the timeout counter series exists for sleepy/chat.
    let m = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(m.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("provider_timeouts_total"),
        "provider_timeouts_total series missing:\n{text}"
    );
    assert!(text.contains("provider=\"sleepy\""), "provider label missing:\n{text}");
    assert!(text.contains("operation=\"chat\""), "operation label missing:\n{text}");
}
```

- [ ] **Step 2: Run to confirm it fails**

Run: `cargo test -p tt-core --test timeout_header timeout_increments_provider_timeouts_total 2>&1 | tail -20`
Expected: FAIL — the 408 assertion passes, but `/metrics` does not yet contain `provider_timeouts_total` (the counter isn't wired), so the `contains` assertion fails. (Compiles + runs — genuine failing-first.)

- [ ] **Step 3: Add the counter helper**

In `crates/core/src/metrics.rs`, after `record_provider_latency` (the last fn), add:
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

- [ ] **Step 4: Wire the chat non-stream site (chat.rs ~1214)**

In `crates/core/src/routes/chat.rs`, find `let dispatch_result: ApiResult<_> = with_request_timeout(request_timeout, async {`. Immediately ABOVE that line, add:
```rust
        let __primary = provider.id();
```
Then find where that statement ends — the `})` followed by `.await;` (the `dispatch_result` assignment, ~line 1250). Immediately AFTER `.await;` (before the "3c-neg. Negative-cache write" comment block), add:
```rust

        if matches!(dispatch_result, Err(ApiError::RequestTimeout { .. })) {
            crate::metrics::record_provider_timeout(__primary, "chat");
        }
```
Leave the async body and the later `if let Err(ref err) = dispatch_result` handling unchanged. (`matches!` borrows `dispatch_result`; it is still consumed normally afterward.)

- [ ] **Step 5: Wire the chat stream site (chat.rs ~843)**

In `crates/core/src/routes/chat.rs`, find `let (provider, served_model, stream) = with_request_timeout(request_timeout, async {` and its closing `})` + `.await?;` (~line 881). Replace the `let (provider, served_model, stream) = …await?;` wrapper (keeping the async body verbatim) so it reads:
```rust
        let __primary = provider.id();
        let __stream_outcome = with_request_timeout(request_timeout, async {
            // …existing async body UNCHANGED…
        })
        .await;
        if matches!(__stream_outcome, Err(ApiError::RequestTimeout { .. })) {
            crate::metrics::record_provider_timeout(__primary, "chat_stream");
        }
        let (provider, served_model, stream) = __stream_outcome?;
```
(Only the first line, the `.await?` → `.await;` + check + destructure are new; the async block between `async {` and `})` is the existing code unchanged.)

- [ ] **Step 6: Wire the embeddings site (embeddings.rs ~226)**

In `crates/core/src/routes/embeddings.rs`, find `let resp = with_request_timeout(ctx.deadline, async {` and its closing `})` + `.await?;`. Replace the wrapper (keeping the async body verbatim):
```rust
    let __primary = provider.id();
    let __emb_outcome = with_request_timeout(ctx.deadline, async {
        // …existing async body UNCHANGED…
    })
    .await;
    if matches!(__emb_outcome, Err(ApiError::RequestTimeout { .. })) {
        crate::metrics::record_provider_timeout(__primary, "embeddings");
    }
    let resp = __emb_outcome?;
```
Confirm `ApiError` is in scope (it is — used as `ApiError::from` nearby). If clippy flags `__primary` as unused on a path, that won't happen — it's used in the `matches!` arm.

- [ ] **Step 7: Run the test + full tt-core suite**

Run: `cargo test -p tt-core --test timeout_header 2>&1 | tail -15`
Expected: PASS — `timeout_increments_provider_timeouts_total` green + the 3 existing 408 tests green.
Run: `cargo test -p tt-core 2>&1 | tail -15`
Expected: PASS — no regressions (metrics_endpoint, chat, embeddings suites).

- [ ] **Step 8: fmt + clippy**

Run: `cargo fmt --check -p tt-core 2>&1 | tail -3` → no diff (if drift: `cargo fmt -p tt-core`, re-check).
Run: `cargo clippy -p tt-core --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean | head` → none.

- [ ] **Step 9: Commit (stage only the four files)**

```bash
git add crates/core/src/metrics.rs crates/core/src/routes/chat.rs crates/core/src/routes/embeddings.rs crates/core/tests/timeout_header.rs
git commit -m "feat(metrics): provider_timeouts_total counter on request-deadline timeouts

The per-provider latency sample was dropped when with_request_timeout cancelled
the dispatch future. Add a dedicated counter (keeps the latency histogram
uncensored), incremented at the chat/chat_stream/embeddings dispatch sites on
ApiError::RequestTimeout, attributed to the primary provider.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-core --test timeout_header 2>&1 | tail -10
cargo test -p tt-core 2>&1 | tail -10
cargo fmt --check -p tt-core
cargo clippy -p tt-core --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean
```
All green / empty output. **Stage only the four changed files** (the working tree also carries an unrelated stale `docs/reviews/...audit-checklist.md` edit + a `rust_out` junk file — do NOT stage them).

## Notes for the implementer
- Capture `let __primary = provider.id();` BEFORE the `with_request_timeout(...)` at the two chat sites because `provider` is moved into the async there (the async returns `(provider, …)`). `provider.id()` is `&'static str`, so capturing it first is free. At the embeddings site `provider` is only borrowed, but capture `__primary` uniformly for consistency.
- `matches!(outcome, Err(ApiError::RequestTimeout { .. }))` borrows the outcome (the variant pattern binds nothing), so the subsequent consumption (`?` / `if let Err(ref err)`) is unaffected.
- Only `RequestTimeout` increments the counter — all other errors flow through unchanged.
- The latency histogram (`provider_request_duration_seconds`) and the failover internal per-candidate records are NOT touched. A failover-path timeout is attributed to the primary provider (the request deadline spans the loop) — intended.
- The test uses a `contains` check (not an exact count) because the Prometheus recorder is process-global (`OnceLock`) and counters accumulate across tests in the binary.
