//! End-to-end tests for the user-facing /v1/routes CRUD.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{CachingRoutingStore, InMemoryRoutingStore, RoutingStore};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

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
        let (i, o) = if model == "gpt-4o" { (5.0, 15.0) } else { (0.15, 0.6) };
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
        _c: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.served.lock().unwrap().push(req.model.clone());
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
        _r: ChatCompletionRequest,
        _c: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _r: EmbeddingsRequest,
        _c: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .unwrap()
        .plaintext
}

async fn app_with_key() -> (axum::Router, String, Arc<Mutex<Vec<String>>>) {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served: Arc::clone(&served),
    }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let routing = Arc::new(CachingRoutingStore::new(
        Arc::new(InMemoryRoutingStore::new()) as Arc<dyn RoutingStore>,
    ));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );
    (app, key, served)
}

fn req(method: &str, uri: &str, key: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(body.map(|v| Body::from(v.to_string())).unwrap_or(Body::empty()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn create_list_get_delete_round_trip() {
    let (app, key, _served) = app_with_key().await;
    let spec = json!({ "name": "downgrade", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let created = body_json(r).await;
    let id = created["id"].as_str().unwrap().to_string();

    let r = app
        .clone()
        .oneshot(req("GET", "/v1/routes", Some(&key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await.as_array().unwrap().len(), 1);

    let r = app
        .clone()
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let r = app
        .clone()
        .oneshot(req("DELETE", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app
        .oneshot(req("GET", &format!("/v1/routes/{id}"), Some(&key), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unauthenticated_is_rejected() {
    let (app, _key, _) = app_with_key().await;
    let r = app
        .oneshot(req("GET", "/v1/routes", None, None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cross_provider_target_rejected() {
    let (app, key, _) = app_with_key().await;
    let spec = json!({ "name": "x", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"claude-haiku-4-5"} });
    let r = app
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn has_images_non_vision_target_rejected() {
    let (app, key, _) = app_with_key().await;
    // gpt-4o-mini in this test registry is Text-only → must reject.
    let spec = json!({ "name": "x", "when": {"has_images": true}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn created_route_applies_immediately_without_ttl_wait() {
    let (app, key, served) = app_with_key().await;
    let spec = json!({ "name": "downgrade", "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} });
    let r = app
        .clone()
        .oneshot(req("POST", "/v1/routes", Some(&key), Some(spec)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let chat = json!({ "model": "gpt-4o", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let r = app
        .oneshot(req("POST", "/v1/chat/completions", Some(&key), Some(chat)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Cache was invalidated on create → the brand-new route applied on the very next request.
    assert_eq!(served.lock().unwrap().clone(), vec!["gpt-4o-mini".to_string()]);
}
