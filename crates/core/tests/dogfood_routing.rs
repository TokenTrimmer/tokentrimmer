//! Integration test: dogfood routing mode wires an in-memory routing store so
//! short flagship model prompts (no auth header) get rewritten to
//! `llama-3.1-8b-instant` when `dogfood_enabled` is set on the AppState.
//!
//! Mirrors what `TT_DOGFOOD_GROQ_ROUTING=1` does in production startup
//! (`crates/cli/src/main.rs`), but without the env-var check — we build the
//! state directly so the test is hermetic and reproducible.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
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

/// A provider that records every model it was asked to serve. Registered for
/// both the flagship model names (request arrives as) AND the Groq target
/// (what the route rewrites to) so the single provider handles the full flow.
struct RecordingProvider {
    served_models: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    /// Models this provider claims to support (registry lookup key).
    model_ids: Vec<&'static str>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn models(&self) -> Vec<ModelInfo> {
        self.model_ids
            .iter()
            .map(|id| ModelInfo {
                id: id.to_string(),
                provider: "recording".into(),
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
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.served_models.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "chatcmpl-dogfood".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("groq says hi".into())),
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
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

/// Build a short (< 200 estimated tokens) chat request with no auth header.
fn unauthenticated_request(model: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Seed the dogfood route (mirrors cli/src/main.rs) into an InMemoryRoutingStore.
fn dogfood_routing_store() -> Arc<CachingRoutingStore> {
    let route_id = Uuid::now_v7();
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        DOGFOOD_ORG_ID,
        vec![Route {
            id: route_id,
            name: "dogfood-short-prompts-to-groq".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec![
                    "claude-sonnet-4-6".into(),
                    "claude-opus-4-7".into(),
                    "gpt-4o".into(),
                    "gpt-4-turbo".into(),
                ],
                input_tokens_lt: Some(200),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "llama-3.1-8b-instant".into(),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
            },
        }],
    );
    Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>))
}

#[tokio::test]
async fn dogfood_routing_rewrites_flagship_model_to_groq() {
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));

    // Register a provider that answers for both the inbound flagship model
    // AND the Groq rewrite target.
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served_models: Arc::clone(&served),
        calls: Arc::clone(&calls),
        model_ids: vec!["claude-sonnet-4-6", "llama-3.1-8b-instant"],
    }));

    let state = AppState::new(registry)
        .with_routing_store(dogfood_routing_store())
        .with_dogfood_enabled();

    let app = build_router(state);

    // Fire a request for a flagship model with NO auth header.
    let resp = app
        .oneshot(unauthenticated_request("claude-sonnet-4-6"))
        .await
        .unwrap();

    // Response must be 200.
    assert_eq!(resp.status(), StatusCode::OK, "expected 200");

    // Provider was called exactly once.
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // Provider received the REWRITTEN model, not the original.
    let saw = served.lock().unwrap().clone();
    assert_eq!(
        saw,
        vec!["llama-3.1-8b-instant".to_string()],
        "provider should have been called with the Groq rewrite target"
    );

    // Response header confirms the served model.
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("llama-3.1-8b-instant"),
        "x-tokentrimmer-model-used header should reflect rewritten model"
    );
}

#[tokio::test]
async fn dogfood_routing_does_not_fire_without_dogfood_flag() {
    // Same routing store seeded with the dogfood route, but dogfood_enabled
    // is NOT set on the state — the nil org guard fires, no rewrite happens.
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served_models: Arc::clone(&served),
        calls: Arc::clone(&calls),
        model_ids: vec!["claude-sonnet-4-6", "llama-3.1-8b-instant"],
    }));

    // No .with_dogfood_enabled() — auth middleware will NOT inject DOGFOOD_ORG_ID.
    let state = AppState::new(registry).with_routing_store(dogfood_routing_store());

    let app = build_router(state);

    let resp = app
        .oneshot(unauthenticated_request("claude-sonnet-4-6"))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    // Without dogfood flag the org stays nil; routing skips; original model served.
    let saw = served.lock().unwrap().clone();
    assert_eq!(
        saw,
        vec!["claude-sonnet-4-6".to_string()],
        "route must not fire when dogfood_enabled is false"
    );
}

#[tokio::test]
async fn dogfood_routing_does_not_fire_for_long_prompts() {
    // A prompt with > 200 estimated tokens must NOT be rewritten by the
    // `input_tokens_lt: Some(200)` condition.
    // The token estimate is len(last_user_text) / 4, so we need > 800 chars.
    let long_content = "a".repeat(900);

    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served_models: Arc::clone(&served),
        calls: Arc::clone(&calls),
        model_ids: vec!["claude-sonnet-4-6", "llama-3.1-8b-instant"],
    }));

    let state = AppState::new(registry)
        .with_routing_store(dogfood_routing_store())
        .with_dogfood_enabled();

    let app = build_router(state);

    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{ "role": "user", "content": long_content }],
        "stream": false,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let saw = served.lock().unwrap().clone();
    assert_eq!(
        saw,
        vec!["claude-sonnet-4-6".to_string()],
        "long prompt (> 200 est. tokens) must not be rewritten"
    );
}
