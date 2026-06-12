//! `X-TokenTrimmer-Warnings: param_dropped:*` is emitted for params the routed
//! provider drops, and absent when nothing is dropped.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};

/// Mock that drops `n` and `seed` when present (mimics Anthropic-style drops).
struct WarnMock;

#[async_trait]
impl Provider for WarnMock {
    fn id(&self) -> &'static str {
        "warnmock"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "wm-1".into(),
            provider: "warnmock".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.1,
            output_per_million: 0.1,
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
    fn dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String> {
        let mut v = Vec::new();
        if req.n.is_some() {
            v.push("n".to_string());
        }
        if req.seed.is_some() {
            v.push("seed".to_string());
        }
        // Mimic a non-OpenAI provider that cannot honor the newer typed compat
        // fields (e.g. Anthropic/Gemini for reasoning_effort).
        if req.reasoning_effort.is_some() {
            v.push("reasoning_effort".to_string());
        }
        if req.parallel_tool_calls.is_some() {
            v.push("parallel_tool_calls".to_string());
        }
        v
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("hi".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
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
        Err(ProviderError::Unsupported("no stream in test".into()))
    }
}

fn app() -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(WarnMock));
    build_router(AppState::new(registry))
}

/// Mock whose `temperature` drop is MODEL-dependent (mimics a reasoning model):
/// it only drops `temperature` when evaluating its own served model. Used to
/// prove the gateway evaluates drops against the *served* model under failover.
struct ReasoningMock {
    id: &'static str,
    model: &'static str,
    fails: bool,
}

#[async_trait]
impl Provider for ReasoningMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.1,
            output_per_million: 0.1,
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
    fn dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String> {
        // Only drops temperature for THIS provider's reasoning model.
        if req.model == self.model && req.temperature.is_some() {
            vec!["temperature".to_string()]
        } else {
            Vec::new()
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        if self.fails {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "down".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("hi".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
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
        Err(ProviderError::Unsupported("no stream in test".into()))
    }
}

/// Under cross-model failover, the warning must be computed against the SERVED
/// model. Primary `m-plain` (no temp drop) fails → backup serves `m-reason`
/// (drops temperature). The originally-requested model is `m-plain`, so a
/// req.model-based computation would MISS the drop (false negative); a
/// served-model computation reports it.
#[tokio::test]
async fn warnings_use_served_model_under_failover() {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(ReasoningMock {
        id: "plain",
        model: "m-plain",
        fails: true,
    }));
    registry.register(Arc::new(ReasoningMock {
        id: "reason",
        model: "m-reason",
        fails: false,
    }));
    let app = build_router(AppState::new(registry));

    let body = json!({
        "model": "m-plain",
        "messages": [{"role":"user","content":"hi"}],
        "temperature": 0.5
    })
    .to_string();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                // Fallback chain: m-plain (fails) → m-reason (serves, drops temp).
                .header("x-tokentrimmer-fallback", "m-reason")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Served model is m-reason → temperature drop must be reported.
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(warn.contains("param_dropped:temperature"), "got: {warn}");
}

#[tokio::test]
async fn warnings_header_lists_dropped_params() {
    let body = json!({
        "model": "wm-1",
        "messages": [{"role":"user","content":"hi"}],
        "n": 2,
        "seed": 7
    })
    .to_string();
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(warn.contains("param_dropped:n"), "got: {warn}");
    assert!(warn.contains("param_dropped:seed"), "got: {warn}");
}

#[tokio::test]
async fn warnings_header_reports_unsupported_typed_compat_field() {
    // A newer typed field (`reasoning_effort`) the routed provider can't honor
    // must surface on the warnings header rather than vanishing silently.
    let body = json!({
        "model": "wm-1",
        "messages": [{"role":"user","content":"hi"}],
        "reasoning_effort": "high"
    })
    .to_string();
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        warn.contains("param_dropped:reasoning_effort"),
        "got: {warn}"
    );
}

#[tokio::test]
async fn no_warnings_header_when_nothing_dropped() {
    let body = json!({
        "model": "wm-1",
        "messages": [{"role":"user","content":"hi"}]
    })
    .to_string();
    let resp = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-tokentrimmer-warnings").is_none());
}
