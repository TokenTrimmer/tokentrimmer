//! Integration test: a routing rule with `fallbacks` makes the chat handler
//! fail over to the next candidate model when the primary provider returns a
//! fallback-eligible error (here, a 503). Proves the wiring from
//! `RouteAction::fallbacks` → `dispatch_with_failover` → served-provider
//! rebind (cost/headers/telemetry reflect the provider that actually served).
//!
//! Mirrors `dogfood_routing.rs`: routes are seeded under `DOGFOOD_ORG_ID` and
//! `with_dogfood_enabled()` lets unauthenticated requests match them, so the
//! test is hermetic (no DB, no network).

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

/// Mock provider that serves one or more model ids, counts calls, and either
/// returns a deterministic 200 response or a fallback-eligible 503.
struct MockProvider {
    id: &'static str,
    /// Models this provider answers for (registry lookup keys).
    models: &'static [&'static str],
    /// `true` → always 503 (fallback-eligible); `false` → 200.
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
                message: "primary is down".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: "chatcmpl-failover".into(),
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

/// Seed a route under `DOGFOOD_ORG_ID` that rewrites `gpt-4o` → `primary-model`
/// with `fallbacks: [fallback-model]`.
fn failover_routing_store() -> Arc<CachingRoutingStore> {
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        DOGFOOD_ORG_ID,
        vec![Route {
            id: Uuid::now_v7(),
            name: "primary-with-fallback".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                input_tokens_lt: Some(200),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "primary-model".into(),
                fallbacks: vec!["fallback-model".into()],
                force_cache_layer: None,
                disable_cache: false,
                max_cost_usd: None,
            },
        }],
    );
    Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>))
}

fn short_request() -> Request<Body> {
    let body = json!({
        "model": "gpt-4o",
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

#[tokio::test]
async fn fails_over_to_fallback_when_primary_returns_503() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));

    let mut registry = ProviderRegistry::new();
    // Primary also answers the inbound `gpt-4o` so the pre-routing resolve
    // succeeds; after the rewrite it serves `primary-model` (and 503s).
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: true,
        calls: Arc::clone(&primary_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "fallback",
        models: &["fallback-model"],
        fails: false,
        calls: Arc::clone(&fallback_calls),
    }));

    let state = AppState::new(registry)
        .with_routing_store(failover_routing_store())
        .with_dogfood_enabled();
    let app = build_router(state);

    let resp = app.oneshot(short_request()).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "fallback should serve a 200");
    assert!(
        primary_calls.load(Ordering::Relaxed) >= 1,
        "primary must have been tried"
    );
    assert_eq!(
        fallback_calls.load(Ordering::Relaxed),
        1,
        "fallback must have served exactly once"
    );
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("fallback-model"),
        "served model header should reflect the fallback"
    );
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("fallback"),
        "provider header should reflect the serving (fallback) provider"
    );
}

#[tokio::test]
async fn uses_primary_and_skips_fallback_when_primary_healthy() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: false,
        calls: Arc::clone(&primary_calls),
    }));
    registry.register(Arc::new(MockProvider {
        id: "fallback",
        models: &["fallback-model"],
        fails: false,
        calls: Arc::clone(&fallback_calls),
    }));

    let state = AppState::new(registry)
        .with_routing_store(failover_routing_store())
        .with_dogfood_enabled();
    let app = build_router(state);

    let resp = app.oneshot(short_request()).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        primary_calls.load(Ordering::Relaxed),
        1,
        "healthy primary should serve once"
    );
    assert_eq!(
        fallback_calls.load(Ordering::Relaxed),
        0,
        "fallback must NOT be called when primary succeeds"
    );
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("primary-model"),
    );
}
