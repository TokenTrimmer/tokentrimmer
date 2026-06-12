//! V3d-2a: cost-based route conditions. An expensive request (estimated cost
//! over the threshold) reroutes to the cheaper model; a cheap one passes
//! through. Cost is estimated pre-flight from the requested model's pricing and
//! the request's `max_tokens`.

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

/// Provider serving gpt-4o (expensive) + gpt-4o-mini (cheap), model-aware
/// pricing so cost estimates differ; records the served model.
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
                max_input_tokens: 4096,
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

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org_id, "k", Environment::Live, Actor::System)
        .await
        .expect("issue tt_live_ key")
        .plaintext
}

/// Chat request with an explicit `max_tokens` (drives the output cost estimate).
fn chat_req(model: &str, bearer: &str, max_tokens: u32) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": max_tokens,
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
async fn expensive_request_reroutes_cheap_one_passes_through() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served: Arc::clone(&served),
    }));

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    // Route: when estimated cost > $0.02, downgrade gpt-4o → gpt-4o-mini.
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "cost-downgrade".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                estimated_cost_gt: Some(0.02),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );

    // Expensive: gpt-4o, max_tokens=2000 → est ≈ (small + 2000×15)/1e6 ≈ $0.030 > 0.02 → reroute.
    let r1 = app
        .clone()
        .oneshot(chat_req("gpt-4o", &key, 2000))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(
        r1.headers()["x-tokentrimmer-model-used"].to_str().unwrap(),
        "gpt-4o-mini",
        "expensive request should downgrade"
    );

    // Cheap: gpt-4o, max_tokens=100 → est ≈ (small + 100×15)/1e6 ≈ $0.0015 < 0.02 → pass through.
    let r2 = app.oneshot(chat_req("gpt-4o", &key, 100)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()["x-tokentrimmer-model-used"].to_str().unwrap(),
        "gpt-4o",
        "cheap request should pass through unrouted"
    );
}

#[tokio::test]
async fn reroute_then_block_on_ceiling() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served: Arc::clone(&served),
    }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    // Downgrade expensive gpt-4o → gpt-4o-mini, with a tight $0.0008 ceiling on
    // the rerouted cost.
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "downgrade-and-cap".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                estimated_cost_gt: Some(0.005),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: Some(0.0008),
                flex: false,
                batch: false,
                compress: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );

    // max_tokens=500 → gpt-4o est ≈ $0.0075 (>0.005 → reroute); mini est ≈ $0.0003 (<0.0008 → served).
    let ok = app
        .clone()
        .oneshot(chat_req("gpt-4o", &key, 500))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        ok.headers()["x-tokentrimmer-model-used"].to_str().unwrap(),
        "gpt-4o-mini"
    );

    // max_tokens=2000 → gpt-4o est ≈ $0.030 (reroute); mini est ≈ $0.0012 (>0.0008 → 402).
    let blocked = app.oneshot(chat_req("gpt-4o", &key, 2000)).await.unwrap();
    assert_eq!(blocked.status(), StatusCode::PAYMENT_REQUIRED);
    // The over-budget request was never dispatched (only the served one ran).
    assert_eq!(
        served.lock().unwrap().clone(),
        vec!["gpt-4o-mini".to_string()]
    );
}
