//! Per-chunk SSE overhead benchmark.
//!
//! Goal: measure ONLY the gateway-side cost of forwarding one SSE chunk
//! (parse → translate → emit). Provider latency is excluded by using an
//! in-process mock that emits a fixed N-chunk stream.
//!
//! Spec budget: < 1 ms per chunk (docs/04-gateway-api-reference.md latency).
//! This bench reports per-chunk cost in nanoseconds so the gate is observable;
//! the gate itself is NOT asserted here (criterion benches are profiling
//! tools, not CI gates — assert in load tests instead).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::Request;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use futures::stream::{BoxStream, StreamExt};
use tokio::runtime::Runtime;
use tower::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{ChunkChoice, ChunkDelta},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
};

/// Mock provider that emits `chunks` text deltas + a final stop chunk. No
/// provider latency — fires the entire stream synchronously, isolating the
/// gateway's per-chunk overhead.
struct FastMock {
    chunks: usize,
}

#[async_trait]
impl Provider for FastMock {
    fn id(&self) -> &'static str {
        "bench-mock"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "bench-stream".into(),
            provider: "bench-mock".into(),
            capabilities: vec![Capability::Streaming],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        None
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("bench-only stream".into()))
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let n = self.chunks;
        let v: Vec<Result<ChatCompletionChunk, ProviderError>> = (0..n)
            .map(|i| {
                Ok(ChatCompletionChunk {
                    id: format!("c{i}"),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: "bench-stream".into(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: if i == 0 {
                                Some("assistant".into())
                            } else {
                                None
                            },
                            content: Some("x".into()),
                            tool_calls: vec![],
                            extra: Default::default(),
                        },
                        finish_reason: if i == n - 1 {
                            Some("stop".into())
                        } else {
                            None
                        },
                        extra: Default::default(),
                    }],
                    usage: None,
                    extra: Default::default(),
                })
            })
            .collect();
        Ok(futures::stream::iter(v).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("n/a".into()))
    }
}

fn streaming_dispatch_overhead(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");
    let mut group = c.benchmark_group("streaming_per_chunk_overhead");
    group.measurement_time(Duration::from_secs(5));

    for &n in &[1usize, 10, 100] {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(FastMock { chunks: n }));
        let app = build_router(AppState::new(registry));

        let body_json = serde_json::json!({
            "model": "bench-stream",
            "messages": [{"role":"user","content":"go"}],
            "stream": true,
        })
        .to_string();

        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                rt.block_on(async {
                    let response = app
                        .clone()
                        .oneshot(
                            Request::builder()
                                .method("POST")
                                .uri("/v1/chat/completions")
                                .header("content-type", "application/json")
                                .body(Body::from(body_json.clone()))
                                .unwrap(),
                        )
                        .await
                        .expect("oneshot");
                    // Drain the body to include SSE encoding cost in the measurement.
                    let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
                });
            });
        });
    }

    group.finish();
}

criterion_group!(benches, streaming_dispatch_overhead);
criterion_main!(benches);
