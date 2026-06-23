//! Tests for the `arbitrate_streaming` method on `ArbiterStrategy`.
//!
//! Covers:
//!   A) BestOfN default-impl replay: returns `ArbiterCostPlan::Known` and the
//!      chosen leg's text verbatim.
//!   B) Synthesize override: dispatches `chat_completion_stream`, returns
//!      `ArbiterCostPlan::Live` and streams the arbiter's chunks.
//!
//! Run with:
//!   cargo test -p tt-core --test panel_streaming

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use uuid::Uuid;

use tt_core::{
    routes::panel::{
        ArbiterCostPlan, ArbiterDetail, ArbiterStrategy, ArbiterStrategyKind, BestOfN, LegResult,
        LegRole, LegStatus, ModelRef, Synthesize,
    },
    routes::sse::{stream_response, PanelStreamLog, StreamLogContext},
    AppState, ProviderRegistry,
};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::request_logs::InMemoryRequestLogWriter;

// ---------------------------------------------------------------------------
// Helpers shared by both tests
// ---------------------------------------------------------------------------

fn test_creds(key: &str) -> ProviderCredentials {
    ProviderCredentials {
        api_key: SecretString::new(key.to_string()),
        base_url: None,
        extra_headers: Vec::new(),
    }
}

fn test_ctx() -> RequestContext {
    RequestContext {
        trace_id: Uuid::now_v7(),
        org_id: Uuid::now_v7(),
        api_key_id: Uuid::now_v7(),
        credentials: test_creds("test-key"),
        tag: None,
        deadline: None,
    }
}

fn base_req() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "mock-leg0".into(),
        messages: vec![Message::User {
            content: MessageContent::Text("hello".into()),
            name: None,
        }],
        stream: false,
        ..Default::default()
    }
}

/// Build an ok LegResult with the given answer text.
fn make_ok_leg(leg_index: usize, answer: &str) -> LegResult {
    LegResult {
        leg_index,
        role: LegRole::Leg,
        model: format!("mock-leg{leg_index}"),
        provider: format!("mock-provider-leg{leg_index}"),
        status: LegStatus::Ok,
        response: Some(ChatCompletionResponse {
            id: format!("chatcmpl-leg{leg_index}"),
            object: "chat.completion".into(),
            created: 0,
            model: format!("mock-leg{leg_index}"),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(answer.to_string())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage::default(),
        }),
        cost_usd: None,
        usage: None,
        latency_ms: 0,
    }
}

/// A streaming content chunk carrying `text` as `delta.content`.
fn content_chunk(text: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: "id".into(),
        object: "chat.completion.chunk".into(),
        created: 0,
        model: "panel-arbiter".into(),
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

/// A finish chunk with an authoritative usage block (clean completion).
fn finish_chunk(completion_tokens: u64) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: "id".into(),
        object: "chat.completion.chunk".into(),
        created: 0,
        model: "panel-arbiter".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            extra: Default::default(),
        }],
        usage: Some(Usage {
            prompt_tokens: 20,
            completion_tokens,
            total_tokens: 20 + completion_tokens,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }),
        extra: Default::default(),
    }
}

/// Drain a `BoxStream<ChatCompletionChunk>` into a `String` by concatenating
/// `choices[0].delta.content` from every chunk.
async fn drain_stream(
    mut stream: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
) -> String {
    let mut out = String::new();
    while let Some(item) = stream.next().await {
        if let Ok(chunk) = item {
            if let Some(choice) = chunk.choices.first() {
                if let Some(ref text) = choice.delta.content {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Mock providers
// ---------------------------------------------------------------------------

/// A judge provider that returns a fixed buffered response (for BestOfN).
struct MockJudge {
    id: &'static str,
    model: &'static str,
    judge_response: &'static str,
}

#[async_trait]
impl Provider for MockJudge {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: 5.0,
                output_per_million: 15.0,
                cached_input_per_million: None,
                cache_write_per_million: None,
                batch_input_per_million: None,
                batch_output_per_million: None,
                flex_input_per_million: None,
                flex_output_per_million: None,
                prompt_cache_min_tokens: None,
                effective_at: Utc::now(),
            })
        } else {
            None
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse {
            id: "chatcmpl-judge".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model.clone(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(self.judge_response.into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
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
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

/// A streaming arbiter provider that emits a fixed sequence of text chunks.
struct MockStreamingArbiter {
    id: &'static str,
    model: &'static str,
    /// Each element becomes one `delta.content` chunk.
    chunks: Vec<&'static str>,
}

#[async_trait]
impl Provider for MockStreamingArbiter {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Streaming],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: 5.0,
                output_per_million: 15.0,
                cached_input_per_million: None,
                cache_write_per_million: None,
                batch_input_per_million: None,
                batch_output_per_million: None,
                flex_input_per_million: None,
                flex_output_per_million: None,
                prompt_cache_min_tokens: None,
                effective_at: Utc::now(),
            })
        } else {
            None
        }
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("non-streaming not used".into()))
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> = self
            .chunks
            .iter()
            .enumerate()
            .map(|(i, text)| {
                Ok(ChatCompletionChunk {
                    id: format!("chunk-{i}"),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: self.model.to_string(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: if i == 0 {
                                Some("assistant".into())
                            } else {
                                None
                            },
                            content: Some(text.to_string()),
                            tool_calls: vec![],
                            extra: Default::default(),
                        },
                        finish_reason: None,
                        extra: Default::default(),
                    }],
                    usage: None,
                    extra: Default::default(),
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

// ---------------------------------------------------------------------------
// Test A: BestOfN default-impl replay
// ---------------------------------------------------------------------------

/// BestOfN default-impl `arbitrate_streaming` runs the buffered `arbitrate`,
/// replays the chosen leg verbatim, returns `ArbiterCostPlan::Known`, and
/// sets `detail.chosen_leg` to the picked leg's index.
#[tokio::test]
async fn best_of_n_streaming_replay_yields_known_plan_and_chosen_leg() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockJudge {
        id: "mock-provider-judge",
        model: "mock-judge",
        // Judge picks candidate 2 (1-indexed), which maps to answers[1] = leg index 1.
        judge_response: "2\nmost complete answer",
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();

    let legs = vec![
        make_ok_leg(0, "Answer from leg 0"),
        make_ok_leg(1, "Answer from leg 1 — the best one"),
        make_ok_leg(2, "Answer from leg 2"),
    ];

    let strategy = BestOfN {
        arbiter_model: ModelRef {
            model: "mock-judge".to_string(),
            provider: None,
        },
    };

    let (stream, plan, detail) = strategy
        .arbitrate_streaming(&base_req(), &legs, &state, &ctx, &creds)
        .await
        .expect("BestOfN arbitrate_streaming should succeed");

    // Cost plan must be Known (replay — do not re-price streamed tokens).
    assert!(
        matches!(plan, ArbiterCostPlan::Known(_)),
        "BestOfN streaming must return ArbiterCostPlan::Known, got: {plan:?}"
    );

    // chosen_leg must be the leg index BestOfN picked.
    assert_eq!(
        detail.chosen_leg,
        Some(1),
        "chosen_leg must be 1 (judge picked candidate 2 = answers[1] = leg_index 1)"
    );

    // The streamed text must be the original leg 1 answer.
    let text = drain_stream(stream).await;
    assert_eq!(
        text, "Answer from leg 1 — the best one",
        "replayed stream must carry the original chosen leg text verbatim"
    );
}

// ---------------------------------------------------------------------------
// Test B: Synthesize override — live streaming
// ---------------------------------------------------------------------------

/// Synthesize::arbitrate_streaming dispatches `chat_completion_stream`,
/// returns `ArbiterCostPlan::Live`, and the returned stream carries the
/// arbiter's chunks.
#[tokio::test]
async fn synthesize_streaming_yields_live_plan_and_streamed_chunks() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockStreamingArbiter {
        id: "mock-streaming-provider",
        model: "mock-synth",
        chunks: vec!["Synth", "esized"],
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();

    // Two ok member legs (Synthesize requires at least one).
    let legs = vec![
        make_ok_leg(0, "First candidate answer"),
        make_ok_leg(1, "Second candidate answer"),
    ];

    let strategy = Synthesize {
        arbiter_model: ModelRef {
            model: "mock-synth".to_string(),
            provider: None,
        },
    };

    let (stream, plan, _detail) = strategy
        .arbitrate_streaming(&base_req(), &legs, &state, &ctx, &creds)
        .await
        .expect("Synthesize arbitrate_streaming should succeed");

    // Cost plan must be Live (fresh arbiter tokens, not a replay).
    assert!(
        matches!(plan, ArbiterCostPlan::Live),
        "Synthesize streaming must return ArbiterCostPlan::Live, got: {plan:?}"
    );

    // Collected stream text must match the mock arbiter's chunks.
    let text = drain_stream(stream).await;
    assert_eq!(
        text, "Synthesized",
        "streamed text must equal the concatenation of the mock arbiter's chunks"
    );
}

// ---------------------------------------------------------------------------
// Test C: DropGuard aggregate panel billing (Task 4)
// ---------------------------------------------------------------------------

/// A minimal provider whose only role here is to give `stream_response` a
/// provider id; the stream it serves is supplied directly to `stream_response`.
struct MockEgressProvider;

#[async_trait]
impl Provider for MockEgressProvider {
    fn id(&self) -> &'static str {
        "panel"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        None
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
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
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

/// Poll the SSE response body to exhaustion so the guarded stream (and its
/// `DropGuard`) is dropped, firing the terminal billing write.
async fn drain_response(response: axum::response::Response) {
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;
}

/// Wait up to 500ms for the writer to accumulate at least `n` rows.
async fn wait_for_rows(writer: &InMemoryRequestLogWriter, n: usize) {
    for _ in 0..50 {
        if writer.rows().len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Replay-strategy pricing: the model is priced, so the streamed answer's own
/// `cost_usd` would be > 0. The `ArbiterCostPlan::Known` path MUST discard that
/// streamed figure (it is the chosen leg's cost, already counted in
/// `leg_cost_total`) and use the pre-computed `Known` value instead — proving
/// the replayed leg is not double-counted.
fn priced() -> ModelPricing {
    ModelPricing {
        input_per_million: 5.0,
        output_per_million: 15.0,
        cached_input_per_million: None,
        cache_write_per_million: None,
        batch_input_per_million: None,
        batch_output_per_million: None,
        flex_input_per_million: None,
        flex_output_per_million: None,
        prompt_cache_min_tokens: None,
        effective_at: Utc::now(),
    }
}

/// Build a `StreamLogContext` whose `panel` field is populated with a BestOfN
/// replay plan: `leg_cost_total = $0.010`, `ArbiterCostPlan::Known($0.002)`.
fn panel_log_ctx(writer: Arc<InMemoryRequestLogWriter>) -> StreamLogContext {
    let leg_records = vec![
        make_ok_leg(0, "Answer from leg 0"),
        make_ok_leg(1, "Answer from leg 1 — the best one"),
    ];
    let panel = PanelStreamLog {
        leg_records,
        leg_cost_total: Some(0.010),
        strategy: ArbiterStrategyKind::BestOfN,
        quorum_required: 1,
        quorum_met: 2,
        arbiter_detail: ArbiterDetail {
            chosen_leg: Some(1),
            ..Default::default()
        },
        arbiter_cost_plan: ArbiterCostPlan::Known(Some(0.002)),
        arbiter_model: "panel-arbiter".to_string(),
        // Fire-and-forget panel_legs writer is exercised by panel_legs_persist.rs;
        // here we focus on the aggregate request_logs row, so leave it None.
        panel_leg_writer: None,
    };

    StreamLogContext {
        writer: Some(writer),
        tracker: None,
        org_id: Uuid::nil(),
        api_key_id: Uuid::nil(),
        trace_id: Uuid::nil(),
        provider_id: "panel".into(),
        model: "panel-arbiter".into(),
        input_tokens: 20,
        cached_tokens: 0,
        retrieval_tokens_saved: 0,
        // Priced so the streamed cost is nonzero — the Known plan must discard it.
        pricing: Some(priced()),
        baseline_pricing: Some(priced()),
        route_id: None,
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
        panel: Some(Arc::new(panel)),
    }
}

/// The streaming panel `DropGuard` writes ONE aggregate `request_logs` row:
/// `provider == "panel"`, `cached == false`, `model == arbiter`, and
/// `cost_usd == Σ legs + arbiter`. With a BestOfN replay plan
/// (`ArbiterCostPlan::Known`), the streamed answer's own cost is discarded — so
/// the total is `leg_cost_total ($0.010) + Known arbiter ($0.002) = $0.012`,
/// NOT `$0.012 + replayed-leg-cost`.
#[tokio::test]
async fn panel_aggregate_row_single_provider_panel_no_double_count() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockEgressProvider) as Arc<dyn Provider>;

    // A small replay stream: content chunks + a clean finish with usage. The
    // streamed usage is what `compute_cost_full` would price — but the Known
    // plan must throw that away.
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> = vec![
        Ok(content_chunk("Answer from leg 1 — ")),
        Ok(content_chunk("the best one")),
        Ok(finish_chunk(64)),
    ];
    let stream = futures::stream::iter(chunks).boxed();

    let response = stream_response(
        stream,
        &provider,
        Uuid::nil(),
        Some(panel_log_ctx(Arc::clone(&writer))),
    );

    drain_response(response).await;
    wait_for_rows(&writer, 1).await;

    let rows = writer.rows();
    assert_eq!(rows.len(), 1, "exactly one aggregate row expected");
    let row = &rows[0];
    assert_eq!(
        row.provider, "panel",
        "aggregate row provider must be 'panel'"
    );
    assert_eq!(
        row.model, "panel-arbiter",
        "aggregate row model must be the arbiter model"
    );
    assert!(!row.cached, "panel aggregate row must be cached == false");
    // Σ legs ($0.010) + Known arbiter ($0.002) = $0.012. The replayed leg's
    // streamed cost is NOT added (no double-count under ArbiterCostPlan::Known).
    assert!(
        (row.cost_usd - 0.012).abs() < 1e-9,
        "cost_usd must be Σ legs + Known arbiter = 0.012, got {}",
        row.cost_usd
    );
    // Panel rows claim no saving: baseline == cost.
    assert!(
        (row.baseline_cost_usd - row.cost_usd).abs() < 1e-9,
        "panel baseline must equal cost (no claimed saving)"
    );
}

// ---------------------------------------------------------------------------
// Test D: terminal SSE event order + panel-aware aggregate usage_event (Task 5)
// ---------------------------------------------------------------------------

/// Collect the raw SSE body as a string so we can inspect event order and data.
async fn collect_sse_body(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body bytes");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parse the SSE body into a list of `(event_type, data)` pairs.
/// Lines starting with `event:` set the current event type.
/// Lines starting with `data:` carry the payload.
/// A blank line dispatches the accumulated event.
fn parse_sse_events(body: &str) -> Vec<(String, String)> {
    let mut events = Vec::new();
    let mut current_event: Option<String> = None;
    let mut current_data: Option<String> = None;
    for line in body.lines() {
        if let Some(ev) = line.strip_prefix("event:") {
            current_event = Some(ev.trim().to_string());
        } else if let Some(d) = line.strip_prefix("data:") {
            current_data = Some(d.trim().to_string());
        } else if line.is_empty() {
            // dispatch
            if let Some(data) = current_data.take() {
                let event_type = current_event.take().unwrap_or_default();
                events.push((event_type, data));
            }
        }
    }
    events
}

/// The `stream_response` terminal sequence for a panel stream must:
///   1. emit a `tokentrimmer.panel` event immediately before `tokentrimmer.usage`,
///   2. emit `tokentrimmer.usage` before `[DONE]`,
///   3. the `tokentrimmer.panel` JSON has `legs` + `arbiter.strategy`,
///   4. the `tokentrimmer.usage` `cost_usd` equals the AGGREGATE (0.012),
///      matching the Task-4 `request_logs` row — not the arbiter-only streamed cost.
#[tokio::test]
async fn panel_terminal_event() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockEgressProvider) as Arc<dyn Provider>;

    // Same replay stream as the Task-4 test.
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> = vec![
        Ok(content_chunk("Answer from leg 1 — ")),
        Ok(content_chunk("the best one")),
        Ok(finish_chunk(64)),
    ];
    let stream = futures::stream::iter(chunks).boxed();

    let response = stream_response(
        stream,
        &provider,
        Uuid::nil(),
        Some(panel_log_ctx(Arc::clone(&writer))),
    );

    let body = collect_sse_body(response).await;
    let events = parse_sse_events(&body);

    // Extract event types in order.
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

    // Find positions of the key events.
    let panel_pos = types
        .iter()
        .position(|&t| t == "tokentrimmer.panel")
        .expect("tokentrimmer.panel event must be present");
    let usage_pos = types
        .iter()
        .position(|&t| t == "tokentrimmer.usage")
        .expect("tokentrimmer.usage event must be present");
    let done_pos = events
        .iter()
        .position(|(_, d)| d == "[DONE]")
        .expect("[DONE] sentinel must be present");

    // Order: panel immediately before usage, both before [DONE].
    assert_eq!(
        panel_pos + 1,
        usage_pos,
        "tokentrimmer.panel must be immediately before tokentrimmer.usage (panel@{panel_pos}, usage@{usage_pos})"
    );
    assert!(
        usage_pos < done_pos,
        "tokentrimmer.usage must come before [DONE] (usage@{usage_pos}, done@{done_pos})"
    );

    // Validate tokentrimmer.panel JSON shape: legs array + arbiter.strategy.
    let panel_json: serde_json::Value = serde_json::from_str(&events[panel_pos].1)
        .expect("tokentrimmer.panel data must be valid JSON");
    assert!(
        panel_json["legs"].is_array(),
        "tokentrimmer.panel must have a `legs` array"
    );
    assert!(
        panel_json["arbiter"]["strategy"].is_string(),
        "tokentrimmer.panel must have `arbiter.strategy`"
    );

    // Validate tokentrimmer.usage cost_usd == aggregate ($0.010 + $0.002 = $0.012).
    let usage_json: serde_json::Value = serde_json::from_str(&events[usage_pos].1)
        .expect("tokentrimmer.usage data must be valid JSON");
    let cost = usage_json["cost_usd"]
        .as_f64()
        .expect("tokentrimmer.usage must have numeric cost_usd");
    assert!(
        (cost - 0.012).abs() < 1e-9,
        "tokentrimmer.usage cost_usd must equal aggregate 0.012, got {cost}"
    );

    // Also verify the request_logs row agrees (Task 4 + Task 5 in lock-step).
    wait_for_rows(&writer, 1).await;
    let rows = writer.rows();
    assert_eq!(rows.len(), 1, "exactly one aggregate row expected");
    assert!(
        (rows[0].cost_usd - cost).abs() < 1e-9,
        "request_logs cost_usd ({}) must equal tokentrimmer.usage cost_usd ({cost})",
        rows[0].cost_usd
    );
}
