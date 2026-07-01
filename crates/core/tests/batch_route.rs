//! End-to-end (hermetic) tests for the ADVISORY batch-eligibility route action
//! (`RouteAction::batch`, research Phase 2.1):
//!
//! (a) a route with `batch: true` + an eligible model → the request is served
//!     and billed normally, the `batch_deferred_unavailable` warning is
//!     emitted, the FORGONE Batch-API discount (standard − batch at the served
//!     model's REAL catalog batch rate) rides the dedicated
//!     `X-TokenTrimmer-Batch-Forgone-Usd` header + the request_logs row, and
//!     the upstream request bytes are NEVER altered (unlike flex's
//!     `service_tier` injection);
//! (b) the marker changes NO realized figure — cost/saved headers equal a
//!     control run without the marker;
//! (c)/(d) HARD ineligibility, enforced in code: a streaming request or an
//!     interactive client (`X-TokenTrimmer-Interactive: 1`) clears the marker
//!     with a `batch_ineligible:<reason>` warning, row stays false/0;
//! (e) a served model with no catalog batch tier is not marked
//!     (`batch_not_available:<model>`) — no real rate, no claim; fail-open 200.
//!
//! No network: a mock provider records every upstream request VERBATIM
//! (serialized JSON) and returns a fixed response, mirroring `flex_route.rs`.
//! The mock prices a batch-eligible model with batch rates at exactly 50% of
//! standard and an ineligible model without, exercising the catalog-driven
//! gate (`ModelPricing::batch_eligible`) and the realized-minus-batch forgone
//! delta exactly.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, ChunkChoice, ChunkDelta, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use tt_telemetry::request_logs::{InMemoryRequestLogWriter, RequestLogWriter};
use uuid::Uuid;

const PROMPT_TOKENS: u64 = 1000;
const COMPLETION_TOKENS: u64 = 500;

/// Expected figures for usage prompt=1000, completion=500:
///   standard = 1000×$10/M + 500×$30/M = 0.01 + 0.015  = 0.025
///   batch    = 1000×$5/M  + 500×$15/M = 0.005 + 0.0075 = 0.0125
///   forgone  = standard − batch                        = 0.0125
fn expected_standard() -> f64 {
    (PROMPT_TOKENS as f64) * 10.0 / 1e6 + (COMPLETION_TOKENS as f64) * 30.0 / 1e6
}
fn expected_batch() -> f64 {
    (PROMPT_TOKENS as f64) * 5.0 / 1e6 + (COMPLETION_TOKENS as f64) * 15.0 / 1e6
}

/// Mock provider that records every upstream request VERBATIM (serialized
/// JSON, so any mutation — a `service_tier`, a `batch` field — would show).
/// Prices a batch-eligible model (`batch-eligible`) with batch rates at 50% of
/// standard and an ineligible model (`batch-ineligible`) without.
struct BatchRecordingProvider {
    /// Serialized upstream request per call.
    requests: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for BatchRecordingProvider {
    fn id(&self) -> &'static str {
        "batchrec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["batch-eligible", "batch-ineligible"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "batchrec".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 8192,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        // Standard $10/$30 per 1M for both; the eligible model also carries
        // batch rates at exactly 50% ($5/$15) — eligibility is the presence of
        // the batch rate, mirroring the real catalog gate.
        let (batch_in, batch_out) = match model {
            "batch-eligible" => (Some(5.0), Some(15.0)),
            _ => (None, None),
        };
        Some(ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: batch_in,
            batch_output_per_million: batch_out,
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&req).expect("serialize upstream request"));
        Ok(ChatCompletionResponse {
            id: "chatcmpl-batch".into(),
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
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&req).expect("serialize upstream request"));
        let chunks = vec![
            Ok(ChatCompletionChunk {
                id: "chatcmpl-batch-stream".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: req.model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        content: Some("ok".into()),
                        ..Default::default()
                    },
                    finish_reason: None,
                    extra: Default::default(),
                }],
                usage: None,
                extra: Default::default(),
            }),
            Ok(ChatCompletionChunk {
                id: "chatcmpl-batch-stream".into(),
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
                    cache_read_input_tokens: None,
                }),
                extra: Default::default(),
            }),
        ];
        Ok(futures::stream::iter(chunks).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

fn chat_request(model: &str, bearer: &str) -> Request<Body> {
    chat_request_with(model, bearer, false, false)
}

/// Build the request, optionally streaming and/or carrying the
/// `X-TokenTrimmer-Interactive: 1` client declaration.
fn chat_request_with(model: &str, bearer: &str, stream: bool, interactive: bool) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello world" }],
        "stream": stream,
    });
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"));
    if interactive {
        builder = builder.header("x-tokentrimmer-interactive", "1");
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// Like [`chat_request_with`], but with an arbitrary raw value on the
/// `X-TokenTrimmer-Interactive` header — for the fail-safe parsing tests.
fn chat_request_with_interactive_value(model: &str, bearer: &str, value: &str) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello world" }],
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .header("x-tokentrimmer-interactive", value)
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(
        store,
        &audit,
        org_id,
        "test-key",
        Environment::Live,
        Actor::System,
    )
    .await
    .expect("issue tt_live_ key")
    .plaintext
}

/// Test handles for one hermetic gateway: app + key + the recorded upstream
/// requests + call counter + the captured request_logs rows.
struct Harness {
    app: axum::Router,
    key: String,
    requests: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    log_writer: Arc<InMemoryRequestLogWriter>,
}

/// Build a gateway whose org has a single route matching `model_in` with the
/// given `batch` action value (no model rewrite — batch is a pure marker).
async fn app_with_batch_route(model_in: &str, batch: bool) -> Harness {
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(BatchRecordingProvider {
        requests: Arc::clone(&requests),
        calls: Arc::clone(&calls),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(
        org_id,
        vec![Route {
            paused: false,
            id: Uuid::now_v7(),
            name: "batch-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec![model_in.into()],
                ..Default::default()
            },
            then: RouteAction {
                format_switch: None,
                diff: false,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                agentic_budget: None,
                target_model: Some(model_in.into()),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch,
                compress: false,
                doc_compaction: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                panel: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let log_writer = Arc::new(InMemoryRequestLogWriter::new());
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing)
            .with_request_log_writer(log_writer.clone() as Arc<dyn RequestLogWriter>),
    );
    Harness {
        app,
        key: plaintext,
        requests,
        calls,
        log_writer,
    }
}

/// Poll the in-memory writer until the fire-and-forget request_logs row lands.
async fn wait_for_row(
    writer: &InMemoryRequestLogWriter,
) -> tt_telemetry::request_logs::RequestLogRow {
    for _ in 0..100 {
        if !writer.rows().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let rows = writer.rows();
    assert_eq!(rows.len(), 1, "expected exactly one telemetry row");
    rows[0].clone()
}

fn header_str<'r>(resp: &'r axum::response::Response, name: &str) -> &'r str {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn header_f64(resp: &axum::response::Response, name: &str) -> f64 {
    header_str(resp, name)
        .parse::<f64>()
        .unwrap_or_else(|_| panic!("header {name} missing or not a number"))
}

fn warnings_of(resp: &axum::response::Response) -> Vec<String> {
    header_str(resp, "x-tokentrimmer-warnings")
        .split(',')
        .map(str::to_string)
        .collect()
}

/// (a) Marked request: served + billed normally, `batch_deferred_unavailable`
/// warning, forgone == standard − batch (at the REAL mock catalog batch rate)
/// to 1e-6 on header AND row, and the upstream request bytes are untouched.
#[tokio::test]
async fn marked_request_attributes_forgone_discount() {
    let h = app_with_batch_route("batch-eligible", true).await;

    let resp = h
        .app
        .oneshot(chat_request("batch-eligible", &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.calls.load(Ordering::Relaxed), 1, "dispatched normally");

    // Advisory warning: the gateway is synchronous — nothing detoured.
    assert!(
        warnings_of(&resp)
            .iter()
            .any(|w| w == "batch_deferred_unavailable"),
        "expected batch_deferred_unavailable, got: {:?}",
        warnings_of(&resp)
    );

    // Forgone = standard − batch, to 1e-6 (0.025 − 0.0125 = 0.0125).
    let expected_forgone = expected_standard() - expected_batch();
    let forgone = header_f64(&resp, "x-tokentrimmer-batch-forgone-usd");
    assert!(
        (forgone - expected_forgone).abs() < 1e-6,
        "forgone ({forgone}) should equal standard − batch ({expected_forgone})"
    );

    // The row carries the marker + the same priced figure.
    let row = wait_for_row(&h.log_writer).await;
    assert!(row.batch_eligible, "row must be tagged batch-eligible");
    assert!(
        (row.batch_forgone_usd - expected_forgone).abs() < 1e-6,
        "row forgone ({}) should equal header figure",
        row.batch_forgone_usd
    );

    // The upstream request is byte-for-byte unmutated: no service_tier, no
    // batch field — the marker NEVER rides to the provider.
    let recorded = h.requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert!(
        !recorded[0].contains("service_tier") && !recorded[0].contains("\"batch\":"),
        "upstream request must not be mutated by the marker: {}",
        recorded[0]
    );
}

/// (b) The marker changes NO realized figure: cost + headline saved equal a
/// control run with `batch: false` (forgone is never folded into either), and
/// the upstream request bytes are identical between the two runs.
#[tokio::test]
async fn saved_usd_and_cost_unchanged_by_marker() {
    let marked = app_with_batch_route("batch-eligible", true).await;
    let control = app_with_batch_route("batch-eligible", false).await;

    let marked_resp = marked
        .app
        .oneshot(chat_request("batch-eligible", &marked.key))
        .await
        .unwrap();
    let control_resp = control
        .app
        .oneshot(chat_request("batch-eligible", &control.key))
        .await
        .unwrap();
    assert_eq!(marked_resp.status(), StatusCode::OK);
    assert_eq!(control_resp.status(), StatusCode::OK);

    for name in ["x-tokentrimmer-cost-usd", "x-tokentrimmer-saved-usd"] {
        assert_eq!(
            header_str(&marked_resp, name),
            header_str(&control_resp, name),
            "{name} must be unchanged by the advisory marker"
        );
    }
    // The control run reports no forgone discount; the marked run does — on
    // its OWN header only.
    assert_eq!(
        header_str(&control_resp, "x-tokentrimmer-batch-forgone-usd"),
        "0.000000"
    );
    assert!(header_f64(&marked_resp, "x-tokentrimmer-batch-forgone-usd") > 0.0);

    // Byte-identical upstream dispatch in both runs.
    let m = marked.requests.lock().unwrap();
    let c = control.requests.lock().unwrap();
    assert_eq!(m[0], c[0], "marker must not alter the upstream request");
}

/// (c) HARD ineligibility: a STREAMING request clears the marker with
/// `batch_ineligible:streaming` (and no advisory warning); the row stays
/// false/0 — a ≤24h batch window cannot serve a live stream.
#[tokio::test]
async fn streaming_request_clears_marker() {
    let h = app_with_batch_route("batch-eligible", true).await;

    let resp = h
        .app
        .oneshot(chat_request_with("batch-eligible", &h.key, true, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "batch_ineligible:streaming"),
        "expected batch_ineligible:streaming, got: {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w == "batch_deferred_unavailable"),
        "a cleared marker must not also claim deferral: {warnings:?}"
    );

    // Drain the SSE body so the drop-guard writes the streamed row.
    let _ = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let row = wait_for_row(&h.log_writer).await;
    assert!(!row.batch_eligible, "streamed row must never be eligible");
    assert_eq!(row.batch_forgone_usd, 0.0);
}

/// (d) HARD ineligibility: an interactive client (`X-TokenTrimmer-Interactive:
/// 1` — what `tt chat` and the /tools loop always send) clears the marker with
/// `batch_ineligible:interactive`; the row stays false/0.
#[tokio::test]
async fn interactive_header_clears_marker() {
    let h = app_with_batch_route("batch-eligible", true).await;

    let resp = h
        .app
        .oneshot(chat_request_with("batch-eligible", &h.key, false, true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "batch_ineligible:interactive"),
        "expected batch_ineligible:interactive, got: {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w == "batch_deferred_unavailable"),
        "a cleared marker must not also claim deferral: {warnings:?}"
    );
    assert_eq!(
        header_str(&resp, "x-tokentrimmer-batch-forgone-usd"),
        "0.000000"
    );

    let row = wait_for_row(&h.log_writer).await;
    assert!(!row.batch_eligible);
    assert_eq!(row.batch_forgone_usd, 0.0);
}

/// (d2) Fail-SAFE parsing: an unrecognized truthy spelling of the interactive
/// header (`X-TokenTrimmer-Interactive: yes`) still clears the marker — any
/// non-empty value other than an explicit `0`/`false` opt-out errs toward
/// "a human is waiting", never toward batch-markable.
#[tokio::test]
async fn unrecognized_interactive_value_clears_marker_fail_safe() {
    let h = app_with_batch_route("batch-eligible", true).await;

    let resp = h
        .app
        .oneshot(chat_request_with_interactive_value(
            "batch-eligible",
            &h.key,
            "yes",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "batch_ineligible:interactive"),
        "unrecognized truthy value must fail interactive-safe, got: {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w == "batch_deferred_unavailable"),
        "a cleared marker must not also claim deferral: {warnings:?}"
    );
    assert_eq!(
        header_str(&resp, "x-tokentrimmer-batch-forgone-usd"),
        "0.000000"
    );

    let row = wait_for_row(&h.log_writer).await;
    assert!(!row.batch_eligible);
    assert_eq!(row.batch_forgone_usd, 0.0);
}

/// (d3) Explicit opt-out: `X-TokenTrimmer-Interactive: 0` (and only the
/// documented falsy spellings) declares "no human waiting" — the marker
/// applies exactly as with no header at all.
#[tokio::test]
async fn explicit_interactive_opt_out_allows_marker() {
    let h = app_with_batch_route("batch-eligible", true).await;

    let resp = h
        .app
        .oneshot(chat_request_with_interactive_value(
            "batch-eligible",
            &h.key,
            "0",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "batch_deferred_unavailable"),
        "an explicit `0` opt-out must mark exactly like no header, got: {warnings:?}"
    );
    assert!(header_f64(&resp, "x-tokentrimmer-batch-forgone-usd") > 0.0);

    let row = wait_for_row(&h.log_writer).await;
    assert!(row.batch_eligible);
    assert!(row.batch_forgone_usd > 0.0);
}

/// (e) A served model with no catalog batch tier is not marked — no real rate,
/// no claim (`batch_not_available:<model>`, forgone 0) — and the request still
/// serves 200 (fail-open).
#[tokio::test]
async fn model_without_batch_tier_not_marked() {
    let h = app_with_batch_route("batch-ineligible", true).await;

    let resp = h
        .app
        .oneshot(chat_request("batch-ineligible", &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "fail-open: never a 5xx");

    let warnings = warnings_of(&resp);
    assert!(
        warnings
            .iter()
            .any(|w| w == "batch_not_available:batch-ineligible"),
        "expected batch_not_available:batch-ineligible, got: {warnings:?}"
    );
    assert_eq!(
        header_str(&resp, "x-tokentrimmer-batch-forgone-usd"),
        "0.000000",
        "no catalog batch rate → no fabricated 0.5x claim"
    );

    let row = wait_for_row(&h.log_writer).await;
    assert!(!row.batch_eligible);
    assert_eq!(row.batch_forgone_usd, 0.0);
}
