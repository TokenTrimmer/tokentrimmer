//! End-to-end: a transient provider error is retried so the request still
//! succeeds (200), and a persistent error exhausts the bounded attempts —
//! proving the retry layer is wired into gateway dispatch.

use std::sync::atomic::{AtomicU32, Ordering};
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

/// Fails the first `fail_first` `chat_completion` calls with a retriable
/// `Timeout`, then succeeds.
struct FlakyProvider {
    calls: Arc<AtomicU32>,
    fail_first: u32,
}

#[async_trait]
impl Provider for FlakyProvider {
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
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_first {
            return Err(ProviderError::Timeout { ms: 1 });
        }
        Ok(ChatCompletionResponse {
            id: "x".into(),
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

fn chat_request() -> Request<Body> {
    let body = json!({
        "model": "gpt-4o",
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

fn app_with(provider: FlakyProvider) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(provider));
    build_router(AppState::new(registry))
}

#[tokio::test]
async fn transient_error_is_retried_to_success() {
    let calls = Arc::new(AtomicU32::new(0));
    let app = app_with(FlakyProvider {
        calls: Arc::clone(&calls),
        fail_first: 1,
    });
    let resp = app.oneshot(chat_request()).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a transient error should be retried to success"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "expected one retry (fail then success)"
    );
}

#[tokio::test]
async fn persistent_error_exhausts_bounded_attempts() {
    let calls = Arc::new(AtomicU32::new(0));
    let app = app_with(FlakyProvider {
        calls: Arc::clone(&calls),
        fail_first: 99,
    });
    let resp = app.oneshot(chat_request()).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "persistent error should fail"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "default policy attempts 3 times then gives up"
    );
}
