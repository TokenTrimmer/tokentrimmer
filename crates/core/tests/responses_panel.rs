//! Phase 6 / Task 1 — `/v1/responses` tt_extras passthrough.
//!
//! Asserts that a `/v1/responses` request whose body carries a top-level
//! `tt_extras` object is accepted (not rejected with "unsupported /v1/responses
//! field: tt_extras"). Full panel-render assertions land in Task 2.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

// ---------------------------------------------------------------------------
// Minimal mock provider
// ---------------------------------------------------------------------------

struct SimpleMock {
    id: &'static str,
    model: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for SimpleMock {
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
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
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
// App builder
// ---------------------------------------------------------------------------

fn build_app() -> (axum::Router, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(SimpleMock {
        id: "openai",
        model: "gpt-4o",
        calls: Arc::clone(&calls),
    }));
    // Second model on a distinct provider id so panel can use 2 members.
    registry.register(Arc::new(SimpleMock {
        id: "openai-mini",
        model: "gpt-4o-mini",
        calls: Arc::clone(&calls),
    }));
    let state = AppState::new(registry).with_panel_enabled(true);
    (build_router(state), calls)
}

// ============================================================================
// Task 1 — tt_extras passthrough: a /v1/responses body with a top-level
// tt_extras object must be accepted (200), NOT rejected with
// "unsupported /v1/responses field for stateless bridge: tt_extras" (400).
// ============================================================================

#[tokio::test]
async fn responses_tt_extras_passthrough_returns_200_not_400() {
    let (app, _calls) = build_app();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        // Generous ceiling so the panel budget gate does not fire.
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "input": [{"role": "user", "content": "hello"}],
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

    // Read the body for a useful diagnostic on failure.
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body_str = std::str::from_utf8(&bytes).unwrap_or("<non-utf8>");

    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "tt_extras must not be rejected as an unsupported field; body: {body_str}"
    );
    assert_eq!(
        status,
        StatusCode::OK,
        "/v1/responses with tt_extras must return 200; body: {body_str}"
    );
}

// ============================================================================
// Regression: a /v1/responses request WITHOUT tt_extras still works (off-by-
// default: no behaviour change when the key is absent).
// ============================================================================

#[tokio::test]
async fn responses_without_tt_extras_still_returns_200() {
    let (app, _calls) = build_app();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "input": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body_str = std::str::from_utf8(&bytes).unwrap_or("<non-utf8>");

    assert_eq!(
        status,
        StatusCode::OK,
        "/v1/responses without tt_extras must still return 200; body: {body_str}"
    );
}
