//! Task 5: run_panel concurrent fan-out with quorum and cost aggregation.
//!
//! Run with:
//!   cargo test -p tt-core --test panel_fanout

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use uuid::Uuid;

use tt_core::{
    routes::panel::{
        admit_panel_request, run_panel, ArbiterStrategyKind, LegRole, LegStatus, ModelRef,
        PanelAdmission, PanelConfig,
    },
    ApiError, AppState, ProviderRegistry,
};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{Choice, ContentPart, ImageUrl, InputAudio, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

// ---------------------------------------------------------------------------
// Mock provider (mirrors cross_provider.rs)
// ---------------------------------------------------------------------------

struct Mock {
    id: &'static str,
    model: &'static str,
    input_price: f64,
    output_price: f64,
    fail: bool,
}

struct CapturingMediaMock {
    id: &'static str,
    model: &'static str,
    vision: bool,
    audio: bool,
    seen: Arc<Mutex<Vec<ChatCompletionRequest>>>,
}

#[async_trait]
impl Provider for CapturingMediaMock {
    fn id(&self) -> &'static str {
        self.id
    }

    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: std::iter::once(Capability::Text)
                .chain(self.vision.then_some(Capability::Vision))
                .chain(self.audio.then_some(Capability::Audio))
                .collect(),
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        (model == self.model).then(|| ModelPricing {
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
    }

    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.seen.lock().expect("capture lock").push(req.clone());
        Ok(ChatCompletionResponse {
            id: "chatcmpl-media".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("media answer".into())),
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

#[async_trait]
impl Provider for Mock {
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
                input_per_million: self.input_price,
                output_per_million: self.output_price,
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
        if self.fail {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "mock failure".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: "chatcmpl-mock".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model.clone(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("mock answer".into())),
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

fn base_req(model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.into(),
        messages: vec![Message::User {
            content: MessageContent::Text("hello".into()),
            name: None,
        }],
        stream: false,
        max_tokens: Some(100),
        ..Default::default()
    }
}

fn media_req(model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.into(),
        messages: vec![Message::User {
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "describe this".into(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url: "https://private.example/image.png?capability=fixture".into(),
                        detail: Some("auto".into()),
                        media_type: Some("image/png".into()),
                    },
                },
            ]),
            name: None,
        }],
        stream: false,
        max_tokens: Some(100),
        ..Default::default()
    }
}

fn audio_req(model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.into(),
        messages: vec![Message::User {
            content: MessageContent::Parts(vec![
                ContentPart::Text {
                    text: "transcribe this".into(),
                },
                ContentPart::InputAudio {
                    input_audio: InputAudio {
                        data: "UklGRiYAAABXQVZFZm10IBAAAAABAAEAgD4AAAB9AAACABAAZGF0YQIAAAAAAA=="
                            .into(),
                        format: "wav".into(),
                    },
                },
            ]),
            name: None,
        }],
        stream: false,
        max_tokens: Some(100),
        ..Default::default()
    }
}

/// Exercise the public engine through the same opaque admission proof that
/// production direct Rust callers must obtain before `run_panel` can fan out.
fn admission(state: &AppState, cfg: &PanelConfig, req: &ChatCompletionRequest) -> PanelAdmission {
    admit_panel_request(state, cfg, req, Some(999.0))
        .expect("priced test panel should pass its explicit admission budget")
}

#[test]
fn media_admission_requires_exact_vision_evidence_for_the_llm_arbiter() {
    let member_seen = Arc::new(Mutex::new(Vec::new()));
    let arbiter_seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CapturingMediaMock {
        id: "media-member-provider",
        model: "media-member",
        vision: true,
        audio: false,
        seen: member_seen,
    }));
    registry.register(Arc::new(CapturingMediaMock {
        id: "text-arbiter-provider",
        model: "text-arbiter",
        vision: false,
        audio: false,
        seen: arbiter_seen,
    }));
    let state = AppState::new(registry);
    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![ModelRef {
            model: "media-member".into(),
            provider: None,
        }],
        arbiter_model: ModelRef {
            model: "text-arbiter".into(),
            provider: None,
        },
        quorum: Some(1),
        max_cost_usd: None,
    };

    let error = match admit_panel_request(&state, &cfg, &media_req("media-member"), Some(999.0)) {
        Ok(_) => panic!("a text-only arbiter must not receive a private image reference"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("vision support for arbiter model"),
        "unexpected error: {error}"
    );
}

#[test]
fn audio_admission_requires_audio_not_vision_evidence_for_every_recipient() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CapturingMediaMock {
        id: "vision-only-provider",
        model: "vision-only-member",
        vision: true,
        audio: false,
        seen: seen.clone(),
    }));
    let state = AppState::new(registry);
    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Majority,
        members: vec![ModelRef {
            model: "vision-only-member".into(),
            provider: None,
        }],
        // Majority never sends the original media to its configured arbiter.
        arbiter_model: ModelRef {
            model: "configured-only-arbiter".into(),
            provider: None,
        },
        quorum: Some(1),
        max_cost_usd: None,
    };

    let error =
        match admit_panel_request(&state, &cfg, &audio_req("vision-only-member"), Some(999.0)) {
            Ok(_) => panic!("Vision alone must not admit an audio recipient"),
            Err(error) => error,
        };
    assert!(
        error.to_string().contains("audio support for member model"),
        "unexpected error: {error}"
    );
    assert!(seen.lock().expect("capture lock").is_empty());
}

#[test]
fn media_admission_rejects_malformed_image_inputs_before_fanout() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CapturingMediaMock {
        id: "media-provider",
        model: "media-model",
        vision: true,
        audio: false,
        seen: seen.clone(),
    }));
    let state = AppState::new(registry);
    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Majority,
        members: vec![ModelRef {
            model: "media-model".into(),
            provider: None,
        }],
        arbiter_model: ModelRef {
            model: "configured-only-arbiter".into(),
            provider: None,
        },
        quorum: Some(1),
        max_cost_usd: None,
    };
    let mut request = media_req("media-model");
    let Message::User {
        content: MessageContent::Parts(parts),
        ..
    } = &mut request.messages[0]
    else {
        panic!("media fixture must contain typed user parts");
    };
    let ContentPart::ImageUrl { image_url } = &mut parts[1] else {
        panic!("media fixture must contain an image part");
    };
    image_url.media_type = Some("image/svg+xml".into());

    let error = match admit_panel_request(&state, &cfg, &request, Some(999.0)) {
        Ok(_) => panic!("unsupported MIME hint must fail before panel admission"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("image media type must be image/jpeg"),
        "unexpected error: {error}"
    );
    assert!(seen.lock().expect("capture lock").is_empty());

    let Message::User {
        content: MessageContent::Parts(parts),
        ..
    } = &mut request.messages[0]
    else {
        panic!("media fixture must contain typed user parts");
    };
    let ContentPart::ImageUrl { image_url } = &mut parts[1] else {
        panic!("media fixture must contain an image part");
    };
    image_url.media_type = None;
    image_url.url = "data:image/png;base64,not-base64!".into();
    let error = match admit_panel_request(&state, &cfg, &request, Some(999.0)) {
        Ok(_) => panic!("malformed inline image must fail before panel admission"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("valid standard base64"),
        "unexpected error: {error}"
    );
    assert!(seen.lock().expect("capture lock").is_empty());
}

#[tokio::test]
async fn media_request_reaches_every_member_and_synthesis_arbiter_unchanged() {
    let member_seen = Arc::new(Mutex::new(Vec::new()));
    let arbiter_seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CapturingMediaMock {
        id: "media-member-provider",
        model: "media-member",
        vision: true,
        audio: false,
        seen: member_seen.clone(),
    }));
    registry.register(Arc::new(CapturingMediaMock {
        id: "media-arbiter-provider",
        model: "media-arbiter",
        vision: true,
        audio: false,
        seen: arbiter_seen.clone(),
    }));
    let state = AppState::new(registry);
    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![ModelRef {
            model: "media-member".into(),
            provider: None,
        }],
        arbiter_model: ModelRef {
            model: "media-arbiter".into(),
            provider: None,
        },
        quorum: Some(1),
        max_cost_usd: None,
    };
    let request = media_req("media-member");
    let admission = admit_panel_request(&state, &cfg, &request, Some(999.0))
        .expect("every actual media recipient has exact vision evidence");
    let creds = HashMap::from([
        (
            "media-member-provider".to_string(),
            test_creds("member-key"),
        ),
        (
            "media-arbiter-provider".to_string(),
            test_creds("arbiter-key"),
        ),
    ]);

    run_panel(
        &state,
        &test_ctx(),
        &request,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect("vision panel should complete");

    let member_requests = member_seen.lock().expect("member capture lock");
    let arbiter_requests = arbiter_seen.lock().expect("arbiter capture lock");
    assert_eq!(member_requests.len(), 1);
    assert_eq!(arbiter_requests.len(), 1);
    assert_eq!(
        serde_json::to_value(&member_requests[0].messages).unwrap(),
        serde_json::to_value(&request.messages).unwrap(),
    );
    assert_eq!(
        serde_json::to_value(&arbiter_requests[0].messages[..request.messages.len()]).unwrap(),
        serde_json::to_value(&request.messages).unwrap(),
        "arbiter must retain the caller's original typed media messages before its synthesis additions",
    );
}

// ---------------------------------------------------------------------------
// Test 1: both legs return → 3 legs (2 member + arbiter), summed cost, quorum_met==2
// ---------------------------------------------------------------------------

#[tokio::test]
async fn both_legs_return_three_legs_and_summed_cost() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "mock-provider-leg1",
        model: "mock-leg1",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));
    registry.register(Arc::new(Mock {
        id: "mock-provider-leg2",
        model: "mock-leg2",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));
    registry.register(Arc::new(Mock {
        id: "mock-provider-arbiter",
        model: "mock-arbiter",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();

    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-leg1".to_string(), test_creds("key1"));
    creds.insert("mock-provider-leg2".to_string(), test_creds("key2"));
    creds.insert("mock-provider-arbiter".to_string(), test_creds("arb-key"));

    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![
            ModelRef {
                model: "mock-leg1".to_string(),
                provider: None,
            },
            ModelRef {
                model: "mock-leg2".to_string(),
                provider: None,
            },
        ],
        arbiter_model: ModelRef {
            model: "mock-arbiter".to_string(),
            provider: None,
        },
        quorum: None,
        max_cost_usd: None,
    };
    let req = base_req("mock-leg1");
    let admission = admission(&state, &cfg, &req);

    let result = run_panel(
        &state,
        &ctx,
        &req,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect("run_panel should succeed");

    assert_eq!(result.legs.len(), 3, "2 member legs + 1 arbiter leg");
    assert_eq!(result.quorum_met, 2, "both members succeeded");
    assert!(result.total_cost_usd.is_some(), "cost should be computed");

    let total = result.total_cost_usd.unwrap();

    // Pin the exact expected cost.  The Mock uses input_price=5.0/M and
    // output_price=15.0/M with prompt_tokens=10 and completion_tokens=10.
    // Per leg: (10 * 5.0 + 10 * 15.0) / 1_000_000 = 0.0002.
    // 3 legs (2 member + 1 arbiter) × 0.0002 = 0.0006.
    let expected: f64 = 0.0006;
    assert!(
        (total - expected).abs() < 1e-9,
        "expected cost {expected}, got {total}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: all legs error → Err(PanelQuorumUnmet { required: 1, met: 0 })
// ---------------------------------------------------------------------------

#[tokio::test]
async fn all_legs_error_returns_quorum_unmet() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "mock-provider-fail",
        model: "mock-fail",
        input_price: 5.0,
        output_price: 15.0,
        fail: true,
    }));
    // Arbiter registered but not reached (quorum fails first)
    registry.register(Arc::new(Mock {
        id: "mock-provider-arbiter",
        model: "mock-arbiter",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();

    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-fail".to_string(), test_creds("key1"));
    creds.insert("mock-provider-arbiter".to_string(), test_creds("arb-key"));

    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![ModelRef {
            model: "mock-fail".to_string(),
            provider: None,
        }],
        arbiter_model: ModelRef {
            model: "mock-arbiter".to_string(),
            provider: None,
        },
        quorum: Some(1),
        max_cost_usd: None,
    };
    let req = base_req("mock-fail");
    let admission = admission(&state, &cfg, &req);

    let err = run_panel(
        &state,
        &ctx,
        &req,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect_err("should fail with quorum unmet");

    match err {
        ApiError::PanelQuorumUnmet { required, met } => {
            assert_eq!(required, 1);
            assert_eq!(met, 0);
        }
        other => panic!("expected PanelQuorumUnmet, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 3: member missing cred → SkippedNoCred, not dispatched, quorum still met
// ---------------------------------------------------------------------------

#[tokio::test]
async fn member_missing_cred_is_skipped() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "mock-provider-has-cred",
        model: "mock-has-cred",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));
    registry.register(Arc::new(Mock {
        id: "mock-provider-no-cred",
        model: "mock-no-cred",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));
    registry.register(Arc::new(Mock {
        id: "mock-provider-arbiter",
        model: "mock-arbiter",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();

    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-has-cred".to_string(), test_creds("key1"));
    // "mock-provider-no-cred" intentionally omitted
    creds.insert("mock-provider-arbiter".to_string(), test_creds("arb-key"));

    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![
            ModelRef {
                model: "mock-has-cred".to_string(),
                provider: None,
            },
            ModelRef {
                model: "mock-no-cred".to_string(),
                provider: None,
            },
        ],
        arbiter_model: ModelRef {
            model: "mock-arbiter".to_string(),
            provider: None,
        },
        quorum: Some(1), // only 1 survivor needed
        max_cost_usd: None,
    };
    let req = base_req("mock-has-cred");
    let admission = admission(&state, &cfg, &req);

    let result = run_panel(
        &state,
        &ctx,
        &req,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect("run_panel should succeed with 1 survivor");

    // 1 ok leg + 1 skipped leg + 1 arbiter leg = 3 total
    assert_eq!(result.legs.len(), 3, "1 ok + 1 skipped + 1 arbiter");
    assert_eq!(result.quorum_met, 1, "only 1 member succeeded");

    // The skipped leg must have SkippedNoCred status
    let skipped = result
        .legs
        .iter()
        .find(|l| l.role == LegRole::Leg && matches!(l.status, LegStatus::SkippedNoCred));
    assert!(skipped.is_some(), "expected a SkippedNoCred leg");
}

// ---------------------------------------------------------------------------
// MockPanic provider — chat_completion panics to exercise JoinSet JoinError
// ---------------------------------------------------------------------------

struct MockPanic {
    id: &'static str,
    model: &'static str,
}

#[async_trait]
impl Provider for MockPanic {
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
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        panic!("deliberate leg panic");
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

// ---------------------------------------------------------------------------
// Credential preflight: reject before any member/arbiter dispatch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_cross_provider_arbiter_credential_stops_before_any_dispatch() {
    // Both providers panic if called. A deterministic credential-preflight
    // error therefore proves the source context's key was neither sent to the
    // arbiter nor used to start the otherwise credentialed member leg.
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockPanic {
        id: "mock-provider-member-panic",
        model: "mock-member-panic",
    }));
    registry.register(Arc::new(MockPanic {
        id: "mock-provider-arbiter-panic",
        model: "mock-arbiter-panic",
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert(
        "mock-provider-member-panic".to_string(),
        test_creds("member-key"),
    );
    // Intentionally omit `mock-provider-arbiter-panic`: the unrelated
    // `ctx.credentials` must not become an arbiter fallback.

    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![ModelRef {
            model: "mock-member-panic".to_string(),
            provider: None,
        }],
        arbiter_model: ModelRef {
            model: "mock-arbiter-panic".to_string(),
            provider: None,
        },
        quorum: Some(1),
        max_cost_usd: None,
    };
    let req = base_req("mock-member-panic");
    let admission = admission(&state, &cfg, &req);

    let error = run_panel(
        &state,
        &ctx,
        &req,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect_err("missing arbiter credential must stop before fan-out");

    assert!(
        matches!(
            &error,
            ApiError::PanelCredentialPreflight {
                required: 1,
                credentialed: 1,
                missing_arbiter: true,
            }
        ),
        "expected missing-arbiter credential preflight error, got {error:?}"
    );
}

#[tokio::test]
async fn insufficient_credentialed_member_quorum_stops_before_any_dispatch() {
    // As above, a provider call would panic. The mapped arbiter isolates this
    // case to the member-quorum half of the preflight.
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockPanic {
        id: "mock-provider-member-one-panic",
        model: "mock-member-one-panic",
    }));
    registry.register(Arc::new(MockPanic {
        id: "mock-provider-member-two-panic",
        model: "mock-member-two-panic",
    }));
    registry.register(Arc::new(MockPanic {
        id: "mock-provider-arbiter-panic",
        model: "mock-arbiter-panic",
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();
    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert(
        "mock-provider-member-one-panic".to_string(),
        test_creds("member-one-key"),
    );
    creds.insert(
        "mock-provider-arbiter-panic".to_string(),
        test_creds("arbiter-key"),
    );

    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![
            ModelRef {
                model: "mock-member-one-panic".to_string(),
                provider: None,
            },
            ModelRef {
                model: "mock-member-two-panic".to_string(),
                provider: None,
            },
        ],
        arbiter_model: ModelRef {
            model: "mock-arbiter-panic".to_string(),
            provider: None,
        },
        quorum: Some(2),
        max_cost_usd: None,
    };
    let req = base_req("mock-member-one-panic");
    let admission = admission(&state, &cfg, &req);

    let error = run_panel(
        &state,
        &ctx,
        &req,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect_err("insufficient credentialed member quorum must stop before fan-out");

    assert!(
        matches!(
            &error,
            ApiError::PanelCredentialPreflight {
                required: 2,
                credentialed: 1,
                missing_arbiter: false,
            }
        ),
        "expected member-quorum credential preflight error, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: only-panicking member → quorum unmet (does not hang or propagate)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn panicked_only_leg_returns_quorum_unmet() {
    // tokio::task::JoinSet catches panics as JoinError — the test process does
    // NOT abort. run_panel must return Err(PanelQuorumUnmet) rather than
    // propagating the panic or hanging.
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockPanic {
        id: "mock-provider-panic",
        model: "mock-panic",
    }));
    registry.register(Arc::new(Mock {
        id: "mock-provider-arbiter",
        model: "mock-arbiter",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();

    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-panic".to_string(), test_creds("key1"));
    creds.insert("mock-provider-arbiter".to_string(), test_creds("arb-key"));

    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![ModelRef {
            model: "mock-panic".to_string(),
            provider: None,
        }],
        arbiter_model: ModelRef {
            model: "mock-arbiter".to_string(),
            provider: None,
        },
        quorum: Some(1),
        max_cost_usd: None,
    };
    let req = base_req("mock-panic");
    let admission = admission(&state, &cfg, &req);

    let err = run_panel(
        &state,
        &ctx,
        &req,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect_err("panicked-only leg must yield quorum unmet");

    match err {
        ApiError::PanelQuorumUnmet { required, met } => {
            assert_eq!(required, 1, "required quorum should be 1");
            assert_eq!(met, 0, "met quorum should be 0 (panic counts as error)");
        }
        other => panic!("expected PanelQuorumUnmet, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 5: one good leg + one panicking leg + arbiter → Error entry in legs,
//          quorum_met counts only the good leg
// ---------------------------------------------------------------------------

#[tokio::test]
async fn panicked_leg_recorded_as_error_good_leg_counts_for_quorum() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "mock-provider-good",
        model: "mock-good",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));
    registry.register(Arc::new(MockPanic {
        id: "mock-provider-panic",
        model: "mock-panic",
    }));
    registry.register(Arc::new(Mock {
        id: "mock-provider-arbiter",
        model: "mock-arbiter",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();

    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-good".to_string(), test_creds("key1"));
    creds.insert("mock-provider-panic".to_string(), test_creds("key2"));
    creds.insert("mock-provider-arbiter".to_string(), test_creds("arb-key"));

    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![
            ModelRef {
                model: "mock-good".to_string(),
                provider: None,
            },
            ModelRef {
                model: "mock-panic".to_string(),
                provider: None,
            },
        ],
        arbiter_model: ModelRef {
            model: "mock-arbiter".to_string(),
            provider: None,
        },
        quorum: Some(1), // 1 survivor is enough
        max_cost_usd: None,
    };
    let req = base_req("mock-good");
    let admission = admission(&state, &cfg, &req);

    let result = run_panel(
        &state,
        &ctx,
        &req,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect("run_panel should succeed — 1 good leg meets quorum=1");

    // 1 good member leg + 1 panicked (Error) member leg + 1 arbiter leg = 3 total
    assert_eq!(result.legs.len(), 3, "good + panicked + arbiter = 3 legs");
    assert_eq!(
        result.quorum_met, 1,
        "only the good leg counts toward quorum"
    );

    // There must be an Error leg in the member results (from the panic).
    let error_leg = result
        .legs
        .iter()
        .find(|l| l.role == LegRole::Leg && matches!(l.status, LegStatus::Error));
    assert!(
        error_leg.is_some(),
        "panicked leg must appear as LegStatus::Error, not be silently dropped"
    );
}

// ---------------------------------------------------------------------------
// MockSleepy provider — chat_completion sleeps briefly to allow latency measurement
// ---------------------------------------------------------------------------

struct MockSleepy {
    id: &'static str,
    model: &'static str,
    sleep_ms: u64,
    seen_keys: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for MockSleepy {
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
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.seen_keys
            .lock()
            .expect("arbiter credential recorder must not be poisoned")
            .push(ctx.credentials.api_key.expose().to_string());
        tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
        Ok(ChatCompletionResponse {
            id: "chatcmpl-sleepy".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model.clone(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("sleepy answer".into())),
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

// ---------------------------------------------------------------------------
// Test 6: arbiter cross-provider credentials and real latency (M1 + M3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn arbiter_cross_provider_creds_and_latency() {
    // Members are on one mock provider; arbiter is on a DIFFERENT mock provider
    // that sleeps 3 ms. The creds map includes the arbiter provider's credential.
    // We assert: (a) arbiter was dispatched (arbiter leg exists and succeeded),
    // and (b) arbiter_leg.latency_ms >= 1.

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(Mock {
        id: "mock-provider-member",
        model: "mock-member",
        input_price: 5.0,
        output_price: 15.0,
        fail: false,
    }));
    let arbiter_seen_keys = Arc::new(Mutex::new(Vec::new()));
    registry.register(Arc::new(MockSleepy {
        id: "mock-provider-arb-sleepy",
        model: "mock-arb-sleepy",
        sleep_ms: 3,
        seen_keys: Arc::clone(&arbiter_seen_keys),
    }));

    let state = AppState::new(registry);
    let ctx = test_ctx();

    let mut creds: HashMap<String, ProviderCredentials> = HashMap::new();
    creds.insert("mock-provider-member".to_string(), test_creds("member-key"));
    // Arbiter is on a DIFFERENT provider — include its credential explicitly.
    creds.insert(
        "mock-provider-arb-sleepy".to_string(),
        test_creds("arb-key"),
    );

    let cfg = PanelConfig {
        strategy: ArbiterStrategyKind::Synthesize,
        members: vec![ModelRef {
            model: "mock-member".to_string(),
            provider: None,
        }],
        arbiter_model: ModelRef {
            model: "mock-arb-sleepy".to_string(),
            provider: None,
        },
        quorum: Some(1),
        max_cost_usd: None,
    };
    let req = base_req("mock-member");
    let admission = admission(&state, &cfg, &req);

    let result = run_panel(
        &state,
        &ctx,
        &req,
        &creds,
        &cfg,
        &admission,
        Duration::from_secs(10),
    )
    .await
    .expect("run_panel should succeed with cross-provider arbiter cred");

    // (a) Arbiter leg was dispatched — verify it exists and succeeded.
    let arbiter_leg = result
        .legs
        .iter()
        .find(|l| l.role == LegRole::Arbiter)
        .expect("arbiter leg must be present");

    assert_eq!(
        arbiter_leg.status,
        LegStatus::Ok,
        "arbiter leg must succeed when its credential is in the creds map"
    );

    // (b) Real latency: the arbiter slept 3 ms, so latency_ms must be >= 1.
    assert!(
        arbiter_leg.latency_ms >= 1,
        "arbiter latency must be measured (got {}), not hardcoded 0",
        arbiter_leg.latency_ms
    );
    let seen_arbiter_keys = arbiter_seen_keys
        .lock()
        .expect("arbiter credential recorder must not be poisoned")
        .clone();
    assert_eq!(
        seen_arbiter_keys,
        vec!["arb-key".to_string()],
        "the cross-provider arbiter must receive only its mapped credential, never ctx's source key"
    );
}
