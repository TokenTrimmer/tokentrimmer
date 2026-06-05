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
            ModelInfo {
                id: "vision-pro".into(),
                provider: "vision-mock".into(),
                capabilities: vision.clone(),
                max_input_tokens: 128_000,
                max_output_tokens: 4096,
            },
            ModelInfo {
                id: "vision-mini".into(),
                provider: "vision-mock".into(),
                capabilities: vision,
                max_input_tokens: 128_000,
                max_output_tokens: 4096,
            },
            ModelInfo {
                id: "text-only".into(),
                provider: "vision-mock".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 4096,
            },
        ]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (i, o) = match model {
            "vision-pro" => (5.0, 15.0),
            "vision-mini" => (0.15, 0.6),
            _ => (1.0, 2.0),
        };
        Some(ModelPricing {
            input_per_million: i,
            output_per_million: o,
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
        self.served_models.lock().unwrap().push(req.model.clone());
        Ok(ChatCompletionResponse {
            id: "chatcmpl-vis".into(),
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
        Err(ProviderError::Unsupported("no".into()))
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(
        store,
        &audit,
        org_id,
        "test-key",
        Environment::Live,
        Actor::System,
    )
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
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
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
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
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
    routes_backing.set_routes(
        org_id,
        vec![Route {
            id: Uuid::now_v7(),
            name: "image-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["vision-pro".into()],
                has_images: Some(true),
                ..Default::default()
            },
            then: RouteAction {
                target_model: target_model.into(),
                fallbacks: Vec::new(),
                force_cache_layer: None,
                disable_cache: false,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );
    (served, plaintext, app)
}

#[tokio::test]
async fn image_request_routed_to_vision_target() {
    let (served, key, app) = setup("vision-mini").await;
    let resp = app
        .oneshot(image_request("vision-pro", &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        served.lock().unwrap().clone(),
        vec!["vision-mini".to_string()]
    );
}

#[tokio::test]
async fn text_only_request_does_not_match_has_images_route() {
    let (served, key, app) = setup("vision-mini").await;
    let resp = app.oneshot(text_request("vision-pro", &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        served.lock().unwrap().clone(),
        vec!["vision-pro".to_string()]
    );
}

#[tokio::test]
async fn image_route_skipped_when_target_not_vision_capable() {
    // Capability guard: an image request requires vision; a text-only target is
    // skipped and the original model is dispatched.
    let (served, key, app) = setup("text-only").await;
    let resp = app
        .oneshot(image_request("vision-pro", &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        served.lock().unwrap().clone(),
        vec!["vision-pro".to_string()]
    );
}
