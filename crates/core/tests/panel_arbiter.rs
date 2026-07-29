//! Unit-level tests for the arbiter strategy layer of `routes::panel`.
//! Run with:
//!   cargo test -p tt-core --test panel_arbiter

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use uuid::Uuid;

use tt_cache::embed::{EmbedError, EmbeddingProvider};
use tt_core::{
    routes::panel::{
        cosine, strategy_for, surviving_answers, ArbiterDetail, ArbiterStrategy,
        ArbiterStrategyKind, BestOfN, LegResult, LegRole, LegStatus, Majority, ModelRef,
        PanelConfig, PanelDefaults,
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
fn strategy_for_majority_returns_ok() {
    let cfg = cfg_for(ArbiterStrategyKind::Majority);
    let result = strategy_for(&cfg);
    assert!(
        result.is_ok(),
        "expected Ok(Majority), got Err: {:?}",
        result.err()
    );
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
    seen_requests: Option<Arc<Mutex<Vec<ChatCompletionRequest>>>>,
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
        if let Some(seen_requests) = &self.seen_requests {
            seen_requests
                .lock()
                .expect("judge request capture lock")
                .push(req.clone());
        }
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
        run_id: None,
        node_id: None,
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

/// Judge returns candidate 2, which must map from the fresh blind ordering back
/// to the correct original leg rather than assuming member position 1.
#[tokio::test]
async fn best_of_n_judge_picks_candidate_2() {
    let mut registry = ProviderRegistry::new();
    let seen_requests = Arc::new(Mutex::new(Vec::new()));
    registry.register(Arc::new(MockJudge {
        id: "mock-provider-judge",
        model: "mock-judge",
        judge_response: "2\nCandidate 2 is the most complete.",
        seen_requests: Some(Arc::clone(&seen_requests)),
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-judge".to_string(), test_creds("judge-key"));

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

    let candidate_2_text = {
        let requests = seen_requests.lock().expect("judge request capture lock");
        let request = requests.first().expect("one judge request");
        request
            .messages
            .iter()
            .find_map(|message| match message {
                Message::User {
                    content: MessageContent::Text(text),
                    name: Some(name),
                } if name == "fusion_candidate_2" => {
                    let encoded = text.strip_prefix("UNTRUSTED_FUSION_CANDIDATE_DATA\n")?;
                    serde_json::from_str::<serde_json::Value>(encoded)
                        .ok()?
                        .get("content")?
                        .as_str()
                        .map(str::to_string)
                }
                _ => None,
            })
            .expect("candidate 2 untrusted-data envelope")
    };

    // The returned response must be the original response represented by the
    // request-local candidate 2 label.
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
        chosen_text, candidate_2_text,
        "returned text must match the original leg behind randomized candidate 2"
    );
    let expected_leg = legs
        .iter()
        .find(|leg| {
            leg.response
                .as_ref()
                .and_then(|response| response.choices.first())
                .is_some_and(|choice| {
                    matches!(
                        &choice.message,
                        Message::Assistant {
                            content: Some(MessageContent::Text(text)),
                            ..
                        } if text == &candidate_2_text
                    )
                })
        })
        .expect("candidate text must identify one original test leg");
    assert_eq!(
        outcome.detail.chosen_leg,
        Some(expected_leg.leg_index),
        "chosen_leg must map randomized candidate 2 back to its original leg"
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

/// Direct strategy callers must supply the arbiter's credential explicitly;
/// `ctx.credentials` is intentionally not a cross-provider fallback.
#[tokio::test]
async fn best_of_n_requires_explicit_arbiter_credential() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockJudge {
        id: "mock-provider-judge",
        model: "mock-judge",
        judge_response: "1",
        seen_requests: None,
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();
    let legs = vec![
        make_ok_leg(0, "Answer from leg 0"),
        make_ok_leg(1, "Answer from leg 1"),
    ];
    let strategy = BestOfN {
        arbiter_model: ModelRef {
            model: "mock-judge".to_string(),
            provider: None,
        },
    };

    let error = match strategy
        .arbitrate(&base_req(), &legs, &state, &ctx, &creds)
        .await
    {
        Ok(_) => panic!("an unmapped arbiter must not inherit the source credential"),
        Err(error) => error,
    };

    assert!(
        matches!(
            &error,
            ApiError::MissingProviderCredential { provider } if provider == "mock-provider-judge"
        ),
        "expected explicit missing-arbiter credential error, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Mock EmbeddingProvider for Majority tests
// ---------------------------------------------------------------------------

/// A deterministic embedding mock that maps specific texts to fixed vectors.
/// Any text not in the map returns an error (configurable via `fail_unknown`).
struct MapEmbedder {
    /// Text → embedding vector mapping.
    map: HashMap<&'static str, Vec<f32>>,
    /// When `true`, any text not found in `map` returns `EmbedError::EmptyInput`.
    fail_unknown: bool,
}

impl MapEmbedder {
    fn new(entries: Vec<(&'static str, Vec<f32>)>) -> Self {
        Self {
            map: entries.into_iter().collect(),
            fail_unknown: false,
        }
    }

    fn always_failing() -> Self {
        Self {
            map: HashMap::new(),
            fail_unknown: true,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for MapEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if let Some(v) = self.map.get(text) {
            Ok(v.clone())
        } else if self.fail_unknown {
            Err(EmbedError::EmptyInput)
        } else {
            // Return zero vector for unknown texts (deterministic fallback).
            Err(EmbedError::EmptyInput)
        }
    }

    fn model(&self) -> &str {
        "map-embedder-v1"
    }
}

// ---------------------------------------------------------------------------
// Majority tests
// ---------------------------------------------------------------------------

/// Build an AppState with L2 wired to the given embedder (no real L2 cache
/// needed for Majority — only the embedder field is consulted).
fn state_with_embedder(embedder: impl EmbeddingProvider + 'static) -> AppState {
    use tt_cache::InMemoryL2Cache;

    let registry = ProviderRegistry::new();
    let l2_cache = Arc::new(InMemoryL2Cache::new());
    let arc_embedder: Arc<dyn EmbeddingProvider> = Arc::new(embedder);
    AppState::new(registry).with_l2(l2_cache, arc_embedder, None)
}

/// 4 legs: A,B,C embed near-identically (cosine ≥ 0.83 to each other),
/// D is orthogonal. The winning cluster must be {A,B,C}, size == 3,
/// and the returned response text is one of "Answer A", "Answer B", "Answer C".
#[tokio::test]
async fn majority_cluster_of_three_wins() {
    // Vectors: A,B,C are clustered; D is orthogonal.
    // cosine([1.0,0.0,0.0],[0.99,0.01,0.0]) ≈ 0.9999
    // cosine([1.0,0.0,0.0],[0.98,0.0,0.02]) ≈ 0.9998
    // cosine([0.99,0.01,0.0],[0.98,0.0,0.02]) ≈ 0.9999
    // All well above 0.83 threshold → same cluster.
    let embedder = MapEmbedder::new(vec![
        ("Answer A", vec![1.0_f32, 0.0, 0.0]),
        ("Answer B", vec![0.99_f32, 0.01, 0.0]),
        ("Answer C", vec![0.98_f32, 0.0, 0.02]),
        ("Answer D", vec![0.0_f32, 0.0, 1.0]),
    ]);

    let state = state_with_embedder(embedder);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();

    let legs = vec![
        make_ok_leg(0, "Answer A"),
        make_ok_leg(1, "Answer B"),
        make_ok_leg(2, "Answer C"),
        make_ok_leg(3, "Answer D"),
    ];

    let outcome = Majority
        .arbitrate(&base_req(), &legs, &state, &ctx, &creds)
        .await
        .expect("Majority should succeed with a 3-cluster winner");

    assert_eq!(
        outcome.detail.winning_cluster_size,
        Some(3),
        "winning cluster should have 3 members (A, B, C)"
    );
    assert!(
        !outcome.detail.no_majority,
        "no_majority must be false when there is a cluster of 3"
    );
    assert!(!outcome.detail.degraded, "degraded must be false");

    let returned_text = outcome
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
        .expect("Majority must return a response with assistant text");

    assert!(
        returned_text == "Answer A" || returned_text == "Answer B" || returned_text == "Answer C",
        "returned text must be from the winning cluster (A, B, or C), got: {returned_text}"
    );
}

/// 3 mutually-orthogonal vectors: no two have cosine ≥ 0.83.
/// All clusters have size 1 → no_majority == true.
/// The returned response must be a real leg (global medoid, but any is fine
/// since all sims are zero).
#[tokio::test]
async fn majority_no_majority_with_orthogonal_vectors() {
    let embedder = MapEmbedder::new(vec![
        ("Answer X", vec![1.0_f32, 0.0, 0.0]),
        ("Answer Y", vec![0.0_f32, 1.0, 0.0]),
        ("Answer Z", vec![0.0_f32, 0.0, 1.0]),
    ]);

    let state = state_with_embedder(embedder);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();

    let legs = vec![
        make_ok_leg(0, "Answer X"),
        make_ok_leg(1, "Answer Y"),
        make_ok_leg(2, "Answer Z"),
    ];

    let outcome = Majority
        .arbitrate(&base_req(), &legs, &state, &ctx, &creds)
        .await
        .expect("Majority should succeed even with no clear majority");

    assert!(
        outcome.detail.no_majority,
        "no_majority must be true when all vectors are orthogonal"
    );
    assert_eq!(
        outcome.detail.winning_cluster_size,
        Some(1),
        "winning cluster size must be 1 (all distinct)"
    );
    assert!(!outcome.detail.degraded, "degraded must be false");

    // Must return one of the real legs.
    let returned_text = outcome
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
        .expect("Majority must return a response with assistant text even on no-majority");

    assert!(
        returned_text == "Answer X" || returned_text == "Answer Y" || returned_text == "Answer Z",
        "returned text must be one of the legs, got: {returned_text}"
    );
}

/// When the embedder returns an error, Majority must fall back to the first
/// leg and set `detail.degraded == true`.
#[tokio::test]
async fn majority_embed_error_falls_back_degraded() {
    let embedder = MapEmbedder::always_failing();
    let state = state_with_embedder(embedder);
    let ctx = test_ctx();
    let creds: HashMap<String, ProviderCredentials> = HashMap::new();

    let legs = vec![
        make_ok_leg(0, "First answer"),
        make_ok_leg(1, "Second answer"),
        make_ok_leg(2, "Third answer"),
    ];

    let outcome = Majority
        .arbitrate(&base_req(), &legs, &state, &ctx, &creds)
        .await
        .expect("Majority should not error on embed failure — it should degrade");

    assert!(
        outcome.detail.degraded,
        "degraded must be true when embedding fails"
    );

    let returned_text = outcome
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
        .expect("Majority must return a response even on embed failure");

    assert_eq!(
        returned_text, "First answer",
        "must fall back to first surviving leg on embed error"
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
        seen_requests: None,
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-judge".to_string(), test_creds("judge-key"));

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
