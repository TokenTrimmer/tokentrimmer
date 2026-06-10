//! Tests: SSE stream wrapped in Drop-observing guard emits `request_logs` row
//! with partial usage and correct `truncated` flag.
//!
//! Three scenarios:
//!
//! 1. 3 content chunks then drop without a terminator → truncated=true, output_tokens > 0.
//! 2. 3 content chunks + terminator with usage block → truncated=false, output_tokens from usage.
//! 3. Zero-chunk drop (immediate client abort) → output_tokens=0, truncated=true.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{BoxStream, StreamExt};
use uuid::Uuid;

use tt_core::routes::sse::{stream_response, StreamLogContext};
use tt_shared::{
    messages::{ChunkChoice, ChunkDelta},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, EmbeddingsRequest, EmbeddingsResponse, ModelInfo,
    ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::request_logs::InMemoryRequestLogWriter;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn content_chunk(text: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: "id".into(),
        object: "chat.completion.chunk".into(),
        created: 0,
        model: "test-model".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: Some(text.into()),
                tool_calls: vec![],
            },
            finish_reason: None,
        }],
        usage: None,
    }
}

fn finish_chunk(completion_tokens: u64) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: "id".into(),
        object: "chat.completion.chunk".into(),
        created: 0,
        model: "test-model".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
        }],
        usage: Some(Usage {
            prompt_tokens: 20,
            completion_tokens,
            total_tokens: 20_u64 + completion_tokens,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
        }),
    }
}

// ─── Mock Provider ────────────────────────────────────────────────────────────

struct MockProvider;

#[async_trait::async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &'static str {
        "mock"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "test-model".into(),
            provider: "mock".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: None,
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
    ) -> Result<tt_shared::ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("n/a".into()))
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

fn make_log_ctx(writer: Arc<InMemoryRequestLogWriter>) -> StreamLogContext {
    StreamLogContext {
        writer: Some(writer),
        org_id: Uuid::nil(),
        api_key_id: Uuid::nil(),
        trace_id: Uuid::nil(),
        provider_id: "mock".into(),
        model: "test-model".into(),
        input_tokens: 20,
        cached_tokens: 0,
        pricing: Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        }),
        baseline_pricing: Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        }),
        route_id: None,
        tag: None,
        request_started: std::time::Instant::now(),
        spend_sink: tt_core::budget::SpendSink::None,
        fee_multiplier: 1.0,
        cache_insert: None,
    }
}

/// Poll the response body to exhaustion so the guarded stream gets dropped.
async fn drain_response(response: axum::response::Response) {
    // `to_bytes` drives the body to completion. We ignore the bytes;
    // what matters is that Axum's Sse body is fully polled so the
    // GuardedStream (and its DropGuard) are dropped.
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;
}

/// Wait up to 500ms for the writer to accumulate at least `n` rows.
async fn wait_for_rows(writer: &InMemoryRequestLogWriter, n: usize) {
    for _ in 0..50 {
        if writer.rows().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ─── Test 1: truncated drop ───────────────────────────────────────────────────

#[tokio::test]
async fn sse_truncated_drop_writes_row_with_truncated_true() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    // 3 content chunks, no finish_reason.
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> = vec![
        Ok(content_chunk("Hello")),
        Ok(content_chunk(" world")),
        Ok(content_chunk("!")),
    ];
    let stream = futures::stream::iter(chunks).boxed();

    let response = stream_response(
        stream,
        &provider,
        Uuid::nil(),
        Some(make_log_ctx(Arc::clone(&writer))),
    );

    drain_response(response).await;
    wait_for_rows(&writer, 1).await;

    let rows = writer.rows();
    assert_eq!(rows.len(), 1, "expected exactly one row");
    let row = &rows[0];
    assert!(
        row.truncated,
        "row.truncated should be true (no finish_reason seen)"
    );
    // §2.12 fix: output_tokens must now come from the tokenizer, NOT byte count.
    // "Hello world!" = 12 bytes, but tt_tokenize (chars/4 for "mock" provider)
    // gives ceil(12/4) = 3 tokens — materially less than the old byte count.
    assert!(
        row.output_tokens > 0,
        "output_tokens should be > 0 (tokenizer estimate)"
    );
    assert!(
        row.output_tokens < 12,
        "output_tokens ({}) should be less than byte count (12) after §2.12 fix",
        row.output_tokens
    );
    // For the "mock" provider (chars/4 heuristic): ceil(12/4) = 3.
    assert_eq!(
        row.output_tokens, 3,
        "output_tokens = tokenizer estimate for 'Hello world!' with chars/4"
    );
    assert_eq!(row.input_tokens, 20);
    assert_eq!(row.provider, "mock");
    assert_eq!(row.model, "test-model");
}

// ─── Test 2: clean completion with usage block ────────────────────────────────

#[tokio::test]
async fn sse_clean_completion_writes_row_with_truncated_false_and_authoritative_tokens() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    // 3 content chunks + finish terminator with usage.
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> = vec![
        Ok(content_chunk("Hi")),
        Ok(content_chunk(" there")),
        Ok(content_chunk(".")),
        Ok(finish_chunk(42)), // authoritative output_tokens = 42
    ];
    let stream = futures::stream::iter(chunks).boxed();

    let response = stream_response(
        stream,
        &provider,
        Uuid::nil(),
        Some(make_log_ctx(Arc::clone(&writer))),
    );

    drain_response(response).await;
    wait_for_rows(&writer, 1).await;

    let rows = writer.rows();
    assert_eq!(rows.len(), 1, "expected exactly one row");
    let row = &rows[0];
    assert!(
        !row.truncated,
        "row.truncated should be false (finish_reason observed)"
    );
    // Authoritative usage from the finish chunk.
    assert_eq!(
        row.output_tokens, 42,
        "output_tokens should come from the terminal usage block"
    );
    assert_eq!(row.input_tokens, 20);
    assert_eq!(row.cached_tokens, 0);
}

// ─── Test 4: routed stream prices baseline against the original model ─────────

#[tokio::test]
async fn sse_routed_stream_logs_baseline_against_original_model() {
    // A streamed request that was routed gpt-4o → gpt-4o-mini must log a
    // baseline priced against the ORIGINAL (expensive) model, so the row shows
    // the real routing saving. Before the fix the drop guard priced baseline
    // against `pricing` (the cheap routed model) → baseline == cost → saving 0.
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    let cheap = ModelPricing {
        input_per_million: 0.15,
        output_per_million: 0.6,
        cached_input_per_million: None,
        cache_write_per_million: None,
        batch_input_per_million: None,
        batch_output_per_million: None,
        prompt_cache_min_tokens: None,
        effective_at: chrono::Utc::now(),
    };
    let expensive = ModelPricing {
        input_per_million: 5.0,
        output_per_million: 15.0,
        cached_input_per_million: None,
        cache_write_per_million: None,
        batch_input_per_million: None,
        batch_output_per_million: None,
        prompt_cache_min_tokens: None,
        effective_at: chrono::Utc::now(),
    };
    let mut ctx = make_log_ctx(Arc::clone(&writer));
    ctx.model = "gpt-4o-mini".into();
    ctx.pricing = Some(cheap);
    ctx.baseline_pricing = Some(expensive);
    ctx.route_id = Some(Uuid::now_v7());

    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("hi")), Ok(finish_chunk(42))];
    let stream = futures::stream::iter(chunks).boxed();

    let response = stream_response(stream, &provider, Uuid::nil(), Some(ctx));
    drain_response(response).await;
    wait_for_rows(&writer, 1).await;

    let rows = writer.rows();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(
        row.baseline_cost_usd > row.cost_usd,
        "routed stream baseline ({}) should exceed routed cost ({})",
        row.baseline_cost_usd,
        row.cost_usd
    );
}

// ─── Test 5: terminal usage event before [DONE] ──────────────────────────────

#[tokio::test]
async fn sse_emits_terminal_usage_event_before_done() {
    // On clean completion the stream must emit a `tokentrimmer.usage` SSE event
    // carrying cost/baseline/saved BEFORE the [DONE] sentinel, so streaming
    // clients can surface per-request savings (which response headers can't).
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("Hi")), Ok(finish_chunk(5))];
    let stream = futures::stream::iter(chunks).boxed();

    let response = stream_response(
        stream,
        &provider,
        Uuid::nil(),
        Some(make_log_ctx(Arc::clone(&writer))),
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);

    assert!(
        text.contains("tokentrimmer.usage"),
        "should emit a tokentrimmer.usage event; body:\n{text}"
    );
    assert!(
        text.contains("saved_usd"),
        "usage event should carry saved_usd"
    );
    let usage_pos = text
        .find("tokentrimmer.usage")
        .expect("usage event present");
    let done_pos = text.find("[DONE]").expect("[DONE] present");
    assert!(
        usage_pos < done_pos,
        "usage event must precede [DONE]; body:\n{text}"
    );
}

// ─── Test 3: zero-chunk immediate abort ──────────────────────────────────────

#[tokio::test]
async fn sse_zero_chunk_drop_writes_row_output_tokens_zero_truncated_true() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    // Empty stream — simulates client aborting before the first chunk arrives.
    let stream: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>> =
        futures::stream::iter(vec![]).boxed();

    let response = stream_response(
        stream,
        &provider,
        Uuid::nil(),
        Some(make_log_ctx(Arc::clone(&writer))),
    );

    drain_response(response).await;
    wait_for_rows(&writer, 1).await;

    let rows = writer.rows();
    assert_eq!(
        rows.len(),
        1,
        "expected exactly one row even on immediate abort"
    );
    let row = &rows[0];
    assert!(row.truncated, "row.truncated should be true (empty stream)");
    assert_eq!(row.output_tokens, 0, "output_tokens should be 0");
    assert_eq!(row.input_tokens, 20);
}
