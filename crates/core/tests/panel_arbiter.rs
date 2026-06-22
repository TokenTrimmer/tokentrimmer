//! Unit-level tests for the arbiter strategy layer of `routes::panel`.
//! Run with:
//!   cargo test -p tt-core --test panel_arbiter

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use uuid::Uuid;

use tt_core::{
    routes::panel::{
        cosine, strategy_for, surviving_answers, ArbiterDetail, ArbiterStrategy,
        ArbiterStrategyKind, BestOfN, LegResult, LegRole, LegStatus, ModelRef, PanelConfig,
        PanelDefaults,
    },
    ApiError, AppState, ProviderRegistry,
};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

// ---------------------------------------------------------------------------
// Helper: build a minimal PanelConfig for a given strategy
// ---------------------------------------------------------------------------

fn cfg_for(strategy: ArbiterStrategyKind) -> PanelConfig {
    PanelConfig::resolve(
        strategy,
        None,
        &PanelDefaults {
            members: vec![ModelRef {
                model: "gpt-4o".to_string(),
                provider: None,
            }],
            arbiter_model: ModelRef {
                model: "gpt-4o".to_string(),
                provider: None,
            },
        },
    )
    .expect("resolve should not fail with non-empty members")
}

// ---------------------------------------------------------------------------
// strategy_for tests
// ---------------------------------------------------------------------------

#[test]
// Asserts the Ok path only; the trait object cannot be downcast to verify the
// concrete type at runtime.
fn strategy_for_synthesize_returns_synthesize() {
    let cfg = cfg_for(ArbiterStrategyKind::Synthesize);
    let strategy = strategy_for(&cfg);
    assert!(
        strategy.is_ok(),
        "expected Ok(Synthesize), got Err: {:?}",
        strategy.err()
    );
}

#[test]
fn strategy_for_best_of_n_returns_ok() {
    let cfg = cfg_for(ArbiterStrategyKind::BestOfN);
    let result = strategy_for(&cfg);
    assert!(
        result.is_ok(),
        "expected Ok(BestOfN), got Err: {:?}",
        result.err()
    );
}

#[test]
fn strategy_for_majority_is_unsupported() {
    let cfg = cfg_for(ArbiterStrategyKind::Majority);
    let result = strategy_for(&cfg);
    match result {
        Err(ApiError::PanelStrategyUnsupported { ref strategy }) if strategy == "majority" => {}
        Err(e) => panic!("expected PanelStrategyUnsupported(majority), got Err({e:?})"),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

// ---------------------------------------------------------------------------
// cosine helper tests
// ---------------------------------------------------------------------------

#[test]
fn cosine_identical_is_one_orthogonal_zero_zeronorm_zero() {
    assert!(
        (cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6,
        "identical unit vectors should have cosine 1.0"
    );
    assert!(
        cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6,
        "orthogonal vectors should have cosine ~0.0"
    );
    assert_eq!(
        cosine(&[0.0, 0.0], &[1.0, 1.0]),
        0.0,
        "zero-norm vector should return 0.0"
    );
}

// ---------------------------------------------------------------------------
// ArbiterDetail default tests
// ---------------------------------------------------------------------------

#[test]
fn arbiter_detail_default_is_empty() {
    let d = ArbiterDetail::default();
    assert!(d.chosen_leg.is_none() && !d.fell_back && !d.no_majority);
}

// ---------------------------------------------------------------------------
// surviving_answers helper tests
// ---------------------------------------------------------------------------

fn make_leg(pos: usize, role: LegRole, status: LegStatus, text: Option<&str>) -> LegResult {
    let response = text.map(|t| ChatCompletionResponse {
        id: "test".to_string(),
        object: "chat.completion".to_string(),
        created: 0,
        model: "test-model".to_string(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text(t.to_string())),
                name: None,
                tool_calls: vec![],
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: Usage::default(),
    });
    LegResult {
        leg_index: pos,
        role,
        model: "test-model".to_string(),
        provider: "test-provider".to_string(),
        status,
        response,
        cost_usd: None,
        usage: None,
        latency_ms: 0,
    }
}

#[test]
fn surviving_answers_filters_non_ok_and_arbiter_legs() {
    let legs = vec![
        make_leg(0, LegRole::Leg, LegStatus::Ok, Some("answer A")),
        make_leg(
            1,
            LegRole::Leg,
            LegStatus::Error,
            Some("should be excluded"),
        ),
        make_leg(2, LegRole::Arbiter, LegStatus::Ok, Some("arbiter excluded")),
        make_leg(3, LegRole::Leg, LegStatus::Ok, Some("answer B")),
    ];
    let answers = surviving_answers(&legs);
    assert_eq!(answers.len(), 2, "only 2 ok member legs should survive");
    assert_eq!(answers[0].0, 0, "first surviving answer is at position 0");
    assert_eq!(answers[0].1, "answer A");
    assert_eq!(answers[1].0, 3, "second surviving answer is at position 3");
    assert_eq!(answers[1].1, "answer B");
}

// ---------------------------------------------------------------------------
// Mock provider for BestOfN arbiter tests
// ---------------------------------------------------------------------------

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
        Err(ProviderError::Unsupported("no".into()))
    }
}

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
        model: "mock-leg1".into(),
        messages: vec![Message::User {
            content: MessageContent::Text("hello".into()),
            name: None,
        }],
        stream: false,
        ..Default::default()
    }
}

/// Build a LegResult with an Ok status and a specific assistant text answer.
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

// ---------------------------------------------------------------------------
// BestOfN tests
// ---------------------------------------------------------------------------

/// Judge returns "2\nCandidate 2 is the most complete." — must pick leg at
/// position 1 in the legs slice (candidate 2 = answers[1]).
#[tokio::test]
async fn best_of_n_judge_picks_candidate_2() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockJudge {
        id: "mock-provider-judge",
        model: "mock-judge",
        judge_response: "2\nCandidate 2 is the most complete.",
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();

    // Build 3 ok member legs with distinct answers (leg_index matches slice pos).
    let legs = vec![
        make_ok_leg(0, "Answer from leg 0"),
        make_ok_leg(1, "Answer from leg 1 — the best one"),
        make_ok_leg(2, "Answer from leg 2"),
    ];

    let arbiter_model = ModelRef {
        model: "mock-judge".to_string(),
        provider: None,
    };
    let strategy = BestOfN { arbiter_model };

    let outcome = strategy
        .arbitrate(&base_req(), &legs, &state, &ctx, &creds)
        .await
        .expect("BestOfN should succeed");

    // The returned response must be the ORIGINAL leg 1 response text.
    let chosen_text = outcome
        .response
        .choices
        .first()
        .and_then(|c| match &c.message {
            Message::Assistant {
                content: Some(MessageContent::Text(t)),
                ..
            } => Some(t.clone()),
            _ => None,
        })
        .expect("chosen response must have assistant text");

    assert_eq!(
        chosen_text, "Answer from leg 1 — the best one",
        "returned text must be the original leg 1 answer"
    );
    assert_eq!(
        outcome.detail.chosen_leg,
        Some(1),
        "chosen_leg must be leg_index of leg 1"
    );
    assert!(
        outcome
            .detail
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("complete"),
        "reason should contain 'complete'"
    );
    assert!(
        !outcome.detail.fell_back,
        "fell_back must be false when judge response is valid"
    );
}

/// Judge returns garbage "banana" — must fall back to first surviving leg.
#[tokio::test]
async fn best_of_n_garbage_judge_falls_back_to_first_leg() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockJudge {
        id: "mock-provider-judge",
        model: "mock-judge",
        judge_response: "banana",
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();

    let legs = vec![
        make_ok_leg(0, "First answer"),
        make_ok_leg(1, "Second answer"),
        make_ok_leg(2, "Third answer"),
    ];

    let arbiter_model = ModelRef {
        model: "mock-judge".to_string(),
        provider: None,
    };
    let strategy = BestOfN { arbiter_model };

    let outcome = strategy
        .arbitrate(&base_req(), &legs, &state, &ctx, &creds)
        .await
        .expect("BestOfN should not error even on garbage judge output");

    assert!(
        outcome.detail.fell_back,
        "fell_back must be true when judge output is unparseable"
    );
    assert_eq!(
        outcome.detail.chosen_leg,
        Some(0),
        "must fall back to first surviving leg (leg_index 0)"
    );

    let chosen_text = outcome
        .response
        .choices
        .first()
        .and_then(|c| match &c.message {
            Message::Assistant {
                content: Some(MessageContent::Text(t)),
                ..
            } => Some(t.clone()),
            _ => None,
        })
        .expect("fallback response must have assistant text");

    assert_eq!(
        chosen_text, "First answer",
        "fallback must return the first surviving leg's original response"
    );
}
