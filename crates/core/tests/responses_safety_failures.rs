//! A05/S01: the `/v1/responses` stateless bridge must inherit the chat
//! pipeline's routing safety invariants. Requests funnel through
//! `chat::handler` with the client's headers, so a privacy policy (redact +
//! disable_cache) must survive route failure, store outage, and pause exactly
//! as it does on `/v1/chat/completions` (route_safety_failures.rs).

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures::stream::BoxStream;
use serde_json::json;
use tower::ServiceExt;
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_cache::{memory::InMemoryL1Cache, CacheError, L1Cache, L1PurgeResult};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, Route, RouteAction, RouteConditions, RoutingStore, RoutingStoreError,
};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

const EMAIL: &str = "jane.doe@example.com";

#[derive(Default)]
struct RecordingProvider(Mutex<Vec<ChatCompletionRequest>>);

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "responses-safety-test"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["source", "text-only"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: self.id().into(),
                capabilities: if id == "source" {
                    vec![Capability::Text, Capability::Vision, Capability::Streaming]
                } else {
                    vec![Capability::Text]
                },
                max_input_tokens: 128_000,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        None
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.0.lock().unwrap().push(req.clone());
        Ok(serde_json::from_value(json!({
            "id": "responses-safety-test", "object": "chat.completion", "created": 0, "model": req.model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        })).unwrap())
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        unreachable!("non-streaming bridge")
    }
    async fn embeddings(
        &self,
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        panic!("embedding dispatch must not run in responses safety tests")
    }
}

#[derive(Debug)]
struct SwitchableStore {
    fail: AtomicBool,
    route: Route,
}
#[async_trait]
impl RoutingStore for SwitchableStore {
    async fn list_for_org(&self, _: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        if self.fail.load(Ordering::SeqCst) {
            Err(RoutingStoreError::Backend(
                "simulated database outage".into(),
            ))
        } else {
            Ok(vec![self.route.clone()])
        }
    }
}

#[derive(Default)]
struct RecordingCache {
    inner: InMemoryL1Cache,
    reads: AtomicUsize,
    writes: AtomicUsize,
}
#[async_trait]
impl L1Cache for RecordingCache {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        if !key.starts_with("revoked:key:") {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.get(key).await
    }
    async fn set(&self, key: &str, value: &[u8], ttl: u64) -> Result<(), CacheError> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.set(key, value, ttl).await
    }
    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        self.inner.delete(key).await
    }
    async fn purge_org(&self, org: Uuid) -> Result<L1PurgeResult, CacheError> {
        self.inner.purge_org(org).await
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    provider: Arc<RecordingProvider>,
    cache: Arc<RecordingCache>,
    backing: Arc<SwitchableStore>,
    routing: Arc<CachingRoutingStore>,
    org: Uuid,
}
impl Harness {
    async fn new(paused: bool) -> Self {
        let org = Uuid::now_v7();
        let keys = Arc::new(InMemoryKeyStore::new());
        let key = issue(
            keys.as_ref(),
            &InMemoryAuditWriter::new(),
            org,
            "safety",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap()
        .plaintext;
        let provider = Arc::new(RecordingProvider::default());
        let cache = Arc::new(RecordingCache::default());
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let backing = Arc::new(SwitchableStore {
            fail: AtomicBool::new(false),
            route: Route {
                id: Uuid::now_v7(),
                name: "protected".into(),
                priority: 1,
                enabled: true,
                paused,
                when: RouteConditions::default(),
                then: RouteAction {
                    target_model: Some("text-only".into()),
                    redact: true,
                    disable_cache: true,
                    fallbacks: vec!["text-only".into()],
                    traffic_pct: Some(100),
                    ..Default::default()
                },
            },
        });
        let routing = Arc::new(CachingRoutingStore::with_ttl(
            backing.clone(),
            Duration::ZERO,
        ));
        let app = build_router(
            AppState::new(registry)
                .with_key_store(keys)
                .with_routing_store(routing.clone())
                .with_l1(cache.clone(), None),
        );
        Self {
            app,
            key,
            provider,
            cache,
            backing,
            routing,
            org,
        }
    }
    /// POST /v1/responses with the Responses-API input shape.
    fn request(&self, forced: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            // Route privacy must defeat even an explicit client cache opt-in.
            .header("x-tokentrimmer-cache", "force-write")
            .header("authorization", format!("Bearer {}", self.key));
        if let Some(name) = forced {
            builder = builder.header("x-tokentrimmer-route", name);
        }
        builder
            .body(Body::from(
                json!({
                    "model": "source",
                    "input": format!("My email is {EMAIL}; describe the image."),
                })
                .to_string(),
            ))
            .unwrap()
    }
}

#[tokio::test]
async fn responses_matched_route_preserves_privacy_in_dispatched_request() {
    let h = Harness::new(false).await;
    for _ in 0..2 {
        let response = h.app.clone().oneshot(h.request(None)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-tokentrimmer-route-matched"],
            "protected"
        );
        let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
    }
    // No cache insertion / lookup leaks through the bridge on repeat calls.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.cache.reads.load(Ordering::SeqCst), 0, "prohibited lookup");
    assert_eq!(
        h.cache.writes.load(Ordering::SeqCst),
        0,
        "prohibited insertion"
    );
    let calls = h.provider.0.lock().unwrap();
    assert_eq!(calls.len(), 2, "no shadow call or cache hit");
    for req in calls.iter() {
        assert_eq!(req.model, "text-only", "routed to privacy target");
        let wire = serde_json::to_string(req).unwrap();
        assert!(!wire.contains(EMAIL), "PII escaped: {wire}");
        assert!(wire.contains("[REDACTED]"));
    }
}

#[tokio::test]
async fn responses_store_outage_refuses_before_provider_io() {
    let h = Harness::new(false).await;
    // Warm the routing cache, then fail the store so refresh must go remote.
    assert_eq!(h.routing.engine_for(h.org).await.unwrap().routes().len(), 1);
    h.backing.fail.store(true, Ordering::SeqCst);
    for forced in [None, Some("protected")] {
        let response = h.app.clone().oneshot(h.request(forced)).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "service_unavailable");
        assert!(
            !body.to_string().contains("simulated database"),
            "no backend details"
        );
        assert!(h.provider.0.lock().unwrap().is_empty());
    }
    // After the second request, cache I/O must also be prohibited.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.cache.reads.load(Ordering::SeqCst), 0);
    assert_eq!(h.cache.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn responses_cold_outage_refuses_before_provider_io() {
    let h = Harness::new(false).await;
    // No warm-up: the first lookup itself hits the failed store.
    h.backing.fail.store(true, Ordering::SeqCst);
    let response = h.app.clone().oneshot(h.request(None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.cache.reads.load(Ordering::SeqCst), 0);
    assert_eq!(h.cache.writes.load(Ordering::SeqCst), 0);
    assert!(h.provider.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn responses_paused_route_still_redacts_and_never_caches() {
    let h = Harness::new(true).await;
    for forced in [None, Some("protected")] {
        let response = h.app.clone().oneshot(h.request(forced)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.cache.reads.load(Ordering::SeqCst), 0);
    assert_eq!(h.cache.writes.load(Ordering::SeqCst), 0);
    let calls = h.provider.0.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for req in calls.iter() {
        assert_eq!(req.model, "source");
        let wire = serde_json::to_string(&calls[0]).unwrap();
        assert!(!wire.contains(EMAIL));
        assert!(wire.contains("[REDACTED]"));
    }
}
