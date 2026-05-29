//! Registry model passthrough: a valid-but-unlisted model whose provider can
//! be inferred from its name should still dispatch (not 404), while
//! truly-unknown models — and inferrable models whose provider isn't
//! registered — return 404.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

/// Provider registered under id "openai" that lists only "gpt-4o" but will
/// serve whatever model it is dispatched — so we can prove that a model
/// absent from `models()` still dispatches via provider inference.
struct OpenAiLike;

#[async_trait]
impl Provider for OpenAiLike {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "gpt-4o".into(),
            provider: "openai".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        None
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse {
            id: "chatcmpl-x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("ok".into())),
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

fn chat_request(model: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn app_with_openai() -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(OpenAiLike));
    build_router(AppState::new(registry))
}

#[tokio::test]
async fn unlisted_but_inferrable_model_dispatches() {
    // "gpt-5.9-unlisted" is not in models() but infer_provider() maps gpt-* to
    // the registered "openai" provider, so the request should dispatch (200)
    // rather than 404.
    let resp = app_with_openai()
        .oneshot(chat_request("gpt-5.9-unlisted"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn truly_unknown_model_still_404s() {
    let resp = app_with_openai()
        .oneshot(chat_request("totally-unknown-xyz-9000"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn inferrable_but_unregistered_provider_404s() {
    // claude-* infers "anthropic", which is NOT registered here → 404.
    let resp = app_with_openai()
        .oneshot(chat_request("claude-something-new"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
