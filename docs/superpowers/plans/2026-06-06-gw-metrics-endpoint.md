# Gateway `/metrics` Prometheus endpoint Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose a real `GET /metrics` Prometheus endpoint and instrument the gateway hot paths (RED + cache/failover/provider-latency/catalog-miss).

**Architecture:** A process-global Prometheus recorder installed idempotently from `build_router` (public repo is a library; serving binary is in cloud). Metric emission via the `metrics` facade macros at event sites in `crates/core`. `/metrics` is auth-exempt (auth passes through tokenless requests, like `/health`). No `AppState` change — the handle is a `OnceLock`.

**Tech Stack:** Rust, axum, `metrics` (facade) + `metrics-exporter-prometheus` (exporter).

Spec: `docs/superpowers/specs/2026-06-06-gw-metrics-endpoint-design.md`

> **CI note (per `ci-verify-all-targets` memory):** this slice adds a workspace dep + a route. Before the final push, run CI's exact gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-run`. `cargo build` alone does NOT compile test targets.

---

### Task 1: Foundation — deps, `metrics` module, `/metrics` route + handler

**Files:**
- Modify: `Cargo.toml` (root, `[workspace.dependencies]`)
- Modify: `crates/core/Cargo.toml`
- Create: `crates/core/src/metrics.rs`
- Create: `crates/core/src/routes/metrics.rs`
- Modify: `crates/core/src/lib.rs` (declare `pub mod metrics;`)
- Modify: `crates/core/src/routes/mod.rs` (declare `pub mod metrics;`)
- Modify: `crates/core/src/server.rs` (register route + call `install()`)
- Test: `crates/core/tests/metrics_endpoint.rs`

- [ ] **Step 1: Add workspace deps**

In the root `Cargo.toml` `[workspace.dependencies]` section, add:

```toml
metrics = "0.24"
metrics-exporter-prometheus = { version = "0.16", default-features = false }
```

(If `cargo build` later reports a version-resolution conflict, adjust both to the latest compatible pair — `metrics-exporter-prometheus` pins a specific `metrics` minor.)

- [ ] **Step 2: Add deps to tt-core**

In `crates/core/Cargo.toml` under `[dependencies]`, add:

```toml
metrics.workspace = true
metrics-exporter-prometheus.workspace = true
```

- [ ] **Step 3: Create the metrics module**

Create `crates/core/src/metrics.rs`:

```rust
//! Process-global Prometheus recorder + render helpers.
//!
//! The public crate is a library; the serving binary lives in the cloud repo.
//! So the recorder self-installs from `build_router` (idempotent via `OnceLock`)
//! and `/metrics` renders the shared handle. Emission elsewhere uses the
//! `metrics` facade macros, which are no-ops until `install()` has run.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use metrics_exporter_prometheus::PrometheusHandle;

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Install the global Prometheus recorder exactly once per process. Idempotent:
/// safe to call from every `build_router` invocation (including many per test
/// binary). The `OnceLock` guard guarantees the body — and therefore
/// `install_recorder()` / `set_global_recorder` — runs only once.
pub fn install() {
    HANDLE.get_or_init(|| {
        START.get_or_init(Instant::now);
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .set_buckets(&[
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ])
            .expect("static bucket list is valid")
            .install_recorder()
            .expect("global metrics recorder not already set");
        metrics::gauge!("tt_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);
        handle
    });
}

/// Render the current Prometheus text exposition, or `None` if not installed.
pub fn render() -> Option<String> {
    HANDLE.get().map(|h| h.render())
}

/// Seconds since the recorder was installed (for `process_uptime_seconds`).
pub fn uptime_seconds() -> f64 {
    START.get().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0)
}

/// Record a provider-dispatch latency sample. DRY helper for the dispatch sites.
pub fn record_provider_latency(provider: &'static str, operation: &'static str, dur: Duration) {
    metrics::histogram!(
        "provider_request_duration_seconds",
        "provider" => provider,
        "operation" => operation,
    )
    .record(dur.as_secs_f64());
}
```

- [ ] **Step 4: Create the route handler**

Create `crates/core/src/routes/metrics.rs`:

```rust
//! `GET /metrics` — Prometheus text exposition for ops scraping.
//!
//! Unauthenticated (like `/health`): a Prometheus scraper sends no bearer
//! token, and the auth middleware passes tokenless requests through. Operators
//! should restrict this endpoint at the network / reverse-proxy layer.

use axum::http::{header::CONTENT_TYPE, StatusCode};
use axum::response::IntoResponse;

pub async fn handler() -> impl IntoResponse {
    metrics::gauge!("process_uptime_seconds").set(crate::metrics::uptime_seconds());
    match crate::metrics::render() {
        Some(body) => ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
```

- [ ] **Step 5: Declare the modules**

In `crates/core/src/lib.rs`, add (near the other `pub mod` declarations):

```rust
pub mod metrics;
```

In `crates/core/src/routes/mod.rs`, add (near the other route module declarations):

```rust
pub mod metrics;
```

- [ ] **Step 6: Register the route + install the recorder**

In `crates/core/src/server.rs`, inside `build_router_with_retrieval`, make the FIRST statement install the recorder, and add the `/metrics` route to the `base` router right after the `/health` route.

Add as the first line of the `build_router_with_retrieval` body:
```rust
    crate::metrics::install();
```

And change the `/health` registration:
```rust
        .route("/health", get(routes::health::handler))
```
to:
```rust
        .route("/health", get(routes::health::handler))
        .route("/metrics", get(routes::metrics::handler))
```

- [ ] **Step 7: Write the endpoint test**

Create `crates/core/tests/metrics_endpoint.rs`:

```rust
//! Integration tests for the `/metrics` Prometheus endpoint.
//!
//! The global recorder is shared across this test binary, so assertions check
//! metric/label PRESENCE, not exact counts.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;
use tt_core::{build_router, AppState, ProviderRegistry};

fn router() -> axum::Router {
    build_router(AppState::new(ProviderRegistry::new()))
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let (status, headers, body) = get(router(), "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/plain; version=0.0.4"
    );
    assert!(body.contains("tt_build_info"), "build_info missing: {body}");
    assert!(
        body.contains("process_uptime_seconds"),
        "uptime missing: {body}"
    );
}

#[tokio::test]
async fn render_is_some_after_build_router() {
    let _ = router();
    assert!(tt_core::metrics::render().is_some());
}
```

- [ ] **Step 8: Build + run the test**

Run: `cargo build -p tt-core 2>&1 | tail -5`
Expected: builds clean. If a metrics version conflict appears, adjust the version pair (Step 1) and rebuild.

Run: `cargo test -p tt-core --test metrics_endpoint 2>&1 | tail -15`
Expected: PASS — both tests green.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml crates/core/Cargo.toml crates/core/src/metrics.rs crates/core/src/routes/metrics.rs crates/core/src/lib.rs crates/core/src/routes/mod.rs crates/core/src/server.rs crates/core/tests/metrics_endpoint.rs
git commit -m "feat(metrics): /metrics Prometheus endpoint + recorder install

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

(If `Cargo.lock` changed, also `git add Cargo.lock`.)

---

### Task 2: RED metrics in the latency middleware

**Files:**
- Modify: `crates/core/src/middleware/latency.rs`
- Test: `crates/core/tests/metrics_endpoint.rs` (add one test)

- [ ] **Step 1: Write the failing test**

Append to `crates/core/tests/metrics_endpoint.rs` (before EOF), reusing the `router`/`get` helpers:

```rust
#[tokio::test]
async fn http_request_metrics_recorded_for_health() {
    let app = router();
    // Drive one request through the stack so the latency middleware records it.
    let (s, _h, _b) = get(app.clone(), "/health").await;
    assert_eq!(s, StatusCode::OK);
    // Now scrape.
    let (_s, _h, body) = get(app, "/metrics").await;
    assert!(
        body.contains("http_requests_total"),
        "http_requests_total missing: {body}"
    );
    assert!(
        body.contains("http_request_duration_seconds"),
        "duration histogram missing: {body}"
    );
    assert!(
        body.contains("endpoint=\"/health\""),
        "matched-path label missing: {body}"
    );
}
```

- [ ] **Step 2: Run to confirm it fails**

Run: `cargo test -p tt-core --test metrics_endpoint http_request_metrics_recorded_for_health 2>&1 | tail -15`
Expected: FAIL — `http_requests_total` is absent (latency.rs doesn't emit metrics yet).

- [ ] **Step 3: Instrument the latency middleware**

Replace the body of `middleware` in `crates/core/src/middleware/latency.rs`. The current function is:

```rust
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

Replace it with (capture method + matched path BEFORE `next.run` consumes `req`):

```rust
pub async fn middleware(req: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let method = req.method().as_str().to_owned();
    let endpoint = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());

    let mut response = next.run(req).await;

    let elapsed = started.elapsed();
    let status = response.status().as_u16().to_string();
    metrics::counter!(
        "http_requests_total",
        "method" => method.clone(),
        "endpoint" => endpoint.clone(),
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "endpoint" => endpoint,
    )
    .record(elapsed.as_secs_f64());

    let ms = elapsed.as_millis();
    if let Ok(value) = HeaderValue::from_str(&ms.to_string()) {
        response.headers_mut().insert(LATENCY_HEADER, value);
    }
    response
}
```

- [ ] **Step 4: Run to confirm it passes**

Run: `cargo test -p tt-core --test metrics_endpoint 2>&1 | tail -15`
Expected: PASS — all three metrics tests green.

Note on `MatchedPath`: it is populated by axum routing and is available to middleware added via `Router::layer` (as `latency::middleware` is at `server.rs:88`). If the `endpoint="/health"` assertion fails because the extension is absent, the layer ordering differs from expected — switch to reading the matched path in `trace::middleware` instead (same approach), since that layer is also router-level; do not fall back to the raw `uri().path()` (cardinality risk).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/middleware/latency.rs crates/core/tests/metrics_endpoint.rs
git commit -m "feat(metrics): record http RED metrics in latency middleware

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Domain instrumentation (cache, failover, provider latency, catalog miss)

**Files:**
- Modify: `crates/core/src/routes/chat.rs`
- Modify: `crates/core/src/failover.rs`
- Modify: `crates/core/src/routes/embeddings.rs`
- Test: `crates/core/tests/metrics_endpoint.rs` (add one integration test)

For each instrumentation point below, READ the file to locate the exact branch, then insert the shown `metrics::` macro line. These are additive insertions (no logic change). `provider.id()` returns `&'static str`; `response.model` is a `String`.

- [ ] **Step 1: Cache hit/miss counters (`chat.rs`)**

At the **L1** lookup (around `chat.rs:1035`, the `l1.cache.get(...)` result): on the hit arm (where the code builds the L1 hit response / `Ok(Some(...))`), insert:
```rust
metrics::counter!("cache_lookups_total", "tier" => "l1", "result" => "hit").increment(1);
```
On the corresponding L1 miss arm (`Ok(None)` — execution falls through to compute/L2), insert:
```rust
metrics::counter!("cache_lookups_total", "tier" => "l1", "result" => "miss").increment(1);
```
At the **L2** lookup (around `chat.rs:1072`, `l2.cache.lookup(...)`): on the hit arm (`Ok(Some((entry, _)))`):
```rust
metrics::counter!("cache_lookups_total", "tier" => "l2", "result" => "hit").increment(1);
```
on the L2 miss arm (`Ok(None)`):
```rust
metrics::counter!("cache_lookups_total", "tier" => "l2", "result" => "miss").increment(1);
```
Only place counters where an L1/L2 cache is actually configured (inside the existing `if let Some(l1)`/`if let Some(l2)` blocks) so unconfigured deployments don't emit misleading misses.

- [ ] **Step 2: Provider failover counter (`failover.rs`)**

In the fallback-eligible error branch (`failover.rs:216-223`, `Err(e) if e.is_fallback_eligible() =>`), where `breaker.record_failure(provider.id(), now)` is called before continuing to the next candidate, insert:
```rust
metrics::counter!("provider_failover_total", "from" => provider.id()).increment(1);
```

- [ ] **Step 3: Per-provider latency at the 4 dispatch sites**

Wrap each provider dispatch with a timer and call the helper after it returns (regardless of Ok/Err). The pattern, applied at each site:
```rust
let __started = std::time::Instant::now();
let <existing binding> = <existing dispatch expression>;
crate::metrics::record_provider_latency(provider.id(), "<op>", __started.elapsed());
```
Apply at:
- `chat.rs:1199` (non-streaming `with_retry(|| provider.chat_completion(...))`) — op `"chat"`.
- `chat.rs:847` (streaming `with_retry(|| provider.chat_completion_stream(...))`) — op `"chat_stream"`.
- `failover.rs:207` (`with_retry(|| provider.chat_completion(...))` in the candidate loop) — op `"chat"`.
- `embeddings.rs:227` (`provider.embeddings(req, &ctx).await`) — op `"embeddings"`.

For the streaming site (`chat.rs:847`) the timer measures time-to-stream-handle (until the stream is returned), which is the meaningful dispatch latency; do not try to time the whole stream body. At each site `provider` is the `&dyn Provider`/`Arc<dyn Provider>` already in scope; if the binding name differs (e.g. `prov`), use whatever resolves to the provider with `.id()`.

- [ ] **Step 4: Catalog zero-price counter (`chat.rs:1300`)**

In the `if pricing.is_none() {` block (around `chat.rs:1300`), alongside the existing `tracing::warn!`, insert:
```rust
metrics::counter!(
    "catalog_zero_price_total",
    "provider" => provider.id(),
    "model" => response.model.clone(),
)
.increment(1);
```

- [ ] **Step 5: Write an integration test for provider latency + catalog miss**

This test drives a chat request through an in-test provider whose `pricing()` returns `None` (so the catalog-miss path fires) and asserts both `provider_request_duration_seconds` and `catalog_zero_price_total` appear in `/metrics`. Append to `crates/core/tests/metrics_endpoint.rs`. Model the provider + request wiring on `crates/core/tests/retrieval_isolation.rs` (EchoProvider + `ApiKeyContext` extension injection). Read that file for the exact `Provider` trait impl shape, then add:

```rust
// ── No-pricing echo provider: forces the catalog-miss path ──────────────────
mod nopricing {
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use tt_shared::messages::{Choice, Message, MessageContent};
    use tt_shared::pricing::Capability;
    use tt_shared::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
        EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
    };

    pub struct NoPricingEcho;

    #[async_trait]
    impl Provider for NoPricingEcho {
        fn id(&self) -> &'static str {
            "nopricing"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "np-1".into(),
                provider: "nopricing".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            }]
        }
        // The whole point: no price for any model → catalog-miss path fires.
        fn pricing(&self, _: &str) -> Option<ModelPricing> {
            None
        }
        async fn chat_completion(
            &self,
            req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<ChatCompletionResponse, ProviderError> {
            Ok(ChatCompletionResponse {
                id: "chatcmpl-np".into(),
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
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_creation_input_tokens: None,
                },
            })
        }
        async fn chat_completion_stream(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            Err(ProviderError::Unsupported("n/a".into()))
        }
        async fn embeddings(
            &self,
            _req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Err(ProviderError::Unsupported("n/a".into()))
        }
    }
}

#[tokio::test]
async fn provider_and_catalog_metrics_recorded() {
    use std::sync::Arc;
    use tt_auth::ApiKeyContext;
    use tt_core::{ProviderRegistry, RetrievalState};
    use uuid::Uuid;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(nopricing::NoPricingEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({
        "model": "np-1",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut().insert(ApiKeyContext {
        key_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        tier: None,
    });
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    assert!(
        metrics_body.contains("provider_request_duration_seconds"),
        "provider latency missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("catalog_zero_price_total"),
        "catalog miss missing: {metrics_body}"
    );
}
```

If the `use tt_core::{... RetrievalState}` import is unused (the request path does not require building a `RetrievalState`), drop it — keep imports to what compiles. Adapt the `ChatCompletionRequest`/response field set to match the current `tt_shared` definitions if they differ from the snippet (read `retrieval_isolation.rs`'s EchoProvider as the source of truth).

- [ ] **Step 6: Run the tests**

Run: `cargo test -p tt-core --test metrics_endpoint 2>&1 | tail -20`
Expected: PASS — including `provider_and_catalog_metrics_recorded`.

Note: cache (`cache_lookups_total`) and failover (`provider_failover_total`) counters are exercised at runtime by the existing `crates/core/tests/l1_cache_hit.rs`, `l2_cache_hit.rs`, and failover integration tests (their request paths now hit the new counters); this task does not add dedicated assertions for them because wiring a live cache/failover topology into a fresh test is heavy and the macro placement is a trivial counter increment at a clearly-identified branch.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/src/failover.rs crates/core/src/routes/embeddings.rs crates/core/tests/metrics_endpoint.rs
git commit -m "feat(metrics): instrument cache, failover, provider latency, catalog miss

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Documentation

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (§17)
- Modify: `docs/tokentrimmer-architecture-spec-v1.md` (§17.2)

- [ ] **Step 1: Update the API reference §17**

In `docs/04-gateway-api-reference.md`, replace the planned-metrics note:

```markdown
> **Planned:** `GET /metrics` (Prometheus exposition) is not yet implemented — tracked as `gw-metrics-endpoint` in the backlog. The telemetry crate already maintains the underlying counters.
```

with:

```markdown
A `/metrics` endpoint is exposed for ops (Prometheus exposition format):

```
GET /metrics
→ 200 text/plain; version=0.0.4
```

Exposed metric families:

| Metric | Type | Labels |
|--------|------|--------|
| `http_requests_total` | counter | `method`, `endpoint`, `status` |
| `http_request_duration_seconds` | histogram | `method`, `endpoint` |
| `cache_lookups_total` | counter | `tier` (`l1`/`l2`), `result` (`hit`/`miss`) |
| `provider_failover_total` | counter | `from` |
| `provider_request_duration_seconds` | histogram | `provider`, `operation` |
| `catalog_zero_price_total` | counter | `provider`, `model` |
| `tt_build_info` | gauge | `version` |
| `process_uptime_seconds` | gauge | — |

> **Operator note:** `/metrics` is unauthenticated (like `/health`). Restrict it at the network / reverse-proxy layer so internal ops counters are not publicly scrapeable.
```

(Keep the surrounding `/health` text intact; this replaces only the `> **Planned:**` block.)

- [ ] **Step 2: Update the architecture spec §17.2**

In `docs/tokentrimmer-architecture-spec-v1.md` §17.2, the current lines are:

```markdown
- Prometheus exposition format
- Key metrics: requests/sec, p50/p95/p99 latency, cache hit rate, error rate by class, provider latency
- Scraped by Grafana Cloud free tier or self-hosted Prometheus + Grafana
```

Replace with (mark live + reflect the actual metric families):

```markdown
- Prometheus exposition format — **implemented**: `GET /metrics` (see gateway API reference §17)
- Metrics: `http_requests_total` + `http_request_duration_seconds` (rate / error / latency by method+endpoint+status), `cache_lookups_total` (hit rate by tier), `provider_failover_total`, `provider_request_duration_seconds` (per-provider latency), `catalog_zero_price_total`, `tt_build_info`, `process_uptime_seconds`
- Scraped by Grafana Cloud free tier or self-hosted Prometheus + Grafana
```

- [ ] **Step 3: Verify the docs render (no broken markdown)**

Run: `grep -n "GET /metrics\|http_requests_total" docs/04-gateway-api-reference.md docs/tokentrimmer-architecture-spec-v1.md`
Expected: the new lines appear in both files.

- [ ] **Step 4: Commit**

```bash
git add docs/04-gateway-api-reference.md docs/tokentrimmer-architecture-spec-v1.md
git commit -m "docs(metrics): flip /metrics to implemented in API ref + arch spec

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)

Run CI's exact gates (per `ci-verify-all-targets`):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
cargo test -p tt-core
```

Expected: fmt clean, clippy clean (whole workspace, all targets), all test targets compile, tt-core tests pass. If `cargo fmt --all -- --check` shows a diff, run `cargo fmt -p tt-core` and re-stage only the affected files.

## Notes for the implementer
- `metrics` facade macros are no-ops until `install()` runs; in tests, `build_router` installs it.
- The global recorder is shared across the whole test binary — assert metric/label PRESENCE, never exact counts.
- Do NOT add an `AppState` field for the handle (it is a `OnceLock` global) — that would churn every `AppState` constructor.
- Stage only the files each task lists; do not whole-workspace `cargo fmt`.
