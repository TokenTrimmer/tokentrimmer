//! Gateway HTTP server.
//!
//! Composes the public router from per-endpoint handler modules under `routes/`.
//! Middleware (auth, telemetry, cache, routing) is layered in subsequent
//! Week-1+ iterations and intentionally NOT wired here yet — keep the skeleton
//! testable end-to-end first.

use axum::http::StatusCode;
use axum::{
    routing::{get, post},
    Router,
};
use tower_http::{
    cors::{Any, CorsLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{middleware, routes, AppState};

/// Build the public router. Returns a fully composed `Router` ready to bind.
///
/// Retrieval middleware is activated when `TT_RETRIEVAL_STORE` and
/// `TT_OPENAI_EMBED_KEY` are set in the process environment.
pub fn build_router(state: AppState) -> Router {
    build_router_with_retrieval(state, middleware::retrieval::build_retrieval_state())
}

/// Internal constructor that accepts an explicit (possibly `None`) retrieval
/// state. Used in integration tests to inject a pre-built `RetrievalState`
/// without relying on process-level env vars.
pub fn build_router_with_retrieval(
    state: AppState,
    retrieval: Option<middleware::retrieval::RetrievalState>,
) -> Router {
    let base = Router::new()
        .route("/health", get(routes::health::handler))
        .route("/v1/models", get(routes::models::handler))
        .route("/v1/chat/completions", post(routes::chat::handler))
        .route("/v1/embeddings", post(routes::embeddings::handler))
        .route(
            "/v1/preview",
            axum::routing::post(crate::routes::preview::post_preview),
        );

    let base = match retrieval {
        Some(rs) => base.layer(axum::middleware::from_fn_with_state(
            rs,
            middleware::retrieval::maybe_substitute,
        )),
        None => base.layer(axum::middleware::from_fn(
            middleware::retrieval::maybe_substitute_disabled,
        )),
    };

    base.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::auth::middleware,
    ))
    .layer(axum::middleware::from_fn(middleware::trace::middleware))
    .layer(TraceLayer::new_for_http())
    .layer(TimeoutLayer::with_status_code(
        StatusCode::GATEWAY_TIMEOUT,
        std::time::Duration::from_secs(600),
    ))
    .layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any),
    )
    .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderRegistry;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    // ── Mock provider ──────────────────────────────────────────────────────────

    use async_trait::async_trait;
    use futures::stream::StreamExt;
    use std::sync::Arc;
    use tt_shared::{
        messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
        pricing::Capability,
        ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
        EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
        Usage,
    };

    /// In-memory mock provider for dispatch tests.
    /// Supports `mock-model-1` (non-streaming) and `mock-streaming` (streaming).
    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            "mock"
        }

        fn models(&self) -> Vec<ModelInfo> {
            vec![
                ModelInfo {
                    id: "mock-model-1".into(),
                    provider: "mock".into(),
                    capabilities: vec![Capability::Text],
                    max_input_tokens: 4096,
                    max_output_tokens: 4096,
                },
                ModelInfo {
                    id: "mock-streaming".into(),
                    provider: "mock".into(),
                    capabilities: vec![Capability::Streaming],
                    max_input_tokens: 4096,
                    max_output_tokens: 4096,
                },
                ModelInfo {
                    id: "mock-error".into(),
                    provider: "mock".into(),
                    capabilities: vec![Capability::Text],
                    max_input_tokens: 4096,
                    max_output_tokens: 4096,
                },
            ]
        }

        fn pricing(&self, _model: &str) -> Option<ModelPricing> {
            Some(ModelPricing {
                input_per_million: 1.0,
                output_per_million: 2.0,
                cached_input_per_million: Some(0.1),
                effective_at: chrono::Utc::now(),
            })
        }

        async fn chat_completion(
            &self,
            req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<ChatCompletionResponse, ProviderError> {
            if req.model == "mock-error" {
                return Err(ProviderError::ProviderUpstream {
                    status: 500,
                    message: "upstream failure".into(),
                });
            }
            Ok(ChatCompletionResponse {
                id: "chatcmpl-mock-1".into(),
                object: "chat.completion".into(),
                created: 0,
                model: req.model,
                choices: vec![Choice {
                    index: 0,
                    message: Message::Assistant {
                        content: Some(MessageContent::Text("Hello!".into())),
                        tool_calls: vec![],
                        name: None,
                    },
                    finish_reason: Some("stop".into()),
                }],
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cached_tokens: 20,
                    cache_creation_input_tokens: None,
                },
            })
        }

        async fn chat_completion_stream(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<
            futures::stream::BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
            ProviderError,
        > {
            let chunks = vec![
                ChatCompletionChunk {
                    id: "c1".into(),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: "mock-streaming".into(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: Some("assistant".into()),
                            content: None,
                            tool_calls: vec![],
                        },
                        finish_reason: None,
                    }],
                    usage: None,
                },
                ChatCompletionChunk {
                    id: "c1".into(),
                    object: "chat.completion.chunk".into(),
                    created: 0,
                    model: "mock-streaming".into(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: None,
                            content: Some("Hi!".into()),
                            tool_calls: vec![],
                        },
                        finish_reason: Some("stop".into()),
                    }],
                    usage: None,
                },
            ];
            Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
        }

        async fn embeddings(
            &self,
            _req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Err(ProviderError::Unsupported("mock has no embeddings".into()))
        }
    }

    // ── Test helpers ───────────────────────────────────────────────────────────

    /// Empty-registry app for tests that don't care about providers.
    fn app() -> Router {
        build_router_with_retrieval(AppState::new(ProviderRegistry::new()), None)
    }

    /// Default-providers app for tests that exercise the model catalog.
    fn app_with_defaults() -> Router {
        build_router_with_retrieval(AppState::with_default_providers(), None)
    }

    /// App pre-wired with `MockProvider`.
    fn app_with_mock() -> Router {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(MockProvider));
        build_router_with_retrieval(AppState::new(registry), None)
    }

    fn chat_request(model: &str, stream: bool) -> Request<Body> {
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello"}],
            "stream": stream,
        });
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    // ── Existing tests (unchanged) ─────────────────────────────────────────────

    #[tokio::test]
    async fn health_returns_ok() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn models_returns_empty_list_when_no_providers_registered() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["object"], "list");
        assert_eq!(body["data"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn models_includes_openai_when_defaults_registered() {
        let response = app_with_defaults()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let data = body["data"].as_array().expect("data should be an array");
        let ids: Vec<&str> = data.iter().filter_map(|m| m["id"].as_str()).collect();
        for expected in [
            "gpt-5.5",
            "gpt-5.4",
            "gpt-4o",
            "gpt-4o-mini",
            "o3",
            "o4-mini",
        ] {
            assert!(
                ids.contains(&expected),
                "expected model {expected} in catalog, got {ids:?}"
            );
        }
        let gpt4o = data
            .iter()
            .find(|m| m["id"] == "gpt-4o")
            .expect("gpt-4o present");
        assert_eq!(gpt4o["tokentrimmer"]["provider"], "openai");

        // Anthropic models should appear too once the adapter registers.
        for expected in ["claude-haiku-4-5", "claude-sonnet-4-6", "claude-opus-4-7"] {
            assert!(
                ids.contains(&expected),
                "expected Anthropic model {expected} in catalog, got {ids:?}"
            );
        }
        let sonnet = data
            .iter()
            .find(|m| m["id"] == "claude-sonnet-4-6")
            .expect("claude-sonnet-4-6 present");
        assert_eq!(sonnet["tokentrimmer"]["provider"], "anthropic");
    }

    #[tokio::test]
    async fn trace_id_header_present_on_every_response() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let header = response
            .headers()
            .get("x-tokentrimmer-trace-id")
            .expect("X-TokenTrimmer-Trace-Id missing on health response");
        let s = header.to_str().unwrap();
        // UUID v7 is 36 chars (with dashes).
        assert_eq!(s.len(), 36, "trace id should be a UUID string, got {s}");
    }

    #[tokio::test]
    async fn trace_id_is_unique_per_request() {
        let app = app();
        let r1 = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let r2 = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let t1 = r1
            .headers()
            .get("x-tokentrimmer-trace-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let t2 = r2
            .headers()
            .get("x-tokentrimmer-trace-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_ne!(t1, t2, "trace ids should differ between requests");
    }

    #[tokio::test]
    async fn chat_completions_returns_404_for_unknown_model() {
        let body = serde_json::json!({
            "model": "unknown-model",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let response = app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["error"]["code"], "model_not_found");
    }

    // ── Dispatch tests (new — w6-gateway-dispatch) ─────────────────────────────

    /// Non-streaming dispatch returns 200 with JSON body and all six
    /// X-TokenTrimmer-* response headers.
    #[tokio::test]
    async fn chat_dispatch_non_streaming_returns_200_with_headers() {
        let response = app_with_mock()
            .oneshot(chat_request("mock-model-1", false))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // All six required headers must be present.
        for header in &[
            "x-tokentrimmer-trace-id",
            "x-tokentrimmer-provider",
            "x-tokentrimmer-model-used",
            "x-tokentrimmer-cost-usd",
            "x-tokentrimmer-baseline-cost-usd",
            "x-tokentrimmer-saved-usd",
        ] {
            assert!(
                response.headers().contains_key(*header),
                "missing response header: {header}"
            );
        }

        // Provider header is "mock".
        assert_eq!(
            response.headers()["x-tokentrimmer-provider"]
                .to_str()
                .unwrap(),
            "mock"
        );

        // cost-usd must parse as f64.
        let cost: f64 = response.headers()["x-tokentrimmer-cost-usd"]
            .to_str()
            .unwrap()
            .parse()
            .expect("x-tokentrimmer-cost-usd should be parseable as f64");

        let baseline: f64 = response.headers()["x-tokentrimmer-baseline-cost-usd"]
            .to_str()
            .unwrap()
            .parse()
            .expect("x-tokentrimmer-baseline-cost-usd should be parseable as f64");

        let saved: f64 = response.headers()["x-tokentrimmer-saved-usd"]
            .to_str()
            .unwrap()
            .parse()
            .expect("x-tokentrimmer-saved-usd should be parseable as f64");

        // baseline >= cost (no routing savings in this iteration, but cached discount applies).
        assert!(
            baseline >= cost,
            "baseline ({baseline}) should be >= actual cost ({cost})"
        );

        // saved = baseline - cost (within floating-point rounding).
        let expected_saved = (baseline - cost).max(0.0);
        assert!(
            (saved - expected_saved).abs() < 1e-9,
            "saved ({saved}) should equal baseline - cost ({expected_saved})"
        );

        // Response body should be valid JSON with `id` and `choices`.
        let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["id"], "chatcmpl-mock-1");
        assert!(!body["choices"].as_array().unwrap().is_empty());
    }

    /// `model: "unknown"` still returns 404 with error code `model_not_found`
    /// even when other models are registered.
    #[tokio::test]
    async fn chat_dispatch_unknown_model_returns_404() {
        let response = app_with_mock()
            .oneshot(chat_request("unknown", false))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["error"]["code"], "model_not_found");
    }

    /// Streaming request returns 200 with `content-type: text/event-stream`
    /// and a body containing `data: {...}\n\n` lines terminated by `data: [DONE]\n\n`.
    #[tokio::test]
    async fn chat_dispatch_streaming_returns_sse() {
        let response = app_with_mock()
            .oneshot(chat_request("mock-streaming", true))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let content_type = response.headers()["content-type"].to_str().unwrap();
        assert!(
            content_type.contains("text/event-stream"),
            "expected text/event-stream, got {content_type}"
        );

        // Collect body.
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&bytes).unwrap();

        // Should contain at least one `data: {` line.
        assert!(
            body_str.contains("data: {"),
            "SSE body should contain at least one JSON chunk; got:\n{body_str}"
        );

        // Should end with `data: [DONE]`.
        assert!(
            body_str.contains("data: [DONE]"),
            "SSE body should contain the [DONE] terminator; got:\n{body_str}"
        );
    }

    /// Streaming response carries `X-TokenTrimmer-Trace-Id` and
    /// `X-TokenTrimmer-Provider` response headers.
    #[tokio::test]
    async fn chat_dispatch_streaming_sets_trace_and_provider_headers() {
        let response = app_with_mock()
            .oneshot(chat_request("mock-streaming", true))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let trace_id = response
            .headers()
            .get("x-tokentrimmer-trace-id")
            .expect("x-tokentrimmer-trace-id missing on streaming response")
            .to_str()
            .unwrap();
        assert_eq!(
            trace_id.len(),
            36,
            "trace id should be a UUID, got {trace_id}"
        );

        assert_eq!(
            response.headers()["x-tokentrimmer-provider"]
                .to_str()
                .unwrap(),
            "mock"
        );
    }

    /// Provider error during dispatch maps to 502 Bad Gateway with an
    /// OpenAI-compatible error envelope.
    #[tokio::test]
    async fn chat_dispatch_provider_error_returns_502() {
        let response = app_with_mock()
            .oneshot(chat_request("mock-error", false))
            .await
            .unwrap();

        // ProviderUpstream with status 500 → 502 Bad Gateway from map_provider_error.
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            envelope["error"]["message"].as_str().is_some(),
            "error envelope should have a message field"
        );
    }

    /// `tt_test_*` keys short-circuit to a deterministic synthetic response
    /// without calling any registered provider. All standard headers populated.
    /// Verifies w11-sandbox-test-key.
    #[tokio::test]
    async fn sandbox_test_key_returns_canned_response_without_provider() {
        // Build a registry with a counting provider — we'll assert it's never called.
        use std::sync::atomic::Ordering;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // Reuse MockProvider — it counts via the global `calls` for the
        // dispatch tests; for this isolated test we instead use a fresh mock
        // by hand to avoid cross-test pollution. Inline a tiny one.
        struct CountingProvider {
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        #[async_trait]
        impl Provider for CountingProvider {
            fn id(&self) -> &'static str {
                "counted-mock"
            }
            fn models(&self) -> Vec<ModelInfo> {
                vec![ModelInfo {
                    id: "counted-model".into(),
                    provider: "counted-mock".into(),
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
                _req: ChatCompletionRequest,
                _ctx: &RequestContext,
            ) -> Result<ChatCompletionResponse, ProviderError> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Err(ProviderError::Unsupported("would-be-called".into()))
            }
            async fn chat_completion_stream(
                &self,
                _req: ChatCompletionRequest,
                _ctx: &RequestContext,
            ) -> Result<
                futures::stream::BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
                ProviderError,
            > {
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
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        }));
        let app = build_router(AppState::new(registry));

        let body = serde_json::json!({
            "model": "counted-model",
            "messages": [{"role":"user","content":"hi"}]
        })
        .to_string();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header(
                        "authorization",
                        "Bearer tt_test_abcdef0123456789abcdef0123456789",
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-tokentrimmer-provider")
                .and_then(|v| v.to_str().ok()),
            Some("sandbox"),
            "sandbox key must report provider=sandbox, NOT the registered provider"
        );
        assert_eq!(
            response
                .headers()
                .get("x-tokentrimmer-cache")
                .and_then(|v| v.to_str().ok()),
            Some("sandbox")
        );
        assert_eq!(
            response
                .headers()
                .get("x-tokentrimmer-cost-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.000000")
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "sandbox short-circuit must NOT dispatch to any provider"
        );

        let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["id"]
            .as_str()
            .unwrap()
            .starts_with("chatcmpl-sandbox-"));
        assert!(body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .starts_with("[sandbox]"));
    }
}
