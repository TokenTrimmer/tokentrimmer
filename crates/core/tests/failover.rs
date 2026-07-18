//! Integration test: a routing rule with `fallbacks` makes the chat handler
//! fail over to the next candidate model when the primary provider returns a
//! fallback-eligible error (here, a 503). Proves the wiring from
//! `RouteAction::fallbacks` → `dispatch_with_failover` → served-provider
//! rebind (cost/headers/telemetry reflect the provider that actually served).
//!
//! Mirrors `dogfood_routing.rs`: routes are seeded under `DOGFOOD_ORG_ID` and
//! `with_dogfood_enabled()` lets unauthenticated requests match them, so the
//! test is hermetic (no DB, no network).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry, DOGFOOD_ORG_ID};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use uuid::Uuid;

/// Mock provider that serves one or more model ids, counts calls, and either
/// returns a deterministic 200 response or a fallback-eligible 503.
struct MockProvider {
    id: &'static str,
    /// Models this provider answers for (registry lookup keys).
    models: &'static [&'static str],
    /// `true` → always 503 (fallback-eligible); `false` → 200.
    fails: bool,
    calls: Arc<AtomicUsize>,
    /// Per-provider pricing lets the failover ceiling tests distinguish a
    /// cheap primary from an expensive fallback.
    pricing: ModelPricing,
    /// Usage override for the 200 response; `None` → a plain 10/5/15 block
    /// with no provider-cache report.
    usage: Option<Usage>,
}

fn mock_pricing(input_per_million: f64, output_per_million: f64) -> ModelPricing {
    ModelPricing {
        input_per_million,
        output_per_million,
        cached_input_per_million: None,
        cache_write_per_million: None,
        batch_input_per_million: None,
        batch_output_per_million: None,
        flex_input_per_million: None,
        flex_output_per_million: None,
        prompt_cache_min_tokens: None,
        effective_at: Utc::now(),
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|m| ModelInfo {
                id: (*m).to_string(),
                provider: self.id.to_string(),
                capabilities: vec![Capability::Text, Capability::Streaming],
                max_input_tokens: 128_000,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(self.pricing.clone())
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fails {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "primary is down".into(),
            });
        }
        Ok(ChatCompletionResponse {
            id: "chatcmpl-failover".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("served".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: self.usage.clone().unwrap_or(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fails {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "primary is down".into(),
            });
        }
        Ok(Box::pin(futures::stream::empty::<
            Result<ChatCompletionChunk, ProviderError>,
        >()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

/// Seed a route under `DOGFOOD_ORG_ID` that rewrites `gpt-4o` → `primary-model`
/// with `fallbacks: [fallback-model]`.
fn failover_routing_store() -> Arc<CachingRoutingStore> {
    failover_routing_store_with_ceiling(None)
}

fn failover_routing_store_with_ceiling(max_cost_usd: Option<f64>) -> Arc<CachingRoutingStore> {
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        DOGFOOD_ORG_ID,
        vec![Route {
            paused: false,
            id: Uuid::now_v7(),
            name: "primary-with-fallback".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                input_tokens_lt: Some(200),
                ..Default::default()
            },
            then: RouteAction {
                workflow: None,
                format_switch: None,
                diff: false,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                agentic_budget: None,
                target_model: Some("primary-model".into()),
                fallbacks: vec!["fallback-model".into()],
                disable_cache: false,
                max_cost_usd,
                flex: false,
                batch: false,
                compress: false,
                doc_compaction: false,
                document_lane: false,
                content_compress: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                panel: None,
            },
        }],
    );
    Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>))
}

fn short_request() -> Request<Body> {
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

fn capped_request(stream: bool) -> Request<Body> {
    let body = json!({
        "model": "gpt-4o",
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 100,
        "stream": stream,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn error_code(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("error response body");
    serde_json::from_slice::<serde_json::Value>(&bytes).expect("JSON error response")["error"]
        ["code"]
        .as_str()
        .expect("error code")
        .to_owned()
}

#[tokio::test]
async fn fails_over_to_fallback_when_primary_returns_503() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));

    let mut registry = ProviderRegistry::new();
    // Primary also answers the inbound `gpt-4o` so the pre-routing resolve
    // succeeds; after the rewrite it serves `primary-model` (and 503s).
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: true,
        calls: Arc::clone(&primary_calls),
        pricing: mock_pricing(0.1, 0.1),
        usage: None,
    }));
    registry.register(Arc::new(MockProvider {
        id: "fallback",
        models: &["fallback-model"],
        fails: false,
        calls: Arc::clone(&fallback_calls),
        pricing: mock_pricing(0.1, 0.1),
        usage: None,
    }));

    let state = AppState::new(registry)
        .with_routing_store(failover_routing_store())
        .with_dogfood_enabled();
    let app = build_router(state);

    let resp = app.oneshot(short_request()).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "fallback should serve a 200");
    assert!(
        primary_calls.load(Ordering::Relaxed) >= 1,
        "primary must have been tried"
    );
    assert_eq!(
        fallback_calls.load(Ordering::Relaxed),
        1,
        "fallback must have served exactly once"
    );
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("fallback-model"),
        "served model header should reflect the fallback"
    );
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-provider")
            .and_then(|v| v.to_str().ok()),
        Some("fallback"),
        "provider header should reflect the serving (fallback) provider"
    );
}

#[tokio::test]
async fn uses_primary_and_skips_fallback_when_primary_healthy() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: false,
        calls: Arc::clone(&primary_calls),
        pricing: mock_pricing(0.1, 0.1),
        usage: None,
    }));
    registry.register(Arc::new(MockProvider {
        id: "fallback",
        models: &["fallback-model"],
        fails: false,
        calls: Arc::clone(&fallback_calls),
        pricing: mock_pricing(0.1, 0.1),
        usage: None,
    }));

    let state = AppState::new(registry)
        .with_routing_store(failover_routing_store())
        .with_dogfood_enabled();
    let app = build_router(state);

    let resp = app.oneshot(short_request()).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        primary_calls.load(Ordering::Relaxed),
        1,
        "healthy primary should serve once"
    );
    assert_eq!(
        fallback_calls.load(Ordering::Relaxed),
        0,
        "fallback must NOT be called when primary succeeds"
    );
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-model-used")
            .and_then(|v| v.to_str().ok()),
        Some("primary-model"),
    );
}

/// A request served by a FALLBACK provider must log THAT provider's raw
/// prompt-cache token counts (research Phase 0.2): failover rebinds the
/// served provider before the single request_logs write site, so the
/// winning provider's usage — not the failed primary's — populates
/// `cache_read_input_tokens` / `cache_creation_input_tokens`.
#[tokio::test]
async fn failover_logs_fallback_providers_cache_token_counts() {
    use tt_telemetry::request_logs::InMemoryRequestLogWriter;

    let primary_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: true,
        calls: Arc::clone(&primary_calls),
        pricing: mock_pricing(0.1, 0.1),
        usage: None,
    }));
    registry.register(Arc::new(MockProvider {
        id: "fallback",
        models: &["fallback-model"],
        fails: false,
        calls: Arc::clone(&fallback_calls),
        pricing: mock_pricing(0.1, 0.1),
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: 8,
            cache_creation_input_tokens: Some(2),
            cache_read_input_tokens: Some(8),
        }),
    }));

    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let state = AppState::new(registry)
        .with_routing_store(failover_routing_store())
        .with_dogfood_enabled()
        .with_request_log_writer(writer.clone());
    let app = build_router(state);

    let resp = app.oneshot(short_request()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "fallback should serve a 200");
    assert_eq!(fallback_calls.load(Ordering::Relaxed), 1);

    // Telemetry write is fire-and-forget — give the spawned task a beat.
    for _ in 0..20 {
        if !writer.rows().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rows = writer.rows();
    assert_eq!(rows.len(), 1, "expected exactly one telemetry row");
    let row = &rows[0];
    assert_eq!(
        row.provider, "fallback",
        "row must carry the provider that actually served"
    );
    assert_eq!(row.model, "fallback-model");
    assert_eq!(
        row.requested_model.as_deref(),
        Some("gpt-4o"),
        "the row must retain the pre-routing caller model, not the fallback served model"
    );
    assert_eq!(
        row.cache_read_input_tokens,
        Some(8),
        "fallback provider's raw cache reads must be persisted"
    );
    assert_eq!(row.cache_creation_input_tokens, Some(2));
    assert_eq!(row.cached_tokens, 8);
}

/// A route ceiling must remain in force for every failover candidate. The
/// cheap routed primary is allowed to establish its 503; the known-priced,
/// expensive fallback must be rejected before any upstream dispatch.
#[tokio::test]
async fn route_cost_ceiling_blocks_priced_fallback_after_primary_503() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: true,
        calls: Arc::clone(&primary_calls),
        pricing: mock_pricing(0.1, 0.1),
        usage: None,
    }));
    registry.register(Arc::new(MockProvider {
        id: "expensive-fallback",
        models: &["fallback-model"],
        fails: false,
        calls: Arc::clone(&fallback_calls),
        pricing: mock_pricing(0.1, 1_000.0),
        usage: None,
    }));

    let app = build_router(
        AppState::new(registry)
            .with_routing_store(failover_routing_store_with_ceiling(Some(0.001)))
            .with_dogfood_enabled(),
    );

    let resp = app.oneshot(capped_request(false)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYMENT_REQUIRED,
        "the expensive fallback must be rejected with the normal cost error"
    );
    assert_eq!(error_code(resp).await, "cost_limit_exceeded");
    assert!(
        primary_calls.load(Ordering::Relaxed) >= 1,
        "the cheap primary must have been attempted first"
    );
    assert_eq!(
        fallback_calls.load(Ordering::Relaxed),
        0,
        "an over-ceiling fallback must not receive an upstream request"
    );
}

/// The same guard must apply while establishing an SSE response. Failover is
/// only permitted before the first chunk, so the expensive candidate must be
/// denied with a normal HTTP 402 instead of opening an SSE stream.
#[tokio::test]
async fn route_cost_ceiling_blocks_priced_fallback_during_stream_establishment() {
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));

    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(MockProvider {
        id: "primary",
        models: &["gpt-4o", "primary-model"],
        fails: true,
        calls: Arc::clone(&primary_calls),
        pricing: mock_pricing(0.1, 0.1),
        usage: None,
    }));
    registry.register(Arc::new(MockProvider {
        id: "expensive-fallback",
        models: &["fallback-model"],
        fails: false,
        calls: Arc::clone(&fallback_calls),
        pricing: mock_pricing(0.1, 1_000.0),
        usage: None,
    }));

    let app = build_router(
        AppState::new(registry)
            .with_routing_store(failover_routing_store_with_ceiling(Some(0.001)))
            .with_dogfood_enabled(),
    );

    let resp = app.oneshot(capped_request(true)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYMENT_REQUIRED,
        "the costly fallback must be denied before stream establishment"
    );
    assert_eq!(error_code(resp).await, "cost_limit_exceeded");
    assert!(
        primary_calls.load(Ordering::Relaxed) >= 1,
        "the cheap primary must have been attempted first"
    );
    assert_eq!(
        fallback_calls.load(Ordering::Relaxed),
        0,
        "an over-ceiling fallback must not receive a stream-establishment call"
    );
}
