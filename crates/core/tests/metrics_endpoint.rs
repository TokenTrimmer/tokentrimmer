//! Integration tests for the `/metrics` Prometheus endpoint.
//!
//! The global recorder is shared across this test binary, so assertions check
//! metric/label PRESENCE, not exact counts.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use tower::util::ServiceExt;
use tt_core::{build_router, AppState, ProviderRegistry};

fn router() -> axum::Router {
    build_router(AppState::new(ProviderRegistry::new()))
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn metrics_endpoint_serves_prometheus_text() {
    let (status, headers, body) = get(router(), "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/plain; version=0.0.4; charset=utf-8"
    );
    assert!(body.contains("tt_build_info"), "build_info missing: {body}");
    assert!(
        body.contains("process_uptime_seconds"),
        "uptime missing: {body}"
    );
}

#[tokio::test]
async fn render_is_some_after_build_router() {
    let _ = router();
    assert!(tt_core::metrics::render().is_some());
}

#[tokio::test]
async fn http_request_metrics_recorded_for_health() {
    let app = router();
    // Drive one request through the stack so the latency middleware records it.
    let (s, _h, _b) = get(app.clone(), "/health").await;
    assert_eq!(s, StatusCode::OK);
    // Now scrape.
    let (_s, _h, body) = get(app, "/metrics").await;
    assert!(
        body.contains("http_requests_total"),
        "http_requests_total missing: {body}"
    );
    assert!(
        body.contains("http_request_duration_seconds"),
        "duration histogram missing: {body}"
    );
    assert!(
        body.contains("endpoint=\"/health\""),
        "matched-path label missing: {body}"
    );
}

mod nopricing {
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use tt_shared::messages::{Choice, Message, MessageContent};
    use tt_shared::pricing::Capability;
    use tt_shared::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
        EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
        Usage,
    };

    pub struct NoPricingEcho;

    #[async_trait]
    impl Provider for NoPricingEcho {
        fn id(&self) -> &'static str {
            "nopricing"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "np-1".into(),
                provider: "nopricing".into(),
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
                id: "chatcmpl-np".into(),
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
                    cache_read_input_tokens: None,
                },
            })
        }
        async fn chat_completion_stream(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            Err(ProviderError::Unsupported("n/a".into()))
        }
        async fn embeddings(
            &self,
            _req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Err(ProviderError::Unsupported("n/a".into()))
        }
    }
}

mod cachereporting {
    use async_trait::async_trait;
    use chrono::Utc;
    use futures::stream::BoxStream;
    use tt_shared::messages::{Choice, Message, MessageContent};
    use tt_shared::pricing::Capability;
    use tt_shared::{
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
        EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
        Usage,
    };

    /// A provider whose usage block reports provider prompt-cache reads/writes.
    pub struct CacheReportingEcho;

    #[async_trait]
    impl Provider for CacheReportingEcho {
        fn id(&self) -> &'static str {
            "cachereporting"
        }
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "cr-1".into(),
                provider: "cachereporting".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            }]
        }
        fn pricing(&self, _: &str) -> Option<ModelPricing> {
            Some(ModelPricing {
                input_per_million: 3.0,
                output_per_million: 6.0,
                cached_input_per_million: Some(0.3),
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
            Ok(ChatCompletionResponse {
                id: "chatcmpl-cr".into(),
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
                    prompt_tokens: 100,
                    completion_tokens: 10,
                    total_tokens: 110,
                    cached_tokens: 80,
                    cache_creation_input_tokens: Some(20),
                    cache_read_input_tokens: Some(80),
                },
            })
        }
        async fn chat_completion_stream(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            Err(ProviderError::Unsupported("n/a".into()))
        }
        async fn embeddings(
            &self,
            _req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Err(ProviderError::Unsupported("n/a".into()))
        }
    }
}

/// A dispatch whose provider reports prompt-cache reads/writes surfaces the
/// per-route provider-cache counters on /metrics (presence-only — the
/// recorder is shared across this test binary).
#[tokio::test]
async fn provider_cache_usage_counters_recorded() {
    use std::sync::Arc;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(cachereporting::CacheReportingEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({
        "model": "cr-1",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    assert!(
        metrics_body.contains("provider_cache_read_tokens_total"),
        "cache read counter missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("provider_cache_write_tokens_total"),
        "cache write counter missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("provider_cache_requests_total"),
        "cache requests counter missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("result=\"hit\""),
        "hit result label missing: {metrics_body}"
    );
}

#[tokio::test]
async fn provider_and_catalog_metrics_recorded() {
    use std::sync::Arc;
    use tt_auth::ApiKeyContext;
    use uuid::Uuid;

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(nopricing::NoPricingEcho));
    let app = build_router(AppState::new(registry));

    let body = serde_json::json!({
        "model": "np-1",
        "messages": [{ "role": "user", "content": "hi" }]
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut().insert(ApiKeyContext {
        key_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        tier: None,
    });
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (_s, _h, metrics_body) = get(app, "/metrics").await;
    assert!(
        metrics_body.contains("provider_request_duration_seconds"),
        "provider latency missing: {metrics_body}"
    );
    assert!(
        metrics_body.contains("catalog_zero_price_total"),
        "catalog miss missing: {metrics_body}"
    );
}
