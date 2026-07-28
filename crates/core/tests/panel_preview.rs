//! Task 7 — `/v1/preview` panel dry-run estimate tests.
//!
//! These tests assert:
//! 1. A preview request with `X-TokenTrimmer-Panel: synthesize` and 2 priced
//!    members returns 200 with a `panel` object: members length 2 + arbiter +
//!    numeric `total_estimated_cost_usd` that matches Fusion's shared static
//!    dispatch plan. The mock dispatch counter MUST remain at ZERO — no
//!    providers are called.
//! 2. A member with a bogus/unpriced model ⇒ `total_estimated_cost_usd: null`
//!    AND `within_budget: false` (fail-closed); still zero dispatch.
//! 3. `max_completion_tokens`, `n`, and a body budget use that same static
//!    plan; Majority and missing output caps fail closed rather than pricing a
//!    flatter, executable-looking plan.
//! 4. No panel header ⇒ response has no `panel` key (unchanged behavior).
//!
//! NOTE: The `/v1/preview` base-model lookup uses the `tt_preview` pricing
//! catalog, which covers real provider models (gpt-4o, gpt-4o-mini, etc.).
//! Panel member cost estimation uses `AppState.registry` (the mock providers
//! registered below). The test models are real catalog names so the base
//! preview succeeds, and the mock providers share those same model ids so
//! `estimate_panel_cost` can price them.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tower::util::ServiceExt;

use tt_core::{
    build_router,
    routes::panel::{
        estimate_panel_cost, ArbiterStrategyKind, ModelRef, PanelAdmissionEstimate, PanelConfig,
    },
    AppState, ProviderRegistry,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 256 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------------------------------------------------------------------------
// CountedMock — priced provider that counts calls to chat_completion.
//
// Model ids use real catalog names (gpt-4o, gpt-4o-mini, gpt-4o-realtime-*)
// so the base /v1/preview model-lookup passes while these providers intercept
// any actual dispatch attempts (which must not happen in dry-run).
// ---------------------------------------------------------------------------

struct CountedMock {
    id: &'static str,
    model: &'static str,
    calls: Arc<AtomicUsize>,
}

impl CountedMock {
    fn new(id: &'static str, model: &'static str, calls: Arc<AtomicUsize>) -> Self {
        Self { id, model, calls }
    }
}

#[async_trait]
impl Provider for CountedMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: 1.0,
                output_per_million: 2.0,
                cached_input_per_million: None,
                cache_write_per_million: None,
                batch_input_per_million: None,
                batch_output_per_million: None,
                flex_input_per_million: None,
                flex_output_per_million: None,
                prompt_cache_min_tokens: None,
                effective_at: Utc::now(),
            })
        } else {
            None
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("answer".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
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

// ---------------------------------------------------------------------------
// App builder: 2 priced members ("gpt-4o" on "openai", "gpt-4o-mini" on
// "openai-mini") and an arbiter ("gpt-4o" on "openai"). All share the
// call counter so any dispatch shows up as > 0.
//
// These are real pricing-catalog model names so the base /v1/preview
// model lookup succeeds; the AppState registry prices them via the mock
// for the panel dry-run estimate.
// ---------------------------------------------------------------------------

fn app_two_priced_members() -> (axum::Router, Arc<AtomicUsize>, AppState) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountedMock::new(
        "openai",
        "gpt-4o",
        Arc::clone(&calls),
    )));
    registry.register(Arc::new(CountedMock::new(
        "openai-mini",
        "gpt-4o-mini",
        Arc::clone(&calls),
    )));
    let state = AppState::new(registry).with_panel_enabled(true);
    (build_router(state.clone()), calls, state)
}

fn synthesize_config(max_cost_usd: Option<f64>) -> PanelConfig {
    PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![
            ModelRef {
                model: "gpt-4o".to_string(),
                provider: None,
            },
            ModelRef {
                model: "gpt-4o-mini".to_string(),
                provider: None,
            },
        ],
        arbiter_model: ModelRef {
            model: "gpt-4o".to_string(),
            provider: None,
        },
        quorum: None,
        max_cost_usd,
    }
}

fn assert_same_cost(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-15,
        "preview must use the shared static plan: actual={actual}, expected={expected}"
    );
}

// ---------------------------------------------------------------------------
// Test 1: panel dry-run returns panel object with 2 members + arbiter +
//         numeric total_estimated_cost_usd; NO dispatch (counter == 0).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_panel_returns_estimate_with_zero_dispatch() {
    let (app, calls, state) = app_two_priced_members();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-panel", "synthesize")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "What is the capital of France?" }],
                "max_tokens": 100,
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "preview with panel header must return 200"
    );

    let body = body_json(resp).await;

    // The panel estimate object must be present.
    let panel = &body["panel"];
    assert!(
        panel.is_object(),
        "response must carry a top-level `panel` object, got: {body}"
    );

    // Strategy must be reported.
    assert_eq!(
        panel["strategy"].as_str().unwrap_or(""),
        "synthesize",
        "strategy must be 'synthesize'"
    );

    // Members: exactly 2.
    let members = panel["members"].as_array().expect("members must be array");
    assert_eq!(members.len(), 2, "must have exactly 2 members");
    for m in members {
        assert!(
            m["model"].is_string(),
            "each member must have a `model` field"
        );
        assert!(
            m["provider"].is_string(),
            "each member must have a `provider` field"
        );
    }

    // Arbiter must be present.
    let arbiter = &panel["arbiter"];
    assert!(
        arbiter.is_object(),
        "panel must carry an `arbiter` object, got: {panel}"
    );
    assert!(
        arbiter["model"].is_string(),
        "arbiter must have a `model` field"
    );

    // total_estimated_cost_usd must be numeric (all models are priced).
    let total = panel["total_estimated_cost_usd"].as_f64();
    assert!(
        total.is_some(),
        "total_estimated_cost_usd must be numeric when all members are priced, got: {panel}"
    );
    assert!(
        total.unwrap() > 0.0,
        "total_estimated_cost_usd must be positive"
    );

    // The total must be the same capped member/arbiter plan that Fusion
    // admission prices — not the old flat 3-leg catalog sum.
    let expected = estimate_panel_cost(
        &state,
        &synthesize_config(None),
        PanelAdmissionEstimate {
            input_tokens: body["current"]["input_tokens_estimated"]
                .as_u64()
                .expect("preview input token estimate") as u32,
            max_tokens: Some(100),
            max_completion_tokens: None,
            n: None,
        },
    )
    .expect("priced static plan");
    assert_same_cost(total.unwrap(), expected);

    // The preview's budget comparison must never be mistaken for the real
    // Fusion admission gate or runtime/provider readiness.
    assert_eq!(panel["estimate_evidence"]["scope"], "preview_only");
    assert_eq!(
        panel["estimate_evidence"]["plan"],
        "shared_static_cost_shape"
    );
    let estimate_reason = panel["estimate_evidence"]["reason"]
        .as_str()
        .expect("preview estimate evidence reason");
    assert!(estimate_reason.contains("does not execute Fusion admission"));
    assert!(estimate_reason.contains("not a cost reservation"));
    assert!(estimate_reason.contains("runtime spend ceiling"));

    // CRITICAL: zero upstream calls — /v1/preview must remain side-effect-free.
    let dispatches = calls.load(Ordering::Relaxed);
    assert_eq!(
        dispatches, 0,
        "INVARIANT: /v1/preview must NEVER dispatch to any provider (zero calls), got {dispatches}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: bogus/unpriced member ⇒ total_estimated_cost_usd: null AND
//         within_budget: false (fail-closed). Still zero dispatch.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_panel_unpriceable_member_fails_closed_with_zero_dispatch() {
    let (app, calls, _state) = app_two_priced_members();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-panel", "synthesize")
        // Provide a cost ceiling so within_budget can be assessed.
        .header("x-tokentrimmer-cost-limit-usd", "999.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hello" }],
                "max_tokens": 100,
                "tt_extras": {
                    "panel": {
                        // "no-such-model-zzz" is not served by any registered mock.
                        "members": ["gpt-4o", "no-such-model-zzz"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "preview with unpriceable member must still return 200 (dry-run, no hard error)"
    );

    let body = body_json(resp).await;
    let panel = &body["panel"];
    assert!(panel.is_object(), "panel object must still be present");

    // Fail-closed: unpriceable member ⇒ total must be null.
    assert!(
        panel["total_estimated_cost_usd"].is_null(),
        "total_estimated_cost_usd must be null when any member is unpriceable (fail-closed), \
         got: {panel}"
    );

    // within_budget must be false when total is null (fail-closed, even with a
    // generous ceiling).
    assert_eq!(
        panel["within_budget"].as_bool(),
        Some(false),
        "within_budget must be false when total is null (fail-closed), got: {panel}"
    );

    // The unpriceable member's individual estimated_cost_usd must also be null.
    let members = panel["members"].as_array().expect("members must be array");
    let unpriceable = members
        .iter()
        .find(|m| m["model"].as_str() == Some("no-such-model-zzz"))
        .expect("unpriceable member must appear in the list");
    assert!(
        unpriceable["estimated_cost_usd"].is_null(),
        "unpriceable member must carry null estimated_cost_usd, got: {unpriceable}"
    );

    // CRITICAL: still zero dispatches.
    let dispatches = calls.load(Ordering::Relaxed);
    assert_eq!(
        dispatches, 0,
        "INVARIANT: /v1/preview must NEVER dispatch even with an unpriceable member (zero calls), got {dispatches}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: a body cap, modern output-cap precedence, and multi-choice member
//         dispatch all use the shared static plan. Still zero dispatch.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_panel_matches_static_plan_for_caps_choices_and_body_budget() {
    let (app, calls, state) = app_two_priced_members();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-panel", "synthesize")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "Plan this request." }],
                "max_tokens": 1,
                "max_completion_tokens": 1000,
                "n": 2,
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o",
                        "max_cost_usd": 999.0
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let panel = &body["panel"];
    let total = panel["total_estimated_cost_usd"]
        .as_f64()
        .expect("priced static plan must have a total");

    let input_tokens = body["current"]["input_tokens_estimated"]
        .as_u64()
        .expect("preview input token estimate") as u32;
    let expected = estimate_panel_cost(
        &state,
        &synthesize_config(Some(999.0)),
        PanelAdmissionEstimate {
            input_tokens,
            max_tokens: Some(1),
            max_completion_tokens: Some(1000),
            n: Some(2),
        },
    )
    .expect("priced modern static plan");
    assert_same_cost(total, expected);

    let legacy_one_choice = estimate_panel_cost(
        &state,
        &synthesize_config(Some(999.0)),
        PanelAdmissionEstimate {
            input_tokens,
            max_tokens: Some(1),
            max_completion_tokens: None,
            n: Some(1),
        },
    )
    .expect("priced legacy static plan");
    assert!(
        total > legacy_one_choice,
        "preview must honor max_completion_tokens precedence and all requested member choices: \
         total={total}, legacy_one_choice={legacy_one_choice}"
    );

    // No cost-limit header was sent. A resolved panel body budget is the
    // admission-compatible fallback for this dry-run comparison.
    assert_eq!(panel["within_budget"], true);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// Test 4: an uncapped request cannot produce a static Fusion total. Still
//         zero dispatch.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_panel_missing_output_cap_fails_closed_with_zero_dispatch() {
    let (app, calls, _state) = app_two_priced_members();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-panel", "synthesize")
        .header("x-tokentrimmer-cost-limit-usd", "999.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hello" }],
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let panel = &body["panel"];

    assert!(
        panel["total_estimated_cost_usd"].is_null(),
        "an uncapped Fusion plan must not pretend to have a bounded total: {panel}"
    );
    assert_eq!(panel["within_budget"], false);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// Test 5: Majority's unrepresented embedding work has no static pricing
//         contract, so preview fails closed rather than pricing the unused
//         LLM arbiter field. Still zero dispatch.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_panel_majority_fails_closed_for_unpriced_embedding_work() {
    let (app, calls, _state) = app_two_priced_members();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-panel", "majority")
        .header("x-tokentrimmer-cost-limit-usd", "999.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hello" }],
                "max_tokens": 100,
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let panel = &body["panel"];

    assert!(
        panel["arbiter"]["estimated_cost_usd"].is_null(),
        "Majority must not price the unused LLM arbiter as an embedding proxy: {panel}"
    );
    assert!(panel["total_estimated_cost_usd"].is_null());
    assert_eq!(panel["within_budget"], false);
    assert!(
        panel["members"]
            .as_array()
            .expect("members array")
            .iter()
            .all(|member| member["estimated_cost_usd"].is_number()),
        "known member work can remain individually visible while total fails closed: {panel}"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// Test 6: no panel header ⇒ response has no `panel` key.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_without_panel_header_has_no_panel_key() {
    let (app, calls, _state) = app_two_priced_members();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        // NO x-tokentrimmer-panel header
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hello" }]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = body_json(resp).await;
    assert!(
        body.get("panel").is_none(),
        "response must NOT contain a `panel` key when no panel header is sent, got: {body}"
    );

    // No dispatch in the dry-run path.
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}
