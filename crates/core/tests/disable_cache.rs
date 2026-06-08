//! A matched `disable_cache` route makes the request skip L1 entirely:
//! two identical requests both hit the provider; the second is not an L1 hit.

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
use tt_cache::memory::InMemoryL1Cache;
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

struct CountingProvider {
    calls: Arc<AtomicUsize>,
    served: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for CountingProvider {
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
        let (i, o) = if model == "gpt-4o" {
            (5.0, 15.0)
        } else {
            (0.15, 0.6)
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
        _c: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
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

fn sensitive_request(model: &str, bearer: &str) -> Request<Body> {
    let body =
        json!({ "model": model, "messages": [{"role":"user","content":"hi"}], "stream": false });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-tokentrimmer-tag", "sensitive")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn setup(disable_cache: bool) -> (axum::Router, String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingProvider {
        calls: Arc::clone(&calls),
        served,
    }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "privacy".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                tag_equals: Some("sensitive".into()),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: vec![],
                disable_cache,
                max_cost_usd: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing)
            .with_l1(Arc::new(InMemoryL1Cache::new()), None),
    );
    (app, key, calls)
}

#[tokio::test]
async fn disable_cache_route_bypasses_l1() {
    let (app, key, calls) = setup(true).await;
    let r1 = app
        .clone()
        .oneshot(sensitive_request("gpt-4o", &key))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r2 = app
        .oneshot(sensitive_request("gpt-4o", &key))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_ne!(
        r2.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "disable_cache route must not serve from L1"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "provider must be called for both requests"
    );
}

#[tokio::test]
async fn control_route_without_disable_cache_hits_l1() {
    let (app, key, calls) = setup(false).await;
    let r1 = app
        .clone()
        .oneshot(sensitive_request("gpt-4o", &key))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r2 = app
        .oneshot(sensitive_request("gpt-4o", &key))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        r2.headers()
            .get("x-tokentrimmer-cache")
            .and_then(|v| v.to_str().ok()),
        Some("hit-l1"),
        "without disable_cache the second identical request is an L1 hit"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider called once; second served from L1"
    );
}
