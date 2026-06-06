# Honor `X-TokenTrimmer-Fallback` (F9) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Honor the `X-TokenTrimmer-Fallback` request header — a comma-separated chain of bare model ids that overrides the route-derived `route_fallbacks` on `/v1/chat/completions`.

**Architecture:** A `fallback_override_from_header` parser; one line in the chat handler reassigns `route_fallbacks` from the header (after the provider-pin clear, before the `failover_candidates` build). The existing failover machinery (candidate build, per-provider cross-provider cred guard, the failover loop) consumes `route_fallbacks` unchanged.

**Tech Stack:** Rust, axum, tokio, the in-crate failover test harness.

---

### Task 1: `fallback_override_from_header` parser

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add fn near the other header readers; unit test in the `provider_override_tests` module)

- [ ] **Step 1: Write the failing unit test**

In `crates/core/src/routes/chat.rs`, inside `#[cfg(test)] mod provider_override_tests`, add:

```rust
    #[test]
    fn fallback_override_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(fallback_override_from_header(&h), None);
        h.insert("x-tokentrimmer-fallback", "a, b ,c".parse().unwrap());
        assert_eq!(
            fallback_override_from_header(&h),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        let mut blank = HeaderMap::new();
        blank.insert("x-tokentrimmer-fallback", " , ".parse().unwrap());
        assert_eq!(fallback_override_from_header(&blank), None);
    }
```

Run: `cargo test -p tt-core fallback_override_header_parsing 2>&1 | tail -8`
Expected: FAIL to compile — `fallback_override_from_header` missing.

- [ ] **Step 2: Implement the parser**

Add immediately after `route_override_from_header` (after `chat.rs:~97`):

```rust
/// `X-TokenTrimmer-Fallback` — comma-separated override of the route's fallback
/// chain (bare model ids). Absent/blank → None (keep the route chain).
pub(crate) fn fallback_override_from_header(headers: &HeaderMap) -> Option<Vec<String>> {
    let raw = headers
        .get("x-tokentrimmer-fallback")
        .and_then(|v| v.to_str().ok())?;
    let chain: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if chain.is_empty() {
        None
    } else {
        Some(chain)
    }
}
```

Run: `cargo test -p tt-core fallback_override_header_parsing 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "feat(core): add fallback_override_from_header parser"
```

---

### Task 2: Integration tests (RED)

**Files:**
- Create: `crates/core/tests/fallback_header.rs`

- [ ] **Step 1: Write the test file**

Create `crates/core/tests/fallback_header.rs`:

```rust
//! `X-TokenTrimmer-Fallback` supplies/overrides the failover chain.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry, DOGFOOD_ORG_ID};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use uuid::Uuid;

struct MockProvider {
    id: &'static str,
    models: &'static [&'static str],
    fails: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|m| ModelInfo {
                id: (*m).to_string(),
                provider: self.id.to_string(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 128_000,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.1,
            output_per_million: 0.1,
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
        if self.fails {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "down".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: "chatcmpl-fb".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("served".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
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
        Err(ProviderError::Unsupported("no streaming".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

fn req(model: &str, fallback_header: Option<&str>) -> Request<Body> {
    let body = json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": false });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(f) = fallback_header {
        b = b.header("x-tokentrimmer-fallback", f);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn served_model(resp: &axum::http::Response<Body>) -> Option<String> {
    resp.headers()
        .get("x-tokentrimmer-model-used")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

#[tokio::test]
async fn fallback_header_enables_failover_without_route() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let backup_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["m-primary"],
        fails: true,
        calls: Arc::clone(&primary_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "backup",
        models: &["m-backup"],
        fails: false,
        calls: Arc::clone(&backup_calls),
    }));
    // No routing store — the header alone supplies the chain.
    let app = build_router(AppState::new(registry));

    let resp = app
        .oneshot(req("m-primary", Some("m-backup")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served_model(&resp).as_deref(), Some("m-backup"));
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("backup")
    );
    assert!(primary_calls.load(Ordering::Relaxed) >= 1, "primary tried");
    assert_eq!(backup_calls.load(Ordering::Relaxed), 1, "backup served once");
}

#[tokio::test]
async fn fallback_header_overrides_route_chain() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let routefb_calls = Arc::new(AtomicUsize::new(0));
    let hdrfb_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    // Primary answers the inbound `gpt-4o` (pre-routing resolve) + the routed
    // `primary-model`, and 503s.
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: true,
        calls: Arc::clone(&primary_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "routefb",
        models: &["route-fb"],
        fails: false,
        calls: Arc::clone(&routefb_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "hdrfb",
        models: &["hdr-fb"],
        fails: false,
        calls: Arc::clone(&hdrfb_calls),
    }));

    // Route rewrites gpt-4o → primary-model with its OWN fallback chain [route-fb].
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        DOGFOOD_ORG_ID,
        vec![Route {
            id: Uuid::now_v7(),
            name: "r".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: "primary-model".into(),
                fallbacks: vec!["route-fb".into()],
                force_cache_layer: None,
                disable_cache: false,
                max_cost_usd: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_routing_store(routing)
            .with_dogfood_enabled(),
    );

    // Header supplies hdr-fb → it must replace the route's [route-fb].
    let resp = app.oneshot(req("gpt-4o", Some("hdr-fb"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served_model(&resp).as_deref(), Some("hdr-fb"));
    assert_eq!(hdrfb_calls.load(Ordering::Relaxed), 1, "header fallback served");
    assert_eq!(
        routefb_calls.load(Ordering::Relaxed),
        0,
        "route's own fallback must NOT be used when the header overrides it"
    );
}
```

- [ ] **Step 2: Run to verify failures (RED)**

Run: `cargo test -p tt-core --test fallback_header 2>&1 | tail -25`
Expected: both FAIL — the header is ignored today, so `fallback_header_enables_failover_without_route` has no failover (primary 503 surfaces as an error / 502, not a 200 from backup), and `fallback_header_overrides_route_chain` serves `route-fb` (or errors), not `hdr-fb`.

- [ ] **Step 3: Commit the failing tests**

```bash
git add crates/core/tests/fallback_header.rs
git commit -m "test(core): X-TokenTrimmer-Fallback behavior (RED)"
```

---

### Task 3: Wire the handler (GREEN)

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (one block after the provider-pin clear, ~line 630)

- [ ] **Step 1: Apply the override**

After the provider-pin block:
```rust
    if provider_pin.is_some() {
        // An explicit provider pin must not fail over to a different provider.
        route_fallbacks.clear();
    }
```
add:
```rust
    // `X-TokenTrimmer-Fallback` overrides the route-derived chain. Applied AFTER
    // the pin's clear, so an explicit chain opts back into failover even when a
    // provider is pinned (the pin still set the primary provider above).
    if let Some(chain) = fallback_override_from_header(&headers) {
        route_fallbacks = chain;
    }
```

- [ ] **Step 2: Run the integration + unit tests (GREEN)**

Run: `cargo test -p tt-core --test fallback_header 2>&1 | tail -20`
Expected: both pass.

Run: `cargo test -p tt-core --test failover 2>&1 | grep -E "test result:"`
Expected: pass (route-fallback behavior unchanged when no header is present).

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "feat(core): honor X-TokenTrimmer-Fallback (override route failover chain)"
```

---

### Task 4: Docs

**Files:**
- Modify: `docs/04-gateway-api-reference.md:410`

- [ ] **Step 1: Flip the row**

Replace:
```
| `X-TokenTrimmer-Fallback` | Comma-separated fallback chain override | Planned (not yet honored) | `openai/gpt-4o,anthropic/claude-3-5-sonnet` |
```
with:
```
| `X-TokenTrimmer-Fallback` | Comma-separated fallback chain (bare model ids) overriding the route's chain. Unresolvable or uncredentialed entries are skipped. | Honored | `gpt-4o-mini,claude-3-5-sonnet` |
```

- [ ] **Step 2: Commit**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs: mark X-TokenTrimmer-Fallback honored (bare model ids)"
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
