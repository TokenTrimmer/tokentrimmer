//! S09: live routing must consult latency for the incoming operation, not a
//! different request population. Providers are recording fixtures only.
use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, LatencyOperation, LatencyTracker, Route, RouteAction, RouteConditions,
    RoutingStore, LATENCY_MIN_SAMPLES,
};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

#[derive(Default)]
struct Recorder(Mutex<Vec<ChatCompletionRequest>>);
#[async_trait]
impl Provider for Recorder {
    fn id(&self) -> &'static str {
        "latency-fixture"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["primary", "alternate"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: self.id().into(),
                capabilities: vec![Capability::Text, Capability::Streaming],
                max_input_tokens: 128_000,
                max_output_tokens: 8192,
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
        Ok(serde_json::from_value(json!({"id":"latency-test","object":"chat.completion","created":0,"model":req.model,
            "choices":[{"index":0,"message":{"role":"assistant","content":"fixture"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})).unwrap())
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
        panic!("no embeddings/provider inference in latency fixtures")
    }
}
#[derive(Debug)]
struct Store(Route);
#[async_trait]
impl RoutingStore for Store {
    async fn list_for_org(&self, _: Uuid) -> Result<Vec<Route>, tt_routing::RoutingStoreError> {
        Ok(vec![self.0.clone()])
    }
}
struct Harness {
    app: axum::Router,
    key: String,
    provider: Arc<Recorder>,
    tracker: Arc<LatencyTracker>,
}
impl Harness {
    async fn new() -> Self {
        let keys = Arc::new(InMemoryKeyStore::new());
        let org = Uuid::new_v4();
        let key = issue(
            keys.as_ref(),
            &InMemoryAuditWriter::new(),
            org,
            "latency-fixture",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap()
        .plaintext;
        let provider = Arc::new(Recorder::default());
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let route = Route {
            id: Uuid::new_v4(),
            name: "slow-primary".into(),
            priority: 1,
            enabled: true,
            paused: false,
            when: RouteConditions {
                upstream_latency_ms_p95_gt: Some(1000),
                ..Default::default()
            },
            then: RouteAction {
                target_model: Some("alternate".into()),
                disable_cache: true,
                ..Default::default()
            },
        };
        let state = AppState::new(registry)
            .with_key_store(keys)
            .with_routing_store(Arc::new(CachingRoutingStore::with_ttl(
                Arc::new(Store(route)),
                Duration::ZERO,
            )));
        let tracker = state.latency_tracker.clone();
        Self {
            app: build_router(state),
            key,
            provider,
            tracker,
        }
    }
    fn seed(&self, operation: LatencyOperation, count: usize, ms: u32) {
        for _ in 0..count {
            self.tracker
                .record("latency-fixture", "primary", operation, ms);
        }
    }
    async fn send(&self, stream: bool) -> String {
        let response=self.app.clone().oneshot(Request::builder().method("POST").uri("/v1/chat/completions")
            .header("authorization",format!("Bearer {}",self.key)).header("content-type","application/json")
            .body(Body::from(json!({"model":"primary","stream":stream,"messages":[{"role":"user","content":"fixture"}]}).to_string())).unwrap()).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let calls = self.provider.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].stream, stream);
        calls[0].model.clone()
    }
}

#[tokio::test]
async fn slow_buffered_history_routes_buffered_requests_without_stream_samples() {
    let h = Harness::new().await;
    h.seed(
        LatencyOperation::BufferedCompletion,
        LATENCY_MIN_SAMPLES,
        2000,
    );
    assert_eq!(h.send(false).await, "alternate");
}

#[tokio::test]
async fn fast_buffered_requests_are_not_rerouted_by_slow_stream_establishment() {
    let h = Harness::new().await;
    h.seed(
        LatencyOperation::BufferedCompletion,
        LATENCY_MIN_SAMPLES,
        10,
    );
    h.seed(
        LatencyOperation::StreamEstablishment,
        LATENCY_MIN_SAMPLES,
        2000,
    );
    assert_eq!(h.send(false).await, "primary");
}

#[tokio::test]
async fn cold_and_boundary_buffered_samples_never_borrow_stream_history() {
    for (count, ms) in [
        (0, 2000),
        (LATENCY_MIN_SAMPLES - 1, 2000),
        (LATENCY_MIN_SAMPLES, 1000),
    ] {
        let h = Harness::new().await;
        h.seed(LatencyOperation::BufferedCompletion, count, ms);
        h.seed(
            LatencyOperation::StreamEstablishment,
            LATENCY_MIN_SAMPLES,
            2000,
        );
        assert_eq!(h.send(false).await, "primary", "count={count}, ms={ms}");
    }
}

#[tokio::test]
async fn streaming_keeps_its_own_population_and_does_not_borrow_buffered_history() {
    for (count, ms, expected) in [
        (0, 0, "primary"),
        (LATENCY_MIN_SAMPLES, 10, "primary"),
        (LATENCY_MIN_SAMPLES, 2000, "alternate"),
    ] {
        let h = Harness::new().await;
        h.seed(
            LatencyOperation::BufferedCompletion,
            LATENCY_MIN_SAMPLES,
            2000,
        );
        h.seed(LatencyOperation::StreamEstablishment, count, ms);
        assert_eq!(h.send(true).await, expected);
    }
}

#[tokio::test]
async fn actual_dispatch_records_only_the_matching_operation_and_new_state_is_cold() {
    for stream in [false, true] {
        let h = Harness::new().await;
        assert_eq!(h.send(stream).await, "primary");
        let (own, other) = if stream {
            (
                LatencyOperation::StreamEstablishment,
                LatencyOperation::BufferedCompletion,
            )
        } else {
            (
                LatencyOperation::BufferedCompletion,
                LatencyOperation::StreamEstablishment,
            )
        };
        assert_eq!(h.tracker.sample_count("latency-fixture", "primary", own), 1);
        assert_eq!(
            h.tracker.sample_count("latency-fixture", "primary", other),
            0
        );
        let other_state = Harness::new().await;
        assert_eq!(
            other_state
                .tracker
                .sample_count("latency-fixture", "primary", own),
            0
        );
    }
}
