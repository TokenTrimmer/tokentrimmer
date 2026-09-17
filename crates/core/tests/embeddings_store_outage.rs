//! A05/S01: `/v1/embeddings` shares `apply_routing` with the chat pipeline
//! via a synthetic request adapter. These tests prove the store-outage
//! fail-closed invariant and the privacy-redaction invariant hold at the
//! embeddings entry point specifically — including batch inputs.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, Route, RouteAction, RouteConditions, RoutingStore, RoutingStoreError,
};
use tt_shared::{
    messages::{EmbeddingData, EmbeddingsRequest, EmbeddingsResponse},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Recording embedder: captures the raw inputs it is dispatched with so the
/// tests can assert both "no provider I/O happened" (outage) and exactly
/// which bytes the provider saw (redaction).
#[derive(Default)]
struct RecordingEmbedder {
    calls: Mutex<Vec<EmbeddingsRequest>>,
}
#[async_trait]
impl Provider for RecordingEmbedder {
    fn id(&self) -> &'static str {
        "embedding-outage-test"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "text-embedding-3-large".into(),
            provider: self.id().into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 0,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        None
    }
    async fn chat_completion(
        &self,
        _: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("chat unused".into()))
    }
    async fn chat_completion_stream(
        &self,
        _: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        unreachable!("streaming unused")
    }
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        self.calls.lock().unwrap().push(req.clone());
        let count = match &req.input {
            tt_shared::messages::EmbeddingInput::Single(_) => 1,
            tt_shared::messages::EmbeddingInput::Batch(texts) => texts.len(),
        } as u32;
        Ok(EmbeddingsResponse {
            object: "list".into(),
            data: (0..count)
                .map(|index| EmbeddingData {
                    object: "embedding".into(),
                    index,
                    embedding: vec![0.1, 0.2],
                })
                .collect(),
            model: req.model,
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 0,
                total_tokens: 10,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        })
    }
}

#[derive(Debug)]
struct SwitchableStore {
    fail: AtomicBool,
    route: Option<Route>,
}
#[async_trait]
impl RoutingStore for SwitchableStore {
    async fn list_for_org(&self, _: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        if self.fail.load(Ordering::SeqCst) {
            Err(RoutingStoreError::Backend("simulated outage".into()))
        } else {
            Ok(self.route.clone().map(|r| vec![r]).unwrap_or_default())
        }
    }
}

/// Privacy route: rewrites to the same model but forces redaction.
fn privacy_route(paused: bool) -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "embedding-privacy".into(),
        priority: 1,
        enabled: true,
        paused,
        when: RouteConditions {
            prompt_contains_any_of: vec!["downgrade me".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: Some("text-embedding-3-large".into()),
            redact: true,
            disable_cache: true,
            ..Default::default()
        },
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    provider: Arc<RecordingEmbedder>,
    backing: Arc<SwitchableStore>,
    routing: Arc<CachingRoutingStore>,
    org: Uuid,
}
impl Harness {
    /// Privacy route with the given pause state; ZERO TTL so the cached
    /// engine snapshot always expires (deterministic warm→outage coverage).
    async fn new(paused: bool) -> Self {
        let org = Uuid::now_v7();
        let keys = Arc::new(InMemoryKeyStore::new());
        let key = issue(
            keys.as_ref(),
            &InMemoryAuditWriter::new(),
            org,
            "embed-safety",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap()
        .plaintext;
        let provider = Arc::new(RecordingEmbedder::default());
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let backing = Arc::new(SwitchableStore {
            fail: AtomicBool::new(false),
            route: Some(privacy_route(paused)),
        });
        let routing = Arc::new(CachingRoutingStore::with_ttl(
            backing.clone(),
            Duration::ZERO,
        ));
        let app = build_router(
            AppState::new(registry)
                .with_key_store(keys)
                .with_routing_store(routing.clone()),
        );
        Self {
            app,
            key,
            provider,
            backing,
            routing,
            org,
        }
    }

    fn embed_req(&self, input: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.key))
            .body(Body::from(
                json!({ "model": "text-embedding-3-large", "input": input }).to_string(),
            ))
            .unwrap()
    }

    async fn assert_503_refusal(&self, input: serde_json::Value) {
        let response = self
            .app
            .clone()
            .oneshot(self.embed_req(input))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "service_unavailable");
        assert!(
            !body.to_string().contains("simulated outage"),
            "no backend details in client error"
        );
        assert!(
            self.provider.calls.lock().unwrap().is_empty(),
            "outage caused provider I/O"
        );
    }
}

#[tokio::test]
async fn cold_store_outage_refuses_embeddings_before_dispatch() {
    let h = Harness::new(false).await;
    h.backing.fail.store(true, Ordering::SeqCst);
    h.assert_503_refusal(json!("please downgrade me now")).await;
}

#[tokio::test]
async fn expired_snapshot_with_store_outage_refuses_embeddings() {
    let h = Harness::new(false).await;
    // Warm the engine cache, then fail the store: the zero TTL forces a
    // refresh that must hit the poisoned backend.
    assert_eq!(h.routing.engine_for(h.org).await.unwrap().routes().len(), 1);
    h.backing.fail.store(true, Ordering::SeqCst);
    h.assert_503_refusal(json!("please downgrade me now")).await;
}

#[tokio::test]
async fn matched_privacy_route_redacts_single_and_batch_embedding_inputs() {
    let h = Harness::new(false).await;
    // Single input with an email + an API key.
    let r1 = h
        .app
        .clone()
        .oneshot(h.embed_req(json!(
            "downgrade me: contact jane.doe@example.com with sk-ant-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        )))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    // Batch: the clean item is untouched, the dirty items are redacted.
    let r2 = h
        .app
        .clone()
        .oneshot(h.embed_req(json!([
            "downgrade me: clean text",
            "downgrade me: reach bob@example.com",
            "downgrade me: use key sk-ant-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        ])))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let _ = to_bytes(r2.into_body(), 4096).await.unwrap();

    let calls = h.provider.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    // Single: redacted.
    match &calls[0].input {
        tt_shared::messages::EmbeddingInput::Single(text) => {
            assert!(!text.contains("jane.doe@example.com"), "PII leaked: {text}");
            assert!(!text.contains("sk-ant-aaaa"), "secret leaked: {text}");
            assert!(text.contains("[REDACTED]"));
        }
        other => panic!("expected single input, got {other:?}"),
    }
    // Batch: clean untouched, dirty redacted — no item is dropped.
    match &calls[1].input {
        tt_shared::messages::EmbeddingInput::Batch(texts) => {
            assert_eq!(texts.len(), 3, "every batch item dispatches");
            assert_eq!(texts[0], "downgrade me: clean text");
            assert!(
                !texts[1].contains("bob@example.com"),
                "PII leaked: {}",
                texts[1]
            );
            assert!(texts[1].contains("[REDACTED]"));
            assert!(
                !texts[2].contains("sk-ant-bbbb"),
                "secret leaked: {}",
                texts[2]
            );
        }
        other => panic!("expected batch input, got {other:?}"),
    }
}

#[tokio::test]
async fn paused_privacy_route_still_redacts_embedding_inputs() {
    let h = Harness::new(true).await;
    let r = h
        .app
        .clone()
        .oneshot(h.embed_req(json!("downgrade me: contact jane.doe@example.com please")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let _ = to_bytes(r.into_body(), 4096).await.unwrap();
    let calls = h.provider.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    match &calls[0].input {
        tt_shared::messages::EmbeddingInput::Single(text) => {
            assert!(!text.contains("jane.doe@example.com"), "PII leaked: {text}");
            assert!(text.contains("[REDACTED]"));
        }
        other => panic!("expected single input, got {other:?}"),
    }
}
