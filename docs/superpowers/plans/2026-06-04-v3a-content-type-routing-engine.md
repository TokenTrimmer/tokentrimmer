# V3a-1 — Content-type Routing Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add input-modality routing — `has_images` / `has_audio` route conditions that the gateway matches against a chat request's content parts — plus the ADR that records the same-provider routing constraint.

**Architecture:** Approach A from the V3a spec: extend the flat `RouteConditions` struct additively (no DB migration — cloud stores conditions as JSONB). Modality detection lives once in `tt_shared` (reused by the matcher); the `tt_routing` matcher gains two arms; the `tt_plan_core` mirror gains the fields but treats them conservatively (historical `RequestLog` has no modality → never matches, so Plan never over-projects). The gateway needs **no** new code — it already calls `engine.evaluate` before cache with a capability guard, so once the matcher understands modality, modality routes work end-to-end; an integration test proves it.

**Tech Stack:** Rust workspace. Crates: `tt-shared`, `tt-routing`, `tt-plan-core`, `tt-core`. No new dependencies.

**Repo / branch:** `/Users/iansimon/Developer/TokenTrimmer/public` on branch `feat/v3a-content-type-routing` (already checked out off the V0 branch). Spec: `docs/superpowers/specs/2026-06-04-v3a-content-type-routing-design.md`.

**Test command note:** `cargo test --workspace` is hook-denied — always scope with `-p <crate>`. Rust "red" = a compile error when a test references a not-yet-defined item; that counts as the failing-test step.

**Verified current-state anchors:**
- `tt_shared::capability_check` (`crates/shared/src/capability_check.rs`) already derives `RequiredCapabilities::from_request`, which scans messages and sets **one** `vision` flag for *both* `ImageUrl` and `InputAudio` (`:64`) — so it cannot distinguish images from audio. The module is `pub` (used by `tt-core`).
- `tt_shared::messages` (`:182-208`): `MessageContent::{Text(String), Parts(Vec<ContentPart>)}`; `ContentPart::{Text{text}, ImageUrl{image_url:ImageUrl}, InputAudio{input_audio:InputAudio}}`; `Message::{System{content}, User{content,name?}, Assistant{content:Option<MessageContent>,tool_calls,name?}, Tool{content,tool_call_id}}`.
- `tt_routing::RouteConditions` (`crates/routing/src/lib.rs:52-65`): `model_in`, `input_tokens_lt`, `input_tokens_gt`, `tag_equals`, all `#[serde(default)]`. Matcher `matches()` (`:142-168`) AND-es them. Test helpers (`:178-220`): `make_route`, `make_req(model)`, `make_ctx(tag)`. The crate-doc comment + `RouteAction.target_model` doc reference **"ADR-007"** for same-provider (`:70-72`).
- `tt_plan_core::types::RouteConditions` (`crates/plan-core/src/types.rs:114-126`) mirrors the four fields; `matches_conditions` (`crates/plan-core/src/routing.rs:18-38`) replays against `RequestLog` (no modality fields). Test helpers `req()`/`route()` (`routing.rs:47-86`).
- Gateway integration harness to mirror: `crates/core/tests/route_rewrite.rs` (RecordingProvider, `chat_request`, `issue_key_for`, `InMemoryRoutingStore`→`CachingRoutingStore`→`build_router`).
- `.claude/DECISIONS.md` ADRs run **001–017**; **ADR-007 is "Apalis on Postgres"** — the routing constraint is mislabeled. Next free number is **ADR-018**.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/shared/src/capability_check.rs` (modify) | Add `request_has_images` / `request_has_audio` pub fns + tests. |
| `crates/routing/src/lib.rs` (modify) | Add `has_images`/`has_audio` to `RouteConditions`; two matcher arms; unit tests; fix the ADR-007→ADR-018 comment. |
| `crates/plan-core/src/types.rs` (modify) | Mirror the two fields on `RouteConditions`. |
| `crates/plan-core/src/routing.rs` (modify) | Conservative modality arm in `matches_conditions` + test. |
| `crates/core/tests/route_content_type.rs` (create) | E2E: image request → vision target; text-only → no match; non-vision target → capability-guard skip. |
| `.claude/DECISIONS.md` (modify) | Add ADR-018 (same-provider routing). |

---

## Task 1: Modality detection helpers in `tt_shared`

**Files:**
- Modify: `crates/shared/src/capability_check.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/shared/src/capability_check.rs`, in the `#[cfg(test)] mod tests` block, extend the import line `messages::{ImageUrl, ResponseFormat, Tool, ToolCall, ToolCallFunction, ToolFunction}` to also bring in `InputAudio`:

```rust
        messages::{ImageUrl, InputAudio, ResponseFormat, Tool, ToolCall, ToolCallFunction, ToolFunction},
```

Then add these tests at the end of the `tests` module (before its closing `}`):

```rust
    #[test]
    fn request_has_images_detects_image_part() {
        let mut req = base_req();
        req.messages = vec![Message::User {
            content: MessageContent::Parts(vec![
                ContentPart::Text { text: "look".into() },
                ContentPart::ImageUrl {
                    image_url: ImageUrl { url: "data:image/png;base64,abc".into(), detail: None },
                },
            ]),
            name: None,
        }];
        assert!(request_has_images(&req));
        assert!(!request_has_audio(&req));
    }

    #[test]
    fn request_has_audio_detects_audio_part() {
        let mut req = base_req();
        req.messages = vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::InputAudio {
                input_audio: InputAudio { data: "abc".into(), format: "wav".into() },
            }]),
            name: None,
        }];
        assert!(request_has_audio(&req));
        assert!(!request_has_images(&req));
    }

    #[test]
    fn plain_text_request_has_no_modality() {
        let req = base_req();
        assert!(!request_has_images(&req));
        assert!(!request_has_audio(&req));
    }
```

- [ ] **Step 2: Run the tests to verify they fail (do not compile)**

Run: `cargo test -p tt-shared capability_check`
Expected: FAIL — `cannot find function request_has_images` / `request_has_audio`.

- [ ] **Step 3: Write the implementation**

In `crates/shared/src/capability_check.rs`, immediately **above** the `#[cfg(test)] mod tests` block, add:

```rust
/// True when any message carries an image (`ContentPart::ImageUrl`) content part.
///
/// Distinct from [`RequiredCapabilities`], which collapses image **and** audio
/// into a single `vision` flag; routing needs to tell the two modalities apart.
pub fn request_has_images(req: &ChatCompletionRequest) -> bool {
    req.messages.iter().any(|m| content_of(m).is_some_and(has_image_part))
}

/// True when any message carries an audio (`ContentPart::InputAudio`) content part.
pub fn request_has_audio(req: &ChatCompletionRequest) -> bool {
    req.messages.iter().any(|m| content_of(m).is_some_and(has_audio_part))
}

/// The content of a message, if it has any (Assistant content is optional).
fn content_of(m: &Message) -> Option<&MessageContent> {
    match m {
        Message::User { content, .. }
        | Message::System { content }
        | Message::Tool { content, .. } => Some(content),
        Message::Assistant { content, .. } => content.as_ref(),
    }
}

fn has_image_part(c: &MessageContent) -> bool {
    matches!(c, MessageContent::Parts(parts)
        if parts.iter().any(|p| matches!(p, ContentPart::ImageUrl { .. })))
}

fn has_audio_part(c: &MessageContent) -> bool {
    matches!(c, MessageContent::Parts(parts)
        if parts.iter().any(|p| matches!(p, ContentPart::InputAudio { .. })))
}
```

(`ContentPart`, `Message`, `MessageContent`, `ChatCompletionRequest` are already imported at the top of the file, `:22-26`.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p tt-shared capability_check`
Expected: PASS (existing capability_check tests + the 3 new ones).

- [ ] **Step 5: Commit**

```bash
git add crates/shared/src/capability_check.rs
git commit -m "feat(shared): request_has_images/has_audio modality detectors"
```

---

## Task 2: `has_images` / `has_audio` conditions + matcher (`tt_routing`)

**Files:**
- Modify: `crates/routing/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

In `crates/routing/src/lib.rs`, in the `#[cfg(test)] mod tests` block, extend the import:

```rust
    use tt_shared::{
        context::{ProviderCredentials, SecretString},
        messages::{ContentPart, ImageUrl, InputAudio},
        ChatCompletionRequest, Message, MessageContent,
    };
```

Add a request builder helper next to `make_req` (after it, inside the tests module):

```rust
    fn make_req_with_part(model: &str, part: ContentPart) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![part]),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    fn image_part() -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrl { url: "data:image/png;base64,abc".into(), detail: None },
        }
    }

    fn audio_part() -> ContentPart {
        ContentPart::InputAudio {
            input_audio: InputAudio { data: "abc".into(), format: "wav".into() },
        }
    }
```

Add the tests at the end of the tests module:

```rust
    #[test]
    fn has_images_true_matches_only_image_requests() {
        let route = Route {
            when: RouteConditions { has_images: Some(true), ..Default::default() },
            ..make_route("vision", 10, vec![], "vision-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        // Image request matches.
        assert!(eng
            .evaluate(&make_req_with_part("gpt-4o", image_part()), &make_ctx(None), 100)
            .is_some());
        // Plain-text request does not.
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn has_images_false_matches_only_non_image_requests() {
        let route = Route {
            when: RouteConditions { has_images: Some(false), ..Default::default() },
            ..make_route("text", 10, vec![], "cheap")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng.evaluate(&make_req("gpt-4o"), &make_ctx(None), 100).is_some());
        assert!(eng
            .evaluate(&make_req_with_part("gpt-4o", image_part()), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn has_audio_true_matches_only_audio_requests() {
        let route = Route {
            when: RouteConditions { has_audio: Some(true), ..Default::default() },
            ..make_route("audio", 10, vec![], "audio-model")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req_with_part("gpt-4o", audio_part()), &make_ctx(None), 100)
            .is_some());
        assert!(eng
            .evaluate(&make_req_with_part("gpt-4o", image_part()), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn modality_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                has_images: Some(true),
                ..Default::default()
            },
            ..make_route("both", 10, vec!["gpt-4o"], "vision-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        // model matches AND image present → match.
        assert!(eng
            .evaluate(&make_req_with_part("gpt-4o", image_part()), &make_ctx(None), 100)
            .is_some());
        // model matches but no image → no match.
        assert!(eng.evaluate(&make_req("gpt-4o"), &make_ctx(None), 100).is_none());
        // image present but wrong model → no match.
        assert!(eng
            .evaluate(&make_req_with_part("other", image_part()), &make_ctx(None), 100)
            .is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail (do not compile)**

Run: `cargo test -p tt-routing`
Expected: FAIL — `RouteConditions` has no field `has_images` / `has_audio`.

- [ ] **Step 3: Add the fields**

In `crates/routing/src/lib.rs`, in `RouteConditions` (after `tag_equals`, before the closing `}` at `:65`):

```rust
    /// Match only if the request carries at least one image input part
    /// (`ContentPart::ImageUrl`). `Some(false)` requires no image; `None` ignores.
    #[serde(default)]
    pub has_images: Option<bool>,
    /// Match only if the request carries at least one audio input part
    /// (`ContentPart::InputAudio`). `Some(false)` requires no audio; `None` ignores.
    #[serde(default)]
    pub has_audio: Option<bool>,
```

- [ ] **Step 4: Add the matcher arms**

In `matches()` (`crates/routing/src/lib.rs:142-168`), immediately before the final `true`:

```rust
    if let Some(want) = c.has_images {
        if tt_shared::capability_check::request_has_images(req) != want {
            return false;
        }
    }
    if let Some(want) = c.has_audio {
        if tt_shared::capability_check::request_has_audio(req) != want {
            return false;
        }
    }
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p tt-routing`
Expected: PASS (existing matcher tests + the 4 new ones).

- [ ] **Step 6: Commit**

```bash
git add crates/routing/src/lib.rs
git commit -m "feat(routing): has_images/has_audio modality conditions + matcher"
```

---

## Task 3: Plan-core mirror (`tt_plan_core`)

**Files:**
- Modify: `crates/plan-core/src/types.rs`
- Modify: `crates/plan-core/src/routing.rs`

- [ ] **Step 1: Write the failing test**

In `crates/plan-core/src/routing.rs`, in the `#[cfg(test)] mod tests` block, add:

```rust
    #[test]
    fn modality_condition_never_matches_historical_log() {
        // RequestLog carries no modality, so a modality-conditioned route must
        // not match — Plan stays conservative and never over-projects savings.
        let r = route(
            "img-only",
            10,
            true,
            RouteConditions { has_images: Some(true), ..Default::default() },
        );
        assert!(match_route(&req("m", 1, None), &[r]).is_none());

        let r2 = route(
            "no-img",
            10,
            true,
            RouteConditions { has_images: Some(false), ..Default::default() },
        );
        assert!(match_route(&req("m", 1, None), &[r2]).is_none());
    }
```

- [ ] **Step 2: Run the test to verify it fails (does not compile)**

Run: `cargo test -p tt-plan-core routing`
Expected: FAIL — `RouteConditions` has no field `has_images`.

- [ ] **Step 3: Add the mirror fields**

In `crates/plan-core/src/types.rs`, in `RouteConditions` (after `tag_equals`, `:126`):

```rust
    /// Mirror of `tt_routing::RouteConditions::has_images`. Not evaluable in
    /// replay (RequestLog records no modality) — see `matches_conditions`.
    #[serde(default)]
    pub has_images: Option<bool>,
    /// Mirror of `tt_routing::RouteConditions::has_audio`. See `has_images`.
    #[serde(default)]
    pub has_audio: Option<bool>,
```

- [ ] **Step 4: Add the conservative arm**

In `crates/plan-core/src/routing.rs`, in `matches_conditions` (`:18-38`), immediately before the final `true`:

```rust
    // Modality conditions cannot be evaluated against historical RequestLog rows
    // (no modality recorded). Treat ANY modality requirement as a non-match so
    // Plan never over-projects savings. Follow-up: capture had_images/had_audio
    // on request_logs to enable modality projection.
    if c.has_images.is_some() || c.has_audio.is_some() {
        return false;
    }
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p tt-plan-core`
Expected: PASS (existing routing tests + the new one).

- [ ] **Step 6: Commit**

```bash
git add crates/plan-core/src/types.rs crates/plan-core/src/routing.rs
git commit -m "feat(plan-core): mirror has_images/has_audio (conservative no-match in replay)"
```

---

## Task 4: Gateway end-to-end test (`tt-core`)

**Files:**
- Create: `crates/core/tests/route_content_type.rs`

- [ ] **Step 1: Write the test file**

Create `crates/core/tests/route_content_type.rs` with the full contents below. It mirrors `route_rewrite.rs` but the provider exposes vision-capable models and the requests carry an image part.

```rust
//! End-to-end: a `has_images` route rewrites image requests to the routed
//! (vision-capable) model, leaves text-only requests alone, and is skipped by
//! the capability guard when the target is not vision-capable.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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

struct VisionProvider {
    served_models: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for VisionProvider {
    fn id(&self) -> &'static str {
        "vision-mock"
    }
    fn models(&self) -> Vec<ModelInfo> {
        let vision = vec![Capability::Text, Capability::Vision];
        vec![
            ModelInfo { id: "vision-pro".into(), provider: "vision-mock".into(),
                capabilities: vision.clone(), max_input_tokens: 128_000, max_output_tokens: 4096 },
            ModelInfo { id: "vision-mini".into(), provider: "vision-mock".into(),
                capabilities: vision, max_input_tokens: 128_000, max_output_tokens: 4096 },
            ModelInfo { id: "text-only".into(), provider: "vision-mock".into(),
                capabilities: vec![Capability::Text], max_input_tokens: 8192, max_output_tokens: 4096 },
        ]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (i, o) = match model {
            "vision-pro" => (5.0, 15.0),
            "vision-mini" => (0.15, 0.6),
            _ => (1.0, 2.0),
        };
        Some(ModelPricing {
            input_per_million: i, output_per_million: o,
            cached_input_per_million: None, cache_write_per_million: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(&self, req: ChatCompletionRequest, _ctx: &RequestContext)
        -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.served_models.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "chatcmpl-vis".into(), object: "chat.completion".into(), created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("ok".into())),
                    tool_calls: vec![], name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage { prompt_tokens: 5, completion_tokens: 5, total_tokens: 10,
                cached_tokens: 0, cache_creation_input_tokens: None },
        })
    }
    async fn chat_completion_stream(&self, _req: ChatCompletionRequest, _ctx: &RequestContext)
        -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(&self, _req: EmbeddingsRequest, _ctx: &RequestContext)
        -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "test-key", Environment::Live, Actor::System)
        .await
        .expect("issue tt_live_ key")
        .plaintext
}

fn text_request(model: &str, bearer: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello world" }],
        "stream": false,
    });
    Request::builder().method("POST").uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string())).unwrap()
}

fn image_request(model: &str, bearer: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": [
            { "type": "text", "text": "what is this?" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" } }
        ]}],
        "stream": false,
    });
    Request::builder().method("POST").uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string())).unwrap()
}

async fn setup(target_model: &str) -> (Arc<Mutex<Vec<String>>>, String, axum::Router) {
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(VisionProvider {
        served_models: Arc::clone(&served),
        calls,
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(org_id, vec![Route {
        id: Uuid::now_v7(),
        name: "image-route".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["vision-pro".into()],
            has_images: Some(true),
            ..Default::default()
        },
        then: RouteAction { target_model: target_model.into(), fallbacks: Vec::new(), force_cache_layer: None },
    }]);
    let routing = Arc::new(CachingRoutingStore::new(routes_backing as Arc<dyn RoutingStore>));

    let app = build_router(
        AppState::new(registry).with_key_store(key_store).with_routing_store(routing),
    );
    (served, plaintext, app)
}

#[tokio::test]
async fn image_request_routed_to_vision_target() {
    let (served, key, app) = setup("vision-mini").await;
    let resp = app.oneshot(image_request("vision-pro", &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served.lock().unwrap().clone(), vec!["vision-mini".to_string()]);
}

#[tokio::test]
async fn text_only_request_does_not_match_has_images_route() {
    let (served, key, app) = setup("vision-mini").await;
    let resp = app.oneshot(text_request("vision-pro", &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served.lock().unwrap().clone(), vec!["vision-pro".to_string()]);
}

#[tokio::test]
async fn image_route_skipped_when_target_not_vision_capable() {
    // Capability guard: an image request requires vision; a text-only target is
    // skipped and the original model is dispatched.
    let (served, key, app) = setup("text-only").await;
    let resp = app.oneshot(image_request("vision-pro", &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(served.lock().unwrap().clone(), vec!["vision-pro".to_string()]);
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p tt-core --test route_content_type`
Expected: PASS (3 tests). If `image_route_skipped_when_target_not_vision_capable` fails because the served model is `text-only`, the capability guard is not engaging for this request shape — STOP and inspect `apply_routing` in `crates/core/src/routes/chat.rs:1629-1652` rather than weakening the test.

- [ ] **Step 3: Commit**

```bash
git add crates/core/tests/route_content_type.rs
git commit -m "test(core): e2e content-type routing (image→vision, text passthrough, guard skip)"
```

---

## Task 5: Record ADR-018 (same-provider routing) + fix the mislabel

**Files:**
- Modify: `.claude/DECISIONS.md`
- Modify: `crates/routing/src/lib.rs`

- [ ] **Step 1: Append ADR-018 to DECISIONS.md**

At the end of `.claude/DECISIONS.md`, append:

```markdown

## ADR-018 — v1 routing is same-provider only (2026-06-04)

**Context:** The routing engine rewrites a request's `model` to a cheaper target.
Cross-provider rewrites (e.g. `gpt-4o` → a Gemini model) need each provider's
pricing table reconciled before Plan can project savings honestly, and they change
the credential/capability resolution path. The cloud create/patch handlers already
reject cross-provider rewrites in `routes_admin.rs::validate_same_provider`.

**Decision:** v1 routing (including the V3a content-type slice) requires the target
model to be on the **same provider** as the source. Cross-provider routing is a
later slice, gated on unified cross-provider pricing in `tt_plan_core`.

**Consequences:** Content-type routes (`has_images`/`has_audio`) pick a same-provider
model of the required capability; the capability guard still skips a target lacking
the capability. NOTE: earlier code/comments/error-messages labelled this constraint
"ADR-007" — that number is actually the Apalis decision. The `tt_routing` comment is
corrected here; the `cloud` error-message string (`routes_admin.rs:59`) is updated in
the V3a-2 (cloud) plan.
```

- [ ] **Step 2: Fix the mislabeled reference in `tt_routing`**

In `crates/routing/src/lib.rs`, in the `RouteAction.target_model` doc comment (`:70-72`), change `ADR-007` to `ADR-018`:

```rust
    /// Rewrite to this model on the same provider as the request (v1 is
    /// same-provider only — see ADR-018 / Plan design for the cross-provider
    /// constraint).
    pub target_model: String,
```

- [ ] **Step 3: Verify + commit**

Run: `grep -n "ADR-018" .claude/DECISIONS.md crates/routing/src/lib.rs`
Expected: a match in each file. Then:

```bash
git add .claude/DECISIONS.md crates/routing/src/lib.rs
git commit -m "docs(adr): ADR-018 same-provider routing; fix ADR-007 mislabel in tt_routing"
```

---

## Task 6: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt -p tt-shared -p tt-routing -p tt-plan-core -p tt-core`
Then: `git diff --quiet || git commit -am "style: cargo fmt (v3a engine)"`

- [ ] **Step 2: Clippy (`-D warnings`)**

Run: `cargo clippy -p tt-shared -p tt-routing -p tt-plan-core --all-targets -- -D warnings`
Then: `cargo clippy -p tt-core --tests --test route_content_type -- -D warnings` (or `cargo clippy -p tt-core --all-targets -- -D warnings` if fast enough).
Expected: no warnings/errors.

- [ ] **Step 3: Build + scoped tests**

Run: `cargo test -p tt-shared -p tt-routing -p tt-plan-core` then `cargo test -p tt-core --test route_content_type --test route_rewrite`
Expected: all pass; `route_rewrite` still green (no regression to existing routing).

- [ ] **Step 4: Confirm clean tree + commits**

```bash
git status
git log --oneline -8
```
Expected: clean tree; the Task 1–5 commits present on `feat/v3a-content-type-routing`.

---

## Self-Review (completed by plan author)

**1. Spec coverage** — every in-scope spec goal maps to a task: extensible framework + modality fields → Tasks 2/3; modality detection reused once → Task 1; gateway matching (no gateway src change needed; capability guard already present) proven → Task 4; ADR-007 recorded → Task 5. Out-of-scope (CLI, user-facing `/v1/routes` API, cloud validation, dashboard) is correctly deferred to V3a-2 per the revised spec slicing.

**2. Placeholder scan** — no TBD/TODO; every code step is complete; every command has expected output. The only "follow-up" note (capture modality in `request_logs`) is an explicit out-of-scope item, not a plan gap.

**3. Type consistency** — `request_has_images`/`request_has_audio` defined in Task 1 (`tt_shared::capability_check`, `pub`) and called fully-qualified in Task 2's matcher. `RouteConditions.has_images/has_audio: Option<bool>` defined identically in `tt_routing` (Task 2) and `tt_plan_core` (Task 3). Test helpers (`make_req`, `make_ctx`, `make_route`, `req`, `route`) match the existing modules. The `tt-core` test mirrors `route_rewrite.rs`'s real types (`build_router`, `AppState`, `InMemoryRoutingStore`, `CachingRoutingStore`, `ProviderRegistry`, `Provider`, `ModelInfo`, `Capability`).

**Known follow-on (V3a-2):** user-facing `/v1/routes` API (org-from-key), `tt route` CLI, cloud `validate_same_provider` capability check for `has_images`, dashboard exposure, and updating the `cloud` ADR-007 error-message string.
