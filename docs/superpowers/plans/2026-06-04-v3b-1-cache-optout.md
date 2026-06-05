# V3b-1 — Privacy Cache Opt-out (`disable_cache`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `disable_cache: bool` route action that makes matched requests skip TokenTrimmer's L1+L2 cache entirely (no lookup, no insert), plus `tt route add --disable-cache` / `--when-tag` to create privacy routes.

**Architecture:** Add `disable_cache` to `tt_routing::RouteAction` (+ the `tt_plan_core` mirror), surface it on the gateway-internal `RouteMatch`, and — since routing runs before the cache lookup — override the already-computed `CacheBehavior` to `do_lookup = do_insert = false` when the matched route sets it. Match reuses the existing `tag_equals` condition (`X-TokenTrimmer-Tag` header).

**Tech Stack:** Rust workspace — `tt-routing`, `tt-plan-core`, `tt-core` (axum), `tt-cli`. No new deps.

**Repo / branch:** `/Users/iansimon/Developer/TokenTrimmer/public` on `feat/v3b-1-cache-optout` (off `main`). Spec: `docs/superpowers/specs/2026-06-04-v3b-1-cache-optout-design.md`.

**Test note:** `cargo test --workspace` is hook-denied — scope with `-p`. Rust "red" = a compile error referencing a not-yet-defined item.

**Verified anchors:**
- `tt_routing::RouteAction` (`crates/routing/src/lib.rs:78-97`): derives `Default`; fields `target_model`, `fallbacks` (`skip_serializing_if = Vec::is_empty`), `force_cache_layer` (`skip_serializing_if = Option::is_none`). Existing serde tests at `:494-543` (incl. `route_action_minimal_serializes_without_new_fields` asserting exact `{"target_model":"x"}`).
- The repo already uses `#[serde(default, skip_serializing_if = "std::ops::Not::not")]` for a `bool` (`crates/shared/src/messages.rs` `stream`) — the proven idiom.
- `tt_plan_core::types::RouteAction` (`crates/plan-core/src/types.rs:143-...`): mirror, **no `Default`**, comment requires identical field order.
- Gateway (`crates/core/src/routes/chat.rs`): `RouteMatch { route_id, fallbacks }` (`:1581-1584`); built in `apply_routing` where `m: &Route`, `route_id = m.id`, `fallbacks = m.then.fallbacks.clone()` (`:1630-1631`), returned `Some(RouteMatch { route_id, fallbacks })` (`:1667`). In the handler: `route_match = apply_routing(...)` (`:397`), consumed at `:400` (`route_match.map(...)`); `let cache_behavior = CacheBehavior::resolve(&req);` (`:415`). `CacheBehavior { do_lookup, do_insert, ttl_secs }` (`:239`). L1/L2 lookup gated on `do_lookup` (`:685,717`); insert on `do_insert` (`:984,1041`).
- `RouteAction { … }` literals needing `disable_cache: false`: `tt-routing` — `cache.rs:140,237`; `store.rs:354,387,420`; `lib.rs:209,496,527`; `validate.rs:68`. `tt-core` tests — `route_rewrite.rs:188,262,368,419,500`; `route_content_type.rs:204`; `failover.rs:138`; `dogfood_routing.rs:150`. `tt-plan-core` — `types.rs:442,473`; `apply.rs:293`; `routing.rs:87`.
- CLI: `crates/cli/src/route/mod.rs` `AddArgs` + `build_new_route`; `crates/cli/src/main.rs` `RouteAction::Add` clap args + the `Command::Route` dispatch.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/routing/src/lib.rs` (modify) | `RouteAction.disable_cache` field; serde test; fix internal literals. |
| `crates/routing/src/{cache,store,validate}.rs` (modify) | `disable_cache: false` in test literals. |
| `crates/plan-core/src/types.rs` (modify) | mirror field; fix literals; serde test. |
| `crates/plan-core/src/{apply,routing}.rs` (modify) | `disable_cache: false` in test literals. |
| `crates/core/src/routes/chat.rs` (modify) | `RouteMatch.disable_cache`; populate; override `cache_behavior`. |
| `crates/core/tests/{route_rewrite,route_content_type,failover,dogfood_routing}.rs` (modify) | `disable_cache: false` in literals. |
| `crates/core/tests/disable_cache.rs` (create) | e2e: matched `disable_cache` route bypasses L1. |
| `crates/cli/src/route/mod.rs` (modify) | `AddArgs.{disable_cache,when_tag}` + `build_new_route` mapping + tests. |
| `crates/cli/src/main.rs` (modify) | `--disable-cache` / `--when-tag` clap args + dispatch. |

---

## Task 1: `tt_routing::RouteAction.disable_cache`

**Files:** Modify `crates/routing/src/lib.rs`, `crates/routing/src/cache.rs`, `crates/routing/src/store.rs`, `crates/routing/src/validate.rs`

- [ ] **Step 1: Write the failing test** — append to the `#[cfg(test)] mod tests` block in `crates/routing/src/lib.rs` (after `route_action_full_round_trip`):

```rust
    #[test]
    fn route_action_disable_cache_defaults_false_and_omits() {
        // Omitted from JSON when false (back-compat: existing rows unchanged).
        let a = RouteAction {
            target_model: "x".into(),
            fallbacks: Vec::new(),
            force_cache_layer: None,
            disable_cache: false,
        };
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"target_model":"x"}"#);
        // Defaults false when absent.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.disable_cache);
        // Present when true.
        let b = RouteAction { disable_cache: true, ..a };
        assert!(serde_json::to_string(&b).unwrap().contains("\"disable_cache\":true"));
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-routing route_action_disable_cache` → FAIL (`RouteAction` has no field `disable_cache`).

- [ ] **Step 3: Add the field** — in `crates/routing/src/lib.rs`, in `RouteAction` after `force_cache_layer`:

```rust
    /// When true, a request this route matches skips L1+L2 entirely (no lookup,
    /// no insert) — for privacy/sensitive traffic that must not persist in the
    /// shared cache. Default false; omitted from JSON when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_cache: bool,
```

- [ ] **Step 4: Fix the in-crate `RouteAction` literals** — in each of these literals add `disable_cache: false,` as the last field (after `force_cache_layer: …,`): `lib.rs:209` (`make_route`), `lib.rs:496` + `lib.rs:527` (the two existing serde tests), `cache.rs:140` + `cache.rs:237`, `store.rs:354` + `store.rs:387` + `store.rs:420`, `validate.rs:68` (`action`). (Each is the same one-line addition; `cargo build -p tt-routing --tests` lists any missed.)

- [ ] **Step 5: Run to verify it passes** — `cargo test -p tt-routing` → PASS (existing + the new test). The `route_action_minimal_serializes_without_new_fields` test still asserts `{"target_model":"x"}` (disable_cache omitted when false).

- [ ] **Step 6: Commit**

```bash
git add crates/routing/src/lib.rs crates/routing/src/cache.rs crates/routing/src/store.rs crates/routing/src/validate.rs
git commit -m "feat(routing): RouteAction.disable_cache (privacy cache opt-out)"
```

---

## Task 2: `tt_plan_core::types::RouteAction` mirror

**Files:** Modify `crates/plan-core/src/types.rs`, `crates/plan-core/src/apply.rs`, `crates/plan-core/src/routing.rs`

- [ ] **Step 1: Write the failing test** — append to the `#[cfg(test)] mod tests` block in `crates/plan-core/src/types.rs`:

```rust
    #[test]
    fn route_action_disable_cache_round_trips() {
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.disable_cache, "defaults false");
        let a = RouteAction {
            target_model: "m".into(),
            fallbacks: Vec::new(),
            force_cache_layer: None,
            disable_cache: true,
        };
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"disable_cache\":true"));
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.disable_cache);
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-plan-core route_action_disable_cache` → FAIL (no field `disable_cache`).

- [ ] **Step 3: Add the mirror field** — in `crates/plan-core/src/types.rs`, in `RouteAction` after `force_cache_layer` (keep field order identical to `tt_routing`):

```rust
    /// Mirror of `tt_routing::RouteAction::disable_cache`. The replay engine does
    /// not yet model cache opt-out (follow-up); present for lossless round-trip.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_cache: bool,
```

- [ ] **Step 4: Fix the in-crate literals** — add `disable_cache: false,` (after `force_cache_layer`) to the `RouteAction { … }` literals at `types.rs:442`, `types.rs:473`, `apply.rs:293`, `routing.rs:87`.

- [ ] **Step 5: Run to verify it passes** — `cargo test -p tt-plan-core` → PASS (incl. the snapshot test `snapshot_canned_replay` — disable_cache omitted when false keeps the snapshot unchanged).

- [ ] **Step 6: Commit**

```bash
git add crates/plan-core/src/types.rs crates/plan-core/src/apply.rs crates/plan-core/src/routing.rs
git commit -m "feat(plan-core): mirror RouteAction.disable_cache for lockstep"
```

---

## Task 3: Gateway wiring + e2e

**Files:** Modify `crates/core/src/routes/chat.rs`; modify the four `tt-core` test files; create `crates/core/tests/disable_cache.rs`

- [ ] **Step 1: Fix the tt-core test literals so the crate compiles** — add `disable_cache: false,` (after `force_cache_layer`) to the `RouteAction { … }` literals at `route_rewrite.rs:188,262,368,419,500`, `route_content_type.rs:204`, `failover.rs:138`, `dogfood_routing.rs:150`.

- [ ] **Step 2: Write the failing e2e test** — create `crates/core/tests/disable_cache.rs`:

```rust
//! A matched `disable_cache` route makes the request skip L1 entirely:
//! two identical requests both hit the provider; the second is not an L1 hit.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{keys::{issue, Environment}, InMemoryKeyStore, KeyStore};
use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

struct CountingProvider { calls: Arc<AtomicUsize>, served: Arc<Mutex<Vec<String>>> }

#[async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &'static str { "recording" }
    fn models(&self) -> Vec<ModelInfo> {
        ["gpt-4o", "gpt-4o-mini"].into_iter().map(|id| ModelInfo {
            id: id.into(), provider: "recording".into(),
            capabilities: vec![Capability::Text], max_input_tokens: 4096, max_output_tokens: 4096,
        }).collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (i, o) = if model == "gpt-4o" { (5.0, 15.0) } else { (0.15, 0.6) };
        Some(ModelPricing { input_per_million: i, output_per_million: o,
            cached_input_per_million: None, cache_write_per_million: None, effective_at: Utc::now() })
    }
    async fn chat_completion(&self, req: ChatCompletionRequest, _c: &RequestContext)
        -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.served.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "x".into(), object: "chat.completion".into(), created: 0, model: req.model,
            choices: vec![Choice { index: 0, message: Message::Assistant {
                content: Some(MessageContent::Text("ok".into())), tool_calls: vec![], name: None },
                finish_reason: Some("stop".into()) }],
            usage: Usage { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10,
                cached_tokens: 0, cache_creation_input_tokens: None },
        })
    }
    async fn chat_completion_stream(&self, _r: ChatCompletionRequest, _c: &RequestContext)
        -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(&self, _r: EmbeddingsRequest, _c: &RequestContext)
        -> Result<EmbeddingsResponse, ProviderError> { Err(ProviderError::Unsupported("no".into())) }
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System).await.unwrap().plaintext
}

fn sensitive_request(model: &str, bearer: &str) -> Request<Body> {
    let body = json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": false });
    Request::builder().method("POST").uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-tokentrimmer-tag", "sensitive")
        .body(Body::from(body.to_string())).unwrap()
}

async fn setup(disable_cache: bool) -> (axum::Router, String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider { calls: Arc::clone(&calls), served }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, vec![Route {
        id: Uuid::now_v7(), name: "privacy".into(), priority: 100, enabled: true,
        when: RouteConditions { tag_equals: Some("sensitive".into()), ..Default::default() },
        then: RouteAction { target_model: "gpt-4o-mini".into(), fallbacks: vec![], force_cache_layer: None, disable_cache },
    }]);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry).with_key_store(key_store).with_routing_store(routing)
            .with_l1(Arc::new(InMemoryL1Cache::new()), None),
    );
    (app, key, calls)
}

#[tokio::test]
async fn disable_cache_route_bypasses_l1() {
    let (app, key, calls) = setup(true).await;
    let r1 = app.clone().oneshot(sensitive_request("gpt-4o", &key)).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r2 = app.oneshot(sensitive_request("gpt-4o", &key)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    // No L1 hit on the second identical request — cache disabled.
    assert_ne!(
        r2.headers().get("x-tokentrimmer-cache").and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "disable_cache route must not serve from L1"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2, "provider must be called for both requests");
}

#[tokio::test]
async fn control_route_without_disable_cache_hits_l1() {
    let (app, key, calls) = setup(false).await;
    let r1 = app.clone().oneshot(sensitive_request("gpt-4o", &key)).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r2 = app.oneshot(sensitive_request("gpt-4o", &key)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers().get("x-tokentrimmer-cache").and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "without disable_cache the second identical request is an L1 hit"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1, "provider called once; second served from L1");
}
```

- [ ] **Step 3: Run to verify it fails** — `cargo test -p tt-core --test disable_cache` → FAIL: `disable_cache_route_bypasses_l1` sees `hit-l1` / `calls == 1` because the override isn't wired yet (cache still active).

- [ ] **Step 4: Add `disable_cache` to `RouteMatch`** — in `crates/core/src/routes/chat.rs`, change the struct (`:1581`):

```rust
struct RouteMatch {
    route_id: Uuid,
    fallbacks: Vec<String>,
    disable_cache: bool,
}
```

Populate it where `fallbacks` is captured (`:1631`) and in the return (`:1667`):

```rust
    let route_id = m.id;
    let fallbacks = m.then.fallbacks.clone();
    let disable_cache = m.then.disable_cache;
```
```rust
    Some(RouteMatch {
        route_id,
        fallbacks,
        disable_cache,
    })
```

- [ ] **Step 5: Override the cache decision in the handler** — in `chat.rs`, between `:398` and `:400` (before `route_match` is consumed) capture:

```rust
    let route_disable_cache = route_match.as_ref().is_some_and(|m| m.disable_cache);
```

Change `:415` to `let mut cache_behavior = CacheBehavior::resolve(&req);` and immediately after it add:

```rust
    // A matched privacy route forces the request to skip the cache entirely.
    if route_disable_cache {
        cache_behavior.do_lookup = false;
        cache_behavior.do_insert = false;
    }
```

- [ ] **Step 6: Run to verify it passes** — `cargo test -p tt-core --test disable_cache` → PASS (2 tests). Then `cargo test -p tt-core --test route_rewrite --test route_content_type` → still green (no regression).

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/tests/disable_cache.rs crates/core/tests/route_rewrite.rs crates/core/tests/route_content_type.rs crates/core/tests/failover.rs crates/core/tests/dogfood_routing.rs
git commit -m "feat(core): honor RouteAction.disable_cache (skip L1+L2 for matched privacy routes)"
```

---

## Task 4: CLI `--disable-cache` / `--when-tag`

**Files:** Modify `crates/cli/src/route/mod.rs`, `crates/cli/src/main.rs`

- [ ] **Step 1: Write the failing tests** — in `crates/cli/src/route/mod.rs`'s `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn disable_cache_and_when_tag_map_through() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o".into()), from: None, to: None,
            when_has_images: false, when_has_audio: false, when_tag: Some("sensitive".into()),
            disable_cache: true, priority: 100, name: None, fallback: vec![], disabled: false,
        }).unwrap();
        assert_eq!(body["when"]["tag_equals"], "sensitive");
        assert_eq!(body["then"]["disable_cache"], true);
    }

    #[test]
    fn disable_cache_omitted_when_false() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o".into()), from: None, to: None,
            when_has_images: false, when_has_audio: false, when_tag: None,
            disable_cache: false, priority: 100, name: None, fallback: vec![], disabled: false,
        }).unwrap();
        assert!(body["then"].get("disable_cache").is_none());
        assert!(body["when"].get("tag_equals").is_none());
    }
```

(The three existing `build_new_route` tests now also need the two new `AddArgs` fields — add `when_tag: None, disable_cache: false,` to each of their `AddArgs { … }` literals.)

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-cli route` → FAIL (`AddArgs` has no field `when_tag` / `disable_cache`).

- [ ] **Step 3: Extend `AddArgs` + `build_new_route`** — in `crates/cli/src/route/mod.rs`, add to `AddArgs` (after `when_has_audio`):

```rust
    pub when_tag: Option<String>,
    pub disable_cache: bool,
```

In `build_new_route`, after the `when_has_audio` block (before building `then`):

```rust
    if let Some(tag) = &args.when_tag {
        when.insert("tag_equals".into(), json!(tag));
    }
```

After the `fallbacks` block in `then`:

```rust
    if args.disable_cache {
        then.insert("disable_cache".into(), json!(true));
    }
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-cli route` → PASS.

- [ ] **Step 5: Wire the clap args + dispatch** — in `crates/cli/src/main.rs`, in `enum RouteAction`'s `Add { … }` variant (after `when_has_audio`):

```rust
        #[arg(long)]
        when_tag: Option<String>,
        #[arg(long)]
        disable_cache: bool,
```

In the `Command::Route` dispatch's `RouteAction::Add { … }` destructure + `AddArgs { … }` construction, add `when_tag,` and `disable_cache,` to both (alongside the other fields).

- [ ] **Step 6: Build + smoke + commit**

Run: `cargo build -p tt-cli && ./target/debug/tt route add --help | grep -E "when-tag|disable-cache"`
Expected: both flags listed. Then:

```bash
git add crates/cli/src/route/mod.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt route add --disable-cache / --when-tag"
```

---

## Task 5: Final verification

**Files:** none.

- [ ] **Step 1: Format** — `cargo fmt -p tt-routing -p tt-plan-core -p tt-core -p tt-cli`; then `git diff --quiet || git commit -am "style: cargo fmt (v3b-1)"`.
- [ ] **Step 2: Clippy** — `cargo clippy -p tt-routing -p tt-plan-core -p tt-cli --all-targets -- -D warnings` then `cargo clippy -p tt-core --tests -- -D warnings`. Expected: clean.
- [ ] **Step 3: Tests** — `cargo test -p tt-routing -p tt-plan-core -p tt-cli` then `cargo test -p tt-core --test disable_cache --test route_rewrite --test route_content_type --test routes_api`. Expected: all pass.
- [ ] **Step 4: Clean tree** — `git status` + `git log --oneline -8` (Task 1–4 commits on `feat/v3b-1-cache-optout`).

---

## Self-Review (completed by plan author)

**1. Spec coverage:** `disable_cache` on `RouteAction` (+ mirror) → Tasks 1–2; surfaced on `RouteMatch` + cache override → Task 3; e2e bypass proof → Task 3 (incl. control); CLI `--disable-cache`/`--when-tag` → Task 4. Match reuses `tag_equals` (no new condition — covered by the e2e using the `x-tokentrimmer-tag` header). `target_model` stays required (test routes pin to `gpt-4o-mini`). Out-of-scope items (local routing, dashboard, org-setting) untouched.

**2. Placeholder scan:** every code step complete; the construction-site fixes are an explicit enumerated file:line list with the identical one-line edit (not a vague "fix the rest"). Commands have expected output.

**3. Type consistency:** `disable_cache: bool` field name + `#[serde(default, skip_serializing_if = "std::ops::Not::not")]` identical across `tt_routing` (Task 1), `tt_plan_core` (Task 2), and the `RouteMatch`/handler wiring (Task 3). CLI `AddArgs.{when_tag: Option<String>, disable_cache: bool}` (Task 4) match the clap args + dispatch. `RouteMatch { route_id, fallbacks, disable_cache }` consumed via `route_match.as_ref().is_some_and(|m| m.disable_cache)` before the existing `:400` move.
