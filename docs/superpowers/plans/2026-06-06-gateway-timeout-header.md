# Honor `X-TokenTrimmer-Timeout-Ms` (F10) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Honor the `X-TokenTrimmer-Timeout-Ms` request header — a per-request upstream timeout (max 600000) enforced gateway-side via `tokio::time::timeout`, returning `408` on expiry, on chat + embeddings.

**Architecture:** A `timeout_ms_from_header` parser; a new `ApiError::RequestTimeout { ms }` → 408; a `with_request_timeout` helper wrapping the dispatch future in both handlers; `ctx.deadline` populated from the header.

**Tech Stack:** Rust, axum, tokio, the in-crate test harness.

---

### Task 1: `timeout_ms_from_header` parser

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add fn near the other header readers; unit test in `provider_override_tests`)

- [ ] **Step 1: Write the failing unit test**

In `crates/core/src/routes/chat.rs`, inside `#[cfg(test)] mod provider_override_tests`, add:

```rust
    #[test]
    fn timeout_ms_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(timeout_ms_from_header(&h), None);
        h.insert("x-tokentrimmer-timeout-ms", " 30000 ".parse().unwrap());
        assert_eq!(timeout_ms_from_header(&h), Some(30_000));
        for bad in ["0", "700000", "abc", "-5"] {
            let mut b = HeaderMap::new();
            b.insert("x-tokentrimmer-timeout-ms", bad.parse().unwrap());
            assert_eq!(timeout_ms_from_header(&b), None, "{bad} must be rejected");
        }
    }
```

Run: `cargo test -p tt-core timeout_ms_header_parsing 2>&1 | tail -8`
Expected: FAIL to compile — `timeout_ms_from_header` missing.

- [ ] **Step 2: Implement the parser**

Add after `fallback_override_from_header` (after `chat.rs:~115`):

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

Run: `cargo test -p tt-core timeout_ms_header_parsing 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "feat(core): add timeout_ms_from_header parser"
```

---

### Task 2: Integration tests (RED)

**Files:**
- Create: `crates/core/tests/timeout_header.rs`

- [ ] **Step 1: Write the test file**

Create `crates/core/tests/timeout_header.rs`:

```rust
//! `X-TokenTrimmer-Timeout-Ms` enforces a per-request deadline → 408.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, EmbeddingData, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

/// Sleeps `delay` before answering (so a short request timeout fires first).
struct SleepyProvider {
    delay: Duration,
}

#[async_trait]
impl Provider for SleepyProvider {
    fn id(&self) -> &'static str {
        "sleepy"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "slow".into(),
            provider: "sleepy".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        tokio::time::sleep(self.delay).await;
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("ok".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _r: ChatCompletionRequest,
        _c: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        _c: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        tokio::time::sleep(self.delay).await;
        Ok(EmbeddingsResponse {
            object: "list".into(),
            data: vec![EmbeddingData {
                object: "embedding".into(),
                index: 0,
                embedding: vec![0.1, 0.2],
            }],
            model: req.model,
            usage: Usage {
                prompt_tokens: 3,
                completion_tokens: 0,
                total_tokens: 3,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
}

fn app(delay_ms: u64) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(SleepyProvider {
        delay: Duration::from_millis(delay_ms),
    }));
    build_router(AppState::new(registry))
}

fn chat(timeout_ms: Option<&str>) -> Request<Body> {
    let body = json!({ "model": "slow", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(t) = timeout_ms {
        b = b.header("x-tokentrimmer-timeout-ms", t);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn timeout_header_returns_408() {
    let resp = app(1_000) // provider would take 1s
        .oneshot(chat(Some("50"))) // but caller allows only 50ms
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
}

#[tokio::test]
async fn no_timeout_header_completes() {
    let resp = app(0).oneshot(chat(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn embeddings_timeout_returns_408() {
    let body = json!({ "model": "slow", "input": "hello" });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-timeout-ms", "50")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app(1_000).oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
}
```

- [ ] **Step 2: Run to verify failures (RED)**

Run: `cargo test -p tt-core --test timeout_header 2>&1 | tail -20`
Expected: `timeout_header_returns_408` and `embeddings_timeout_returns_408` FAIL — the header is ignored today, so each request waits ~1s and returns `200`, not `408`. `no_timeout_header_completes` passes.

- [ ] **Step 3: Commit the failing tests**

```bash
git add crates/core/tests/timeout_header.rs
git commit -m "test(core): X-TokenTrimmer-Timeout-Ms → 408 (RED)"
```

---

### Task 3: Implement enforcement (GREEN)

**Files:**
- Modify: `crates/core/src/error.rs` (new variant + 408 mapping)
- Modify: `crates/core/src/routes/chat.rs` (two exhaustive match arms; `with_request_timeout`; ctx.deadline; two dispatch wraps)
- Modify: `crates/core/src/routes/embeddings.rs` (import; ctx.deadline; dispatch wrap)

- [ ] **Step 1: Add the `ApiError::RequestTimeout` variant + 408 mapping**

In `crates/core/src/error.rs`, add to the `enum ApiError` (after `RateLimited`):

```rust
    /// A caller-set per-request deadline (`X-TokenTrimmer-Timeout-Ms`) elapsed.
    #[error("request timed out after {ms} ms")]
    RequestTimeout { ms: u64 },
```

In `impl IntoResponse for ApiError`'s `match`, add an arm:

```rust
            ApiError::RequestTimeout { ms } => (
                StatusCode::REQUEST_TIMEOUT,
                "timeout_error",
                "request_timeout",
                format!("Request exceeded the {ms} ms X-TokenTrimmer-Timeout-Ms deadline."),
            ),
```

- [ ] **Step 2: Handle the new variant in the two exhaustive `chat.rs` matches**

In `is_deterministic_client_error` (`chat.rs:234`), add `RequestTimeout` to the never-cache (`=> false`) group:

```rust
        | ApiError::RateLimited { .. }
        | ApiError::RequestTimeout { .. }
        | ApiError::Internal(_)
```

In `error_status_code` (`chat.rs`), add an arm:

```rust
        ApiError::RequestTimeout { .. } => StatusCode::REQUEST_TIMEOUT,
```

- [ ] **Step 3: Add `with_request_timeout` + read the header**

Add near `attach_cost_headers` in `chat.rs`:

```rust
/// Run `fut` under an optional per-request deadline; on expiry return 408.
pub(crate) async fn with_request_timeout<T>(
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

In the chat handler, near the other header reads (after `forced_route`, ~line 388):

```rust
    let request_timeout = timeout_ms_from_header(&headers).map(std::time::Duration::from_millis);
```

Change the `ctx` construction `deadline: None,` → `deadline: request_timeout,`.

- [ ] **Step 4: Wrap the non-streaming dispatch**

Replace the non-streaming dispatch (`let dispatch_result: ApiResult<_> = if route_fallbacks.is_empty() { … } else { … };`) by wrapping the `if/else` in `with_request_timeout` (the inner block already yields `ApiResult<(provider, resp)>`):

```rust
        let dispatch_result: ApiResult<_> = with_request_timeout(request_timeout, async {
            if route_fallbacks.is_empty() {
                with_retry(&RetryPolicy::default(), || {
                    provider.chat_completion(req.clone(), &ctx)
                })
                .await
                .map(|resp| (provider, resp))
                .map_err(ApiError::from)
            } else {
                let cap_required = tt_shared::RequiredCapabilities::from_request(&req);
                let cap_est_tokens = {
                    let combined = tt_shared::message_text_for_estimation(&req);
                    tt_tokenize::estimate_tokens(provider.id(), &combined) as u64
                };
                crate::failover::dispatch_with_failover(
                    &state.registry,
                    &state.breaker,
                    &RetryPolicy::default(),
                    &failover_candidates,
                    &req,
                    &ctx,
                    &failover_creds,
                    Utc::now(),
                    Some(crate::failover::CapCheck {
                        required: &cap_required,
                        estimated_tokens: cap_est_tokens,
                    }),
                )
                .await
                .map_err(ApiError::from)
            }
        })
        .await;
```

(The negative-cache-write block that inspects `dispatch_result` for `Err` is unchanged — a `RequestTimeout` is correctly treated as never-cache by Step 2.)

- [ ] **Step 5: Wrap the streaming dispatch**

Replace the streaming `let (provider, served_model, stream) = if route_fallbacks.is_empty() { … } else { … };` with a timeout-wrapped block that returns `ApiResult<(_, _, _)>`:

```rust
        let (provider, served_model, stream) = with_request_timeout(request_timeout, async {
            if route_fallbacks.is_empty() {
                let stream = with_retry(&RetryPolicy::default(), || {
                    provider.chat_completion_stream(req.clone(), &ctx)
                })
                .await?;
                Ok((provider, req.model.clone(), stream))
            } else {
                let cap_required = tt_shared::RequiredCapabilities::from_request(&req);
                let cap_est_tokens = estimated_input_tokens.max(0) as u64;
                crate::failover::dispatch_stream_with_failover(
                    &state.registry,
                    &state.breaker,
                    &RetryPolicy::default(),
                    &failover_candidates,
                    &req,
                    &ctx,
                    &failover_creds,
                    Utc::now(),
                    Some(crate::failover::CapCheck {
                        required: &cap_required,
                        estimated_tokens: cap_est_tokens,
                    }),
                )
                .await
                .map_err(ApiError::from)
            }
        })
        .await?;
```

(`with_retry(...).await?` inside the block now resolves against the block's `ApiResult` via the existing `From<ProviderError> for ApiError`.)

- [ ] **Step 6: Wrap the embeddings dispatch**

In `crates/core/src/routes/embeddings.rs`:
- Add `timeout_ms_from_header` and `with_request_timeout` to the `use crate::routes::chat::{…}` import (both are `pub(crate)` per Steps 1 + 3).
- Near the other header reads, add:
  ```rust
  let request_timeout = timeout_ms_from_header(&headers).map(std::time::Duration::from_millis);
  ```
- Change the `ctx` `deadline: None,` → `deadline: request_timeout,`.
- Replace `let resp = provider.embeddings(req, &ctx).await?;` (line 226) with:
  ```rust
  let resp = with_request_timeout(request_timeout, async {
      provider.embeddings(req, &ctx).await.map_err(ApiError::from)
  })
  .await?;
  ```

- [ ] **Step 7: Run the integration + unit tests (GREEN)**

Run: `cargo test -p tt-core --test timeout_header 2>&1 | tail -20`
Expected: all 3 pass (the two 408 tests return fast — the timeout drops the sleep).

Run: `cargo test -p tt-core 2>&1 | grep -E "test result:" | tail`
Expected: all pass (no regressions in failover/retry/cache suites).

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/error.rs crates/core/src/routes/chat.rs crates/core/src/routes/embeddings.rs
git commit -m "feat(core): honor X-TokenTrimmer-Timeout-Ms (per-request deadline → 408)"
```

---

### Task 4: Docs

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (§6.1 `X-TokenTrimmer-Timeout-Ms` row)

- [ ] **Step 1: Flip the row**

Replace:
```
| `X-TokenTrimmer-Timeout-Ms` | Per-request timeout override (max 600000) | Planned (not yet honored) | `30000` |
```
with:
```
| `X-TokenTrimmer-Timeout-Ms` | Per-request upstream timeout in ms (1–600000); `408` on expiry. Invalid/over-max values are ignored (the global 600s limit still applies). | Honored | `30000` |
```

- [ ] **Step 2: Commit**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs: mark X-TokenTrimmer-Timeout-Ms honored"
```

---

### Task 5: Gates + finish

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt`
Then: `git diff --quiet || (git add -A && git commit -m "style: cargo fmt")`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: no warnings. Fix any, re-run.

- [ ] **Step 3: Tests**

Run: `cargo test -p tt-core 2>&1 | grep -E "test result:" | tail`
Expected: all pass.

- [ ] **Step 4: Doc gate**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-core --no-deps 2>&1 | tail -10`
Expected: no NEW errors beyond the pre-existing crate-wide unresolved-link warnings (not a CI gate).

- [ ] **Step 5: Advisories**

Run: `cargo deny check advisories 2>&1 | tail -5`
Expected: ok.

- [ ] **Step 6: Commit any residual gate fixes**

```bash
git status --porcelain
```
```
