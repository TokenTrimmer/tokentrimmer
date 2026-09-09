//! S01: failed routing must not turn a privacy policy into unprotected egress.
//! Hermetic HTTP tests: real preparation/cache gates, recording provider and
//! cache, and a store whose refresh can fail after a retained snapshot expires.

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
use futures::stream::{BoxStream, StreamExt};
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
        "safety-test"
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
            "id": "safety-test", "object": "chat.completion", "created": 0, "model": req.model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        })).unwrap())
    }
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.0.lock().unwrap().push(req);
        Ok(futures::stream::empty().boxed())
    }
    async fn embeddings(
        &self,
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        panic!("embedding dispatch must not run in chat safety tests")
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
        // L1 also backs cross-replica key revocation. Policy cache denial
        // forbids request/response caching, not these value-free auth checks.
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
        Self::with_privacy(paused, true).await
    }

    async fn with_privacy(paused: bool, privacy: bool) -> Self {
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
                    redact: privacy,
                    disable_cache: privacy,
                    // These effects must NOT survive a rejected target or pause.
                    shadow_model: Some("text-only".into()),
                    fallbacks: vec!["text-only".into()],
                    compress: true,
                    traffic_pct: Some(100),
                    ..Default::default()
                },
            },
        });
        // Zero TTL gives deterministic expired-snapshot coverage without sleeps.
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
    fn request(&self, stream: bool, forced: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            // Route privacy must defeat even an explicit client cache opt-in.
            .header("x-tokentrimmer-cache", "force-write")
            .header("authorization", format!("Bearer {}", self.key));
        if let Some(name) = forced {
            builder = builder.header("x-tokentrimmer-route", name);
        }
        builder.body(Body::from(json!({
            "model": "source", "stream": stream,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": format!("My email is {EMAIL}; describe the image.")},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
            ]}]
        }).to_string())).unwrap()
    }
    async fn assert_no_cache_io(&self) {
        // Cache insertions are spawned; let an incorrectly armed write run.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            self.cache.reads.load(Ordering::SeqCst),
            0,
            "prohibited cache lookup"
        );
        assert_eq!(
            self.cache.writes.load(Ordering::SeqCst),
            0,
            "prohibited cache insertion"
        );
    }
}

#[tokio::test]
async fn unprotected_control_performs_cache_io() {
    let h = Harness::with_privacy(true, false).await;
    let response = h.app.clone().oneshot(h.request(false, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for _ in 0..50 {
        if h.cache.writes.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(h.cache.reads.load(Ordering::SeqCst) > 0);
    assert!(h.cache.writes.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn capability_suppression_preserves_privacy_in_buffered_and_streaming_requests() {
    for stream in [false, true] {
        let h = Harness::new(false).await;
        for _ in 0..2 {
            let response = h
                .app
                .clone()
                .oneshot(h.request(stream, None))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()["x-tokentrimmer-route-matched"],
                "protected"
            );
            let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
        }
        h.assert_no_cache_io().await;
        let calls = h.provider.0.lock().unwrap();
        assert_eq!(
            calls.len(),
            2,
            "no shadow call or cache hit after suppression"
        );
        for req in calls.iter() {
            assert_eq!(req.model, "source");
            let wire = serde_json::to_string(req).unwrap();
            assert!(!wire.contains(EMAIL), "PII escaped to provider: {wire}");
            assert!(wire.contains("[REDACTED]"));
        }
    }
}

#[tokio::test]
async fn expired_snapshot_with_store_outage_refuses_normal_and_forced_requests() {
    for stream in [false, true] {
        for forced in [None, Some("protected")] {
            let h = Harness::new(false).await;
            assert_eq!(h.routing.engine_for(h.org).await.unwrap().routes().len(), 1);
            h.backing.fail.store(true, Ordering::SeqCst);
            let response = h
                .app
                .clone()
                .oneshot(h.request(stream, forced))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "service_unavailable");
            assert!(
                !body.to_string().contains("simulated database"),
                "no backend details in client error"
            );
            h.assert_no_cache_io().await;
            assert!(
                h.provider.0.lock().unwrap().is_empty(),
                "outage caused provider I/O"
            );
        }
    }
}

#[tokio::test]
async fn cold_store_outage_refuses_before_provider_io() {
    let h = Harness::new(false).await;
    h.backing.fail.store(true, Ordering::SeqCst);
    let response = h.app.clone().oneshot(h.request(false, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    h.assert_no_cache_io().await;
    assert!(h.provider.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_or_incompatible_forced_route_refuses_before_provider_io() {
    for name in ["missing", "protected"] {
        let h = Harness::new(false).await;
        let response = h
            .app
            .clone()
            .oneshot(h.request(false, Some(name)))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        h.assert_no_cache_io().await;
        assert!(h.provider.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn pause_preserves_privacy_even_when_forced_or_streaming() {
    for stream in [false, true] {
        for forced in [None, Some("protected")] {
            let h = Harness::new(true).await;
            let response = h
                .app
                .clone()
                .oneshot(h.request(stream, forced))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            h.assert_no_cache_io().await;
            let calls = h.provider.0.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].model, "source");
            let wire = serde_json::to_string(&calls[0]).unwrap();
            assert!(!wire.contains(EMAIL));
            assert!(wire.contains("[REDACTED]"));
        }
    }
}
