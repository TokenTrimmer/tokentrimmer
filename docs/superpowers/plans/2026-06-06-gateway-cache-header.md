# Honor `X-TokenTrimmer-Cache` (F7) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Honor the `X-TokenTrimmer-Cache` request header on `/v1/chat/completions` (modes: disabled / read-only / bypass / force-write), overriding the request-body cache decision but never the privacy-route `disable_cache`, and never caching tool-call responses.

**Architecture:** A `cache_override_from_header` parser maps the four documented header values to `(do_lookup, do_insert)`, applied in the chat handler between `CacheBehavior::resolve(&req)` and the existing `route_disable_cache` force-off. No `CacheBehavior` field or insert-site change — `force-write` overrides eligibility by being applied after `resolve()`; the tool-call exclusion stays at the insert sites. All in `crates/core/src/routes/chat.rs` + docs.

**Tech Stack:** Rust, axum, tokio, the in-crate cache test harness (`with_l1` + `CountingProvider`).

---

### Task 1: `cache_override_from_header` parser

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add the fn near `CacheBehavior`; unit test in the `provider_override_tests` module or a new module)

- [ ] **Step 1: Write the failing unit test**

Add a test module near the other `#[cfg(test)]` modules in `chat.rs`:

```rust
#[cfg(test)]
mod cache_header_tests {
    use super::*;
    use axum::http::HeaderMap;

    fn hv(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-tokentrimmer-cache", v.parse().unwrap());
        h
    }

    #[test]
    fn cache_override_parsing() {
        assert_eq!(
            cache_override_from_header(&HeaderMap::new()).unwrap(),
            None
        );
        assert_eq!(cache_override_from_header(&hv("disabled")).unwrap(), Some((false, false)));
        assert_eq!(cache_override_from_header(&hv("read-only")).unwrap(), Some((true, false)));
        assert_eq!(cache_override_from_header(&hv("bypass")).unwrap(), Some((false, true)));
        assert_eq!(cache_override_from_header(&hv(" Force-Write ")).unwrap(), Some((true, true)));
        assert_eq!(cache_override_from_header(&hv("   ")).unwrap(), None);
        assert!(cache_override_from_header(&hv("nope")).is_err());
    }
}
```

Run: `cargo test -p tt-core cache_override_parsing 2>&1 | tail -8`
Expected: FAIL to compile — `cache_override_from_header` does not exist.

- [ ] **Step 2: Implement the parser**

Add immediately after the `impl CacheBehavior { … }` block (after `chat.rs:402`):

```rust
/// `X-TokenTrimmer-Cache` → `(do_lookup, do_insert)` per the documented modes.
/// Absent/blank → `None`. Unknown value → `400` (the four values are documented).
fn cache_override_from_header(headers: &HeaderMap) -> ApiResult<Option<(bool, bool)>> {
    let Some(raw) = headers
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    let v = raw.trim().to_ascii_lowercase();
    if v.is_empty() {
        return Ok(None);
    }
    let pair = match v.as_str() {
        "disabled" => (false, false),
        "read-only" => (true, false),
        "bypass" => (false, true),
        "force-write" => (true, true),
        other => {
            return Err(ApiError::InvalidRequest(format!(
                "invalid X-TokenTrimmer-Cache value: {other} (expected disabled, read-only, bypass, or force-write)"
            )))
        }
    };
    Ok(Some(pair))
}
```

Run: `cargo test -p tt-core cache_override_parsing 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "feat(core): add cache_override_from_header parser"
```

---

### Task 2: Integration tests (RED)

**Files:**
- Create: `crates/core/tests/cache_header.rs`

- [ ] **Step 1: Write the test file**

Create `crates/core/tests/cache_header.rs`:

```rust
//! `X-TokenTrimmer-Cache` request-header overrides (F7).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent, ToolCall, ToolCallFunction},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

struct CountingProvider {
    calls: Arc<AtomicUsize>,
    tool_call: bool,
}

#[async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &'static str {
        "counting"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "counting-1".into(),
            provider: "counting".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        let message = if self.tool_call {
            Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: "f".into(),
                        arguments: "{}".into(),
                    },
                }],
                name: None,
            }
        } else {
            Message::Assistant {
                content: Some(MessageContent::Text("live".into())),
                tool_calls: vec![],
                name: None,
            }
        };
        Ok(ChatCompletionResponse {
            id: "chatcmpl-live".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message,
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// App with an L1 cache and a counting provider (no auth required).
fn app_with_l1(tool_call: bool) -> (axum::Router, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
        tool_call,
    }));
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry).with_l1(l1, None);
    (build_router(state), calls)
}

/// Build a chat request with optional cache header / temperature / body cache mode.
fn req(
    cache_header: Option<&str>,
    temperature: Option<f64>,
    body_cache_mode: Option<&str>,
) -> Request<Body> {
    let mut body = json!({
        "model": "counting-1",
        "messages": [{ "role": "user", "content": "hello there" }],
        "stream": false,
    });
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    if let Some(m) = body_cache_mode {
        body["tt_extras"] = json!({ "cache": { "mode": m } });
    }
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(c) = cache_header {
        b = b.header("x-tokentrimmer-cache", c);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn cache_hdr(resp: &axum::http::Response<Body>) -> Option<String> {
    resp.headers()
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

async fn warm(app: &axum::Router) {
    let _ = app.clone().oneshot(req(None, None, None)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn header_read_only_reads_warm_cache() {
    let (app, calls) = app_with_l1(false);
    warm(&app).await; // calls == 1, cache populated
    let r = app
        .clone()
        .oneshot(req(Some("read-only"), None, None))
        .await
        .unwrap();
    assert_eq!(cache_hdr(&r).as_deref(), Some("hit-l1"));
    assert_eq!(calls.load(Ordering::Relaxed), 1, "read-only must read");
}

#[tokio::test]
async fn header_read_only_does_not_write() {
    let (app, calls) = app_with_l1(false);
    // read-only on a cold cache: miss + provider called, nothing written.
    let r = app
        .clone()
        .oneshot(req(Some("read-only"), None, None))
        .await
        .unwrap();
    assert_eq!(cache_hdr(&r).as_deref(), Some("miss"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    // A following normal request still misses (read-only wrote nothing).
    let r2 = app.clone().oneshot(req(None, None, None)).await.unwrap();
    assert_eq!(cache_hdr(&r2).as_deref(), Some("miss"));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn header_disabled_skips_read_and_write() {
    let (app, calls) = app_with_l1(false);
    warm(&app).await; // calls == 1, cache populated
    // disabled must NOT read the warm cache.
    let r = app
        .clone()
        .oneshot(req(Some("disabled"), None, None))
        .await
        .unwrap();
    assert_ne!(cache_hdr(&r).as_deref(), Some("hit-l1"));
    assert_eq!(calls.load(Ordering::Relaxed), 2, "disabled must not read");
}

#[tokio::test]
async fn header_bypass_skips_read_but_writes() {
    let (app, calls) = app_with_l1(false);
    warm(&app).await; // calls == 1
    // bypass skips the lookup → provider called again...
    let r = app
        .clone()
        .oneshot(req(Some("bypass"), None, None))
        .await
        .unwrap();
    assert_ne!(cache_hdr(&r).as_deref(), Some("hit-l1"), "bypass skips lookup");
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    tokio::time::sleep(Duration::from_millis(50)).await;
    // ...but it refreshed the cache → a normal request now hits.
    let r2 = app.clone().oneshot(req(None, None, None)).await.unwrap();
    assert_eq!(cache_hdr(&r2).as_deref(), Some("hit-l1"));
    assert_eq!(calls.load(Ordering::Relaxed), 2, "bypass wrote, so no new dispatch");
}

#[tokio::test]
async fn force_write_caches_ineligible_request() {
    let (app, calls) = app_with_l1(false);
    // temperature 0.7 → normally ineligible (no read, no write). force-write forces both.
    let r1 = app
        .clone()
        .oneshot(req(Some("force-write"), Some(0.7), None))
        .await
        .unwrap();
    assert_eq!(cache_hdr(&r1).as_deref(), Some("miss"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r2 = app
        .clone()
        .oneshot(req(Some("force-write"), Some(0.7), None))
        .await
        .unwrap();
    assert_eq!(cache_hdr(&r2).as_deref(), Some("hit-l1"), "force-write cached the ineligible request");
    assert_eq!(calls.load(Ordering::Relaxed), 1, "second force-write read the cache");
}

#[tokio::test]
async fn force_write_does_not_cache_tool_calls() {
    let (app, calls) = app_with_l1(true); // provider returns tool calls
    let _ = app
        .clone()
        .oneshot(req(Some("force-write"), None, None))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = app
        .clone()
        .oneshot(req(Some("force-write"), None, None))
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "tool-call responses must never be cached, even under force-write"
    );
}

#[tokio::test]
async fn header_beats_body_tt_extras() {
    let (app, calls) = app_with_l1(false);
    warm(&app).await; // calls == 1, cache populated
    // Body `tt_extras.cache.mode = "bypass"` (CacheMode::Bypass → no lookup) would
    // skip the read; the header `read-only` reads. Header wins → hit. (Body uses
    // CacheMode names — bypass/read-only/refresh/normal — NOT the header names.)
    let r = app
        .clone()
        .oneshot(req(Some("read-only"), None, Some("bypass")))
        .await
        .unwrap();
    assert_eq!(cache_hdr(&r).as_deref(), Some("hit-l1"), "header read-only beats body bypass");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn invalid_header_value_is_400() {
    let (app, _calls) = app_with_l1(false);
    let r = app
        .clone()
        .oneshot(req(Some("nope"), None, None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn privacy_route_disable_cache_beats_force_write() {
    // Routing requires auth: issue a key for an org and set a disable_cache route.
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
        tool_call: false,
    }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let audit = InMemoryAuditWriter::new();
    let key = issue(&raw, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .unwrap()
        .plaintext;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "privacy".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                tag_equals: Some("sensitive".into()),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "counting-1".into(),
                fallbacks: vec![],
                force_cache_layer: None,
                disable_cache: true,
                max_cost_usd: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing)
            .with_l1(Arc::new(InMemoryL1Cache::new()), None),
    );

    let build = || {
        let body = json!({
            "model": "counting-1",
            "messages": [{ "role": "user", "content": "hello there" }],
            "stream": false,
        });
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .header("x-tokentrimmer-tag", "sensitive")
            .header("x-tokentrimmer-cache", "force-write")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let _ = app.clone().oneshot(build()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = app.clone().oneshot(build()).await.unwrap();
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "privacy route disable_cache must override force-write (nothing cached)"
    );
}
```

- [ ] **Step 2: Run to verify failures (RED)**

Run: `cargo test -p tt-core --test cache_header 2>&1 | tail -30`
Expected: the override tests FAIL (header ignored today): `header_read_only_reads_warm_cache` warm+read-only still behaves per body=normal so it may hit — but `header_disabled_skips_read_and_write` (disabled ignored → reads warm cache → hit, calls==1, assert calls==2 FAILS), `header_bypass_skips_read_but_writes` (bypass ignored → hits, FAILS), `force_write_caches_ineligible_request` (ignored → temp 0.7 ineligible → never cached → r2 miss, FAILS), `invalid_header_value_is_400` (ignored → 200, FAILS), `header_beats_body_tt_extras` (body disabled honored → no read → not hit, FAILS), `privacy_route_disable_cache_beats_force_write` passes already (route disables). Confirm at least the disabled/bypass/force-write/invalid/header-beats-body tests fail.

- [ ] **Step 3: Commit the failing tests**

```bash
git add crates/core/tests/cache_header.rs
git commit -m "test(core): X-TokenTrimmer-Cache header behavior (RED)"
```

---

### Task 3: Wire the handler (GREEN)

**Files:**
- Modify: `crates/core/src/routes/chat.rs:646` (apply the header override between `resolve` and `route_disable_cache`)

- [ ] **Step 1: Apply the override**

Replace:
```rust
    let mut cache_behavior = CacheBehavior::resolve(&req);
    if route_disable_cache {
        cache_behavior.do_lookup = false;
        cache_behavior.do_insert = false;
    }
```
with:
```rust
    let mut cache_behavior = CacheBehavior::resolve(&req);
    // `X-TokenTrimmer-Cache` overrides the request-body decision (header beats
    // body). force-write=(true,true) here overrides the eligibility gate that
    // `resolve()` may have applied; the tool-call exclusion at insert time is
    // unaffected, so tool-call responses are still never cached.
    if let Some((lookup, insert)) = cache_override_from_header(&headers)? {
        cache_behavior.do_lookup = lookup;
        cache_behavior.do_insert = insert;
    }
    // A privacy route's disable_cache wins over both body and header.
    if route_disable_cache {
        cache_behavior.do_lookup = false;
        cache_behavior.do_insert = false;
    }
```

- [ ] **Step 2: Run the integration + unit tests (GREEN)**

Run: `cargo test -p tt-core --test cache_header 2>&1 | tail -20`
Expected: all 9 pass.

Run: `cargo test -p tt-core cache_override_parsing cache_header 2>&1 | grep -E "test result:" | tail`
Expected: pass.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "feat(core): honor X-TokenTrimmer-Cache header (route privacy wins, header beats body)"
```

---

### Task 4: Docs

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (§6.1 row line 407; §6.3 prose lines 436-440)

- [ ] **Step 1: Flip the request-header row**

Replace:
```
| `X-TokenTrimmer-Cache` | Override cache behavior for this request | Planned (not yet honored) | `bypass` / `force-write` / `read-only` / `disabled` |
```
with:
```
| `X-TokenTrimmer-Cache` | Override cache behavior for this request (overrides the request-body `tt_extras.cache`; a privacy route's `disable_cache` still wins). | Honored | `bypass` / `force-write` / `read-only` / `disabled` |
```

- [ ] **Step 2: Fix the §6.3 prose**

Replace the `bypass` and `force-write` bullets:
```
- `bypass` — skip cache lookup, still write result to cache (the same as default but explicit)
- `force-write` — write to cache even for normally non-cacheable requests (USE WITH CAUTION; can poison cache)
```
with:
```
- `bypass` — skip the cache lookup, but still write/refresh the result (forces a fresh upstream call, then repopulates the cache)
- `force-write` — write to cache even for normally-ineligible requests (e.g. `temperature` > 0, `n` > 1, `seed` set). Tool-call responses are still never cached. USE WITH CAUTION; can poison the shared cache.
```

- [ ] **Step 3: Commit**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs: mark X-TokenTrimmer-Cache as honored + clarify modes"
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

- [ ] **Step 3: Test the crate**

Run: `cargo test -p tt-core 2>&1 | grep -E "test result:" | tail`
Expected: all pass.

- [ ] **Step 4: Doc gate**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-core --no-deps 2>&1 | tail -10`
Expected: no NEW errors beyond the pre-existing crate-wide unresolved-link warnings (this gate is not run in CI).

- [ ] **Step 5: Advisories**

Run: `cargo deny check advisories 2>&1 | tail -5`
Expected: ok.

- [ ] **Step 6: Commit any residual gate fixes**

```bash
git status --porcelain
```
```
