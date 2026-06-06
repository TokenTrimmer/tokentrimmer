# Gateway + Client Embeddings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `POST /v1/embeddings` a real, routed, billing-correct endpoint (it returns 501 today) and add `Client::embed()` to the `tt-client` SDK.

**Architecture:** The embeddings handler mirrors chat's non-streaming routed dispatch, reusing chat's helpers (made `pub(crate)`). Routing is evaluated by handing `apply_routing` a synthetic `ChatCompletionRequest` built from the embeddings input, then copying the rewritten model back. The SDK gets a typed `embed()` method following the existing `parse_cost`/`Error` conventions.

**Tech Stack:** Rust, axum 0.7 (gateway), tt-shared types, tt-client (SDK), httpmock + the gateway's `app()`/integration-test harness.

Spec: `docs/superpowers/specs/2026-06-05-gateway-and-client-embeddings-design.md`. Branch `gateway-client-embeddings` (off `main`, spec committed).

**Key facts (verified against source):**
- `compute_cost(usage: &Usage, pricing: Option<&ModelPricing>, baseline_pricing: Option<&ModelPricing>, fee_multiplier: f64) -> (f64, f64)` — chat.rs:1453.
- `attach_cost_headers(headers: &mut HeaderMap, trace_id: Uuid, provider_id: &str, model_used: &str, cost_usd, baseline_cost_usd, saved_usd)` — chat.rs:1512.
- `resolve_credentials(state, org_id, provider_id, raw_bearer) -> ProviderCredentials` (chat.rs:1552); `resolve_credentials_for(state, org_id, provider_id, raw_bearer, allow_bearer_fallback) -> Option<ProviderCredentials>` (chat.rs:1577).
- `apply_routing(state, ctx, &mut ChatCompletionRequest) -> Option<RouteMatch>` (chat.rs:1725); `estimate_cost_usd(&ModelPricing, input_tokens: u32, max_tokens: Option<u32>) -> f64` (chat.rs:60).
- `struct RouteMatch { route_id: Uuid, fallbacks: Vec<String>, disable_cache: bool, max_cost_usd: Option<f64>, input_tokens_estimate: u32 }` (chat.rs:1707).
- `fee_multiplier` is `provider.fee_multiplier()`. Baseline pricing = `requested_pricing` when a route matched, else the served model's pricing (chat.rs:1048-1052).
- Provider trait: `async fn embeddings(&self, req: EmbeddingsRequest, ctx: &RequestContext) -> Result<EmbeddingsResponse, ProviderError>` (default = Unsupported; OpenAI native + compat implement it).
- Embedding models priced in pricing.toml: `text-embedding-3-small` ($0.02/M in, $0/M out), `text-embedding-3-large` ($0.13/M).

---

### Task 1: Make chat helpers reusable + `ChatCompletionRequest: Default`

**Files:**
- Modify: `crates/core/src/routes/chat.rs` (visibility of 6 fns + `RouteMatch`)
- Modify: `crates/shared/src/messages.rs` (`ChatCompletionRequest` derive + a test)

- [ ] **Step 1: Add `Default` to `ChatCompletionRequest` + a failing test**

In `crates/shared/src/messages.rs`, change the derive on `ChatCompletionRequest` (~line 119) from `#[derive(Debug, Clone, Serialize, Deserialize)]` to `#[derive(Debug, Clone, Default, Serialize, Deserialize)]`.

Add a test (in the `#[cfg(test)] mod tests` of `messages.rs`; if none exists, create one at the end of the file):

```rust
#[cfg(test)]
mod embeddings_default_tests {
    use super::*;

    #[test]
    fn chat_request_default_is_empty() {
        let r = ChatCompletionRequest::default();
        assert_eq!(r.model, "");
        assert!(r.messages.is_empty());
        assert!(!r.stream);
        assert!(r.tools.is_empty());
        assert!(r.max_tokens.is_none());
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p tt-shared chat_request_default_is_empty`
Expected: PASS (compiles only if every field is `Default`-able, which they are).

- [ ] **Step 3: Make the chat helpers `pub(crate)`**

In `crates/core/src/routes/chat.rs`, change these item signatures from private to `pub(crate)` (add `pub(crate)` before each — no other change):
- `fn estimate_cost_usd(` (~line 60) → `pub(crate) fn estimate_cost_usd(`
- `fn compute_cost(` (~line 1453) → `pub(crate) fn compute_cost(`
- `fn attach_cost_headers(` (~line 1512) → `pub(crate) fn attach_cost_headers(`
- `async fn resolve_credentials(` (~line 1552) → `pub(crate) async fn resolve_credentials(`
- `async fn resolve_credentials_for(` (~line 1577) → `pub(crate) async fn resolve_credentials_for(`
- `async fn apply_routing(` (~line 1725) → `pub(crate) async fn apply_routing(`
- `struct RouteMatch {` (~line 1707) → `pub(crate) struct RouteMatch {` and mark its five fields `pub(crate)`:
  ```rust
  pub(crate) struct RouteMatch {
      pub(crate) route_id: Uuid,
      pub(crate) fallbacks: Vec<String>,
      pub(crate) disable_cache: bool,
      pub(crate) max_cost_usd: Option<f64>,
      pub(crate) input_tokens_estimate: u32,
  }
  ```

- [ ] **Step 4: Verify chat still compiles + all chat tests green**

Run: `cargo test -p tt-core --lib routes::chat`
Expected: PASS (pure visibility change — no behavior difference). Also `cargo build -p tt-core`.

- [ ] **Step 5: Commit**

```bash
git add crates/shared/src/messages.rs crates/core/src/routes/chat.rs
git commit -m "refactor(core): pub(crate) dispatch helpers + Default for ChatCompletionRequest"
```

---

### Task 2: Embeddings handler + dispatch test

**Files:**
- Modify: `crates/core/src/routes/embeddings.rs` (replace the 501 stub)
- Modify: `crates/core/src/server.rs` (mock provider `embeddings()` returns vectors; replace the 501 test)

- [ ] **Step 1: Replace the handler**

Replace the entire contents of `crates/core/src/routes/embeddings.rs` with:

```rust
//! `POST /v1/embeddings` — OpenAI-compatible embeddings with routing + cost.
//!
//! Mirrors the chat handler's non-streaming routed dispatch (minus cache,
//! streaming, and failover). Routing is evaluated against a synthetic chat
//! request built from the embedding input, then the rewritten model is copied
//! back onto the embeddings request.

use axum::{
    extract::{Extension, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use uuid::Uuid;

use tt_auth::ApiKeyContext;
// `EmbeddingInput` is only exported from `tt_shared::messages` (not the crate root).
use tt_shared::messages::{EmbeddingInput, Message, MessageContent};
use tt_shared::{ChatCompletionRequest, EmbeddingsRequest, RequestContext};

use crate::middleware::trace::TraceId;
use crate::routes::chat::{
    apply_routing, attach_cost_headers, compute_cost, estimate_cost_usd, resolve_credentials,
    resolve_credentials_for,
};
use crate::{ApiError, ApiResult, AppState};

/// Flatten the embedding input to text for routing evaluation only (token
/// estimate + prompt-contains). A batch joins on newlines.
fn input_as_text(input: &EmbeddingInput) -> String {
    match input {
        EmbeddingInput::Single(s) => s.clone(),
        EmbeddingInput::Batch(v) => v.join("\n"),
    }
}

pub async fn handler(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(mut req): Json<EmbeddingsRequest>,
) -> ApiResult<Response> {
    // 1. Resolve provider (re-resolved after routing may rewrite the model).
    let mut provider = state
        .registry
        .resolve(&req.model)
        .ok_or_else(|| ApiError::ModelNotFound {
            model: req.model.clone(),
        })?;

    // 2. Bearer + trace id.
    let raw_bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .unwrap_or("")
        .to_string();
    let trace_id = if !trace.0.is_empty() {
        Uuid::parse_str(&trace.0).unwrap_or_else(|_| Uuid::now_v7())
    } else {
        Uuid::now_v7()
    };

    // 3. Identity + credentials (embeddings aren't cached, so no caller_tier/L2).
    let (org_id, api_key_id) = match auth_ctx.as_deref() {
        Some(c) => (c.org_id, c.key_id),
        None => (Uuid::nil(), Uuid::nil()),
    };
    let source_provider_id = provider.id().to_string();
    let credentials = resolve_credentials(&state, org_id, provider.id(), &raw_bearer).await;
    let mut ctx = RequestContext {
        trace_id,
        org_id,
        api_key_id,
        credentials,
        tag: headers
            .get("x-tokentrimmer-tag")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        deadline: None,
    };

    // 4. Baseline pricing on the ORIGINAL model, before routing rewrites it.
    let requested_pricing = provider.pricing(&req.model);

    // 5. Routing via a synthetic chat request (model + input text; no modality).
    let mut synth = ChatCompletionRequest {
        model: req.model.clone(),
        messages: vec![Message::User {
            content: MessageContent::Text(input_as_text(&req.input)),
            name: None,
        }],
        ..Default::default()
    };
    let route_match = apply_routing(&state, &ctx, &mut synth).await;
    req.model = synth.model; // adopt the routed model
    let matched = route_match.is_some();
    if matched {
        provider = state
            .registry
            .resolve(&req.model)
            .ok_or_else(|| ApiError::ModelNotFound {
                model: req.model.clone(),
            })?;
        // Cross-provider rewrite: re-resolve target credentials, fail closed.
        if provider.id() != source_provider_id {
            match resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, false).await {
                Some(c) => ctx.credentials = c,
                None => {
                    return Err(ApiError::MissingProviderCredential {
                        provider: provider.id().to_string(),
                    })
                }
            }
        }
        // Post-rewrite cost ceiling (V3d-2b). Output tokens are 0 for embeddings.
        if let Some(ceiling) = route_match.as_ref().and_then(|m| m.max_cost_usd) {
            if let Some(pr) = provider.pricing(&req.model) {
                let tokens = route_match
                    .as_ref()
                    .map(|m| m.input_tokens_estimate)
                    .unwrap_or(0);
                let routed_cost = estimate_cost_usd(&pr, tokens, None);
                if routed_cost > ceiling {
                    return Err(ApiError::CostLimitExceeded {
                        estimated_usd: routed_cost,
                        ceiling_usd: ceiling,
                    });
                }
            }
        }
    }

    // 6. Dispatch. Capture the served model + its pricing before `req` moves.
    let served_model = req.model.clone();
    let routed_pricing = provider.pricing(&served_model);
    let resp = provider.embeddings(req, &ctx).await?;

    // 7. Cost + headers + spend. Baseline against the original model when routed.
    let baseline_pricing = if matched {
        requested_pricing
    } else {
        routed_pricing.clone()
    };
    let (cost_usd, baseline_cost_usd) = compute_cost(
        &resp.usage,
        routed_pricing.as_ref(),
        baseline_pricing.as_ref(),
        provider.fee_multiplier(),
    );
    let saved_usd = (baseline_cost_usd - cost_usd).max(0.0_f64);
    state.spend_sink().record(org_id, cost_usd, Utc::now());

    let mut http = (StatusCode::OK, Json(resp)).into_response();
    attach_cost_headers(
        http.headers_mut(),
        trace_id,
        provider.id(),
        &served_model,
        cost_usd,
        baseline_cost_usd,
        saved_usd,
    );
    Ok(http)
}
```

- [ ] **Step 2: Make the mock provider return real embeddings**

In `crates/core/src/server.rs`, the `MockProvider`'s `embeddings()` (~line 236) currently returns `Unsupported`. Replace its body so dispatch succeeds:

```rust
        async fn embeddings(
            &self,
            req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Ok(EmbeddingsResponse {
                object: "list".into(),
                data: vec![EmbeddingData {
                    object: "embedding".into(),
                    index: 0,
                    embedding: vec![0.1, 0.2, 0.3],
                }],
                model: req.model,
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 0,
                    total_tokens: 100,
                    cached_tokens: 0,
                    cache_creation_input_tokens: None,
                },
            })
        }
```

Ensure `EmbeddingData` is imported in `server.rs`'s test module (add it to the existing `use tt_shared::...` test import list; the names `EmbeddingsRequest`/`EmbeddingsResponse`/`Usage` are already imported there).

- [ ] **Step 3: Replace the 501 test with a dispatch test**

In `crates/core/src/server.rs`, replace `embeddings_returns_501_not_implemented` (~line 454) with:

```rust
    #[tokio::test]
    async fn embeddings_dispatch_returns_200_with_headers() {
        let body = serde_json::json!({ "model": "mock-model-1", "input": "hello" });
        let response = app_with_mock()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        for h in [
            "x-tokentrimmer-trace-id",
            "x-tokentrimmer-provider",
            "x-tokentrimmer-model-used",
            "x-tokentrimmer-cost-usd",
            "x-tokentrimmer-baseline-cost-usd",
            "x-tokentrimmer-saved-usd",
        ] {
            assert!(response.headers().contains_key(h), "missing header {h}");
        }
        assert_eq!(
            response.headers()["x-tokentrimmer-model-used"]
                .to_str()
                .unwrap(),
            "mock-model-1"
        );
        // 100 input tokens × $1.0/M (mock pricing) = $0.0001.
        let cost: f64 = response.headers()["x-tokentrimmer-cost-usd"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((cost - 0.0001).abs() < 1e-9, "cost = {cost}");

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"][0]["embedding"][0], 0.1);
        assert_eq!(v["model"], "mock-model-1");
    }
```

- [ ] **Step 4: Build + run the gateway tests**

Run: `cargo test -p tt-core --lib embeddings`
Expected: `embeddings_dispatch_returns_200_with_headers` passes; no other test references the removed 501 test.
Run: `cargo test -p tt-core --lib` — all gateway unit tests green.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/embeddings.rs crates/core/src/server.rs
git commit -m "feat(core): implement /v1/embeddings (routed dispatch + cost headers)"
```

---

### Task 3: Embeddings routing integration test

**Files:**
- Create: `crates/core/tests/embeddings_routing.rs`

This mirrors `crates/core/tests/cost_routing.rs`'s harness but exercises the embeddings path: a `prompt_contains` route rewrites `text-embedding-3-large` → `-small`, proving the synthetic-request text extraction + model rewrite + savings work.

- [ ] **Step 1: Write the integration test**

Create `crates/core/tests/embeddings_routing.rs`:

```rust
//! Embeddings routing: a `prompt_contains` route rewrites the embedding model to
//! a cheaper one. Proves the synthetic-request adapter feeds the embedding input
//! text into the routing engine and that savings are reported.

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
    messages::{EmbeddingData, EmbeddingsRequest, EmbeddingsResponse},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Provider serving the two OpenAI embedding models with their real-ish input
/// rates; records the served model from each embeddings call.
struct RecordingEmbedder {
    served: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for RecordingEmbedder {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["text-embedding-3-large", "text-embedding-3-small"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "recording".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 0,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let input_per_million = match model {
            "text-embedding-3-large" => 0.13,
            "text-embedding-3-small" => 0.02,
            _ => 1.0,
        };
        Some(ModelPricing {
            input_per_million,
            output_per_million: 0.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("chat not used".into()))
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
        req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        self.served.lock().unwrap().push(req.model.clone());
        Ok(EmbeddingsResponse {
            object: "list".into(),
            data: vec![EmbeddingData {
                object: "embedding".into(),
                index: 0,
                embedding: vec![0.1, 0.2],
            }],
            model: req.model,
            usage: Usage {
                prompt_tokens: 1000,
                completion_tokens: 0,
                total_tokens: 1000,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "k", Environment::Live, Actor::System)
        .await
        .expect("issue tt_live_ key")
        .plaintext
}

fn embed_req(model: &str, bearer: &str, input: &str) -> Request<Body> {
    let body = json!({ "model": model, "input": input });
    Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn embeddings_prompt_route_downgrades_and_reports_savings() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingEmbedder {
        served: Arc::clone(&served),
    }));

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    // Route: when the input text contains "downgrade me", rewrite large → small.
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "embed-downgrade".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                prompt_contains_any_of: vec!["downgrade me".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: "text-embedding-3-small".into(),
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

    // Matching input → downgrade to -small, savings reported (baseline = large).
    let r1 = app
        .clone()
        .oneshot(embed_req(
            "text-embedding-3-large",
            &key,
            "please downgrade me now",
        ))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(
        r1.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "text-embedding-3-small",
        "matching input should downgrade the embedding model"
    );
    let saved: f64 = r1.headers()["x-tokentrimmer-saved-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    // baseline 1000×0.13/M − cost 1000×0.02/M = 0.00013 − 0.00002 = 0.00011.
    assert!(saved > 0.0, "expected positive savings, got {saved}");

    // Non-matching input → no route, served unchanged.
    let r2 = app
        .oneshot(embed_req("text-embedding-3-large", &key, "unrelated text"))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "text-embedding-3-large",
        "non-matching input should pass through"
    );

    let served = served.lock().unwrap().clone();
    assert_eq!(served, vec!["text-embedding-3-small", "text-embedding-3-large"]);
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p tt-core --test embeddings_routing`
Expected: PASS. (If `RouteConditions` field names differ — e.g. `prompt_contains_any_of` — fix to match the actual `tt_routing::RouteConditions` definition; check `crates/routing/src/lib.rs`. If `RouteAction` has fields beyond those listed, set them with `..Default::default()` only if it derives Default, else fill explicitly as `cost_routing.rs` does.)

- [ ] **Step 3: Commit**

```bash
git add crates/core/tests/embeddings_routing.rs
git commit -m "test(core): embeddings routing downgrade + savings integration test"
```

---

### Task 4: SDK `Client::embed()`

**Files:**
- Create: `crates/client/src/embeddings.rs`
- Modify: `crates/client/src/lib.rs` (module wire-up + re-exports)

- [ ] **Step 1: Re-export the embedding types**

In `crates/client/src/lib.rs`, extend the `pub use tt_shared::messages::{…}` block to also export the embedding types:

```rust
pub use tt_shared::messages::{
    ChatCompletionResponse, Choice, ContentPart, EmbeddingData, EmbeddingInput, EmbeddingsRequest,
    EmbeddingsResponse, ImageUrl, InputAudio, Message, MessageContent, Tool, ToolCall,
    ToolCallFunction, ToolChoice, ToolChoiceFunction, ToolFunction,
};
```

(Keep `pub use tt_shared::Usage;` as-is.)

- [ ] **Step 2: Add the module declaration**

In `crates/client/src/lib.rs`, near the other `mod`/`pub use` lines (after `pub use tools::{…}`), add:

```rust
mod embeddings;
pub use embeddings::EmbedOutcome;
```

- [ ] **Step 3: Write `embeddings.rs` with `embed()` + `EmbedOutcome`**

Create `crates/client/src/embeddings.rs`:

```rust
//! Embeddings: `Client::embed` posts to `/v1/embeddings` and returns the typed
//! response plus the gateway's cost/savings headers.

use serde_json::json;

use crate::{parse_cost, Client, CostInfo, EmbeddingInput, EmbeddingsResponse, Error, Result};

/// A completed embeddings call: the typed response plus parsed cost/savings.
#[derive(Debug, Clone)]
pub struct EmbedOutcome {
    pub response: EmbeddingsResponse,
    pub cost: CostInfo,
}

impl EmbedOutcome {
    /// The embedding rows, in returned order.
    pub fn vectors(&self) -> impl Iterator<Item = &[f32]> {
        self.response.data.iter().map(|d| d.embedding.as_slice())
    }
}

impl Client {
    /// Embed `input` with `model`. Returns the vectors + cost.
    ///
    /// # Errors
    /// [`Error::MissingModel`] if `model` is empty, [`Error::Request`] on
    /// transport failure, [`Error::Status`] on a non-2xx response (carrying the
    /// cost/trace telemetry), [`Error::Decode`] if the body isn't a valid
    /// embeddings response.
    pub async fn embed(
        &self,
        model: impl Into<String>,
        input: EmbeddingInput,
    ) -> Result<EmbedOutcome> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(Error::MissingModel);
        }
        let body = json!({ "model": model, "input": input });
        let resp = self
            .http
            .post(format!("{}/v1/embeddings", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(Error::Request)?;
        let cost = parse_cost(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
                cost: Box::new(cost),
            });
        }
        let response = resp
            .json::<EmbeddingsResponse>()
            .await
            .map_err(Error::Decode)?;
        Ok(EmbedOutcome { response, cost })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Client;
    use httpmock::prelude::*;

    fn embeddings_body() -> serde_json::Value {
        json!({
            "object": "list",
            "data": [
                { "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] },
                { "object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6] }
            ],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": 8, "completion_tokens": 0, "total_tokens": 8 }
        })
    }

    #[tokio::test]
    async fn embed_returns_vectors_and_cost() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_contains("text-embedding-3-small");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-cost-usd", "0.0002")
                .header("x-tokentrimmer-model-used", "text-embedding-3-small")
                .json_body(embeddings_body());
        });

        let client = Client::new(server.base_url(), "k");
        let out = client
            .embed(
                "text-embedding-3-small",
                EmbeddingInput::Batch(vec!["a".into(), "b".into()]),
            )
            .await
            .unwrap();

        let rows: Vec<&[f32]> = out.vectors().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], &[0.1, 0.2, 0.3]);
        assert_eq!(out.cost.cost_usd, Some(0.0002));
        assert_eq!(out.cost.model_used.as_deref(), Some("text-embedding-3-small"));
    }

    #[tokio::test]
    async fn embed_surfaces_status_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(501).body("not implemented");
        });
        let client = Client::new(server.base_url(), "k");
        let result = client
            .embed("m", EmbeddingInput::Single("hi".into()))
            .await;
        assert!(matches!(result, Err(Error::Status { status: 501, .. })));
    }

    #[tokio::test]
    async fn embed_without_model_errors_before_any_request() {
        // dead base — no network is touched because the model is empty.
        let client = Client::new("http://127.0.0.1:1", "k");
        let result = client.embed("  ", EmbeddingInput::Single("hi".into())).await;
        assert!(matches!(result, Err(Error::MissingModel)));
    }
}
```

- [ ] **Step 4: Run the SDK tests**

Run: `cargo test -p tt-client embed`
Expected: the three `embed_*` tests pass.
Run: `cargo test -p tt-client` — full SDK suite green.

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/embeddings.rs crates/client/src/lib.rs
git commit -m "feat(tt-client): Client::embed() + EmbedOutcome"
```

---

### Task 5: Gates + finish the branch

**Files:** none (verification + PR)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `git diff --quiet || git commit -am "style: cargo fmt"`

- [ ] **Step 2: Clippy (workspace, all targets, deny warnings)**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0. Fix anything flagged (e.g. a `needless_borrow`/`is_some_and` nit) and re-run.

- [ ] **Step 3: Tests + advisories + docs**

Run: `cargo test -p tt-core -p tt-client -p tt-shared`
Expected: all pass (incl. `embeddings_routing` integration test).
Run: `cargo test --workspace` once to confirm nothing else regressed (the `pub(crate)`/`Default` changes are workspace-visible).
Run: `cargo deny check advisories` — ok.
Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-client --no-deps` — exit 0.

- [ ] **Step 4: Finish the branch**

Use the **superpowers:finishing-a-development-branch** skill: verify tests, push `gateway-client-embeddings`, create the PR (option 2). PR body: gateway `/v1/embeddings` (routed dispatch, cost headers, spend), the `pub(crate)`/`Default` reuse, and the SDK `embed()`; note out-of-scope (cache, failover, dimensions/encoding_format, sandbox short-circuit).

- [ ] **Step 5: Adversarial review + CI**

After the PR is open, run a Workflow-based adversarial review (lenses: billing/cost correctness — baseline-vs-routed pricing, fee multiplier, saved_usd; routing-adapter correctness — synthetic request, cross-provider creds fail-closed, cost ceiling; SDK + API hygiene) with per-finding verification against the real source. Watch CI; fix confirmed findings before merge. Update roadmap memory when green.

---

## Notes for the implementer

- **One billing-critical handler:** keep `routes/embeddings.rs` structurally identical to chat's setup (org/credential/trace extraction copied verbatim) so auth + attribution behave the same. The only embeddings-specific logic is the synthetic-request routing adapter and the no-cache/no-stream/no-failover simplification.
- **Sandbox + L2 deliberately omitted:** embeddings has no `tt_test_*` short-circuit and no L2 entitlement (it isn't cached). A `tt_test_*` key falls through to normal dispatch.
- **`provider.embeddings(req, &ctx)` takes `req` by value** — capture `served_model`/`routed_pricing` before the move (the plan code already does).
- **Baseline pricing:** `requested_pricing` (original model) only when a route matched; otherwise the served model's pricing → `saved_usd == 0` for un-routed requests (matches chat).
- **`RouteConditions`/`RouteAction` fields:** the integration test uses `prompt_contains_any_of` and the `RouteAction` shape from `cost_routing.rs`. If the actual structs differ, mirror `cost_routing.rs` exactly (it's the source of truth for the harness).
