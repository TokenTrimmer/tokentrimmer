//! W3C TraceContext ingest at the gateway request-entry layer.
//!
//! A request carrying a valid inbound `traceparent` must have its gateway
//! request span *continue* that trace: the exported request span's `trace_id`
//! equals the inbound one and its `parent_span_id` equals the inbound span-id.
//! A request WITHOUT a `traceparent` starts a fresh root (different trace_id,
//! no inbound parent).
//!
//! Hermetic: an in-memory OTel span exporter captures the request span; no
//! network, no collector, no mock upstream needed beyond a trivial provider so
//! the route returns 200.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use opentelemetry::trace::TracerProvider as _;
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

// W3C TraceContext spec example (§3.2.1).
const INBOUND_TRACE_ID_HEX: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const INBOUND_PARENT_ID_HEX: &str = "00f067aa0ba902b7";
const INBOUND_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// Trivial provider so `/v1/chat/completions` returns 200 (the trace middleware
/// wraps the whole request regardless, but a 200 keeps the test focused).
struct TraceMock;

#[async_trait]
impl Provider for TraceMock {
    fn id(&self) -> &'static str {
        "tracemock"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "tm-1".into(),
            provider: "tracemock".into(),
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
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse {
            id: "chatcmpl-trace".into(),
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
        Err(ProviderError::Unsupported("n/a".into()))
    }
}

fn app() -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(TraceMock));
    build_router(AppState::new(registry))
}

fn chat_request(traceparent: Option<&str>) -> Request<Body> {
    let body = json!({
        "model": "tm-1",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": false,
    });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(tp) = traceparent {
        b = b.header("traceparent", tp);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// Drive one request through the gateway under a scoped OTel subscriber and
/// return the captured `http_request` span (the gateway's request-entry span).
///
/// A current-thread runtime keeps the whole request on this thread so the
/// thread-local subscriber installed by `with_default` is in scope for the
/// polled future as well.
fn run_request_capturing_span(req: Request<Body>) -> opentelemetry_sdk::export::trace::SpanData {
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("traceparent-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    tracing::subscriber::with_default(subscriber, || {
        let resp = rt.block_on(async { app().oneshot(req).await.unwrap() });
        assert_eq!(resp.status(), StatusCode::OK, "route should return 200");
    });

    // Spans export on close; the request span closed when the future finished.
    provider.force_flush();
    let spans = exporter.get_finished_spans().expect("finished spans");
    spans
        .into_iter()
        .find(|s| s.name == "http_request")
        .expect("gateway request span 'http_request' should have been recorded")
}

/// WITH a valid inbound `traceparent`: the request span continues that trace —
/// same trace_id, and the inbound span-id is its parent.
#[test]
fn inbound_traceparent_is_continued_as_parent() {
    let span = run_request_capturing_span(chat_request(Some(INBOUND_TRACEPARENT)));

    assert_eq!(
        span.span_context.trace_id().to_string(),
        INBOUND_TRACE_ID_HEX,
        "request span must inherit the inbound trace_id"
    );
    assert_eq!(
        span.parent_span_id.to_string(),
        INBOUND_PARENT_ID_HEX,
        "request span's parent must be the inbound traceparent span-id"
    );
}

/// WITHOUT a `traceparent`: a fresh local trace — a different trace_id and the
/// span is NOT parented on the inbound span-id.
///
/// (The `http_request` span may still have a *local* parent from the ambient
/// `tower_http::TraceLayer` request span; what matters is that the inbound trace
/// was not adopted — the trace_id differs and the inbound span is not its
/// parent.)
#[test]
fn absent_traceparent_starts_fresh_root() {
    let span = run_request_capturing_span(chat_request(None));

    assert_ne!(
        span.span_context.trace_id().to_string(),
        INBOUND_TRACE_ID_HEX,
        "a request with no traceparent must not adopt the spec-example trace_id"
    );
    assert!(
        span.span_context.trace_id() != opentelemetry::trace::TraceId::INVALID,
        "a fresh root must still have a valid generated trace_id"
    );
    assert_ne!(
        span.parent_span_id.to_string(),
        INBOUND_PARENT_ID_HEX,
        "with no traceparent, the request span must not be parented on the inbound span-id"
    );
}
