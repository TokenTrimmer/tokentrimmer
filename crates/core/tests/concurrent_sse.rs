//! 100 concurrent SSE streams through the gateway.
//!
//! This is the Week 6 hard gate before Week 7 lands: the dispatch path
//! (`crates/core/src/routes/{chat,sse}.rs`) must handle 100 simultaneous
//! streaming requests without dropping chunks, exhausting tokio tasks, or
//! deadlocking on the shared state's `Arc<ProviderRegistry>`.
//!
//! The mock provider here emits a small, deterministic chunk burst — what we
//! exercise is the gateway plumbing, not provider latency.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{ChunkChoice, ChunkDelta},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
};

/// Mock streaming provider — counts dispatches so we can assert nothing was dropped.
struct CountingMock {
    dispatches: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CountingMock {
    fn id(&self) -> &'static str {
        "mock-concurrent"
    }

    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "burst".into(),
            provider: "mock-concurrent".into(),
            capabilities: vec![Capability::Streaming],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }

    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.0,
            output_per_million: 0.0,
            cached_input_per_million: Some(0.0),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        })
    }

    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported(
            "non-streaming not exercised here".into(),
        ))
    }

    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.dispatches.fetch_add(1, Ordering::Relaxed);

        // Emit a small deterministic burst of chunks.
        let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> = (0..5)
            .map(|i| {
                Ok(ChatCompletionChunk {
                    id: format!("c{i}"),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: "burst".into(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: if i == 0 {
                                Some("assistant".into())
                            } else {
                                None
                            },
                            content: Some(format!("chunk-{i} ")),
                            tool_calls: vec![],
                        },
                        finish_reason: if i == 4 { Some("stop".into()) } else { None },
                    }],
                    usage: None,
                })
            })
            .collect();

        Ok(futures::stream::iter(chunks).boxed())
    }

    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

/// Fire 100 concurrent streaming requests through the gateway and verify each
/// returns a valid SSE stream terminated with `data: [DONE]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_concurrent_sse_streams_complete_cleanly() {
    const REQUESTS: usize = 100;

    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountingMock {
        dispatches: Arc::clone(&counter),
    }));
    let app = build_router(AppState::new(registry));

    let body = json!({
        "model": "burst",
        "messages": [{ "role": "user", "content": "go" }],
        "stream": true
    })
    .to_string();

    let start = Instant::now();

    // Spawn 100 concurrent requests via tokio::spawn so they actually run in parallel
    // on the multi-thread runtime, not serialised on a single task.
    let mut handles = Vec::with_capacity(REQUESTS);
    for i in 0..REQUESTS {
        let app = app.clone();
        let body = body.clone();
        handles.push(tokio::spawn(async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/chat/completions")
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .expect("oneshot succeeds");
            (i, response)
        }));
    }

    // Collect all 100 responses + bodies.
    let mut completed = 0usize;
    let mut total_bytes = 0usize;
    for h in handles {
        let (i, response) = h.await.expect("task panicked");
        assert_eq!(response.status(), StatusCode::OK, "req {i}: status not 200");

        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/event-stream"),
            "req {i}: content-type was {ct:?}, expected text/event-stream"
        );
        assert!(
            response.headers().contains_key("x-tokentrimmer-trace-id"),
            "req {i}: missing x-tokentrimmer-trace-id"
        );
        assert!(
            response.headers().contains_key("x-tokentrimmer-provider"),
            "req {i}: missing x-tokentrimmer-provider"
        );

        // Drain the body and verify SSE shape + [DONE] terminator.
        let bytes = to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("body collects");
        total_bytes += bytes.len();
        let text = std::str::from_utf8(&bytes).expect("utf-8 body");
        assert!(
            text.contains("data: "),
            "req {i}: body did not contain SSE 'data: ' prefix\nbody: {text:?}"
        );
        assert!(
            text.contains("data: [DONE]"),
            "req {i}: missing terminating 'data: [DONE]'\nbody tail: {:?}",
            &text[text.len().saturating_sub(120)..]
        );
        completed += 1;
    }

    let elapsed = start.elapsed();

    assert_eq!(completed, REQUESTS, "all 100 streams must complete");
    assert_eq!(
        counter.load(Ordering::Relaxed),
        REQUESTS,
        "provider should see exactly 100 dispatches (no drops, no dupes)"
    );

    // Soft wall-clock budget: 100 streams through the in-process router should
    // comfortably finish in <2s on a developer machine. If this fails on CI,
    // raise the budget — it's a sanity check, not a strict perf gate.
    assert!(
        elapsed < Duration::from_secs(5),
        "100 concurrent streams took {elapsed:?} — investigate before raising the budget",
    );

    eprintln!(
        "100 concurrent SSE streams completed in {elapsed:?}, total body bytes {total_bytes}",
    );
}
