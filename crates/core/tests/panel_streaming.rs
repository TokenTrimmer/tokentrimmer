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
        run_id: None,
        node_id: None,
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
    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-judge".to_string(), test_creds("judge-key"));

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
    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert(
        "mock-streaming-provider".to_string(),
        test_creds("arbiter-key"),
    );

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

/// The live arbiter stream also requires the explicitly mapped provider
/// credential; it must not inherit the source request's credential.
#[tokio::test]
async fn synthesize_streaming_requires_explicit_arbiter_credential() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockStreamingArbiter {
        id: "mock-streaming-provider",
        model: "mock-synth",
        chunks: vec!["should", "not", "dispatch"],
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();
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

    let error = match strategy
        .arbitrate_streaming(&base_req(), &legs, &state, &ctx, &creds)
        .await
    {
        Ok(_) => panic!("unmapped streaming arbiter must not inherit source credentials"),
        Err(error) => error,
    };

    assert!(
        matches!(
            &error,
            tt_core::ApiError::MissingProviderCredential { provider }
                if provider == "mock-streaming-provider"
        ),
        "expected explicit missing-arbiter credential error, got {error:?}"
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
        // Simulate a panel triggered by a matched route: the aggregate stream
        // must retain the exact immutable definition that selected it.
        route_id: Some(Uuid::from_u128(0x42)),
        route_version_id: Some(9_876_543_210),
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
    assert_eq!(row.route_id, Some(Uuid::from_u128(0x42)));
    assert_eq!(
        row.route_version_id,
        Some(9_876_543_210),
        "streamed panel aggregate must preserve the matched immutable route version"
    );
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

// ===========================================================================
// Task E — truncated streaming panel billing (Phase-5 coverage gap)
//
// §D5 truncation discipline for streaming panels: a Synthesize panel whose
// arbiter stream ends WITHOUT a terminal `finish_reason` chunk (e.g. the
// client disconnects mid-stream) must still write exactly ONE aggregate
// `request_logs` row with `provider="panel"`, `truncated=true`,
// `cached=false`, and a cost that is the partial-but-honest figure
// (leg costs + tokenizer-estimated arbiter cost).
// ===========================================================================

/// Build a `StreamLogContext` with `ArbiterCostPlan::Live` (Synthesize
/// strategy).  Used by the truncation test: the streamed answer's cost IS
/// the arbiter's cost — no replay, no discard.
fn panel_log_ctx_live(writer: Arc<InMemoryRequestLogWriter>) -> StreamLogContext {
    let leg_records = vec![
        make_ok_leg(0, "Answer from leg 0"),
        make_ok_leg(1, "Answer from leg 1"),
    ];
    let panel = PanelStreamLog {
        leg_records,
        leg_cost_total: Some(0.010),
        strategy: ArbiterStrategyKind::Synthesize,
        quorum_required: 1,
        quorum_met: 2,
        arbiter_detail: ArbiterDetail::default(),
        // Live: the DropGuard uses the streamed usage as the arbiter cost.
        arbiter_cost_plan: ArbiterCostPlan::Live,
        arbiter_model: "panel-arbiter".to_string(),
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
        // 20 input tokens → nonzero input cost even when output_tokens == 0.
        input_tokens: 20,
        cached_tokens: 0,
        retrieval_tokens_saved: 0,
        pricing: Some(priced()),
        baseline_pricing: Some(priced()),
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
        panel: Some(Arc::new(panel)),
    }
}

/// §D5 truncation billing for streaming panels:
///
/// A Synthesize panel whose arbiter stream ends without a terminal
/// `finish_reason` chunk must write exactly ONE aggregate `request_logs` row:
///   - `provider == "panel"`            — panel sentinel, not the arbiter provider
///   - `truncated == true`              — no finish_reason observed
///   - `cached == false`                — panels never cache
///   - `cost_usd > leg_cost_total`      — partial-but-honest: leg costs + nonzero
///                                        arbiter input cost (20 input tokens * $5/M)
///
/// This pins the invariant that truncated streaming panels are billed honestly
/// (the partial estimate, not zero and not full) and do not suppress the row.
/// Run this test a few times to confirm the drop-ordering is deterministic
/// (poll-to-exhaustion via `drain_response` mirrors `sse_partial_cost.rs`'s
/// `sse_truncated_drop_writes_row_with_truncated_true`).
#[tokio::test]
async fn truncated_streaming_panel_writes_row_with_truncated_true() {
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let provider = Arc::new(MockEgressProvider) as Arc<dyn Provider>;

    // Content chunks with NO terminal finish_reason — simulates a client
    // disconnect mid-stream (mirrors the truncation pattern in sse_partial_cost.rs).
    let chunks: Vec<Result<ChatCompletionChunk, ProviderError>> =
        vec![Ok(content_chunk("partial")), Ok(content_chunk(" answer"))];
    let stream = futures::stream::iter(chunks).boxed();

    let response = stream_response(
        stream,
        &provider,
        Uuid::nil(),
        Some(panel_log_ctx_live(Arc::clone(&writer))),
    );

    // Poll the body to exhaustion so the GuardedStream / DropGuard drops,
    // firing the terminal billing write — mirroring drain_response in
    // sse_partial_cost.rs (poll-to-exhaustion pattern).
    drain_response(response).await;
    wait_for_rows(&writer, 1).await;

    let rows = writer.rows();
    assert_eq!(rows.len(), 1, "exactly one aggregate row expected");
    let row = &rows[0];

    // §D5 structural invariants.
    assert_eq!(
        row.provider, "panel",
        "aggregate row provider must be 'panel' for a streaming panel"
    );
    assert_eq!(
        row.model, "panel-arbiter",
        "aggregate row model must be the arbiter model"
    );
    assert!(
        row.truncated,
        "truncated must be true: no finish_reason chunk was sent"
    );
    assert!(
        !row.cached,
        "panel aggregate row must be cached == false (panels never cache)"
    );

    // Partial-but-honest cost: at minimum the leg floor ($0.010) plus nonzero
    // arbiter input cost (20 tokens * $5/M = $0.0001). The aggregate must
    // exceed the leg floor, proving the arbiter contribution is included.
    let leg_floor = 0.010_f64;
    assert!(
        row.cost_usd > leg_floor,
        "cost_usd ({}) must exceed leg_cost_total ({leg_floor}): \
         arbiter input cost (20 tokens * $5/M) must be added even on truncation",
        row.cost_usd
    );

    // Sanity upper bound: 20 input + at most ~50 output tokens at $5/$15/M
    // → well under $0.012.  Guards against a cost-doubling regression.
    let sanity_ceiling = 0.012_f64;
    assert!(
        row.cost_usd < sanity_ceiling,
        "cost_usd ({}) exceeded sanity ceiling ({sanity_ceiling}): \
         only 20 input tokens + a few partial output tokens expected",
        row.cost_usd
    );
}

// ---------------------------------------------------------------------------
// Task B: cost_incomplete=false for a normal Synthesize Live streaming panel
//
// Background: `panel_body_json` sets `cost_incomplete=true` when any surviving
// (status==Ok) *member* leg has `cost_usd == None` at the time the terminal
// `tokentrimmer.panel` SSE event is emitted.  The arbiter leg is explicitly
// excluded from this scan because:
//
//   1. Member-leg unpriceability is blocked by the fail-closed
//      `panel_budget_gate` (returns 402 if any member/arbiter is unpriceable),
//      which runs before any dispatch — so a streaming panel that reaches the
//      stream phase always has all member legs priced (covered by
//      `panel_budget.rs::panel_budget_gate_err_when_unpriceable`).
//
//   2. For Synthesize Live, the arbiter leg's cost_usd is None in the legs
//      slice at panel-event emit time (the Live cost is deferred to the
//      DropGuard at stream end).  If the arbiter leg were included in the scan,
//      cost_incomplete would incorrectly be set to `true` on every normal
//      Synthesize stream.  The fix (`l.role != LegRole::Arbiter`) excludes it.
//
// The test below pins that a normal all-member-priced Synthesize Live stream
// correctly reports cost_incomplete=false.  It FAILS before the fix (the
// arbiter's deferred None cost triggered cost_incomplete=true) and PASSES after.
// ---------------------------------------------------------------------------

/// Synthesize Live streaming panel: all member legs are priced → the terminal
/// `tokentrimmer.panel` SSE event must report `cost_incomplete == false`.
///
/// This pins the fix in `panel_body_json` that excludes the arbiter leg from
/// the `cost_incomplete` scan.  Without the fix this test would see `true`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn synthesize_streaming_panel_cost_incomplete_false() {
    let (app, _writer, _tracker, _calls) = app_synthesize_stream(false);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            serde_json::json!({
                "model": "model-a",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "cost_incomplete pin" }],
                "stream": true,
                "tt_extras": {
                    "panel": {
                        "members": ["model-a", "model-b"],
                        "arbiter_model": "model-synth"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "streaming synthesize panel must return 200"
    );

    let body = router_sse_body(resp).await;
    let events = parse_sse_events(&body);

    let panel_pos = events
        .iter()
        .position(|(t, _)| t == "tokentrimmer.panel")
        .expect("tokentrimmer.panel event must be present");
    let panel_json: serde_json::Value =
        serde_json::from_str(&events[panel_pos].1).expect("tokentrimmer.panel must be valid JSON");

    let cost_incomplete = panel_json["cost_incomplete"]
        .as_bool()
        .expect("tokentrimmer.panel must have a boolean cost_incomplete field");

    assert!(
        !cost_incomplete,
        "cost_incomplete must be false for a normal all-member-priced Synthesize Live stream; \
         got true (arbiter leg's deferred None cost was incorrectly included in the scan)"
    );
}

// ===========================================================================
// Task 6 — router-level end-to-end streaming-panel suite.
//
// These tests build the real `build_router` app (like `panel_engine.rs`) and
// POST `/v1/chat/completions` with `stream: true` + an `X-TokenTrimmer-Panel`
// header. They exercise `complete_panel_streaming` + the gate wire-up end to
// end: the SSE body, the deferred-to-DropGuard aggregate `request_logs` row,
// the fail-closed quorum-unmet path, and the off-by-default single-model
// regression.
// ===========================================================================

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::task::TaskTracker;
use tower::util::ServiceExt;
use tt_core::build_router;

// ---------------------------------------------------------------------------
// Router-level mock providers
// ---------------------------------------------------------------------------

/// A buffered member-leg provider serving one model with a FIXED distinct
/// answer text, so BestOfN's verbatim replay is observable. When `fail` is
/// set, `chat_completion` returns a 503 (used to manufacture quorum-unmet).
struct MemberMock {
    id: &'static str,
    model: &'static str,
    answer: &'static str,
    calls: Arc<AtomicUsize>,
    fail: bool,
}

#[async_trait]
impl Provider for MemberMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: 1.0,
                output_per_million: 2.0,
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "mock member failure".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-{}", self.id),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(self.answer.into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
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
        // Members are dispatched buffered; the stream path is unused here.
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

/// A judge provider (for BestOfN): `chat_completion` returns a candidate
/// number on the first line so the strategy picks a deterministic leg.
struct JudgeMock {
    id: &'static str,
    model: &'static str,
    /// First-line candidate number (1-indexed) + reason.
    pick: &'static str,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for JudgeMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: 1.0,
                output_per_million: 2.0,
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "chatcmpl-judge".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(self.pick.into())),
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

/// A streaming arbiter provider (for Synthesize): `chat_completion_stream`
/// emits a fixed chunk sequence then a clean finish-with-usage chunk.
struct StreamingArbiterMock {
    id: &'static str,
    model: &'static str,
    chunks: Vec<&'static str>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for StreamingArbiterMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Streaming],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        let model = self.model.to_string();
        let mut items: Vec<Result<ChatCompletionChunk, ProviderError>> = self
            .chunks
            .iter()
            .enumerate()
            .map(|(i, text)| {
                Ok(ChatCompletionChunk {
                    id: format!("chunk-{i}"),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: model.clone(),
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
        // Clean finish chunk with an authoritative usage block.
        items.push(Ok(ChatCompletionChunk {
            id: "chunk-fin".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model,
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: Some("stop".into()),
                extra: Default::default(),
            }],
            usage: Some(Usage {
                prompt_tokens: 30,
                completion_tokens: 40,
                total_tokens: 70,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            extra: Default::default(),
        }));
        Ok(futures::stream::iter(items).boxed())
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
// Router builders
// ---------------------------------------------------------------------------

/// Build a router whose member legs return DISTINCT answers and whose arbiter
/// STREAMS (Synthesize). Layout:
///   "vendor-a" → "model-a"  (member 1, answer "Alpha answer")
///   "vendor-b" → "model-b"  (member 2, answer "Beta answer")
///   "vendor-s" → "model-synth" (streaming arbiter)
fn app_synthesize_stream(
    fail_members: bool,
) -> (
    axum::Router,
    Arc<InMemoryRequestLogWriter>,
    TaskTracker,
    Arc<AtomicUsize>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MemberMock {
        id: "vendor-a",
        model: "model-a",
        answer: "Alpha answer",
        calls: Arc::clone(&calls),
        fail: fail_members,
    }));
    registry.register(Arc::new(MemberMock {
        id: "vendor-b",
        model: "model-b",
        answer: "Beta answer",
        calls: Arc::clone(&calls),
        fail: fail_members,
    }));
    registry.register(Arc::new(StreamingArbiterMock {
        id: "vendor-s",
        model: "model-synth",
        chunks: vec!["Synth", "esized"],
        calls: Arc::clone(&calls),
    }));
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let tracker = TaskTracker::new();
    let state = AppState::new(registry)
        .with_panel_enabled(true)
        .with_request_log_writer(writer.clone())
        .with_telemetry_tracker(tracker.clone());
    (build_router(state), writer, tracker, calls)
}

/// Build a router for the BestOfN replay path. Layout:
///   "vendor-a" → "model-a"  (member 1, answer "Alpha answer")
///   "vendor-b" → "model-b"  (member 2, answer "Beta answer — the best one")
///   "vendor-j" → "model-judge" (buffered judge, picks candidate 2 = leg 1)
fn app_best_of_n() -> (
    axum::Router,
    Arc<InMemoryRequestLogWriter>,
    TaskTracker,
    Arc<AtomicUsize>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MemberMock {
        id: "vendor-a",
        model: "model-a",
        answer: "Alpha answer",
        calls: Arc::clone(&calls),
        fail: false,
    }));
    registry.register(Arc::new(MemberMock {
        id: "vendor-b",
        model: "model-b",
        answer: "Beta answer — the best one",
        calls: Arc::clone(&calls),
        fail: false,
    }));
    registry.register(Arc::new(JudgeMock {
        id: "vendor-j",
        model: "model-judge",
        // Pick candidate 2 (1-indexed) → answers[1] → leg "model-b".
        pick: "2\nmost complete answer",
        calls: Arc::clone(&calls),
    }));
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let tracker = TaskTracker::new();
    let state = AppState::new(registry)
        .with_panel_enabled(true)
        .with_request_log_writer(writer.clone())
        .with_telemetry_tracker(tracker.clone());
    (build_router(state), writer, tracker, calls)
}

/// Build a single-model router (no panel members) for the off-by-default
/// streaming regression: a `stream:true` request with NO panel header must go
/// down `handle_streaming` unchanged.
fn app_single_model() -> (axum::Router, Arc<InMemoryRequestLogWriter>, TaskTracker) {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(StreamingArbiterMock {
        id: "vendor-solo",
        model: "model-solo",
        chunks: vec!["Hello", " world"],
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let tracker = TaskTracker::new();
    let state = AppState::new(registry)
        .with_panel_enabled(true)
        .with_request_log_writer(writer.clone())
        .with_telemetry_tracker(tracker.clone());
    (build_router(state), writer, tracker)
}

// ---------------------------------------------------------------------------
// Router-level SSE helpers
// ---------------------------------------------------------------------------

/// Drain all spawned telemetry writes, then return the captured rows.
async fn drain_rows_router(
    writer: &Arc<InMemoryRequestLogWriter>,
    tracker: TaskTracker,
) -> Vec<tt_telemetry::request_logs::RequestLogRow> {
    tracker.close();
    tracker.wait().await;
    writer.rows()
}

/// Read the SSE body of a router response into a single string.
async fn router_sse_body(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ===========================================================================
// Case 1 — Synthesize live ⇒ 200 text/event-stream; arbiter chunks then
// tokentrimmer.panel then tokentrimmer.usage then [DONE]; one provider='panel'
// row, cost = Σ legs + arbiter, cached=false.
// ===========================================================================

// Router-level SSE drains drive the gateway middleware stack (TimeoutLayer,
// keep-alive) — use a multi-thread runtime so the streamed body and the
// stream-end DropGuard's spawned telemetry writes both make progress while the
// test task awaits `to_bytes` (matches `concurrent_sse.rs`). A current-thread
// runtime deadlocks here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_e2e_synthesize_live_aggregate_row_and_event_order() {
    let (app, writer, tracker, _calls) = app_synthesize_stream(false);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            serde_json::json!({
                "model": "model-a",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": true,
                "tt_extras": {
                    "panel": {
                        "members": ["model-a", "model-b"],
                        "arbiter_model": "model-synth"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "streaming synthesize panel must return 200"
    );
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "content-type must be text/event-stream, got {ct}"
    );

    let body = router_sse_body(resp).await;
    let events = parse_sse_events(&body);
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

    // The arbiter's streamed chunk content must appear in the body (arbiter
    // chunks come before the terminal panel/usage events).
    assert!(
        body.contains("Synth") && body.contains("esized"),
        "arbiter streamed chunks must be present in the SSE body"
    );

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

    assert_eq!(
        panel_pos + 1,
        usage_pos,
        "tokentrimmer.panel must be immediately before tokentrimmer.usage"
    );
    assert!(
        usage_pos < done_pos,
        "tokentrimmer.usage must come before [DONE]"
    );

    // Exactly one aggregate request_logs row, provider == 'panel', cached=false.
    let rows = drain_rows_router(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        1,
        "streaming panel must write exactly ONE aggregate request_logs row"
    );
    let row = &rows[0];
    assert_eq!(
        row.provider, "panel",
        "aggregate row provider must be 'panel'"
    );
    assert_eq!(
        row.model, "model-synth",
        "aggregate row model must be the arbiter model"
    );
    assert!(!row.cached, "panel aggregate row must be cached == false");
    // Σ legs (2 priced @ 100in/50out) + arbiter (Live, streamed 30in/40out) > 0.
    assert!(
        row.cost_usd > 0.0,
        "aggregate cost must be Σ legs + arbiter > 0, got {}",
        row.cost_usd
    );
    // Panel rows claim no saving: baseline == cost.
    assert!(
        (row.baseline_cost_usd - row.cost_usd).abs() < 1e-9,
        "panel baseline must equal cost (no claimed saving)"
    );

    // The tokentrimmer.usage cost_usd matches the row cost (lock-step).
    let usage_json: serde_json::Value =
        serde_json::from_str(&events[usage_pos].1).expect("usage data is JSON");
    let usage_cost = usage_json["cost_usd"].as_f64().expect("numeric cost_usd");
    assert!(
        (usage_cost - row.cost_usd).abs() < 1e-9,
        "tokentrimmer.usage cost_usd ({usage_cost}) must equal request_logs cost_usd ({})",
        row.cost_usd
    );
}

// ===========================================================================
// Case 2 — BestOfN replay ⇒ streamed chunks reconstruct the chosen leg
// verbatim; aggregate cost = Σ legs + judge (NOT + replayed leg).
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_e2e_best_of_n_replays_chosen_leg_verbatim_no_double_count() {
    let (app, writer, tracker, _calls) = app_best_of_n();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "best-of-n")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            serde_json::json!({
                "model": "model-a",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "which is best?" }],
                "stream": true,
                "tt_extras": {
                    "panel": {
                        "members": ["model-a", "model-b"],
                        "arbiter_model": "model-judge"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "best-of-n stream must return 200"
    );

    let body = router_sse_body(resp).await;
    let events = parse_sse_events(&body);

    // Reconstruct the streamed assistant text from the content chunks (the
    // `data:` payloads BEFORE the terminal tokentrimmer.* events).
    let mut streamed = String::new();
    for (etype, data) in &events {
        if !etype.is_empty() || data == "[DONE]" {
            continue; // skip tokentrimmer.* + [DONE]
        }
        if let Ok(chunk) = serde_json::from_str::<serde_json::Value>(data) {
            if let Some(c) = chunk["choices"][0]["delta"]["content"].as_str() {
                streamed.push_str(c);
            }
        }
    }

    // Cross-check the stream against the tokentrimmer.panel event to verify
    // verbatim replay of the ACTUALLY chosen leg — order-independent, since
    // JoinSet completes legs in non-deterministic order.
    //
    // The known model→answer mapping for this test's mock providers:
    //   "model-a" → "Alpha answer"
    //   "model-b" → "Beta answer — the best one"
    let model_answers: std::collections::HashMap<&str, &str> = [
        ("model-a", "Alpha answer"),
        ("model-b", "Beta answer — the best one"),
    ]
    .into();

    // Parse the tokentrimmer.panel event to read chosen_leg + legs[].
    let panel_pos = events
        .iter()
        .position(|(t, _)| t == "tokentrimmer.panel")
        .expect("tokentrimmer.panel event must be present");
    let panel_json: serde_json::Value =
        serde_json::from_str(&events[panel_pos].1).expect("tokentrimmer.panel data must be JSON");

    // arbiter.chosen_leg is the leg_index the judge selected.
    let chosen_leg_index = panel_json["arbiter"]["chosen_leg"]
        .as_u64()
        .expect("tokentrimmer.panel must have arbiter.chosen_leg")
        as usize;

    // Find the leg in legs[] with that leg_index to learn the model.
    let chosen_model = panel_json["legs"]
        .as_array()
        .expect("tokentrimmer.panel must have legs array")
        .iter()
        .find(|l| l["leg_index"].as_u64() == Some(chosen_leg_index as u64))
        .and_then(|l| l["model"].as_str())
        .expect("chosen leg must appear in legs array with a model field");

    // Look up the expected answer for the chosen model.
    let expected_answer = model_answers
        .get(chosen_model)
        .unwrap_or_else(|| panic!("unknown chosen model {chosen_model:?} in test mapping"));

    // The reconstructed stream must exactly match the chosen leg's known text.
    assert_eq!(
        &streamed, expected_answer,
        "replayed stream must carry the chosen leg ({chosen_model:?}) text verbatim, got {streamed:?}"
    );

    // Additionally, the stream must be exactly one of the two known member answers
    // (proves it is a verbatim replay of some surviving leg, not a synthesis).
    assert!(
        model_answers.values().any(|&a| a == streamed),
        "streamed text must be verbatim one of the known member answers, got {streamed:?}"
    );

    // Aggregate cost = Σ legs + judge. The Known plan DISCARDS the replayed
    // (streamed) leg cost, so it is NOT counted twice.
    //
    //   each member leg: 100 in @ $1/M + 50 out @ $2/M = $0.0001 + $0.0001 = $0.0002
    //   two members:                                                      = $0.0004
    //   judge: 10 in @ $1/M + 10 out @ $2/M = $0.00001 + $0.00002         = $0.00003
    //   Σ legs + judge                                                    = $0.00043
    // The replayed leg's streamed tokens would add ANOTHER $0.0002 if double-
    // counted; the assertion below proves they are not.
    let rows = drain_rows_router(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        1,
        "best-of-n stream must write ONE aggregate row"
    );
    let row = &rows[0];
    assert_eq!(
        row.provider, "panel",
        "aggregate row provider must be 'panel'"
    );
    assert!(!row.cached, "panel aggregate row must be cached == false");
    let expected = 0.0004 + 0.00003;
    assert!(
        (row.cost_usd - expected).abs() < 1e-9,
        "cost must be Σ legs + judge = {expected}, got {} (double-counted replay?)",
        row.cost_usd
    );
}

// ===========================================================================
// Case 3 — quorum-unmet (all member legs error) ⇒ 502, ZERO request_logs
// rows, no stream (fail-closed before 200).
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_e2e_quorum_unmet_returns_502_zero_rows_no_stream() {
    let (app, writer, tracker, _calls) = app_synthesize_stream(true /* fail members */);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            serde_json::json!({
                "model": "model-a",
                "max_tokens": 64,
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": true,
                "tt_extras": {
                    "panel": {
                        "members": ["model-a", "model-b"],
                        "arbiter_model": "model-synth"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    // Fail-closed BEFORE any 200/stream: 502 BAD_GATEWAY, NOT text/event-stream.
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "quorum-unmet streaming panel must return 502 BAD_GATEWAY"
    );
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        !ct.starts_with("text/event-stream"),
        "a fail-closed quorum-unmet must NOT open an SSE stream, got content-type {ct}"
    );

    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["error"]["code"], "panel_quorum_unmet",
        "error code must be panel_quorum_unmet, got {body}"
    );

    // ZERO request_logs rows — no billing side effect, no stream-end DropGuard.
    let rows = drain_rows_router(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        0,
        "INVARIANT: a quorum-unmet streaming panel must write ZERO request_logs rows"
    );
}

// ===========================================================================
// Case 4 — off-by-default: stream:true with NO panel header ⇒ unchanged
// single-model stream (goes down handle_streaming, single-model row).
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_e2e_off_by_default_single_model_stream_unchanged() {
    let (app, writer, tracker) = app_single_model();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        // NO x-tokentrimmer-panel header.
        .body(Body::from(
            serde_json::json!({
                "model": "model-solo",
                "messages": [{ "role": "user", "content": "hello" }],
                "stream": true
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "single-model stream must return 200"
    );
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.starts_with("text/event-stream"),
        "content-type must be text/event-stream, got {ct}"
    );

    let body = router_sse_body(resp).await;
    let events = parse_sse_events(&body);
    let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();

    // The single-model stream carries the provider chunks and NO panel event.
    assert!(
        body.contains("Hello") && body.contains("world"),
        "single-model streamed chunks must be present"
    );
    assert!(
        !types.contains(&"tokentrimmer.panel"),
        "a non-panel stream must NOT emit a tokentrimmer.panel event (off-by-default)"
    );
    assert!(
        events.iter().any(|(_, d)| d == "[DONE]"),
        "[DONE] sentinel must be present"
    );

    // The single-model row is provider='vendor-solo' (NOT 'panel').
    let rows = drain_rows_router(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        1,
        "single-model stream must write exactly ONE request_logs row"
    );
    assert_eq!(
        rows[0].provider, "vendor-solo",
        "single-model row provider must be the served provider, not 'panel'"
    );
}
