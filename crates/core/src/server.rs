//! Gateway HTTP server.
//!
//! Composes the public router from per-endpoint handler modules under `routes/`.
//! Middleware (auth, telemetry, cache, routing) is layered in subsequent
//! Week-1+ iterations and intentionally NOT wired here yet — keep the skeleton
//! testable end-to-end first.

use axum::extract::DefaultBodyLimit;
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

/// Per-route request-timeout tiers (ARCH-4).
///
/// A single flat 600 s `TimeoutLayer` on every route let a hung *non-streaming*
/// upstream pin a gateway slot for ten minutes on a thin machine. Instead we
/// apply two tiers via nested per-group `TimeoutLayer`s:
///
/// * [`STREAMING_TIMEOUT_SECS`] — the generous ceiling, reserved for the
///   completion endpoints that may stream a long-lived response.
/// * [`SHORT_TIMEOUT_SECS`] — every endpoint that never streams (models,
///   embeddings, preview, routes API, health/ready/metrics).
///
/// Note: `/v1/chat/completions`, `/v1/messages` and `/v1/responses` multiplex
/// streaming and non-streaming on a single path (decided by the request body's
/// `stream` flag), so they cannot be given the short tier at the router layer
/// without truncating legitimate streams. A finer body-aware split for the
/// *non-streaming* use of those routes would have to live in the chat handler
/// and is intentionally left out of this router-level fix.
pub(crate) const STREAMING_TIMEOUT_SECS: u64 = 600;
/// Short request-timeout tier for endpoints that never stream. See
/// [`STREAMING_TIMEOUT_SECS`].
pub(crate) const SHORT_TIMEOUT_SECS: u64 = 60;

/// Resolve the streaming-tier timeout (seconds), letting an operator override
/// [`STREAMING_TIMEOUT_SECS`] via the `TT_STREAMING_TIMEOUT_SECS` env var. Unset,
/// unparseable, or `0` → the compiled default (never panics).
pub(crate) fn streaming_timeout_secs() -> u64 {
    timeout_secs_from_lookup(
        |k| std::env::var(k).ok(),
        "TT_STREAMING_TIMEOUT_SECS",
        STREAMING_TIMEOUT_SECS,
    )
}

/// Resolve the short (non-streaming) route timeout (seconds), letting an operator
/// override [`SHORT_TIMEOUT_SECS`] via the `TT_ROUTE_TIMEOUT_SECS` env var. Unset,
/// unparseable, or `0` → the compiled default (never panics).
pub(crate) fn short_timeout_secs() -> u64 {
    timeout_secs_from_lookup(
        |k| std::env::var(k).ok(),
        "TT_ROUTE_TIMEOUT_SECS",
        SHORT_TIMEOUT_SECS,
    )
}

/// Parse a timeout (seconds) from a key→value lookup. A missing var uses
/// `default`; a present value must parse as a `u64 >= 1` (a `0`s timeout would
/// shed every request). Unparseable / zero values log a warning and fall back
/// to `default` — this NEVER panics. The lookup indirection lets tests exercise
/// the parsing without mutating process env.
fn timeout_secs_from_lookup(get: impl Fn(&str) -> Option<String>, key: &str, default: u64) -> u64 {
    match get(key) {
        None => default,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(v) if v >= 1 => v,
            Ok(_) => {
                tracing::warn!(
                    env_var = key,
                    "route-timeout override must be >= 1s; using default"
                );
                default
            }
            Err(_) => {
                tracing::warn!(
                    env_var = key,
                    value = %raw,
                    "unparseable route-timeout override; using default"
                );
                default
            }
        },
    }
}

/// Read the `TT_PANEL_ENABLED` environment variable and return the resolved
/// kill-switch value for [`AppState::with_panel_enabled`].
///
/// Truthy values: `"1"` or any case-insensitive variant of `"true"`.
/// Absent or any other value → `false` (off by default).
pub fn panel_enabled_from_env() -> bool {
    std::env::var("TT_PANEL_ENABLED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Read `TT_PANEL_MIN_TIER` → the minimum `CallerTier` allowed to use the panel.
/// `"pro"|"team"|"scale"` (case-insensitive) → that tier; absent/unknown → `Free`
/// (allow-all — the default, so the panel works today behind the kill-switch
/// until an operator tightens it or cloud injects real tiers).
pub fn panel_min_tier_from_env() -> tt_shared::CallerTier {
    use tt_shared::CallerTier::*;
    match std::env::var("TT_PANEL_MIN_TIER")
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        Ok("pro") => Pro,
        Ok("team") => Team,
        Ok("scale") => Scale,
        Ok("free") | Err(_) => Free,
        Ok(other) => {
            tracing::warn!(value = %other, "unknown TT_PANEL_MIN_TIER, defaulting to Free");
            Free
        }
    }
}

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
    crate::metrics::install();

    // ARCH-4: per-route timeout tiers. The completion endpoints may stream a
    // long-lived response and keep the generous ceiling; everything else gets
    // the short tier so a hung non-streaming upstream can't pin a slot for ten
    // minutes. Each group carries its own `TimeoutLayer` (applied INSIDE the
    // shared middleware below), so the per-group 504 still bubbles up through
    // the outermost latency layer and gets the `X-TokenTrimmer-Latency-Ms`
    // stamp.
    let streaming = Router::new()
        .route("/v1/chat/completions", post(routes::chat::handler))
        .route("/v1/messages", post(routes::messages::handler))
        .route("/v1/responses", post(routes::responses::handler))
        // Workflow runs may stream a long-lived SSE response (W3c Task 3);
        // moved from the `short` (60 s) group to the `streaming` (600 s) group.
        .route(
            "/v1/workflows/:id/runs",
            post(routes::workflows::create_run),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            std::time::Duration::from_secs(streaming_timeout_secs()),
        ));

    let short = Router::new()
        .route("/health", get(routes::health::handler))
        .route("/ready", get(routes::ready::handler))
        .route("/metrics", get(routes::metrics::handler))
        .route("/v1/models", get(routes::models::handler))
        .route("/v1/embeddings", post(routes::embeddings::handler))
        // POST creates a new run; GET lists the caller's runs (org-scoped,
        // newest-first, durable Postgres view — no transcript). The bare list
        // route must appear BEFORE the parameterized :id route so axum's router
        // does not mistake "runs" as an id; in practice axum differentiates by
        // segment count, so ordering here is just for clarity.
        .route(
            "/v1/agent/runs",
            post(routes::agent_run::create_run).get(routes::agent_run::list_runs),
        )
        .route("/v1/agent/runs/:id", get(routes::agent_run::get_run))
        .route(
            "/v1/agent/runs/:id/tool_outputs",
            post(routes::agent_run::submit_tool_outputs),
        )
        .route(
            "/v1/preview",
            axum::routing::post(crate::routes::preview::post_preview),
        )
        .route(
            "/v1/routes",
            get(routes::routes_api::list).post(routes::routes_api::create),
        )
        .route(
            "/v1/routes/:id",
            get(routes::routes_api::get).delete(routes::routes_api::delete),
        )
        .route("/v1/routes/:id/pause", post(routes::routes_api::pause))
        .route("/v1/routes/:id/resume", post(routes::routes_api::resume))
        .route("/v1/routes/:id/savings", get(routes::routes_api::savings))
        // Tenant-facing spend summary (spend-today + MTD + budget-remaining) —
        // org derived from the authenticated tt_live_ key. Backs the MCP
        // cost-control tools.
        .route("/v1/spend", get(routes::spend_api::get_spend))
        // Tenant-facing cap write: set/clear the org's (or a key's) monthly spend
        // cap. Same auth seam; backs the MCP `set_cost_limit` tool.
        .route("/v1/spend/limit", post(routes::spend_api::set_spend_limit))
        // Batch Lane (slice 2): OpenAI-compatible submit/status + file proxy.
        // Non-streaming, so the short timeout tier. The slice-3 worker owns
        // long-running polling; these handlers only proxy + persist.
        .route("/v1/files", post(routes::batches::upload_file))
        .route(
            "/v1/files/:id/content",
            get(routes::batches::download_file_content),
        )
        .route(
            "/v1/batches",
            post(routes::batches::create_batch).get(routes::batches::list_batches),
        )
        .route("/v1/batches/:id", get(routes::batches::get_batch))
        .route(
            "/v1/batches/:id/cancel",
            post(routes::batches::cancel_batch),
        )
        // Workflow engine: CRUD + cost estimate on short (60 s); runs moved to
        // the streaming (600 s) group above so streaming callers aren't capped.
        .route(
            "/v1/workflows",
            post(routes::workflows::create).get(routes::workflows::list),
        )
        // Literal `/secrets` segment must come before the parameterised `:id`
        // route so the path is unambiguous (axum prefers literals, but explicit
        // ordering documents intent).
        .route(
            "/v1/workflows/secrets",
            post(routes::workflows::set_workflow_secret),
        )
        .route("/v1/workflows/:id", get(routes::workflows::get))
        .route(
            "/v1/workflows/:id/estimate",
            post(routes::workflows::estimate),
        )
        // WF-6: run history — the rows were persisted by create_run but had no
        // HTTP route, so runs + receipts vanished on navigation. Both org-scoped.
        .route(
            "/v1/workflows/:id/runs",
            get(routes::workflows::list_workflow_runs),
        )
        .route(
            "/v1/workflows/runs/:run_id",
            get(routes::workflows::get_workflow_run),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            std::time::Duration::from_secs(short_timeout_secs()),
        ));

    let base = streaming.merge(short);

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
    // Request timeouts are applied per-route above (ARCH-4), not as a single
    // flat layer here, so streaming completions keep the long ceiling while
    // non-streaming endpoints get a short one.
    //
    // CORS is intentionally fully open (any origin/method/header): this is a
    // bearer-token API with no cookies/ambient credentials, so a permissive
    // CORS policy grants a browser nothing it couldn't already do server-side
    // — every request must still carry a valid `Authorization: Bearer` key.
    .layer(
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any),
    )
    // Explicit request-body cap. Axum's default is only 2MB — too small for
    // large-context chat requests (256k-token windows can exceed it). Sized
    // for the largest supported context; returns 413 on exceed. (§4.15)
    .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
    // Outermost app middleware: stamps `X-TokenTrimmer-Latency-Ms` on EVERY
    // response — including the timeout 504 and body-limit 413 produced by the
    // layers it wraps (the later `.layer()` is the outer one in axum/tower).
    .layer(axum::middleware::from_fn(middleware::latency::middleware))
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
        messages::{Choice, ChunkChoice, ChunkDelta, EmbeddingData, Message, MessageContent},
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
                cache_write_per_million: None,
                batch_input_per_million: None,
                batch_output_per_million: None,
                flex_input_per_million: None,
                flex_output_per_million: None,
                prompt_cache_min_tokens: None,
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
                    cache_read_input_tokens: None,
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
                            extra: Default::default(),
                        },
                        finish_reason: None,
                        extra: Default::default(),
                    }],
                    usage: None,
                    extra: Default::default(),
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
                            extra: Default::default(),
                        },
                        finish_reason: Some("stop".into()),
                        extra: Default::default(),
                    }],
                    usage: None,
                    extra: Default::default(),
                },
            ];
            Ok(futures::stream::iter(chunks.into_iter().map(Ok)).boxed())
        }

        async fn embeddings(
            &self,
            req: EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<EmbeddingsResponse, ProviderError> {
            Ok(EmbeddingsResponse {
                object: "list".into(),
                data: vec![EmbeddingData {
                    object: "embedding".into(),
                    index: 0,
                    embedding: vec![0.1, 0.2, 0.3],
                }],
                model: req.model,
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 0,
                    total_tokens: 100,
                    cached_tokens: 0,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
            })
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
    async fn ready_route_is_wired_and_returns_200_in_degraded_mode() {
        // No DB pool / L1 wired (the app() default) → readiness has no
        // configured hard dependency down, so /ready answers 200 ready.
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/ready")
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
        assert_eq!(body["status"], "ready");
        assert_eq!(body["checks"]["postgres"], "not_configured");
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

    fn chat_request_with(model: &str, max_tokens: u32, cost_limit: Option<&str>) -> Request<Body> {
        let body = serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": "the quick brown fox" }],
            "max_tokens": max_tokens,
            "stream": false,
        });
        let mut b = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json");
        if let Some(cl) = cost_limit {
            b = b.header("x-tokentrimmer-cost-limit-usd", cl);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn latency_header_present_on_success_and_error() {
        // Success (dispatch) — header present + parseable.
        let ok = app_with_mock()
            .oneshot(chat_request("mock-model-1", false))
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let _ms: u64 = ok.headers()["x-tokentrimmer-latency-ms"]
            .to_str()
            .unwrap()
            .parse()
            .expect("latency-ms parseable");

        // Error (unknown model → 404) — middleware still stamps the header.
        let err = app_with_mock()
            .oneshot(chat_request("does-not-exist", false))
            .await
            .unwrap();
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert!(
            err.headers().contains_key("x-tokentrimmer-latency-ms"),
            "latency header must be present even on error responses"
        );
    }

    #[tokio::test]
    async fn cost_limit_header_rejects_over_limit() {
        // mock pricing $1/M in, $2/M out; max_tokens 1000 → est ≈ $0.002 > 1e-9.
        let response = app_with_mock()
            .oneshot(chat_request_with("mock-model-1", 1000, Some("0.000000001")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "cost_limit_exceeded");
    }

    #[tokio::test]
    async fn cost_limit_counts_full_prompt_not_just_last_message() {
        // A large system prompt + a tiny trailing user message. The limit is set
        // so the 402 trips ONLY when the FULL prompt is counted — estimating from
        // just the last user message (the prior bug) would undercount and pass.
        let big_system = "word ".repeat(400); // ~2000 chars → ~500 tokens (mock = chars/4)
        let body = serde_json::json!({
            "model": "mock-model-1",
            "messages": [
                { "role": "system", "content": big_system },
                { "role": "user", "content": "hi" }
            ],
            "max_tokens": 1,
            "stream": false,
        });
        let response = app_with_mock()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("x-tokentrimmer-cost-limit-usd", "0.0001")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn cost_limit_header_allows_under_limit() {
        let response = app_with_mock()
            .oneshot(chat_request_with("mock-model-1", 1000, Some("100")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cost_limit_header_absent_is_noop() {
        let response = app_with_mock()
            .oneshot(chat_request_with("mock-model-1", 1000, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn embeddings_dispatch_returns_200_with_headers() {
        let body = serde_json::json!({ "model": "mock-model-1", "input": "hello" });
        let response = app_with_mock()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        for h in [
            "x-tokentrimmer-trace-id",
            "x-tokentrimmer-provider",
            "x-tokentrimmer-model-used",
            "x-tokentrimmer-cost-usd",
            "x-tokentrimmer-baseline-cost-usd",
            "x-tokentrimmer-saved-usd",
        ] {
            assert!(response.headers().contains_key(h), "missing header {h}");
        }
        assert_eq!(
            response.headers()["x-tokentrimmer-model-used"]
                .to_str()
                .unwrap(),
            "mock-model-1"
        );
        // 100 input tokens × $1.0/M (mock pricing) = $0.0001.
        let cost: f64 = response.headers()["x-tokentrimmer-cost-usd"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((cost - 0.0001).abs() < 1e-9, "cost = {cost}");

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"][0]["embedding"][0], 0.1);
        assert_eq!(v["model"], "mock-model-1");
    }

    #[tokio::test]
    async fn embeddings_sandbox_key_short_circuits() {
        let body = serde_json::json!({ "model": "mock-model-1", "input": "hello" });
        let response = app_with_mock()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer tt_test_abc123")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        // The sandbox short-circuit reports provider "sandbox"; a real dispatch
        // through MockProvider would report "mock" — proving no provider call.
        assert_eq!(
            response.headers()["x-tokentrimmer-provider"]
                .to_str()
                .unwrap(),
            "sandbox"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["data"][0]["embedding"].is_array());
        assert_eq!(v["model"], "mock-model-1");
    }

    #[tokio::test]
    async fn embeddings_cost_limit_rejects_over_limit() {
        let body = serde_json::json!({
            "model": "mock-model-1",
            "input": "the quick brown fox jumps over the lazy dog"
        });
        let response = app_with_mock()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings")
                    .header("content-type", "application/json")
                    .header("x-tokentrimmer-cost-limit-usd", "0.000000001")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
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

        // All seven required headers must be present.
        for header in &[
            "x-tokentrimmer-trace-id",
            "x-tokentrimmer-provider",
            "x-tokentrimmer-model-used",
            "x-tokentrimmer-cost-usd",
            "x-tokentrimmer-baseline-cost-usd",
            "x-tokentrimmer-saved-usd",
            "x-tokentrimmer-provider-cache-saved-usd",
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

        let provider_cache_saved: f64 = response.headers()
            ["x-tokentrimmer-provider-cache-saved-usd"]
            .to_str()
            .unwrap()
            .parse()
            .expect("x-tokentrimmer-provider-cache-saved-usd should be parseable as f64");

        // baseline >= cost (no routing savings in this iteration, but cached discount applies).
        assert!(
            baseline >= cost,
            "baseline ({baseline}) should be >= actual cost ({cost})"
        );

        // No TT optimization (no routing, no TT cache): the provider's
        // automatic cached-token discount must NOT be claimed as TT savings.
        assert!(
            saved.abs() < 1e-9,
            "saved ({saved}) should be 0 — the cached-token discount is provider-side"
        );
        let expected_provider_saved = (baseline - cost).max(0.0);
        assert!(
            (provider_cache_saved - expected_provider_saved).abs() < 1e-9,
            "provider-cache-saved ({provider_cache_saved}) should equal \
             baseline - cost ({expected_provider_saved})"
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

    // ── Per-route timeout tiers (ARCH-4) ───────────────────────────────────────

    /// The tier constants are ordered and within the intended band: the short
    /// tier sits in the 30–60 s window and is strictly tighter than the
    /// streaming ceiling, which stays at the original 600 s.
    #[test]
    fn timeout_tiers_are_ordered_and_sane() {
        // Bind to runtime locals so the assertions exercise the values rather
        // than const-folding to a literal (clippy::assertions_on_constants).
        let short = std::hint::black_box(SHORT_TIMEOUT_SECS);
        let streaming = std::hint::black_box(STREAMING_TIMEOUT_SECS);
        assert!(
            (30..=60).contains(&short),
            "short tier should be 30–60s, got {short}"
        );
        assert_eq!(
            streaming, 600,
            "streaming completions keep the 600s ceiling"
        );
        assert!(
            short < streaming,
            "short tier must be tighter than the streaming ceiling"
        );
    }

    /// The env-tunable route-timeout seam: unset → the compiled default;
    /// a valid value overrides; `0` and unparseable values fall back to the
    /// default (never panic, never a 0s timeout).
    #[test]
    fn timeout_secs_from_lookup_covers_unset_set_and_bad_values() {
        // Unset → compiled default, byte-identical to today.
        assert_eq!(
            timeout_secs_from_lookup(|_| None, "TT_ROUTE_TIMEOUT_SECS", SHORT_TIMEOUT_SECS),
            SHORT_TIMEOUT_SECS,
        );
        // Set → override.
        assert_eq!(
            timeout_secs_from_lookup(
                |_| Some("45".to_string()),
                "TT_ROUTE_TIMEOUT_SECS",
                SHORT_TIMEOUT_SECS,
            ),
            45,
        );
        // Unparseable → default (no panic).
        assert_eq!(
            timeout_secs_from_lookup(
                |_| Some("soon".to_string()),
                "TT_STREAMING_TIMEOUT_SECS",
                STREAMING_TIMEOUT_SECS,
            ),
            STREAMING_TIMEOUT_SECS,
        );
        // Zero would shed every request → clamped back to the default.
        assert_eq!(
            timeout_secs_from_lookup(
                |_| Some("0".to_string()),
                "TT_ROUTE_TIMEOUT_SECS",
                SHORT_TIMEOUT_SECS,
            ),
            SHORT_TIMEOUT_SECS,
        );
    }

    /// Proves the per-group timeout wiring this router relies on: a slow handler
    /// behind the SHORT-tier `TimeoutLayer` is shed with a 504, while a fast
    /// handler in a separate group with a long timeout is unaffected — and the
    /// outermost latency middleware still stamps `X-TokenTrimmer-Latency-Ms` on
    /// the timeout 504 (the property that lets streaming keep 600 s while
    /// everything else gets the short tier, without losing the latency header).
    #[tokio::test]
    async fn per_group_timeout_sheds_slow_route_but_not_fast_group() {
        async fn slow_handler() -> &'static str {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            "slow"
        }
        async fn fast_handler() -> &'static str {
            "fast"
        }

        // Mirror production wiring: two route groups, each with its OWN
        // TimeoutLayer, merged, then the latency middleware applied OUTERMOST.
        let short_group =
            Router::new()
                .route("/short", get(slow_handler))
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::GATEWAY_TIMEOUT,
                    std::time::Duration::from_millis(50),
                ));
        let long_group =
            Router::new()
                .route("/long", get(fast_handler))
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::GATEWAY_TIMEOUT,
                    std::time::Duration::from_secs(30),
                ));
        let app = short_group
            .merge(long_group)
            .layer(axum::middleware::from_fn(middleware::latency::middleware));

        // Slow handler under the short tier → 504, with the latency header still
        // stamped by the outermost layer.
        let slow = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/short")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            slow.status(),
            StatusCode::GATEWAY_TIMEOUT,
            "slow handler must be shed by the short-tier timeout"
        );
        assert!(
            slow.headers().contains_key("x-tokentrimmer-latency-ms"),
            "latency header must be stamped even on the per-route timeout 504"
        );

        // Fast handler in the long-tier group is unaffected → 200.
        let fast = app
            .oneshot(Request::builder().uri("/long").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            fast.status(),
            StatusCode::OK,
            "a route in the long-tier group is not throttled by the short tier"
        );
    }
}
