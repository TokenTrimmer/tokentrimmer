//! S02/S01: explicit output caps must survive routing, and known-incompatible
//! targets must not receive provider I/O. Hermetic authenticated HTTP tests;
//! recording adapters never contact a provider or a database.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tower::ServiceExt;
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{CachingRoutingStore, Route, RouteAction, RoutingStore};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

const EMAIL: &str = "output-limit-fixture@example.test";
#[derive(Default)]
struct RecordingProvider(Mutex<Vec<ChatCompletionRequest>>);
#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "output-limits"
    }
    fn models(&self) -> Vec<ModelInfo> {
        [
            ("caller", 8192),
            ("small", 128),
            ("large", 8192),
            ("down", 8192),
        ]
        .into_iter()
        .map(|(id, max_output_tokens)| ModelInfo {
            id: id.into(),
            provider: self.id().into(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 128_000,
            max_output_tokens,
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
        if req.model == "down" {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "fixture outage".into(),
            });
        }
        Ok(serde_json::from_value(json!({
            "id":"output-test", "object":"chat.completion", "created":0, "model":req.model,
            "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}
        })).unwrap())
    }
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.0.lock().unwrap().push(req.clone());
        if req.model == "down" {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "fixture outage".into(),
            });
        }
        Ok(futures::stream::empty().boxed())
    }
    async fn embeddings(
        &self,
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        panic!("no embedding calls in output-limit fixtures")
    }
}
#[derive(Debug)]
struct FixedStore(Route);
#[async_trait]
impl RoutingStore for FixedStore {
    async fn list_for_org(&self, _: Uuid) -> Result<Vec<Route>, tt_routing::RoutingStoreError> {
        Ok(vec![self.0.clone()])
    }
}
struct Harness {
    app: axum::Router,
    key: String,
    provider: Arc<RecordingProvider>,
}
impl Harness {
    async fn new(target: &str, fallbacks: &[&str]) -> Self {
        let org = Uuid::new_v4();
        let keys = Arc::new(InMemoryKeyStore::new());
        let key = issue(
            keys.as_ref(),
            &InMemoryAuditWriter::new(),
            org,
            "output-test",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap()
        .plaintext;
        let provider = Arc::new(RecordingProvider::default());
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let routing = Arc::new(CachingRoutingStore::with_ttl(
            Arc::new(FixedStore(Route {
                id: Uuid::new_v4(),
                name: "bounded".into(),
                priority: 1,
                enabled: true,
                paused: false,
                when: Default::default(),
                then: RouteAction {
                    target_model: Some(target.into()),
                    fallbacks: fallbacks.iter().map(|s| s.to_string()).collect(),
                    redact: true,
                    disable_cache: true,
                    ..Default::default()
                },
            })),
            Duration::ZERO,
        ));
        let app = build_router(
            AppState::new(registry)
                .with_key_store(keys)
                .with_routing_store(routing)
                .with_l1(Arc::new(InMemoryL1Cache::new()), None),
        );
        Self { app, key, provider }
    }
    async fn send(&self, limits: Value, stream: bool, forced: bool) -> (StatusCode, String) {
        let mut body = json!({"model":"caller", "messages":[{"role":"user","content":format!("Contact {EMAIL}")}], "stream":stream});
        body.as_object_mut()
            .unwrap()
            .extend(limits.as_object().unwrap().clone());
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", format!("Bearer {}", self.key))
            .header("content-type", "application/json")
            .header("x-tokentrimmer-cache", "force-write");
        if forced {
            builder = builder.header("x-tokentrimmer-route", "bounded");
        }
        let response = self
            .app
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let code = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        (code, String::from_utf8_lossy(&bytes).into_owned())
    }
    fn calls(&self) -> Vec<ChatCompletionRequest> {
        self.provider.0.lock().unwrap().clone()
    }
}
fn assert_guarded(req: &ChatCompletionRequest, expected: u32) {
    assert_eq!(
        req.max_completion_tokens.or(req.max_tokens),
        Some(expected),
        "must not silently clamp the caller cap"
    );
    assert!(
        !serde_json::to_string(&req.messages)
            .unwrap()
            .contains(EMAIL),
        "privacy must survive capability suppression/fallback"
    );
}

#[tokio::test]
async fn oversized_route_target_is_suppressed_without_losing_privacy_or_cache_prohibition() {
    for stream in [false, true] {
        for limit in [
            json!({"max_tokens":512}),
            json!({"max_completion_tokens":512}),
        ] {
            let h = Harness::new("small", &[]).await;
            for _ in 0..2 {
                assert_eq!(h.send(limit.clone(), stream, false).await.0, StatusCode::OK);
            }
            let calls = h.calls();
            assert_eq!(
                calls.len(),
                2,
                "prohibited cache insertion cannot turn the repeat into a hit"
            );
            for call in calls {
                assert_eq!(call.model, "caller");
                assert_guarded(&call, 512);
            }
        }
    }
}

#[tokio::test]
async fn forced_oversized_target_refuses_before_provider_io() {
    for stream in [false, true] {
        for limits in [
            json!({"max_tokens":129}),
            json!({"max_tokens":64,"max_completion_tokens":129}),
        ] {
            let h = Harness::new("small", &[]).await;
            let (code, body) = h.send(limits, stream, true).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
            assert!(body.contains("output_limit_too_large"));
            assert!(h.calls().is_empty());
        }
    }
}

#[tokio::test]
async fn exact_boundary_and_completion_token_precedence_keep_eligible_routes() {
    for stream in [false, true] {
        for limits in [
            json!({"max_tokens":128}),
            json!({"max_completion_tokens":128}),
            json!({"max_tokens":8192,"max_completion_tokens":128}),
        ] {
            let h = Harness::new("small", &[]).await;
            assert_eq!(h.send(limits, stream, false).await.0, StatusCode::OK);
            let calls = h.calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].model, "small");
            assert_guarded(&calls[0], 128);
        }
    }
}

#[tokio::test]
async fn buffered_and_streaming_failover_skip_incompatible_output_limits() {
    for stream in [false, true] {
        for limits in [
            json!({"max_tokens":512}),
            json!({"max_tokens":64,"max_completion_tokens":512}),
        ] {
            let h = Harness::new("down", &["small", "large"]).await;
            let (code, body) = h.send(limits, stream, false).await;
            assert_eq!(code, StatusCode::OK, "{body}");
            let calls = h.calls();
            assert!(calls.iter().any(|r| r.model == "down"));
            assert!(!calls.iter().any(|r| r.model == "small"));
            assert_eq!(calls.last().unwrap().model, "large");
            for call in calls {
                assert_guarded(&call, 512);
            }
        }
    }
}

#[tokio::test]
async fn exhausting_eligible_candidates_never_dispatches_to_the_small_fallback() {
    for stream in [false, true] {
        let h = Harness::new("down", &["small"]).await;
        assert!(!h
            .send(json!({"max_completion_tokens":512}), stream, false)
            .await
            .0
            .is_success());
        let calls = h.calls();
        assert!(!calls.is_empty());
        for call in calls {
            assert_eq!(call.model, "down");
            assert_guarded(&call, 512);
        }
    }
}
