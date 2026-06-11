//! End-to-end test: a non-cached chat completion writes a `request_logs`
//! row via the configured `RequestLogWriter`. Asserts every field the
//! dashboard's `/api/telemetry` endpoint will read from.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::request_logs::InMemoryRequestLogWriter;

struct TestProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for TestProvider {
    fn id(&self) -> &'static str {
        "test-provider"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "test-1".into(),
            provider: "test-provider".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "chatcmpl-test".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("hello back".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 120,
                completion_tokens: 60,
                total_tokens: 180,
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
        Err(ProviderError::Unsupported("n/a".into()))
    }
}

#[tokio::test]
async fn chat_miss_writes_request_log_row() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(TestProvider {
        calls: Arc::clone(&calls),
    }));
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let state = AppState::new(registry).with_request_log_writer(writer.clone());
    let app = build_router(state);

    let body = json!({
        "model": "test-1",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": false,
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-tag", "ci-suite")
        .body(Body::from(body))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // Telemetry write is fire-and-forget — give the spawned task a beat.
    for _ in 0..20 {
        if !writer.rows().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let rows = writer.rows();
    assert_eq!(rows.len(), 1, "expected exactly one telemetry row");
    let r = &rows[0];
    assert_eq!(r.provider, "test-provider");
    assert_eq!(r.model, "test-1");
    assert_eq!(r.input_tokens, 120);
    assert_eq!(r.output_tokens, 60);
    assert_eq!(r.cached_tokens, 0);
    assert!(!r.cached);
    assert!(r.cache_layer.is_none());
    assert_eq!(r.status, 200);
    assert_eq!(r.tag.as_deref(), Some("ci-suite"));
    // 3 * 120 + 6 * 60 = 720, /1_000_000 = 0.00072
    assert!(
        (r.cost_usd - 0.00072).abs() < 1e-9,
        "cost_usd was {}",
        r.cost_usd
    );
    assert!((r.baseline_cost_usd - 0.00072).abs() < 1e-9);
    assert!(r.trace_id.is_some());
    // Latency is whatever the test took; just sanity-bound it.
    assert!(r.latency_ms >= 0);
    assert!(r.latency_ms < 10_000);
}

#[tokio::test]
async fn l1_hit_writes_request_log_with_cache_layer_l1() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(TestProvider {
        calls: Arc::clone(&calls),
    }));
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let l1 = Arc::new(InMemoryL1Cache::new());
    let state = AppState::new(registry)
        .with_l1(l1, None)
        .with_request_log_writer(writer.clone());
    let app = build_router(state);

    fn build_req() -> Request<Body> {
        let body = json!({
            "model": "test-1",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": false,
        })
        .to_string();
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    // First call: miss → provider hit → writes 1 row with cached=false.
    let r1 = app.clone().oneshot(build_req()).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // Give the spawned L1 insert + telemetry write a beat.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Second call: hits L1, no provider call → writes 1 row with cached=true.
    let r2 = app.oneshot(build_req()).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider must NOT be called on L1 hit"
    );

    for _ in 0..20 {
        if writer.rows().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let rows = writer.rows();
    assert!(
        rows.len() >= 2,
        "expected miss + hit rows, got {}",
        rows.len()
    );
    let hit = rows
        .iter()
        .find(|r| r.cached)
        .expect("at least one cached row");
    assert_eq!(hit.cache_layer.as_deref(), Some("l1"));
    assert_eq!(hit.cost_usd, 0.0);
    assert!(
        hit.baseline_cost_usd > 0.0,
        "L1 hit must record a non-zero baseline"
    );
    assert_eq!(
        hit.provider, "test-provider",
        "envelope preserved provider id"
    );
    assert_eq!(hit.model, "test-1");
    assert_eq!(hit.status, 200);
}

#[tokio::test]
async fn no_writer_means_no_panic_and_no_row() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(TestProvider {
        calls: Arc::clone(&calls),
    }));
    // State without a writer.
    let state = AppState::new(registry);
    let app = build_router(state);

    let body = json!({
        "model": "test-1",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": false,
    })
    .to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
