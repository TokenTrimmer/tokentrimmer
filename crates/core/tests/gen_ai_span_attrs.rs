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
//! The L1 and L2 cache-HIT paths are covered too: a hit serves from the
//! synthetic `cache` pseudo-provider with `gen_ai.system = "cache"`,
//! `tokentrimmer.cache = "hit-l1"`/`"hit-l2"`, `tokentrimmer.cost_usd = 0`, and
//! `saved_usd == baseline_cost_usd` (100% TokenTrimmer-attributed savings).
//! These are the exact attributes the dashboard's cache-hit-rate and savings
//! panels query, so the hit span path is the most dashboard-relevant one.
//!
//! Hermetic: an in-memory OTel span exporter captures the request span; a
//! trivial mock provider returns a fixed token usage so the cost is
//! deterministic. No network, no collector, no real upstream.
//!
//! Span capture relies on a thread-local `tracing-opentelemetry` subscriber
//! installed by `with_default`, so every test here runs on its own
//! current-thread runtime and this file holds **only** `#[test]` functions (no
//! `#[tokio::test]`): mixing in multi-threaded tokio tests would poll futures
//! on worker threads where the subscriber is not in scope and the request span
//! would silently escape the exporter.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::{Duration as ChronoDuration, Utc};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;
use uuid::Uuid;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::Value;
use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::TracerProvider;
use tracing_subscriber::prelude::*;

use tt_auth::ApiKeyContext;
use tt_cache::{memory::InMemoryL1Cache, CacheEntry, EmbeddingProvider, InMemoryL2Cache, L2Cache};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{
        Choice, ChunkChoice, ChunkDelta, EmbeddingData, EmbeddingsRequest, EmbeddingsResponse,
        Message, MessageContent,
    },
    pricing::Capability,
    CallerTier, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo,
    ModelPricing, Provider, ProviderError, RequestContext, Usage,
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
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        // Two chunks: a content delta, then a terminal chunk carrying the same
        // fixed authoritative usage as the non-streaming path so the streamed
        // cost is deterministic and matches `request_span_carries_*`.
        use tt_shared::messages::{ChunkChoice, ChunkDelta};
        let content = ChatCompletionChunk {
            id: "chatcmpl-cost-stream".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: req.model.clone(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: Some("ok".into()),
                    tool_calls: vec![],
                    extra: Default::default(),
                },
                finish_reason: None,
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        };
        let terminal = ChatCompletionChunk {
            id: "chatcmpl-cost-stream".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: req.model,
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: Some("stop".into()),
                extra: Default::default(),
            }],
            usage: Some(Usage {
                prompt_tokens: PROMPT_TOKENS,
                completion_tokens: COMPLETION_TOKENS,
                total_tokens: PROMPT_TOKENS + COMPLETION_TOKENS,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            }),
            extra: Default::default(),
        };
        Ok(futures::stream::iter(vec![Ok(content), Ok(terminal)]).boxed())
    }
}

fn app() -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CostMock));
    build_router(AppState::new(registry))
}

fn chat_request(stream: bool) -> Request<Body> {
    let body = json!({
        "model": "gpt-test",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": stream,
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
///
/// The response body is fully drained before the span is read: on the streaming
/// path the gen_ai.*/cost attributes are recorded in the SSE drop guard (once
/// the per-request cost is known), which only fires when the body is consumed.
fn run_request_capturing_attrs(stream: bool) -> HashMap<String, Value> {
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
        rt.block_on(async {
            let resp = app().oneshot(chat_request(stream)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "route should return 200");
            // Drain the body so the SSE GuardedStream (and its DropGuard) drops,
            // recording the streaming span attributes. Harmless for non-stream.
            let _ = axum::body::to_bytes(resp.into_body(), usize::MAX).await;
        });
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
    let attrs = run_request_capturing_attrs(false);

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

/// A streaming `POST /v1/chat/completions` (`stream: true`) must leave the same
/// gen_ai.* + cost attributes on the `http_request` span as the non-streaming
/// path. Cost is computed asynchronously in the SSE drop guard, so this guards
/// against streaming traffic being silently dropped from the spend/savings/
/// tokens/cache dashboards.
#[test]
fn streaming_request_span_carries_gen_ai_and_cost_attributes() {
    let attrs = run_request_capturing_attrs(true);

    // GenAI semantic-convention attributes — identical mapping to non-stream.
    assert_eq!(
        attrs.get("gen_ai.system"),
        Some(&Value::String("openai".into())),
        "streaming: gen_ai.system must map the provider id"
    );
    assert_eq!(
        attrs.get("gen_ai.operation.name"),
        Some(&Value::String("chat".into()))
    );
    assert_eq!(
        attrs.get("gen_ai.request.model"),
        Some(&Value::String("gpt-test".into()))
    );
    assert_eq!(
        attrs.get("gen_ai.response.model"),
        Some(&Value::String("gpt-test".into()))
    );
    assert_eq!(
        attrs.get("gen_ai.usage.input_tokens"),
        Some(&Value::I64(PROMPT_TOKENS as i64)),
        "streaming: input tokens come from the terminal usage chunk"
    );
    assert_eq!(
        attrs.get("gen_ai.usage.output_tokens"),
        Some(&Value::I64(COMPLETION_TOKENS as i64)),
        "streaming: output tokens come from the terminal usage chunk"
    );

    // TokenTrimmer cost attributes — same deterministic cost as non-stream.
    let expected_cost =
        (PROMPT_TOKENS as f64) * 1.0 / 1_000_000.0 + (COMPLETION_TOKENS as f64) * 2.0 / 1_000_000.0;
    match attrs.get("tokentrimmer.cost_usd") {
        Some(Value::F64(c)) => assert!(
            (c - expected_cost).abs() < 1e-12,
            "streaming tokentrimmer.cost_usd = {c}, expected {expected_cost}"
        ),
        other => panic!("streaming tokentrimmer.cost_usd should be an f64, got {other:?}"),
    }
    assert_eq!(
        attrs.get("tokentrimmer.saved_usd"),
        Some(&Value::F64(0.0)),
        "streaming: saved_usd is 0 with no routing/cache saving"
    );
    assert_eq!(
        attrs.get("tokentrimmer.cache"),
        Some(&Value::String("none".into())),
        "streaming: no cache layer configured → cache outcome 'none'"
    );
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

// ── Cache-hit span coverage ──────────────────────────────────────────────────
//
// The non-streaming MISS and streaming paths above prove the live-dispatch
// span. The L1/L2 HIT paths record DIFFERENT attributes (`gen_ai.system =
// cache`, `tokentrimmer.cache = hit-l1`/`hit-l2`, `cost_usd = 0`, `saved ==
// baseline`) and feed the dashboard's cache-hit-rate (panel keyed on
// `span.tokentrimmer.cache =~ "hit-.*"`) and savings panels — so they get their
// own span assertions here. A regression that recorded `miss` on a hit, or
// dropped the span call entirely, would be invisible to the header-only
// l1_cache_hit.rs / l2_cache_hit.rs suites but must fail below.

/// A provider that counts dispatches, so a cache hit can assert the provider
/// was bypassed. Prices counting-1 at $3/M input + $6/M output (matching the
/// header-level cache-hit suites) so the recorded baseline is comparable.
struct CountingProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &'static str {
        "counting"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "counting-1".into(),
            provider: "counting".into(),
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
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "chatcmpl-live".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("live from provider".into())),
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
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let chunks = vec![Ok(ChatCompletionChunk {
            id: "c1".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: "counting-1".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: Some("assistant".into()),
                    content: Some("hi".into()),
                    tool_calls: vec![],
                    extra: Default::default(),
                },
                finish_reason: Some("stop".into()),
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        })];
        Ok(futures::stream::iter(chunks).boxed())
    }
}

/// Deterministic embedder returning a fixed vector for every text, so the L2
/// lookup hits the primed entry at similarity 1.0.
struct FixedEmbedder {
    vec: Vec<f32>,
}

#[async_trait]
impl EmbeddingProvider for FixedEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, tt_cache::EmbedError> {
        Ok(self.vec.clone())
    }
    fn model(&self) -> &str {
        "fixed-embed"
    }
}

fn cache_chat_request(tier: Option<CallerTier>) -> Request<Body> {
    cache_chat_request_stream(tier, false)
}

/// Like [`cache_chat_request`] but lets the caller request a streaming response,
/// so the fake-stream L1-hit path (a `stream: true` request that hits L1) can be
/// exercised.
fn cache_chat_request_stream(tier: Option<CallerTier>, stream: bool) -> Request<Body> {
    let body = json!({
        "model": "counting-1",
        "messages": [{ "role": "user", "content": "hello there" }],
        "stream": stream,
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    // L2 lookup is a paid-tier entitlement; stamp a tier so the hit can fire.
    // (The auth middleware leaves a pre-inserted extension intact when no key
    // store is wired, as in these tests.)
    if let Some(t) = tier {
        req.extensions_mut().insert(ApiKeyContext {
            key_id: Uuid::nil(),
            org_id: Uuid::nil(),
            tier: Some(t),
        });
    }
    req
}

/// Assert the gen_ai.* + cost attributes common to every cache HIT: served from
/// the synthetic `cache` pseudo-provider, no routing (request == response
/// model), cached 100/50 usage, zero cost, and full savings against `baseline`.
fn assert_cache_hit_attrs(attrs: &HashMap<String, Value>, cache_outcome: &str, baseline: f64) {
    assert_eq!(
        attrs.get("gen_ai.system"),
        Some(&Value::String("cache".into())),
        "a cache hit serves from the `cache` pseudo-provider → gen_ai.system = cache"
    );
    assert_eq!(
        attrs.get("gen_ai.operation.name"),
        Some(&Value::String("chat".into()))
    );
    assert_eq!(
        attrs.get("gen_ai.request.model"),
        Some(&Value::String("counting-1".into()))
    );
    assert_eq!(
        attrs.get("gen_ai.response.model"),
        Some(&Value::String("counting-1".into())),
        "no routing on a hit → response model equals the cached model"
    );
    assert_eq!(
        attrs.get("gen_ai.usage.input_tokens"),
        Some(&Value::I64(100))
    );
    assert_eq!(
        attrs.get("gen_ai.usage.output_tokens"),
        Some(&Value::I64(50))
    );

    assert_eq!(
        attrs.get("tokentrimmer.cost_usd"),
        Some(&Value::F64(0.0)),
        "a cache hit costs nothing — no provider call"
    );
    match attrs.get("tokentrimmer.baseline_cost_usd") {
        Some(Value::F64(b)) => assert!(
            (b - baseline).abs() < 1e-9,
            "{cache_outcome} baseline = {b}, expected {baseline}"
        ),
        other => panic!("tokentrimmer.baseline_cost_usd should be f64, got {other:?}"),
    }
    // saved == baseline (100% TT-attributed, cost is 0).
    match attrs.get("tokentrimmer.saved_usd") {
        Some(Value::F64(s)) => assert!(
            (s - baseline).abs() < 1e-9,
            "{cache_outcome} saved = {s} must equal baseline {baseline} (cost is 0)"
        ),
        other => panic!("tokentrimmer.saved_usd should be f64, got {other:?}"),
    }
    assert_eq!(
        attrs.get("tokentrimmer.cache"),
        Some(&Value::String(cache_outcome.to_string().into())),
        "the dashboard cache-hit-rate panel keys on tokentrimmer.cache"
    );
}

/// Find the `http_request` span carrying the given `tokentrimmer.cache` outcome
/// among the exporter's finished spans, returning its attributes as a map.
fn cache_hit_span_attrs(
    exporter: &InMemorySpanExporter,
    cache_outcome: &str,
) -> HashMap<String, Value> {
    exporter
        .get_finished_spans()
        .expect("finished spans")
        .into_iter()
        .filter(|s| s.name == "http_request")
        .map(|s| {
            s.attributes
                .into_iter()
                .map(|kv| (kv.key.to_string(), kv.value))
                .collect::<HashMap<_, _>>()
        })
        .find(|a| {
            a.get("tokentrimmer.cache") == Some(&Value::String(cache_outcome.to_string().into()))
        })
        .unwrap_or_else(|| {
            panic!("an http_request span with tokentrimmer.cache = {cache_outcome} should exist")
        })
}

/// An L1 cache HIT must leave the gen_ai.* + cost attributes on the
/// `http_request` span with `tokentrimmer.cache = "hit-l1"`. The first (miss)
/// request primes L1; the second (hit) request is the span under test.
#[test]
fn l1_hit_span_carries_gen_ai_and_cost_attributes() {
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("l1-hit-span-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(subscriber, || {
        rt.block_on(async {
            let mut registry = ProviderRegistry::new();
            registry.register(Arc::new(CountingProvider {
                calls: Arc::clone(&calls),
            }));
            let l1 = Arc::new(InMemoryL1Cache::new());
            let state = AppState::new(registry).with_l1(l1, None);
            let app = build_router(state);

            // First request — miss, primes L1.
            let r1 = app.clone().oneshot(cache_chat_request(None)).await.unwrap();
            assert_eq!(r1.status(), StatusCode::OK);
            let _ = to_bytes(r1.into_body(), 8 * 1024).await.unwrap();

            // Allow the spawned L1 insert to land.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            // Second request — L1 hit, the span under test.
            let r2 = app.oneshot(cache_chat_request(None)).await.unwrap();
            assert_eq!(r2.status(), StatusCode::OK);
            assert_eq!(
                r2.headers()
                    .get("x-tokentrimmer-cache")
                    .and_then(|v| v.to_str().ok()),
                Some("hit-l1"),
                "second request should hit L1"
            );
            let _ = to_bytes(r2.into_body(), 8 * 1024).await.unwrap();
        });
    });
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider must NOT be called on the L1 hit"
    );

    provider.force_flush();
    let attrs = cache_hit_span_attrs(&exporter, "hit-l1");
    // Envelope baseline = CountingProvider catalog ($3/M input + $6/M output)
    // over the cached 100/50 usage.
    let expected_baseline = (100.0 * 3.0 + 50.0 * 6.0) / 1_000_000.0;
    assert_cache_hit_attrs(&attrs, "hit-l1", expected_baseline);
}

/// A *streaming* L1 cache HIT (`stream: true` request served from the
/// fake-stream path) must also leave the gen_ai.* + cost attributes on the
/// `http_request` span with `tokentrimmer.cache = "hit-l1"`. This path passes
/// `log_ctx = None` to `stream_response` (the hit is logged separately), so the
/// SSE drop guard records nothing — the attributes are stamped synchronously at
/// the fake-stream call site. Without that, every streaming cache hit would be
/// invisible to the dashboards (the gap this test guards). The first
/// (non-streaming) miss primes L1; the streaming hit shares the cache key and is
/// the span under test.
#[test]
fn streaming_l1_hit_span_carries_gen_ai_and_cost_attributes() {
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("streaming-l1-hit-span-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(subscriber, || {
        rt.block_on(async {
            let mut registry = ProviderRegistry::new();
            registry.register(Arc::new(CountingProvider {
                calls: Arc::clone(&calls),
            }));
            let l1 = Arc::new(InMemoryL1Cache::new());
            let state = AppState::new(registry).with_l1(l1, None);
            let app = build_router(state);

            // First request — non-streaming miss, primes L1 with deterministic
            // 100/50 usage and a known catalog baseline. Streaming and
            // non-streaming variants of the same prompt share the L1 key.
            let r1 = app.clone().oneshot(cache_chat_request(None)).await.unwrap();
            assert_eq!(r1.status(), StatusCode::OK);
            let _ = to_bytes(r1.into_body(), 8 * 1024).await.unwrap();

            // Allow the spawned L1 insert to land.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            // Second request — STREAMING L1 hit via the fake-stream path. Note
            // the fake-stream path does not stamp the `x-tokentrimmer-cache`
            // response header (unlike the non-streaming hit builders); the hit is
            // confirmed below by the provider NOT being re-dispatched (calls == 1)
            // and the recorded `tokentrimmer.cache = hit-l1` span attribute.
            let r2 = app
                .oneshot(cache_chat_request_stream(None, true))
                .await
                .unwrap();
            assert_eq!(r2.status(), StatusCode::OK);
            // Drain the SSE body so the response (and any guard) is fully
            // consumed, matching how a real client reads the stream.
            let _ = to_bytes(r2.into_body(), 8 * 1024).await.unwrap();
        });
    });
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "provider must NOT be re-dispatched on the streaming L1 hit"
    );

    provider.force_flush();
    let attrs = cache_hit_span_attrs(&exporter, "hit-l1");
    // Same envelope baseline as the non-streaming L1-hit test.
    let expected_baseline = (100.0 * 3.0 + 50.0 * 6.0) / 1_000_000.0;
    assert_cache_hit_attrs(&attrs, "hit-l1", expected_baseline);
}

/// An L2 cache HIT must leave the gen_ai.* + cost attributes on the
/// `http_request` span with `tokentrimmer.cache = "hit-l2"`, and the baseline
/// must be the ROW's stored catalog baseline (distinct from the $1/M·$2/M
/// placeholder, proving the hit reads the stored value).
#[test]
fn l2_hit_span_carries_gen_ai_and_cost_attributes() {
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("l2-hit-span-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Stored catalog baseline distinct from the $1/M·$2/M placeholder.
    let stored_baseline = 0.000777_f64;
    let calls = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::with_default(subscriber, || {
        rt.block_on(async {
            let mut registry = ProviderRegistry::new();
            registry.register(Arc::new(CountingProvider {
                calls: Arc::clone(&calls),
            }));
            let cache = Arc::new(InMemoryL2Cache::new());

            let primed = ChatCompletionResponse {
                id: "chatcmpl-cached".into(),
                object: "chat.completion".into(),
                created: 0,
                model: "counting-1".into(),
                choices: vec![Choice {
                    index: 0,
                    message: Message::Assistant {
                        content: Some(MessageContent::Text("from cache".into())),
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
                },
            };
            let entry_vec = vec![1.0; 1536];
            let entry = CacheEntry {
                id: Uuid::now_v7(),
                org_id: Uuid::nil(),
                embedding: entry_vec.clone(),
                response: serde_json::to_vec(&primed).unwrap(),
                model: "counting-1".into(),
                embedding_model: "fixed-embed".into(),
                input_tokens: 100,
                output_tokens: 50,
                baseline_cost_usd: Some(stored_baseline),
                hit_count: 0,
                created_at: Utc::now(),
                expires_at: Utc::now() + ChronoDuration::hours(1),
            };
            cache.insert(entry).await.unwrap();

            let embedder = Arc::new(FixedEmbedder { vec: entry_vec });
            let state = AppState::new(registry).with_l2(cache.clone(), embedder, Some(0.99));
            let app = build_router(state);

            // Pro-tier so the paid-only L2 lookup fires.
            let resp = app
                .oneshot(cache_chat_request(Some(CallerTier::Pro)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(
                resp.headers()
                    .get("x-tokentrimmer-cache")
                    .and_then(|v| v.to_str().ok()),
                Some("hit-l2")
            );
            let _ = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        });
    });
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "provider must NOT be called on the L2 hit"
    );

    provider.force_flush();
    let attrs = cache_hit_span_attrs(&exporter, "hit-l2");
    assert_cache_hit_attrs(&attrs, "hit-l2", stored_baseline);
    // Prove the baseline is NOT the legacy $1/M·$2/M placeholder.
    let placeholder = (100.0 * 1.0 + 50.0 * 2.0) / 1_000_000.0;
    match attrs.get("tokentrimmer.baseline_cost_usd") {
        Some(Value::F64(b)) => assert!(
            (b - placeholder).abs() > 1e-9,
            "L2 hit baseline must be the stored row value, not the placeholder {placeholder}"
        ),
        other => panic!("tokentrimmer.baseline_cost_usd should be f64, got {other:?}"),
    }
}

// ─── Embeddings ────────────────────────────────────────────────────────────────
//
// `POST /v1/embeddings` records the same span attributes as the chat path but
// with `gen_ai.operation.name = embeddings` and zero output tokens. This keeps
// embeddings traffic visible in the spend/savings/tokens dashboards. The
// attribute is stamped at the embeddings success path (`embeddings.rs`).

const EMB_PROMPT_TOKENS: u64 = 1_000;

/// Embedding provider with a fixed token usage so cost is deterministic:
/// $0.13/1M input, no output → cost = 1000 * 0.13e-6 USD.
struct EmbeddingMock;

#[async_trait]
impl Provider for EmbeddingMock {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "text-embedding-3-small".into(),
            provider: "openai".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 0,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.13,
            output_per_million: 0.0,
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
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        Err(ProviderError::Unsupported("chat not used".into()))
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
        req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Ok(EmbeddingsResponse {
            object: "list".into(),
            data: vec![EmbeddingData {
                object: "embedding".into(),
                index: 0,
                embedding: vec![0.1, 0.2],
            }],
            model: req.model,
            usage: Usage {
                prompt_tokens: EMB_PROMPT_TOKENS,
                completion_tokens: 0,
                total_tokens: EMB_PROMPT_TOKENS,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
            },
        })
    }
}

fn embeddings_request() -> Request<Body> {
    let body = json!({
        "model": "text-embedding-3-small",
        "input": "hello there",
    });
    Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// A `POST /v1/embeddings` request must record the gen_ai.* + cost attributes on
/// the `http_request` span with `gen_ai.operation.name = embeddings`, zero output
/// tokens, and the deterministic embedding cost — so embeddings traffic is not
/// dropped from the spend/tokens dashboards.
#[test]
fn embeddings_request_span_carries_gen_ai_and_cost_attributes() {
    let exporter = InMemorySpanExporter::default();
    let provider = TracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let tracer = provider.tracer("embeddings-span-test");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(otel_layer);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let attrs = tracing::subscriber::with_default(subscriber, || {
        rt.block_on(async {
            let mut registry = ProviderRegistry::new();
            registry.register(Arc::new(EmbeddingMock));
            let app = build_router(AppState::new(registry));
            let resp = app.oneshot(embeddings_request()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let _ = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
        });

        provider.force_flush();
        exporter
            .get_finished_spans()
            .expect("finished spans")
            .into_iter()
            .find(|s| s.name == "http_request")
            .expect("the gateway request span 'http_request' should be recorded")
            .attributes
            .into_iter()
            .map(|kv| (kv.key.to_string(), kv.value))
            .collect::<HashMap<_, _>>()
    });

    assert_eq!(
        attrs.get("gen_ai.operation.name"),
        Some(&Value::String("embeddings".into())),
        "the embeddings route must stamp operation = embeddings"
    );
    assert_eq!(
        attrs.get("gen_ai.system"),
        Some(&Value::String("openai".into()))
    );
    assert_eq!(
        attrs.get("gen_ai.request.model"),
        Some(&Value::String("text-embedding-3-small".into()))
    );
    assert_eq!(
        attrs.get("gen_ai.response.model"),
        Some(&Value::String("text-embedding-3-small".into())),
        "no routing → response model equals the requested model"
    );
    assert_eq!(
        attrs.get("gen_ai.usage.input_tokens"),
        Some(&Value::I64(EMB_PROMPT_TOKENS as i64))
    );
    assert_eq!(
        attrs.get("gen_ai.usage.output_tokens"),
        Some(&Value::I64(0)),
        "embeddings produce no output tokens"
    );

    let expected_cost = EMB_PROMPT_TOKENS as f64 * 0.13 / 1_000_000.0;
    match attrs.get("tokentrimmer.cost_usd") {
        Some(Value::F64(c)) => assert!(
            (c - expected_cost).abs() < 1e-12,
            "embeddings tokentrimmer.cost_usd = {c}, expected {expected_cost}"
        ),
        other => panic!("tokentrimmer.cost_usd should be f64, got {other:?}"),
    }
    assert_eq!(
        attrs.get("tokentrimmer.cache"),
        Some(&Value::String("none".into())),
        "embeddings are never cache-served → cache outcome 'none'"
    );
}
