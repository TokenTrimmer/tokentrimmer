# Honor `X-TokenTrimmer-Route` + emit `Route-Matched` (F8) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Honor `X-TokenTrimmer-Route` (force a named route, ignoring its conditions; unknown → 400) and emit `X-TokenTrimmer-Route-Matched` (applied route's name) on `/v1/chat/completions`.

**Architecture:** Add `RoutingEngine::find_by_name` (tt_routing). In `apply_routing` (chat.rs), accept a `forced_route` and short-circuit `evaluate_with_cost`; change its return to `ApiResult` so an unknown forced route is a 400. Carry the route name in `RouteMatch` and stamp `x-tokentrimmer-route-matched` on each routed response via a `with_route_matched` wrapper at the handler exits. Embeddings passes `None`.

**Tech Stack:** Rust, axum, tokio, the in-crate routing test harness.

---

### Task 1: `RoutingEngine::find_by_name` (tt_routing)

**Files:**
- Modify: `crates/routing/src/lib.rs` (add method after `evaluate_with_cost` ~line 186; unit test in the `#[cfg(test)]` module)

- [ ] **Step 1: Write the failing unit test**

In `crates/routing/src/lib.rs`, inside `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn find_by_name_matches_enabled_route_by_exact_name() {
        let mut enabled = make_route("alpha", 10, vec!["gpt-4o"], "gpt-4o-mini");
        enabled.name = "alpha".into();
        let mut disabled = make_route("beta", 10, vec!["gpt-4o"], "gpt-4o-mini");
        disabled.name = "beta".into();
        disabled.enabled = false;
        let eng = RoutingEngine::with_routes(vec![enabled, disabled]);
        assert!(eng.find_by_name("alpha").is_some());
        assert_eq!(eng.find_by_name("alpha").unwrap().name, "alpha");
        assert!(eng.find_by_name("beta").is_none(), "disabled route not found");
        assert!(eng.find_by_name("missing").is_none());
    }
```

Run: `cargo test -p tt-routing find_by_name 2>&1 | tail -8`
Expected: FAIL to compile — `find_by_name` does not exist.

- [ ] **Step 2: Implement `find_by_name`**

Add after the `evaluate_with_cost` method (before the closing `}` of `impl RoutingEngine`, ~line 187):

```rust
    /// Find an enabled route by exact name (case-sensitive), bypassing condition
    /// evaluation — used to honor a forced-route request header.
    pub fn find_by_name(&self, name: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.enabled && r.name == name)
    }
```

Run: `cargo test -p tt-routing find_by_name 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/routing/src/lib.rs
git commit -m "feat(routing): RoutingEngine::find_by_name (by exact enabled-route name)"
```

---

### Task 2: `route_override_from_header` reader (chat.rs)

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (add fn near `provider_override_from_header`; unit test in `provider_override_tests` or a new module)

- [ ] **Step 1: Write the failing unit test**

In `crates/core/src/routes/chat.rs`, inside the `#[cfg(test)] mod provider_override_tests` block, add:

```rust
    #[test]
    fn route_override_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(route_override_from_header(&h), None);
        // case-preserved (route names are case-sensitive labels), trimmed.
        h.insert("x-tokentrimmer-route", "  Cheap-For-Short ".parse().unwrap());
        assert_eq!(
            route_override_from_header(&h).as_deref(),
            Some("Cheap-For-Short")
        );
        let mut empty = HeaderMap::new();
        empty.insert("x-tokentrimmer-route", "   ".parse().unwrap());
        assert_eq!(route_override_from_header(&empty), None);
    }
```

Run: `cargo test -p tt-core route_override_header_parsing 2>&1 | tail -8`
Expected: FAIL to compile — `route_override_from_header` missing.

- [ ] **Step 2: Implement the reader**

Add immediately after `provider_override_from_header` (after `chat.rs:~89`):

```rust
/// `X-TokenTrimmer-Route` — an exact route name to force (case-sensitive).
pub(crate) fn route_override_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tokentrimmer-route")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
```

Run: `cargo test -p tt-core route_override_header_parsing 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "feat(core): add route_override_from_header reader"
```

---

### Task 3: Integration tests (RED)

**Files:**
- Create: `crates/core/tests/route_header.rs`

- [ ] **Step 1: Write the test file**

Create `crates/core/tests/route_header.rs`:

```rust
//! `X-TokenTrimmer-Route` forces a named route; `X-TokenTrimmer-Route-Matched`
//! reports the applied route's name.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

const MODELS: [&str; 3] = ["m1", "m2", "m3"];

struct MultiModelProvider;

#[async_trait]
impl Provider for MultiModelProvider {
    fn id(&self) -> &'static str {
        "multi"
    }
    fn models(&self) -> Vec<ModelInfo> {
        MODELS
            .iter()
            .map(|id| ModelInfo {
                id: (*id).into(),
                provider: "multi".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            })
            .collect()
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
        _r: EmbeddingsRequest,
        _c: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .unwrap()
        .plaintext
}

/// Build an app whose org has `routes`. Returns (app, key).
async fn app_with_routes(routes: Vec<Route>) -> (axum::Router, String) {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MultiModelProvider));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, routes);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );
    (app, key)
}

fn route(name: &str, priority: u32, tag: Option<&str>, target: &str) -> Route {
    Route {
        id: Uuid::now_v7(),
        name: name.into(),
        priority,
        enabled: true,
        when: RouteConditions {
            tag_equals: tag.map(String::from),
            ..Default::default()
        },
        then: RouteAction {
            target_model: target.into(),
            fallbacks: vec![],
            force_cache_layer: None,
            disable_cache: false,
            max_cost_usd: None,
        },
    }
}

fn chat_req(model: &str, force_route: Option<&str>, key: &str) -> Request<Body> {
    let body = json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": false });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"));
    if let Some(r) = force_route {
        b = b.header("x-tokentrimmer-route", r);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn hdr(resp: &axum::http::Response<Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

#[tokio::test]
async fn forced_route_applies_ignoring_conditions() {
    // Route conditions require tag "other" (not present), so it would NOT match
    // normally. Forcing it by name applies it anyway.
    let (app, key) = app_with_routes(vec![route("force-me", 100, Some("other"), "m2")]).await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", Some("force-me"), &key))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(hdr(&r, "x-tokentrimmer-model-used").as_deref(), Some("m2"));
    assert_eq!(hdr(&r, "x-tokentrimmer-route-matched").as_deref(), Some("force-me"));
}

#[tokio::test]
async fn forced_route_overrides_normal_match() {
    // `auto` would match (no tag condition); `manual` would not (needs tag).
    // Forcing `manual` wins.
    let (app, key) = app_with_routes(vec![
        route("auto", 100, None, "m2"),
        route("manual", 50, Some("never"), "m3"),
    ])
    .await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", Some("manual"), &key))
        .await
        .unwrap();
    assert_eq!(hdr(&r, "x-tokentrimmer-model-used").as_deref(), Some("m3"));
    assert_eq!(hdr(&r, "x-tokentrimmer-route-matched").as_deref(), Some("manual"));
}

#[tokio::test]
async fn unknown_forced_route_is_400() {
    let (app, key) = app_with_routes(vec![route("auto", 100, None, "m2")]).await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", Some("nope"), &key))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn condition_matched_route_emits_route_matched() {
    let (app, key) = app_with_routes(vec![route("auto", 100, None, "m2")]).await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", None, &key))
        .await
        .unwrap();
    assert_eq!(hdr(&r, "x-tokentrimmer-model-used").as_deref(), Some("m2"));
    assert_eq!(hdr(&r, "x-tokentrimmer-route-matched").as_deref(), Some("auto"));
}

#[tokio::test]
async fn no_route_no_header() {
    // A route that requires a tag that isn't present → no match, no header.
    let (app, key) = app_with_routes(vec![route("auto", 100, Some("never"), "m2")]).await;
    let r = app
        .clone()
        .oneshot(chat_req("m1", None, &key))
        .await
        .unwrap();
    assert_eq!(hdr(&r, "x-tokentrimmer-model-used").as_deref(), Some("m1"));
    assert_eq!(hdr(&r, "x-tokentrimmer-route-matched"), None);
}
```

- [ ] **Step 2: Run to verify failures (RED)**

Run: `cargo test -p tt-core --test route_header 2>&1 | tail -30`
Expected: compile FAILS — `apply_routing` signature (the handler doesn't pass `forced_route` yet, and the header isn't honored). The whole file won't compile/pass until Task 4. (If it compiles because the handler is unchanged, the forced/route-matched assertions FAIL.)

- [ ] **Step 3: Commit the failing tests**

```bash
git add crates/core/tests/route_header.rs
git commit -m "test(core): X-TokenTrimmer-Route + Route-Matched behavior (RED)"
```

---

### Task 4: Implement forced routing + response header (GREEN)

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (RouteMatch, apply_routing, with_route_matched, handler wiring)
- Modify: `crates/core/src/routes/embeddings.rs` (call-site `?` + `None`)

- [ ] **Step 1: Add `route_name` to `RouteMatch`**

```rust
pub(crate) struct RouteMatch {
    pub(crate) route_id: Uuid,
    pub(crate) route_name: String,
    pub(crate) fallbacks: Vec<String>,
    pub(crate) disable_cache: bool,
    pub(crate) max_cost_usd: Option<f64>,
    pub(crate) input_tokens_estimate: u32,
}
```

- [ ] **Step 2: Rework `apply_routing` (signature + forced selection + ApiResult)**

Add this helper just above `apply_routing`:

```rust
/// A forced route that can't be honored is a 400; absence of routing is fine
/// for an unforced request.
fn forced_miss(forced: Option<&str>) -> ApiResult<Option<RouteMatch>> {
    match forced {
        Some(name) => Err(ApiError::InvalidRequest(format!("unknown route: {name}"))),
        None => Ok(None),
    }
}
```

Change the signature + the three early returns + the selection + the construction:

```rust
pub(crate) async fn apply_routing(
    state: &AppState,
    ctx: &RequestContext,
    req: &mut ChatCompletionRequest,
    forced_route: Option<&str>,
) -> ApiResult<Option<RouteMatch>> {
    let Some(store) = state.routing_store.as_ref() else {
        return forced_miss(forced_route);
    };
    if ctx.org_id == Uuid::nil() {
        return forced_miss(forced_route);
    }

    let engine = match store.engine_for(ctx.org_id).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, org_id = %ctx.org_id, "routing store lookup failed — passing request through unrouted");
            return Ok(None); // never fail user traffic on a transient backend error
        }
    };
```

Keep the `input_tokens` + `estimated_cost_usd` computation as-is, then replace the `let m = engine.evaluate_with_cost(...)?;` line with:

```rust
    // `m` is `&Route` (inferred from the engine accessors below) regardless of arm.
    let m = match forced_route {
        Some(name) => engine
            .find_by_name(name)
            .ok_or_else(|| ApiError::InvalidRequest(format!("unknown route: {name}")))?,
        None => match engine.evaluate_with_cost(req, ctx, input_tokens, estimated_cost_usd) {
            Some(r) => r,
            None => return Ok(None),
        },
    };
    let route_id = m.id;
    let route_name = m.name.clone();
    let fallbacks = m.then.fallbacks.clone();
    let disable_cache = m.then.disable_cache;
    let max_cost_usd = m.then.max_cost_usd;
```

The capability-guard block is unchanged except its `return None;` becomes `return Ok(None);`. The final construction becomes:

```rust
    let original = std::mem::replace(&mut req.model, m.then.target_model.clone());
    tracing::debug!(
        org_id = %ctx.org_id,
        route_id = %route_id,
        from = %original,
        to = %req.model,
        fallbacks = ?fallbacks,
        "routing rewrite"
    );
    Ok(Some(RouteMatch {
        route_id,
        route_name,
        fallbacks,
        disable_cache,
        max_cost_usd,
        input_tokens_estimate: input_tokens,
    }))
}
```

(`tt_routing::Route` is referenced in the `match` type annotation; if `Route` is already imported in chat.rs use the short name, otherwise the fully-qualified form compiles.)

- [ ] **Step 3: Add the `with_route_matched` wrapper**

Add near `attach_cost_headers` in `chat.rs`:

```rust
/// Stamp `X-TokenTrimmer-Route-Matched` with the applied route's name (no-op when
/// `name` is `None` or not header-safe).
fn with_route_matched(mut resp: Response, name: Option<&str>) -> Response {
    if let Some(name) = name {
        if let Ok(v) = name.parse() {
            resp.headers_mut()
                .insert("x-tokentrimmer-route-matched", v);
        }
    }
    resp
}
```

- [ ] **Step 4: Wire the handler**

Near the other header reads (after `provider_pin`/`raw_bearer`, ~line 386):
```rust
    let forced_route = route_override_from_header(&headers);
```

Change the routing call (line 544):
```rust
    let route_match = apply_routing(&state, &ctx, &mut req, forced_route.as_deref()).await?;
```

Right after it (alongside `matched_route_id` capture, ~545):
```rust
    let route_matched_name = route_match.as_ref().map(|m| m.route_name.clone());
```

Wrap the success exits:
- Line 718: `return Ok(with_route_matched(sse::stream_response(fake, &provider, trace_id, None), route_matched_name.as_deref()));`
- Line 887: `Ok(with_route_matched(sse::stream_response(stream, &provider, trace_id, log_ctx), route_matched_name.as_deref()))`
- Line 934: `return Ok(with_route_matched(resp, route_matched_name.as_deref()));`
- Line 972 and 1066: `return Ok(with_route_matched(build_hit_l1_response(entry, trace_id), route_matched_name.as_deref()));`
- Line 1013: `return Ok(with_route_matched(build_hit_l2_response(entry, similarity, trace_id)?, route_matched_name.as_deref()));`
- Line 1392 (dispatched non-stream) — before `Ok(http_response)`, set the header on the mutable local:
  ```rust
        if let Some(name) = route_matched_name.as_deref() {
            if let Ok(v) = name.parse() {
                http_response
                    .headers_mut()
                    .insert("x-tokentrimmer-route-matched", v);
            }
        }
        Ok(http_response)
  ```

- [ ] **Step 5: Update the embeddings call site**

`crates/core/src/routes/embeddings.rs:154`:
```rust
    let route_match = apply_routing(&state, &ctx, &mut synth, None).await?;
```

- [ ] **Step 6: Run the integration + unit tests (GREEN)**

Run: `cargo test -p tt-core --test route_header 2>&1 | tail -20`
Expected: all 5 pass.

Run: `cargo test -p tt-core -p tt-routing 2>&1 | grep -E "test result:" | tail`
Expected: all pass (no regressions in disable_cache/cost_routing/embeddings_routing, which exercise `apply_routing`).

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/src/routes/embeddings.rs
git commit -m "feat(core): honor X-TokenTrimmer-Route (forced, conditions ignored) + emit Route-Matched"
```

---

### Task 5: Docs

**Files:**
- Modify: `docs/04-gateway-api-reference.md` (§6.1 line 408; §6.2 line ~426)

- [ ] **Step 1: Flip the request-header row**

Replace:
```
| `X-TokenTrimmer-Route` | Force a specific named route | Planned (not yet honored) | `cheap-for-short` |
```
with:
```
| `X-TokenTrimmer-Route` | Force a specific named route, ignoring its conditions (unknown name → `400`; chat completions only). | Honored | `cheap-for-short` |
```

- [ ] **Step 2: Flip the response-header row**

Replace:
```
| `X-TokenTrimmer-Route-Matched` | Planned (not yet emitted) | `cheap-for-short` |
```
with:
```
| `X-TokenTrimmer-Route-Matched` | the applied route's name, on routed responses (forced or condition-matched) | `cheap-for-short` |
```

- [ ] **Step 3: Commit**

```bash
git add docs/04-gateway-api-reference.md
git commit -m "docs: mark X-TokenTrimmer-Route honored + Route-Matched emitted"
```

---

### Task 6: Gates + finish

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt`
Then: `git diff --quiet || (git add -A && git commit -m "style: cargo fmt")`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: no warnings. Fix any, re-run.

- [ ] **Step 3: Tests**

Run: `cargo test -p tt-core -p tt-routing 2>&1 | grep -E "test result:" | tail`
Expected: all pass.

- [ ] **Step 4: Doc gate**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-routing --no-deps 2>&1 | tail -10`
Expected: clean (tt-routing has no pre-existing doc-link issues). (`tt-core` has pre-existing crate-wide unresolved-link warnings unrelated to this change; not a CI gate.)

- [ ] **Step 5: Advisories**

Run: `cargo deny check advisories 2>&1 | tail -5`
Expected: ok.

- [ ] **Step 6: Commit any residual gate fixes**

```bash
git status --porcelain
```
```
