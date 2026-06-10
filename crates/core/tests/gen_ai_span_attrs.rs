//! OpenTelemetry GenAI semconv + TokenTrimmer cost attributes on the gateway
//! request span.
//!
//! A non-streaming `POST /v1/chat/completions` that reaches a provider must
//! leave the gateway `http_request` span carrying the GenAI semantic-convention
//! attributes (`gen_ai.system`, `gen_ai.request.model`, `gen_ai.response.model`,
//! `gen_ai.usage.input_tokens`/`output_tokens`, `gen_ai.operation.name`) plus
//! the TokenTrimmer cost attributes that mirror the `x-tokentrimmer-*` response
//! headers (`tokentrimmer.cost_usd`, `tokentrimmer.saved_usd`, `tokentrimmer.cache`).
//!
//! Hermetic: an in-memory OTel span exporter captures the request span; a
//! trivial mock provider returns a fixed token usage so the cost is
//! deterministic. No network, no collector, no real upstream.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::Value;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::TracerProvider;
use tracing_subscriber::prelude::*;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};

const PROMPT_TOKENS: u64 = 100;
const COMPLETION_TOKENS: u64 = 20;

/// Provider returning a fixed token usage so cost is deterministic.
/// `$1/1M` input and `$2/1M` output → cost = 100*1e-6 + 20*2e-6 = 1.4e-4 USD.
struct CostMock;

#[async_trait]
impl Provider for CostMock {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "gpt-test".into(),
            provider: "openai".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
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
            id: "chatcmpl-cost".into(),
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
                prompt_tokens: PROMPT_TOKENS,
                completion_tokens: COMPLETION_TOKENS,
                total_tokens: PROMPT_TOKENS + COMPLETION_TOKENS,
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
        Err(ProviderError::Unsupported("n/a".into()))
    }
}

fn app() -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CostMock));
    build_router(AppState::new(registry))
}

fn chat_request() -> Request<Body> {
    let body = json!({
        "model": "gpt-test",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Drive one request through the gateway under a scoped OTel subscriber and
/// return the captured `http_request` span's attributes as a name→Value map.
fn run_request_capturing_attrs() -> HashMap<String, Value> {
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("gen-ai-span-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    tracing::subscriber::with_default(subscriber, || {
        let resp = rt.block_on(async { app().oneshot(chat_request()).await.unwrap() });
        assert_eq!(resp.status(), StatusCode::OK, "route should return 200");
    });

    provider.force_flush();
    let spans = exporter.get_finished_spans().expect("finished spans");
    let span = spans
        .into_iter()
        .find(|s| s.name == "http_request")
        .expect("gateway request span 'http_request' should have been recorded");
    span.attributes
        .into_iter()
        .map(|kv| (kv.key.to_string(), kv.value))
        .collect()
}

#[test]
fn request_span_carries_gen_ai_and_cost_attributes() {
    let attrs = run_request_capturing_attrs();

    // GenAI semantic-convention attributes.
    assert_eq!(
        attrs.get("gen_ai.system"),
        Some(&Value::String("openai".into())),
        "gen_ai.system must map the provider id"
    );
    assert_eq!(
        attrs.get("gen_ai.provider.name"),
        Some(&Value::String("openai".into())),
        "newer semconv spelling must be emitted too"
    );
    assert_eq!(
        attrs.get("gen_ai.operation.name"),
        Some(&Value::String("chat".into()))
    );
    assert_eq!(
        attrs.get("gen_ai.request.model"),
        Some(&Value::String("gpt-test".into())),
        "request model is the model the caller asked for"
    );
    assert_eq!(
        attrs.get("gen_ai.response.model"),
        Some(&Value::String("gpt-test".into())),
        "response model is the model that served the request"
    );
    assert_eq!(
        attrs.get("gen_ai.usage.input_tokens"),
        Some(&Value::I64(PROMPT_TOKENS as i64))
    );
    assert_eq!(
        attrs.get("gen_ai.usage.output_tokens"),
        Some(&Value::I64(COMPLETION_TOKENS as i64))
    );

    // TokenTrimmer cost attributes (mirroring the x-tokentrimmer-* headers).
    let expected_cost =
        (PROMPT_TOKENS as f64) * 1.0 / 1_000_000.0 + (COMPLETION_TOKENS as f64) * 2.0 / 1_000_000.0;
    match attrs.get("tokentrimmer.cost_usd") {
        Some(Value::F64(c)) => assert!(
            (c - expected_cost).abs() < 1e-12,
            "tokentrimmer.cost_usd = {c}, expected {expected_cost}"
        ),
        other => panic!("tokentrimmer.cost_usd should be an f64, got {other:?}"),
    }
    // No routing → no TT-attributed saving on a plain miss.
    assert_eq!(
        attrs.get("tokentrimmer.saved_usd"),
        Some(&Value::F64(0.0)),
        "saved_usd is 0 with no routing/cache saving"
    );
    assert_eq!(
        attrs.get("tokentrimmer.cache"),
        Some(&Value::String("none".into())),
        "no cache layer configured in this test → cache outcome 'none'"
    );
    // Present even when zero so spend/savings panels can sum across spans.
    assert!(attrs.contains_key("tokentrimmer.baseline_cost_usd"));
    assert!(attrs.contains_key("tokentrimmer.provider_cache_saved_usd"));
}

/// The committed Grafana dashboard is valid JSON and has the expected shape
/// (panels referencing the emitted span attributes). Guards against a
/// hand-edited dashboard landing malformed.
#[test]
fn grafana_dashboard_json_is_valid() {
    // CARGO_MANIFEST_DIR is crates/core; the dashboard lives at the repo root.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/observability/grafana-tokentrimmer-gateway.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("dashboard JSON not found at {}: {e}", path.display()));

    let dash: serde_json::Value =
        serde_json::from_str(&raw).expect("Grafana dashboard must be valid JSON");

    let panels = dash
        .get("panels")
        .and_then(|p| p.as_array())
        .expect("dashboard must have a panels array");
    assert!(!panels.is_empty(), "dashboard should define panels");

    // At least one panel must query a TokenTrimmer cost attribute and one a
    // gen_ai attribute, so the dashboard actually surfaces what the gateway
    // emits.
    assert!(
        raw.contains("span.tokentrimmer.cost_usd"),
        "a panel should query tokentrimmer.cost_usd"
    );
    assert!(
        raw.contains("span.gen_ai.system") || raw.contains("span.gen_ai.response.model"),
        "a panel should query a gen_ai.* attribute"
    );
}
