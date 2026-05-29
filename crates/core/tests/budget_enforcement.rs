//! End-to-end budget enforcement: an authenticated org over its monthly spend
//! cap (or per-minute rate) gets a 429 from the auth middleware, with the
//! budget headers set.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, BudgetLimits, InMemoryBudgetEnforcer, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Provider with non-zero pricing so requests record real spend.
struct PricedProvider;

#[async_trait]
impl Provider for PricedProvider {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "gpt-4o".into(),
            provider: "openai".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            effective_at: chrono::Utc::now(),
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
                prompt_tokens: 100,
                completion_tokens: 100,
                total_tokens: 200,
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
        Err(ProviderError::Unsupported("no".into()))
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "k", Environment::Live, Actor::System)
        .await
        .expect("issue key")
        .plaintext
}

fn chat_request(bearer: &str) -> Request<Body> {
    let body = json!({
        "model": "gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }],
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

fn app_with_budget(limits: BudgetLimits, key_store: Arc<dyn KeyStore>) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(PricedProvider));
    build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_budget(Arc::new(InMemoryBudgetEnforcer::new(limits))),
    )
}

#[tokio::test]
async fn spend_cap_returns_429_after_spend_recorded() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let app = app_with_budget(
        BudgetLimits {
            monthly_cap_usd: Some(0.000_000_1), // any recorded spend exceeds it
            max_requests_per_min: None,
        },
        Arc::new(raw),
    );

    // First request: spend starts at 0 → allowed, records ~$0.0003.
    let r1 = app.clone().oneshot(chat_request(&key)).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK, "first request should pass");

    // Second request: accumulated spend now exceeds the cap → 429.
    let r2 = app.oneshot(chat_request(&key)).await.unwrap();
    assert_eq!(
        r2.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "over-cap request should 429"
    );
    assert!(
        r2.headers().contains_key("x-tt-budget-remaining-usd"),
        "429 should carry the budget-remaining header"
    );
}

#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let app = app_with_budget(
        BudgetLimits {
            monthly_cap_usd: None,
            max_requests_per_min: Some(2),
        },
        Arc::new(raw),
    );

    assert_eq!(
        app.clone()
            .oneshot(chat_request(&key))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(chat_request(&key))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let r3 = app.oneshot(chat_request(&key)).await.unwrap();
    assert_eq!(
        r3.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "third request in the window should 429"
    );
    assert!(
        r3.headers().contains_key("retry-after"),
        "rate-limit 429 should carry Retry-After"
    );
}

#[tokio::test]
async fn unmetered_when_no_budget_configured() {
    // No .with_budget(...) → no enforcement, repeated requests all pass.
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(PricedProvider));
    let app = build_router(AppState::new(registry).with_key_store(Arc::new(raw)));

    for _ in 0..5 {
        assert_eq!(
            app.clone()
                .oneshot(chat_request(&key))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
}
