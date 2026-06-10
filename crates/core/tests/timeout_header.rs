//! `X-TokenTrimmer-Timeout-Ms` enforces a per-request deadline → 408.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, EmbeddingData, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

/// Sleeps `delay` before answering (so a short request timeout fires first).
struct SleepyProvider {
    delay: Duration,
}

#[async_trait]
impl Provider for SleepyProvider {
    fn id(&self) -> &'static str {
        "sleepy"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "slow".into(),
            provider: "sleepy".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
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
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        tokio::time::sleep(self.delay).await;
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
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _r: ChatCompletionRequest,
        _c: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        _c: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        tokio::time::sleep(self.delay).await;
        Ok(EmbeddingsResponse {
            object: "list".into(),
            data: vec![EmbeddingData {
                object: "embedding".into(),
                index: 0,
                embedding: vec![0.1, 0.2],
            }],
            model: req.model,
            usage: Usage {
                prompt_tokens: 3,
                completion_tokens: 0,
                total_tokens: 3,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
}

fn app(delay_ms: u64) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(SleepyProvider {
        delay: Duration::from_millis(delay_ms),
    }));
    build_router(AppState::new(registry))
}

fn chat(timeout_ms: Option<&str>) -> Request<Body> {
    let body =
        json!({ "model": "slow", "messages": [{"role":"user","content":"hi"}], "stream": false });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(t) = timeout_ms {
        b = b.header("x-tokentrimmer-timeout-ms", t);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn timeout_header_returns_408() {
    let resp = app(1_000) // provider would take 1s
        .oneshot(chat(Some("50"))) // but caller allows only 50ms
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
}

#[tokio::test]
async fn no_timeout_header_completes() {
    let resp = app(0).oneshot(chat(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn embeddings_timeout_returns_408() {
    let body = json!({ "model": "slow", "input": "hello" });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-timeout-ms", "50")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app(1_000).oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
}

#[tokio::test]
async fn timeout_increments_provider_timeouts_total() {
    let router = app(1_000); // provider sleeps 1s
    let resp = router.clone().oneshot(chat(Some("50"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);

    let m = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(m.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("provider_timeouts_total"),
        "provider_timeouts_total series missing:\n{text}"
    );
    assert!(
        text.contains("provider=\"sleepy\""),
        "provider label missing:\n{text}"
    );
    assert!(
        text.contains("operation=\"chat\""),
        "operation label missing:\n{text}"
    );
}
