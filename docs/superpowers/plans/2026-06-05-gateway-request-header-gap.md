# Gateway Request-Header Gap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Honor `X-TokenTrimmer-Cost-Limit-Usd` (request → 402) and `X-TokenTrimmer-Latency-Ms` (response, every request) on the chat + embeddings endpoints, and correct `docs/04-gateway-api-reference.md` so the remaining unimplemented headers are marked "Planned".

**Architecture:** Two `pub(crate)` cost-limit helpers in `chat.rs` reused by both handlers; latency is a response middleware mirroring the `trace` middleware; the docs are edited to match reality.

**Tech Stack:** Rust, axum 0.7 (gateway middleware via `from_fn`), tt-tokenize, the gateway `app_with_mock` test harness.

Spec: `docs/superpowers/specs/2026-06-05-gateway-request-header-gap-design.md`. Branch `gateway-header-gap` (off `main`, spec committed).

**Verified anchors:**
- `estimate_cost_usd` (now `pub(crate)`) — chat.rs:60; `ApiError::CostLimitExceeded { estimated_usd, ceiling_usd }` → 402 (error.rs:115); `last_user_message_text(&ChatCompletionRequest) -> Option<&str>` — chat.rs:1278; `HeaderMap` imported (chat.rs:22), `Instant` (chat.rs:18), `tt_tokenize` in use.
- Cost-limit insertion point: chat.rs after the `if matched_route_id.is_some()` block closes (line ~472), before the `// For a failover chain` comment (~474).
- Latency middleware mirrors `crates/core/src/middleware/trace.rs`; router layers at `server.rs:69` (`.layer(axum::middleware::from_fn(middleware::trace::middleware))`).
- Embeddings handler: `routes/embeddings.rs`, `input_as_text(&req.input)` already defined; cost-limit goes after the routing block, before `// 6. Dispatch`.

---

### Task 1: Cost-limit helpers + chat wiring

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (helpers after `estimate_cost_usd` ~line 65; wiring after the route block ~472)
- Test: `crates/core/src/server.rs` (`mod tests`)

- [ ] **Step 1: Add the two helpers**

In `crates/core/src/routes/chat.rs`, immediately after the `estimate_cost_usd` fn (ends ~line 65), add:

```rust
/// Parse `X-TokenTrimmer-Cost-Limit-Usd` (a positive USD ceiling), if present
/// and well-formed. Malformed / non-positive values are ignored (no limit).
pub(crate) fn cost_limit_from_header(headers: &HeaderMap) -> Option<f64> {
    headers
        .get("x-tokentrimmer-cost-limit-usd")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
}

/// Reject with 402 when the estimated request cost exceeds the header limit.
/// Permissive when pricing is unknown (can't prove an exceedance) — same
/// semantics as the route `max_cost_usd` ceiling.
pub(crate) fn enforce_cost_limit(
    limit: Option<f64>,
    pricing: Option<&ModelPricing>,
    input_tokens: u32,
    max_tokens: Option<u32>,
) -> ApiResult<()> {
    if let (Some(limit), Some(pr)) = (limit, pricing) {
        let est = estimate_cost_usd(pr, input_tokens, max_tokens);
        if est > limit {
            return Err(ApiError::CostLimitExceeded {
                estimated_usd: est,
                ceiling_usd: limit,
            });
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Wire the check into the chat handler**

In `crates/core/src/routes/chat.rs`, find the `// For a failover chain` comment (~line 474, right after the `if matched_route_id.is_some()` block closes). Insert immediately BEFORE it:

```rust
    // Per-request cost ceiling from the `X-TokenTrimmer-Cost-Limit-Usd` header.
    // Applies to every request (routed or not), priced on the final model.
    {
        let cl_input_tokens = last_user_message_text(&req)
            .map(|s| tt_tokenize::estimate_tokens(provider.id(), s))
            .unwrap_or(0);
        enforce_cost_limit(
            cost_limit_from_header(&headers),
            provider.pricing(&req.model).as_ref(),
            cl_input_tokens,
            req.max_tokens,
        )?;
    }

```

- [ ] **Step 3: Write the chat tests**

In `crates/core/src/server.rs` `mod tests`, add (near the other dispatch tests; `app_with_mock`, `StatusCode`, `Request`, `Body`, `serde_json`, `ServiceExt`/`oneshot` are already in scope):

```rust
    fn chat_request_with(model: &str, max_tokens: u32, cost_limit: Option<&str>) -> Request<Body> {
        let body = serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": "the quick brown fox" }],
            "max_tokens": max_tokens,
            "stream": false,
        });
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json");
        if let Some(cl) = cost_limit {
            b = b.header("x-tokentrimmer-cost-limit-usd", cl);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn cost_limit_header_rejects_over_limit() {
        // mock pricing $1/M in, $2/M out; max_tokens 1000 → est ≈ $0.002 > 1e-9.
        let response = app_with_mock()
            .oneshot(chat_request_with("mock-model-1", 1000, Some("0.000000001")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "cost_limit_exceeded");
    }

    #[tokio::test]
    async fn cost_limit_header_allows_under_limit() {
        let response = app_with_mock()
            .oneshot(chat_request_with("mock-model-1", 1000, Some("100")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cost_limit_header_absent_is_noop() {
        let response = app_with_mock()
            .oneshot(chat_request_with("mock-model-1", 1000, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
```

Note: confirm the 402 error body's `code` is `cost_limit_exceeded` by checking `crates/core/src/error.rs` (the `ApiError::CostLimitExceeded` arm). If the code string differs, match it.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tt-core --lib cost_limit`
Expected: all three pass.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/src/server.rs
git commit -m "feat(core): honor X-TokenTrimmer-Cost-Limit-Usd (chat → 402)"
```

---

### Task 2: Latency middleware

**Files:**
- Create: `crates/core/src/middleware/latency.rs`
- Modify: `crates/core/src/middleware/mod.rs` (add `pub mod latency;`)
- Modify: `crates/core/src/server.rs` (`build_router_with_retrieval` layer + a test)

- [ ] **Step 1: Create the middleware**

Create `crates/core/src/middleware/latency.rs`:

```rust
//! Response-latency middleware: times every request and sets
//! `X-TokenTrimmer-Latency-Ms` on the response (present on every response —
//! success, cache hit, sandbox, or error). Mirrors [`super::trace`].

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};

/// Response header set on every HTTP response.
pub const LATENCY_HEADER: HeaderName = HeaderName::from_static("x-tokentrimmer-latency-ms");

/// Axum `from_fn`-compatible middleware: stamps wall-clock request latency (ms)
/// onto the response.
pub async fn middleware(req: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let mut response = next.run(req).await;
    let ms = started.elapsed().as_millis();
    if let Ok(value) = HeaderValue::from_str(&ms.to_string()) {
        response.headers_mut().insert(LATENCY_HEADER, value);
    }
    response
}
```

- [ ] **Step 2: Export the module**

In `crates/core/src/middleware/mod.rs`, add to the `pub mod` list (after `pub mod key_cache;` or alphabetically):

```rust
pub mod latency;
```

(Optionally add a one-line doc bullet to the module-doc list mirroring the others; not required to compile.)

- [ ] **Step 3: Wire the layer into the router**

In `crates/core/src/server.rs` `build_router_with_retrieval`, add the latency layer next to the trace layer (~line 69):

```rust
    .layer(axum::middleware::from_fn(middleware::trace::middleware))
    .layer(axum::middleware::from_fn(middleware::latency::middleware))
```

- [ ] **Step 4: Write the test**

In `crates/core/src/server.rs` `mod tests`, add:

```rust
    #[tokio::test]
    async fn latency_header_present_on_success_and_error() {
        // Success (dispatch) — header present + parseable.
        let ok = app_with_mock()
            .oneshot(chat_request("mock-model-1", false))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let ms: u64 = ok.headers()["x-tokentrimmer-latency-ms"]
            .to_str()
            .unwrap()
            .parse()
            .expect("latency-ms parseable");
        let _ = ms; // any non-negative value is acceptable

        // Error (unknown model → 404) — middleware still stamps the header.
        let err = app_with_mock()
            .oneshot(chat_request("does-not-exist", false))
            .await
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert!(
            err.headers().contains_key("x-tokentrimmer-latency-ms"),
            "latency header must be present even on error responses"
        );
    }
```

(`chat_request(model, stream)` already exists in the test module — server.rs:264.)

- [ ] **Step 5: Run the test**

Run: `cargo test -p tt-core --lib latency_header_present_on_success_and_error`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/middleware/latency.rs crates/core/src/middleware/mod.rs crates/core/src/server.rs
git commit -m "feat(core): X-TokenTrimmer-Latency-Ms response middleware"
```

---

### Task 3: Embeddings cost-limit wiring

**Files:**
- Modify: `crates/core/src/routes/embeddings.rs` (import helpers + check before dispatch)
- Test: `crates/core/src/server.rs` (`mod tests`)

- [ ] **Step 1: Import the helpers**

In `crates/core/src/routes/embeddings.rs`, extend the `use crate::routes::chat::{…}` import to include the two new helpers:

```rust
use crate::routes::chat::{
    apply_routing, attach_cost_headers, compute_cost, cost_limit_from_header, enforce_cost_limit,
    estimate_cost_usd, resolve_credentials, resolve_credentials_for,
};
```

Also ensure `tt_tokenize` is reachable — add `use tt_tokenize;` is NOT needed (call fully-qualified `tt_tokenize::estimate_tokens`); confirm `tt-tokenize` is a dependency of `tt-core` (it is — chat.rs uses it).

- [ ] **Step 2: Add the cost-limit check before dispatch**

In `crates/core/src/routes/embeddings.rs`, find the `// 6. Dispatch.` comment. Insert immediately BEFORE it:

```rust
    // Per-request cost ceiling from the `X-TokenTrimmer-Cost-Limit-Usd` header,
    // priced on the final (post-routing) embedding model. Output tokens are 0.
    {
        let cl_input_tokens = tt_tokenize::estimate_tokens(provider.id(), &input_as_text(&req.input));
        enforce_cost_limit(
            cost_limit_from_header(&headers),
            provider.pricing(&req.model).as_ref(),
            cl_input_tokens,
            None,
        )?;
    }

```

- [ ] **Step 3: Write the test**

In `crates/core/src/server.rs` `mod tests`, add:

```rust
    #[tokio::test]
    async fn embeddings_cost_limit_rejects_over_limit() {
        let body = serde_json::json!({
            "model": "mock-model-1",
            "input": "the quick brown fox jumps over the lazy dog"
        });
        let response = app_with_mock()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings")
                    .header("content-type", "application/json")
                    .header("x-tokentrimmer-cost-limit-usd", "0.000000001")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    }
```

- [ ] **Step 4: Run the test + the embeddings suite**

Run: `cargo test -p tt-core --lib embeddings`
Expected: the new `embeddings_cost_limit_rejects_over_limit` passes alongside the existing dispatch/sandbox tests.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/embeddings.rs crates/core/src/server.rs
git commit -m "feat(core): honor X-TokenTrimmer-Cost-Limit-Usd on /v1/embeddings"
```

---

### Task 4: Doc-fix `04-gateway-api-reference.md`

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (§6.1 ~403-412, §6.2 ~416-429)

- [ ] **Step 1: Mark request headers (§6.1)**

In `docs/04-gateway-api-reference.md`, replace the §6.1 request-headers table (lines ~403-412) with one that adds a **Status** column, marking honored vs planned:

```markdown
| Header | Purpose | Status | Example |
|---|---|---|---|
| `X-TokenTrimmer-Tag` | Free-form tag for cost attribution | Honored | `feature=chat-support,user=u_123` |
| `X-TokenTrimmer-Cost-Limit-Usd` | Reject (402) if estimated cost > limit | Honored | `0.05` |
| `X-TokenTrimmer-Cache` | Override cache behavior for this request | Planned (not yet honored) | `bypass` / `force-write` / `read-only` / `disabled` |
| `X-TokenTrimmer-Route` | Force a specific named route | Planned (not yet honored) | `cheap-for-short` |
| `X-TokenTrimmer-Provider` | Override provider selection | Planned (not yet honored) | `anthropic` |
| `X-TokenTrimmer-Fallback` | Comma-separated fallback chain override | Planned (not yet honored) | `openai/gpt-4o,anthropic/claude-3-5-sonnet` |
| `X-TokenTrimmer-Timeout-Ms` | Per-request timeout override (max 600000) | Planned (not yet honored) | `30000` |
| `X-TokenTrimmer-Trace-Parent` | W3C traceparent for distributed tracing | Planned (not yet honored) | (standard format) |
```

- [ ] **Step 2: Mark response headers (§6.2)**

Replace the §6.2 response-headers table (lines ~416-427) so the always-present column reflects reality and adds the latency row:

```markdown
| Header | Always present | Example |
|---|---|---|
| `X-TokenTrimmer-Trace-Id` | yes (every response) | `5f3a1c...` |
| `X-TokenTrimmer-Latency-Ms` | yes (every response) | `412` |
| `X-TokenTrimmer-Provider` | on dispatched/cached responses | `anthropic` |
| `X-TokenTrimmer-Model-Used` | on dispatched/cached responses | `claude-3-5-haiku-20241022` |
| `X-TokenTrimmer-Cache` | on dispatched/cached responses | `hit-l1` / `hit-l2` / `miss` / `none` |
| `X-TokenTrimmer-Cost-Usd` | on dispatched/cached responses | `0.0034` |
| `X-TokenTrimmer-Baseline-Cost-Usd` | on dispatched/cached responses | `0.0218` |
| `X-TokenTrimmer-Saved-Usd` | on dispatched/cached responses | `0.0184` |
| `X-TokenTrimmer-Route-Matched` | Planned (not yet emitted) | `cheap-for-short` |
| `X-TokenTrimmer-Warnings` | Planned (not yet emitted) | `param_dropped:frequency_penalty` |
```

- [ ] **Step 3: Correct the "always present" sentence**

Replace the line after the §6.2 table — "Customers can rely on these headers being present in every response (success or error) for telemetry purposes." — with:

```markdown
`X-TokenTrimmer-Trace-Id` and `X-TokenTrimmer-Latency-Ms` are present on every
response (success or error). The cost/provider/model/cache headers are attached on
responses that reach dispatch, cache, or the sandbox path; they are not emitted on
early validation errors (4xx returned before dispatch).
```

- [ ] **Step 4: Commit**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs: mark unimplemented gateway headers as Planned; honor Cost-Limit-Usd + Latency-Ms"
```

---

### Task 5: Gates + finish the branch

**Files:** none (verification + PR)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt --all`
Then: `git diff --quiet || git commit -am "style: cargo fmt"`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0. Fix anything flagged and re-run.

- [ ] **Step 2: Tests + advisories**

Run: `cargo test -p tt-core`
Expected: all pass (the three cost-limit tests, the latency test, the embeddings cost-limit test, and the full pre-existing suite).
Run: `cargo deny check advisories`
Expected: ok.

- [ ] **Step 3: Finish the branch**

Use the **superpowers:finishing-a-development-branch** skill: verify tests, push `gateway-header-gap`, create the PR (option 2). PR body: the two honored headers (Cost-Limit-Usd → 402; Latency-Ms middleware) on chat + embeddings, and the doc-fix marking the rest "Planned".

- [ ] **Step 4: Adversarial review + CI**

After the PR is open, run a Workflow-based adversarial review (lenses: cost-limit correctness/billing — estimate, permissiveness, 402 mapping, placement vs routing; middleware correctness — header on all paths, no perf/ordering issue; doc accuracy — every table row matches code) with per-finding verification against the real source. Watch CI; fix confirmed findings before merge. Update roadmap memory when green — this completes the post-roadmap follow-up queue.

---

## Notes for the implementer

- **Cost-limit placement:** after routing (so it prices the final model), before failover/cache/dispatch. It is independent of the route's own `max_cost_usd` ceiling — both can apply.
- **Permissive on unknown pricing:** `enforce_cost_limit` only rejects when pricing is known and the estimate exceeds the limit, matching the route-ceiling semantics (never reject on data we don't have).
- **Latency is middleware, not per-handler:** the `attach_cost_headers` sites live in free functions without `request_started` in scope; the middleware covers every response uniformly (and makes the "always present" doc claim true).
- **Test token estimate:** the mock provider id is `mock`, so `tt_tokenize::estimate_tokens("mock", …)` uses the chars/4 heuristic — a multi-word input guarantees ≥1 token, so a `1e-9` limit reliably trips the 402 while a `100` limit passes.
