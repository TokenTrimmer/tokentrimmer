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
        ArbiterCostPlan, ArbiterStrategy, BestOfN, LegResult, LegRole, LegStatus, ModelRef,
        Synthesize,
    },
    AppState, ProviderRegistry,
};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

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
