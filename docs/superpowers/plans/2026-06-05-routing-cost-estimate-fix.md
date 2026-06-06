# Routing Cost-Estimate Undercount Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `apply_routing`'s cost-condition / route-ceiling token estimate use the full prompt (`message_text_for_estimation`) instead of only the last user message, fixing the multi-turn/system-prompt undercount.

**Architecture:** One focused change in `crates/core/src/routes/chat.rs::apply_routing` — compute the full-prompt token estimate once, use it for both the cost path and the capability guard, fix the false comment, and delete the now-unused `last_user_message_text`. Add a failing-first integration test proving a large-prompt request now reroutes on a cost condition.

**Tech Stack:** Rust, axum 0.7 gateway, tt-tokenize, the `tt_routing` in-memory store + `build_router` integration-test harness (as in `crates/core/tests/cost_routing.rs`).

Spec: `docs/superpowers/specs/2026-06-05-routing-cost-estimate-fix-design.md`. Branch `routing-cost-estimate-fix` (off `main`, spec committed).

**Verified anchors:**
- `apply_routing` input estimate: chat.rs:1795-1805 (comment + `input_tokens` via `last_user_message_text`).
- Capability `estimated_tokens`: chat.rs:1827-1830 (already full-prompt via `message_text_for_estimation`).
- `fn last_user_message_text`: chat.rs:1323-1339 (doc comment + body); its ONLY caller is chat.rs:1803 (workspace grep confirmed) → becomes dead after the fix.
- `tt_shared::message_text_for_estimation(&ChatCompletionRequest) -> String` (capability_check.rs:134).
- Harness reference: `crates/core/tests/cost_routing.rs` (RecordingProvider + `estimated_cost_gt` route + `build_router`).

---

### Task 1: Failing integration test (proves the undercount)

**Files:**
- Create: `crates/core/tests/routing_full_prompt_cost.rs`

- [ ] **Step 1: Write the test**

Create `crates/core/tests/routing_full_prompt_cost.rs`:

```rust
//! Routing cost conditions estimate the FULL prompt (system + all turns), not
//! just the last user message. A request with a large system prompt + a tiny
//! last user message must reroute on an `estimated_cost_gt` route — under the old
//! last-message-only estimate its cost fell below the threshold and it would not.

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

/// Serves gpt-4o (expensive) + gpt-4o-mini (cheap); records the served model.
struct RecordingProvider {
    served: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["gpt-4o", "gpt-4o-mini"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "recording".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 200_000,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (input_per_million, output_per_million) = match model {
            "gpt-4o" => (5.0, 15.0),
            "gpt-4o-mini" => (0.15, 0.6),
            _ => (1.0, 2.0),
        };
        Some(ModelPricing {
            input_per_million,
            output_per_million,
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
        self.served.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "chatcmpl-rec".into(),
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
        Err(ProviderError::Unsupported("n/a".into()))
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "k", Environment::Live, Actor::System)
        .await
        .expect("issue tt_live_ key")
        .plaintext
}

/// gpt-4o request with a large SYSTEM prompt and a tiny last user message.
fn big_system_request(bearer: &str) -> Request<Body> {
    let big_system = "word ".repeat(2000); // ~10k chars → ~2.5k tokens (recording = chars/4)
    let body = json!({
        "model": "gpt-4o",
        "messages": [
            { "role": "system", "content": big_system },
            { "role": "user", "content": "hi" }
        ],
        "max_tokens": 10,
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// gpt-4o request with no system prompt and a tiny user message (control).
fn small_request(bearer: &str) -> Request<Body> {
    let body = json!({
        "model": "gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 10,
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn cost_condition_counts_full_prompt() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served: Arc::clone(&served),
    }));

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    // Route: when estimated cost > $0.001, downgrade gpt-4o → gpt-4o-mini.
    // Threshold sits BETWEEN the tiny-last-message cost (~$0.00016) and the
    // full-prompt cost (~$0.0126), so only full-prompt counting trips it.
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "cost-downgrade".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                estimated_cost_gt: Some(0.001),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
                force_cache_layer: None,
                disable_cache: false,
                max_cost_usd: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );

    // Large system prompt → full-prompt cost > $0.001 → reroute to gpt-4o-mini.
    let r1 = app
        .clone()
        .oneshot(big_system_request(&key))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(
        r1.headers()["x-tokentrimmer-model-used"].to_str().unwrap(),
        "gpt-4o-mini",
        "large system prompt should push full-prompt cost over the threshold and downgrade"
    );

    // Tiny single-message request → cost < $0.001 → passes through (unchanged).
    let r2 = app.oneshot(small_request(&key)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()["x-tokentrimmer-model-used"].to_str().unwrap(),
        "gpt-4o",
        "a tiny request stays under the threshold and is not rerouted"
    );
}
```

- [ ] **Step 2: Run it — confirm it FAILS on the current code**

Run: `cargo test -p tt-core --test routing_full_prompt_cost`
Expected: FAIL — the first assertion gets `gpt-4o` (not `gpt-4o-mini`), because the current last-message-only estimate prices the big-system request at ~$0.00016 < $0.001 so the route does not fire. (The control `r2` already passes.) This proves the bug.

---

### Task 2: Fix `apply_routing`

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (input estimate ~1795-1805; capability `estimated_tokens` ~1827-1830; delete `last_user_message_text` ~1323-1339)

- [ ] **Step 1: Use the full prompt for the input estimate**

In `crates/core/src/routes/chat.rs`, replace the comment + `input_tokens` block (the comment starting "// Input-tokens estimate for the route conditions." through the `let input_tokens = …unwrap_or(0);`):

```rust
    // Input-tokens estimate for the route conditions. The engine deliberately
    // leaves tokenization to callers; we delegate to the shared `tt-tokenize`
    // estimator so routing decisions use the SAME count `/v1/preview` reports
    // (tiktoken for openai/anthropic, chars/4 elsewhere) instead of a separate
    // heuristic. Tokenizer choice is keyed on the originally-requested model's
    // provider (resolved before any rewrite).
    let req_provider = state.registry.resolve(&req.model);
    let provider_id = req_provider.as_ref().map(|p| p.id()).unwrap_or("");
    let input_tokens = last_user_message_text(req)
        .map(|s| tt_tokenize::estimate_tokens(provider_id, s))
        .unwrap_or(0);
```

with:

```rust
    // Input-tokens estimate for the route conditions. Counts the ENTIRE prompt
    // (system + every turn) via the shared `message_text_for_estimation` helper —
    // the SAME text `/v1/preview`, live dispatch, and the capability guard below
    // all tokenize. Counting only the last user message undercounts multi-turn /
    // large-system-prompt requests, under-firing cost conditions and the route
    // `max_cost_usd` ceiling. Tokenizer choice is keyed on the originally-requested
    // model's provider (resolved before any rewrite).
    let req_provider = state.registry.resolve(&req.model);
    let provider_id = req_provider.as_ref().map(|p| p.id()).unwrap_or("");
    let input_tokens = {
        let combined = tt_shared::message_text_for_estimation(req);
        tt_tokenize::estimate_tokens(provider_id, &combined)
    };
```

- [ ] **Step 2: Reuse the estimate for the capability guard**

In the capability guard, replace (chat.rs:1827-1830):

```rust
    let estimated_tokens = {
        let combined = tt_shared::message_text_for_estimation(req);
        tt_tokenize::estimate_tokens(provider_id, &combined) as u64
    };
```

with:

```rust
    let estimated_tokens = u64::from(input_tokens);
```

- [ ] **Step 3: Delete the now-unused `last_user_message_text`**

Remove the function and its doc comment (chat.rs:1323-1339):

```rust
/// Extract the trailing user message's text content for embedding. Returns
/// `None` if the request has no user messages or the last user message is
/// multimodal-only (no text parts).
fn last_user_message_text(req: &ChatCompletionRequest) -> Option<&str> {
    for msg in req.messages.iter().rev() {
        if let Message::User { content, .. } = msg {
            return match content {
                MessageContent::Text(s) => Some(s.as_str()),
                MessageContent::Parts(parts) => parts.iter().find_map(|p| match p {
                    tt_shared::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                }),
            };
        }
    }
    None
}
```

(Delete the whole block. It has no other caller and no unit test.)

- [ ] **Step 4: Run the new test — confirm it PASSES**

Run: `cargo test -p tt-core --test routing_full_prompt_cost`
Expected: PASS — the big-system request now reroutes to `gpt-4o-mini`; the control still passes.

- [ ] **Step 5: Run the existing routing tests — confirm no regression**

Run: `cargo test -p tt-core --test cost_routing --test route_rewrite --test cross_provider --test embeddings_routing`
Expected: all PASS (these use single-message requests, where last-message == full-prompt, so the estimate is unchanged).

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/tests/routing_full_prompt_cost.rs
git commit -m "fix(core): route cost conditions estimate the full prompt, not just the last message"
```

---

### Task 3: Gates + finish the branch

**Files:** none (verification + PR)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt --all`
Then: `git diff --quiet || git commit -am "style: cargo fmt"`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0. In particular this confirms `last_user_message_text` was fully removed (no dead-code error) and `provider_id` is still used (it is — both the input estimate and capability guard use it).

- [ ] **Step 2: Tests + advisories**

Run: `cargo test -p tt-core`
Expected: all pass (the new test + the full pre-existing suite).
Run: `cargo deny check advisories`
Expected: ok.

- [ ] **Step 3: Finish the branch**

Use the **superpowers:finishing-a-development-branch** skill: verify tests, push `routing-cost-estimate-fix`, create the PR (option 2). PR body: the undercount fix (full-prompt estimate for cost conditions + ceiling, unified with the capability guard), the behavior-change note (cost-based routes now evaluate the full prompt), and the new test.

- [ ] **Step 4: Adversarial review + CI**

After the PR is open, run a Workflow-based adversarial review (lenses: estimate correctness/parity with /v1/preview + live dispatch; behavior-change blast radius — what existing routing decisions shift; test validity — does it truly fail pre-fix and pass post-fix) with per-finding verification against the real source. Watch CI; fix confirmed findings before merge. Update roadmap memory (F1 done) when green.

---

## Notes for the implementer

- **`provider_id` stays in use:** both the input estimate (step 1) and the capability guard (step 2) reference it, so removing `last_user_message_text` doesn't orphan it.
- **Why the test threshold is $0.001:** with the `recording` provider's chars/4 tokenizer, the big-system request (~10k chars → ~2.5k tokens × $5/M ≈ $0.0125) clears it while the tiny last message alone (~1 token ≈ $0.0000…) plus 10 output tokens (×$15/M = $0.00015) stays under — so the assertion flips exactly on the full-prompt-vs-last-message change.
- **No output-token change:** the estimate still uses `req.max_tokens` (or the default) for the output side; only the input side is corrected.
