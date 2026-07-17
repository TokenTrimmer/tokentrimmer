//! `tokentrimmer.panel.*` span attributes — integration tests.
//!
//! Verifies the four additive panel attributes on the `http_request` span:
//!
//! * `tokentrimmer.panel.strategy`
//! * `tokentrimmer.panel.leg_count`
//! * `tokentrimmer.panel.quorum_required`
//! * `tokentrimmer.panel.quorum_met`
//!
//! ADDITIVE invariant: each attribute is **set only on the panel path**; a
//! non-panel request carries none of them.
//!
//! The test harness mirrors `gen_ai_span_attrs.rs`: a current-thread Tokio
//! runtime under a scoped `tracing-opentelemetry` subscriber with an in-memory
//! span exporter.  All tests are plain `#[test]` (not `#[tokio::test]`) for the
//! same thread-local subscriber reason documented there.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::Value;
use opentelemetry_sdk::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::prelude::*;

use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

// ---------------------------------------------------------------------------
// Shared mock provider — two models (member + arbiter), priced
// ---------------------------------------------------------------------------

/// A mock provider that serves two models (`panel-m1`, `panel-arbiter`) with
/// fixed token usage. This lets a two-member synthesize panel run end-to-end in
/// memory: both members + the arbiter dispatch to the same mock, which returns a
/// deterministic response so the quorum and leg-count figures are predictable.
struct PanelMock;

#[async_trait]
impl Provider for PanelMock {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "panel-m1".into(),
                provider: "openai".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 8192,
            },
            ModelInfo {
                id: "panel-arbiter".into(),
                provider: "openai".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 8192,
            },
        ]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
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
            effective_at: chrono::Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Ok(ChatCompletionResponse {
            id: "chatcmpl-panel".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("answer".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
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
        Ok(futures::stream::iter(vec![]).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no embeddings".into()))
    }
}

// ---------------------------------------------------------------------------
// OTel span capture harness (mirrors gen_ai_span_attrs.rs)
// ---------------------------------------------------------------------------

/// Drive one HTTP request through the router under a scoped OTel subscriber
/// and return the `http_request` span's attributes as a name → Value map.
///
/// The body is fully drained before reading span attributes so any drop-guard
/// (SSE path) fires.
fn run_capturing_attrs(req: Request<Body>) -> HashMap<String, Value> {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(PanelMock));
    let state = AppState::new(registry).with_panel_enabled(true);
    let app = build_router(state);

    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("panel-span-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    tracing::subscriber::with_default(subscriber, || {
        rt.block_on(async {
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "route should return 200");
            let _ = to_bytes(resp.into_body(), usize::MAX).await;
        });
    });

    provider.force_flush().expect("force_flush should succeed");
    let spans = exporter.get_finished_spans().expect("finished spans");
    let span = spans
        .into_iter()
        .find(|s| s.name == "http_request")
        .expect("gateway 'http_request' span should have been recorded");
    span.attributes
        .into_iter()
        .map(|kv| (kv.key.to_string(), kv.value))
        .collect()
}

/// A panel request using the Synthesize strategy with one member + arbiter.
///
/// The panel config sends one member leg (`panel-m1`) and uses `panel-arbiter`
/// as the arbiter.  With quorum defaulting to 1 and one member succeeding,
/// `quorum_required == quorum_met == 1`.  Total legs = member + arbiter = 2.
fn panel_request() -> Request<Body> {
    // Panel config embedded in tt_extras so no per-request header parsing is
    // needed for the member list.  `X-TokenTrimmer-Panel: synthesize` triggers
    // the panel path; the arbiter and member list come from the extra field.
    // The string form for members/arbiter_model matches the wire format used by
    // the existing panel_dispatch.rs integration tests.
    let body = json!({
        "model": "panel-arbiter",
        "max_tokens": 64,
        "messages": [{ "role": "user", "content": "research this" }],
        "stream": false,
        "tt_extras": {
            "panel": {
                "members": ["panel-m1"],
                "arbiter_model": "panel-arbiter",
                "quorum": 1
            }
        }
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        // Generous cost limit so the budget gate never fires.
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A plain (non-panel) request.
fn non_panel_request() -> Request<Body> {
    let body = json!({
        "model": "panel-m1",
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A panel request must stamp all four `tokentrimmer.panel.*` attributes on the
/// `http_request` span with values matching the panel config / result.
///
/// Config: 1 member leg + 1 arbiter → leg_count = 2.
/// Quorum default = 1 → quorum_required = 1; one member succeeded → quorum_met = 1.
/// Strategy = "synthesize".
#[test]
fn panel_request_span_carries_panel_attributes() {
    let attrs = run_capturing_attrs(panel_request());

    assert_eq!(
        attrs.get("tokentrimmer.panel.strategy"),
        Some(&Value::String("synthesize".into())),
        "panel.strategy must be 'synthesize'"
    );
    // 1 member leg + 1 arbiter leg = 2 total.
    assert_eq!(
        attrs.get("tokentrimmer.panel.leg_count"),
        Some(&Value::I64(2)),
        "panel.leg_count must be member count + arbiter = 2"
    );
    assert_eq!(
        attrs.get("tokentrimmer.panel.quorum_required"),
        Some(&Value::I64(1)),
        "panel.quorum_required must be 1 (default quorum for 1-member panel)"
    );
    assert_eq!(
        attrs.get("tokentrimmer.panel.quorum_met"),
        Some(&Value::I64(1)),
        "panel.quorum_met must be 1 (the single member succeeded)"
    );

    // The standard gen_ai.* + cost attributes must also be present on the panel span.
    assert!(
        attrs.contains_key("gen_ai.system"),
        "panel span must carry gen_ai.system"
    );
    assert!(
        attrs.contains_key("tokentrimmer.cost_usd"),
        "panel span must carry tokentrimmer.cost_usd"
    );
}

/// A NON-panel request must carry NONE of the four `tokentrimmer.panel.*`
/// attributes — the additive invariant: non-panel spans are byte-identical to
/// what they were before the feature landed.
#[test]
fn non_panel_request_span_omits_panel_attributes() {
    let attrs = run_capturing_attrs(non_panel_request());

    assert!(
        !attrs.contains_key("tokentrimmer.panel.strategy"),
        "non-panel span must NOT carry tokentrimmer.panel.strategy"
    );
    assert!(
        !attrs.contains_key("tokentrimmer.panel.leg_count"),
        "non-panel span must NOT carry tokentrimmer.panel.leg_count"
    );
    assert!(
        !attrs.contains_key("tokentrimmer.panel.quorum_required"),
        "non-panel span must NOT carry tokentrimmer.panel.quorum_required"
    );
    assert!(
        !attrs.contains_key("tokentrimmer.panel.quorum_met"),
        "non-panel span must NOT carry tokentrimmer.panel.quorum_met"
    );

    // Standard gen_ai attrs are still present on non-panel spans.
    assert!(
        attrs.contains_key("gen_ai.system"),
        "non-panel span must still carry gen_ai.system"
    );
}
