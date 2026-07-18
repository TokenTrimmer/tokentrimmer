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
                extra: Default::default(),
            },
            finish_reason: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
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
            extra: Default::default(),
        }],
        usage: Some(Usage {
            prompt_tokens: 20,
            completion_tokens,
            total_tokens: 20_u64 + completion_tokens,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }),
        extra: Default::default(),
    }
}

/// A finish chunk whose terminal usage carries raw provider cache counts.
fn finish_chunk_with_cache(
    completion_tokens: u64,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
) -> ChatCompletionChunk {
    let mut chunk = finish_chunk(completion_tokens);
    let usage = chunk.usage.as_mut().unwrap();
    usage.cached_tokens = cache_read.unwrap_or(0);
    usage.cache_read_input_tokens = cache_read;
    usage.cache_creation_input_tokens = cache_creation;
    chunk
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
            flex_input_per_million: None,
            flex_output_per_million: None,
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
        // No graceful-shutdown drain in this test — bare spawn fallback.
        tracker: None,
        org_id: Uuid::nil(),
        api_key_id: Uuid::nil(),
        trace_id: Uuid::nil(),
        provider_id: "mock".into(),
        model: "test-model".into(),
        input_tokens: 20,
        cached_tokens: 0,
        retrieval_tokens_saved: 0,
        pricing: Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
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
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: chrono::Utc::now(),
        }),
        route_id: None,
        route_version_id: None,
        tag: None,
        request_started: std::time::Instant::now(),
        spend_sink: tt_core::budget::SpendSink::None,
        fee_multiplier: 1.0,
        flex_applied: false,
        pass_effects: tt_core::passes::PassEffects::default(),
        cache_insert: None,
        include_usage: false,
        span_ctx: None,
        traffic_split_arm: None,
        route_paused: false,
        panel: None,
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
        flex_input_per_million: None,
        flex_output_per_million: None,
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
        flex_input_per_million: None,
        flex_output_per_million: None,
        prompt_cache_min_tokens: None,
        effective_at: chrono::Utc::now(),
    };
    let mut ctx = make_log_ctx(Arc::clone(&writer));
    ctx.model = "gpt-4o-mini".into();
    ctx.pricing = Some(cheap);
    ctx.baseline_pricing = Some(expensive);
    ctx.route_id = Some(Uuid::now_v7());
    ctx.route_version_id = Some(9_876_543_210);

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
    assert_eq!(
        row.route_version_id,
        Some(9_876_543_210),
        "SSE DropGuard must preserve the immutable route-version ID"
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

// ─── GW-STREAM: include_usage end-to-end ─────────────────────────────────────

/// (a) When the client set `stream_options.include_usage = true`, the gateway
/// must emit an OpenAI-native final usage chunk (`choices: []`, populated
/// `usage`) before `[DONE]`, even when the provider folded usage into its
/// finish_reason chunk (as Anthropic/Gemini do) rather than emitting a
/// standalone usage chunk.
#[tokio::test]
async fn include_usage_emits_openai_native_usage_chunk() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;
    // Provider folds usage into the finish chunk (no standalone usage chunk).
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("Hi")), Ok(finish_chunk(5))];
    let stream = futures::stream::iter(chunks).boxed();

    let mut ctx = make_log_ctx(Arc::clone(&writer));
    ctx.include_usage = true;

    let response = stream_response(stream, &provider, Uuid::nil(), Some(ctx));
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);

    // A standalone OpenAI-native usage chunk: empty choices + a usage block,
    // emitted as a normal `data:` SSE chunk (not the tokentrimmer.usage frame).
    let usage_chunk_line = text
        .lines()
        .find(|l| l.contains("\"choices\":[]") && l.contains("\"usage\""))
        .unwrap_or_else(|| panic!("expected OpenAI-native usage chunk; body:\n{text}"));
    let v: serde_json::Value =
        serde_json::from_str(usage_chunk_line.trim_start_matches("data:").trim()).unwrap();
    assert_eq!(v["object"], "chat.completion.chunk");
    assert!(v["choices"].as_array().unwrap().is_empty());
    assert_eq!(v["usage"]["completion_tokens"], 5);
    assert_eq!(v["usage"]["prompt_tokens"], 20);

    // The usage chunk must precede the tokentrimmer.usage frame and [DONE].
    let chunk_pos = text.find("\"choices\":[]").unwrap();
    let tt_pos = text.find("tokentrimmer.usage").unwrap();
    let done_pos = text.find("[DONE]").unwrap();
    assert!(chunk_pos < tt_pos, "usage chunk must precede tt frame");
    assert!(tt_pos < done_pos, "tt frame must precede [DONE]");
}

/// (a-neg) When the client did NOT request include_usage, the gateway must not
/// inject a standalone OpenAI-native usage chunk (a chunk with empty choices).
/// The provider's own folded-usage finish chunk is untouched.
#[tokio::test]
async fn no_include_usage_does_not_inject_standalone_usage_chunk() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("Hi")), Ok(finish_chunk(5))];
    let stream = futures::stream::iter(chunks).boxed();

    // include_usage defaults to false in make_log_ctx.
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
        !text.contains("\"choices\":[]"),
        "should not inject a standalone empty-choices usage chunk; body:\n{text}"
    );
}

/// (b) The `tokentrimmer.usage` frame keeps its documented shape exactly — the
/// parallel SDK lane parses it. Adding the include_usage chunk must not change
/// these fields.
#[tokio::test]
async fn tokentrimmer_usage_frame_shape_unchanged_with_include_usage() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("Hi")), Ok(finish_chunk(5))];
    let stream = futures::stream::iter(chunks).boxed();

    let mut ctx = make_log_ctx(Arc::clone(&writer));
    ctx.include_usage = true;

    let response = stream_response(stream, &provider, Uuid::nil(), Some(ctx));
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);

    // Find the `event: tokentrimmer.usage` frame's data line.
    let mut lines = text.lines().peekable();
    let mut data_json: Option<serde_json::Value> = None;
    while let Some(line) = lines.next() {
        if line.trim() == "event: tokentrimmer.usage" {
            // The data line follows the event line.
            let data_line = lines
                .find(|l| l.starts_with("data:"))
                .expect("tokentrimmer.usage frame must have a data line");
            data_json =
                Some(serde_json::from_str(data_line.trim_start_matches("data:").trim()).unwrap());
            break;
        }
    }
    let v = data_json.expect("tokentrimmer.usage frame present");
    // Documented golden keys — the parallel SDK depends on these EXACTLY.
    // (`cache_bust_usd` added by the cache-aware pass lane: the explicit
    // negative-savings entry, so streaming clients see the bust MAGNITUDE and
    // not just the penalty-reduced `saved_usd` headline.)
    // TR-6: the per-lever breakdown fields now ride the streaming event too
    // (the streaming CostBreakdown carries the same levers as the non-streaming
    // response headers).
    for key in [
        "cost_usd",
        "baseline_cost_usd",
        "saved_usd",
        "provider_cache_saved_usd",
        "cache_bust_usd",
        "compression_saved_usd",
        "flex_saved_usd",
        "doc_compaction_saved_usd",
        "diff_saved_usd",
        "minify_saved_est_usd",
        "format_switch_saved_est_usd",
        "content_compress_saved_est_usd",
        "doc_vision_saved_est_usd",
        "summarizer_tax_usd",
        "batch_forgone_usd",
        "input_tokens",
        "output_tokens",
        "cached_tokens",
    ] {
        assert!(
            v.get(key).is_some(),
            "tokentrimmer.usage frame missing documented key `{key}`; frame:\n{v}"
        );
    }
    let obj = v.as_object().unwrap();
    assert_eq!(
        obj.len(),
        18,
        "tokentrimmer.usage frame must carry exactly the 18 documented keys; got {obj:?}"
    );
    assert_eq!(v["input_tokens"], 20);
    assert_eq!(v["output_tokens"], 5);
}

/// (c) An injected unknown field on a provider chunk survives the gateway's
/// serialize round-trip rather than being silently dropped.
#[tokio::test]
async fn unknown_provider_chunk_field_survives_passthrough() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    let mut content = content_chunk("Hi");
    content
        .extra
        .insert("system_fingerprint".into(), serde_json::json!("fp_zzz"));
    content.choices[0]
        .extra
        .insert("logprobs".into(), serde_json::json!({ "content": [] }));
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content), Ok(finish_chunk(5))];
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
        text.contains("\"system_fingerprint\":\"fp_zzz\""),
        "unknown top-level chunk field must survive; body:\n{text}"
    );
    assert!(
        text.contains("\"logprobs\""),
        "unknown per-choice chunk field must survive; body:\n{text}"
    );
}

// ─── Provider-cache token telemetry (research Phase 0.2) ─────────────────────

/// A cleanly-completed stream whose terminal usage reports raw provider cache
/// counts persists them verbatim on the request_logs row.
#[tokio::test]
async fn sse_clean_completion_logs_provider_cache_token_counts() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> = vec![
        Ok(content_chunk("Hi")),
        Ok(finish_chunk_with_cache(42, Some(80), Some(20))),
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
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(!row.truncated);
    assert_eq!(row.cache_read_input_tokens, Some(80));
    assert_eq!(row.cache_creation_input_tokens, Some(20));
    // The folded column keeps its legacy semantics alongside.
    assert_eq!(row.cached_tokens, 80);
}

/// A truncated stream (no terminal usage) logs NULL provider-cache counts —
/// estimates are never fabricated into the raw columns.
#[tokio::test]
async fn sse_truncated_stream_logs_null_provider_cache_tokens() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    // Content but no finish/usage chunk → truncated.
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("Hello")), Ok(content_chunk(" world"))];
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
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(row.truncated);
    assert_eq!(
        row.cache_read_input_tokens, None,
        "truncated stream must log NULL, never an estimate"
    );
    assert_eq!(row.cache_creation_input_tokens, None);
}

/// A terminal usage block WITHOUT raw cache fields (provider didn't report)
/// logs NULL — Some(0) is reserved for an explicit provider-reported zero.
#[tokio::test]
async fn sse_clean_completion_without_cache_report_logs_null() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("Hi")), Ok(finish_chunk(42))];
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
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(!row.truncated);
    assert_eq!(row.cache_read_input_tokens, None);
    assert_eq!(row.cache_creation_input_tokens, None);
}

/// Pre-fix folded chunks (cached_tokens > 0 but no raw field — e.g. a cached
/// L1 fake-stream or an older adapter) must not regress real cache reads to
/// NULL: the nonzero fold maps to Some(n).
#[tokio::test]
async fn sse_folded_nonzero_cached_tokens_maps_to_some() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockProvider) as Arc<dyn Provider>;

    let mut chunk = finish_chunk(42);
    {
        let usage = chunk.usage.as_mut().unwrap();
        usage.cached_tokens = 15; // folded only; raw field absent
        usage.cache_read_input_tokens = None;
    }
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("Hi")), Ok(chunk)];
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
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].cache_read_input_tokens,
        Some(15),
        "a nonzero fold proves real cache reads — must not regress to NULL"
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
