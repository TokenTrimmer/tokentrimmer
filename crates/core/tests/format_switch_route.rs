//! End-to-end (hermetic) tests for the opt-in FORMAT SWITCH route action
//! (`RouteAction::format_switch`, research Phase 3.3):
//!
//! (a) applied: the upstream request carries NO `response_format` and a
//!     trailing System instruction; the caller receives the stripped CSV; the
//!     warnings header advertises `format_switch:csv`; the estimated-saving
//!     header is > 0; the request_logs row records `format_switched='csv'`;
//! (b) fail-open: the model emits JSON anyway → the body passes through
//!     UNTOUCHED with `format_switch_failed:csv`, the estimate stays 0, and
//!     the response is NEVER inserted into L1 (a second identical request
//!     dispatches again);
//! (c) streaming request through the route → untouched + skipped:streaming;
//! (d) strict json_schema (grammar lock) → untouched + skipped:strict_schema;
//! (e) a VALIDATED switch IS L1-cached and the hit response carries the
//!     `format_switch:csv` token (hit-path advertisement);
//! (f) control: a route without the action leaves the upstream request
//!     byte-identical to an unrouted dispatch.
//!
//! No network: a mock provider records every upstream request VERBATIM and
//! returns a configurable body, mirroring `batch_route.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_cache::memory::InMemoryL1Cache;
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

const CLEAN_CSV: &str = "name,qty,price\nwidget,2,9.99\ngadget,1,12.50\ncable,7,3.25";
const JSON_ANYWAY: &str =
    r#"[{"name":"widget","qty":2,"price":9.99},{"name":"gadget","qty":1,"price":12.50}]"#;

/// Mock provider: records every upstream request VERBATIM (serialized JSON)
/// and returns the configured assistant body.
struct FsRecordingProvider {
    requests: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    body: Arc<Mutex<String>>,
    /// finish_reason for the non-streaming response ("stop" unless a test
    /// simulates a token-limit truncation with "length").
    finish: Arc<Mutex<String>>,
}

#[async_trait]
impl Provider for FsRecordingProvider {
    fn id(&self) -> &'static str {
        "fsrec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "fs-1".into(),
            provider: "fsrec".into(),
            // JsonMode: the test requests carry response_format, and the
            // routing capability guard requires the TARGET model to support
            // it (else the route is skipped wholesale).
            capabilities: vec![Capability::Text, Capability::JsonMode],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&req).expect("serialize upstream request"));
        Ok(ChatCompletionResponse {
            id: "chatcmpl-fs".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(self.body.lock().unwrap().clone())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some(self.finish.lock().unwrap().clone()),
            }],
            usage: Usage {
                prompt_tokens: 1000,
                completion_tokens: 40,
                total_tokens: 1040,
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
        let chunks = vec![Ok(ChatCompletionChunk {
            id: "chunk-fs".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: req.model,
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some("streamed".into()),
                    ..Default::default()
                },
                finish_reason: Some("stop".into()),
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        })];
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

struct Harness {
    app: axum::Router,
    key: String,
    requests: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    body: Arc<Mutex<String>>,
    finish: Arc<Mutex<String>>,
    log_writer: Arc<InMemoryRequestLogWriter>,
}

/// Build a gateway whose org has a single route matching `fs-1` with the
/// given `format_switch` action (no model rewrite), L1 wired.
async fn app_with_format_switch_route(format_switch: Option<&str>) -> Harness {
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let body = Arc::new(Mutex::new(CLEAN_CSV.to_string()));
    let finish = Arc::new(Mutex::new("stop".to_string()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(FsRecordingProvider {
        requests: Arc::clone(&requests),
        calls: Arc::clone(&calls),
        body: Arc::clone(&body),
        finish: Arc::clone(&finish),
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
            name: "fs-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["fs-1".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: "fs-1".into(),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                redact: false,
                format_switch: format_switch.map(str::to_string),
                diff: false,
                traffic_pct: None,
                shadow_model: None,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
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
            .with_l1(Arc::new(InMemoryL1Cache::new()), None)
            .with_request_log_writer(log_writer.clone() as Arc<dyn RequestLogWriter>),
    );
    Harness {
        app,
        key: plaintext,
        requests,
        calls,
        body,
        finish,
        log_writer,
    }
}

fn records_schema() -> serde_json::Value {
    json!({
        "name": "items",
        "strict": false,
        "schema": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "qty": {"type": "integer"},
                    "price": {"type": "number"}
                },
                "required": ["name", "qty", "price"]
            }
        }
    })
}

fn chat_request(bearer: &str, stream: bool, strict: bool) -> Request<Body> {
    let mut schema = records_schema();
    if strict {
        schema["strict"] = json!(true);
    }
    let body = json!({
        "model": "fs-1",
        "messages": [{ "role": "user", "content": "list the inventory items" }],
        "stream": stream,
        "response_format": { "type": "json_schema", "json_schema": schema },
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

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
    assert!(!rows.is_empty(), "expected a telemetry row");
    rows[0].clone()
}

fn header_str<'r>(resp: &'r axum::response::Response, name: &str) -> &'r str {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn warnings_of(resp: &axum::response::Response) -> Vec<String> {
    header_str(resp, "x-tokentrimmer-warnings")
        .split(',')
        .map(str::to_string)
        .collect()
}

async fn body_content(resp: axum::response::Response) -> String {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
    v["choices"][0]["message"]["content"]
        .as_str()
        .expect("assistant content")
        .to_string()
}

/// (a) Applied: instruction + no response_format upstream; stripped CSV to
/// the caller; advertised; estimated header > 0; row marked.
#[tokio::test]
async fn applied_switch_strips_validates_and_advertises() {
    let h = app_with_format_switch_route(Some("csv")).await;

    let resp = h
        .app
        .oneshot(chat_request(&h.key, false, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.calls.load(Ordering::Relaxed), 1);

    // Upstream mutation: response_format dropped, trailing System
    // instruction appended (deterministic bytes). Scoped so the guard is
    // released before the awaits below.
    {
        let recorded = h.requests.lock().unwrap();
        assert!(
            !recorded[0].contains("response_format"),
            "switched dispatch must drop response_format: {}",
            recorded[0]
        );
        assert!(
            recorded[0].contains("Respond ONLY with CSV data")
                && recorded[0].contains("Header: name,qty,price"),
            "instruction must ride as a System message: {}",
            recorded[0]
        );
    }

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "format_switch:csv"),
        "every switched response must advertise the changed contract: {warnings:?}"
    );
    let est: f64 = header_str(&resp, "x-tokentrimmer-format-switch-saved-est-usd")
        .parse()
        .expect("est header");
    assert!(
        est > 0.0,
        "tokenizer-grounded estimate must be > 0, got {est}"
    );
    // The estimate NEVER rides the invoice-reconciled headline.
    assert_eq!(header_str(&resp, "x-tokentrimmer-saved-usd"), "0.000000");

    assert_eq!(body_content(resp).await, CLEAN_CSV);

    let row = wait_for_row(&h.log_writer).await;
    assert_eq!(row.format_switched.as_deref(), Some("csv"));
    assert!(row.format_switch_saved_est_usd > 0.0);
}

/// (b) Fail-open: the model emitted JSON anyway → untouched body,
/// `format_switch_failed:csv`, est 0, and the response is NOT L1-cached (a
/// second identical request dispatches again).
#[tokio::test]
async fn failed_validation_fails_open_and_is_never_cached() {
    let h = app_with_format_switch_route(Some("csv")).await;
    *h.body.lock().unwrap() = JSON_ANYWAY.to_string();

    let resp = h
        .app
        .clone()
        .oneshot(chat_request(&h.key, false, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "fail-open: never a 5xx");
    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "format_switch_failed:csv"),
        "expected format_switch_failed:csv, got {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w == "format_switch:csv"),
        "a failed switch must not also advertise success: {warnings:?}"
    );
    assert_eq!(
        header_str(&resp, "x-tokentrimmer-format-switch-saved-est-usd"),
        "0.000000",
        "no saving may be booked for a failed switch"
    );
    assert_eq!(body_content(resp).await, JSON_ANYWAY, "body untouched");

    // Allow any (wrongly) spawned insert to land, then re-issue: the second
    // request must DISPATCH (calls=2) — an unswitched body under the
    // switched key would make the hit-path advertisement a lie.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let resp2 = h
        .app
        .oneshot(chat_request(&h.key, false, false))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    assert_eq!(
        h.calls.load(Ordering::Relaxed),
        2,
        "fail-open bodies must never be served from the switched key"
    );
}

/// (b2) Truncation gate: a token-limit-cut emission (`finish_reason:
/// "length"`) fails OPEN even when the truncated text validates as clean
/// CSV — a cut landing exactly on a record-line boundary passes the
/// per-line arity check while silently dropping trailing records, whereas
/// the JSON contract being replaced would have been detectably invalid.
#[tokio::test]
async fn truncated_emission_fails_open_untouched() {
    let h = app_with_format_switch_route(Some("csv")).await;
    // A fenced-but-valid CSV body: if validation (wrongly) ran, the served
    // body would be the STRIPPED form — asserting the fenced bytes proves
    // the fail-open arm served the body untouched.
    let fenced = format!("```csv\n{CLEAN_CSV}\n```");
    *h.body.lock().unwrap() = fenced.clone();
    *h.finish.lock().unwrap() = "length".to_string();

    let resp = h
        .app
        .clone()
        .oneshot(chat_request(&h.key, false, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "fail-open: never a 5xx");
    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "format_switch_failed:csv"),
        "expected format_switch_failed:csv on a truncated emission, got {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w == "format_switch:csv"),
        "a truncated emission must never advertise a validated switch: {warnings:?}"
    );
    assert_eq!(
        header_str(&resp, "x-tokentrimmer-format-switch-saved-est-usd"),
        "0.000000",
        "no estimate may be booked for a truncated emission"
    );
    assert_eq!(
        body_content(resp).await,
        fenced,
        "the truncated body passes through UNTOUCHED (not stripped)"
    );

    // Same non-caching contract as every other fail-open: the second
    // identical request must dispatch again.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let resp2 = h
        .app
        .oneshot(chat_request(&h.key, false, false))
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    assert_eq!(
        h.calls.load(Ordering::Relaxed),
        2,
        "truncated fail-open bodies must never be cached under the switched key"
    );
}

/// (c) Streaming request through the route: untouched (response_format rides
/// upstream, no instruction) + `format_switch_skipped:streaming`.
#[tokio::test]
async fn streaming_request_skips_switch() {
    let h = app_with_format_switch_route(Some("csv")).await;

    let resp = h
        .app
        .oneshot(chat_request(&h.key, true, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warnings = warnings_of(&resp);
    assert!(
        warnings
            .iter()
            .any(|w| w == "format_switch_skipped:streaming"),
        "expected format_switch_skipped:streaming, got {warnings:?}"
    );

    let recorded = h.requests.lock().unwrap();
    assert!(
        recorded[0].contains("response_format"),
        "streaming dispatch must be untouched: {}",
        recorded[0]
    );
    assert!(
        !recorded[0].contains("Respond ONLY with CSV data"),
        "no instruction on the streaming path: {}",
        recorded[0]
    );
}

/// (d) Strict structured output (grammar lock) disables the switch.
#[tokio::test]
async fn strict_schema_skips_switch() {
    let h = app_with_format_switch_route(Some("csv")).await;

    let resp = h
        .app
        .oneshot(chat_request(&h.key, false, true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warnings = warnings_of(&resp);
    assert!(
        warnings
            .iter()
            .any(|w| w == "format_switch_skipped:strict_schema"),
        "expected format_switch_skipped:strict_schema, got {warnings:?}"
    );

    let recorded = h.requests.lock().unwrap();
    assert!(
        recorded[0].contains("\"strict\":true"),
        "strict response_format must ride upstream untouched: {}",
        recorded[0]
    );
}

/// (e) A VALIDATED switch IS L1-cached, and the hit response carries the
/// `format_switch:csv` advertisement (the hit-path token attachment).
#[tokio::test]
async fn validated_switch_is_cached_and_hit_advertises() {
    let h = app_with_format_switch_route(Some("csv")).await;

    let r1 = h
        .app
        .clone()
        .oneshot(chat_request(&h.key, false, false))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(h.calls.load(Ordering::Relaxed), 1);
    // Allow the spawned L1 insert to land.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let r2 = h
        .app
        .oneshot(chat_request(&h.key, false, false))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        header_str(&r2, "x-tokentrimmer-cache"),
        "hit-l1",
        "validated switch must be servable from L1"
    );
    assert_eq!(
        h.calls.load(Ordering::Relaxed),
        1,
        "the hit must not re-dispatch"
    );
    let warnings = warnings_of(&r2);
    assert!(
        warnings.iter().any(|w| w == "format_switch:csv"),
        "the changed contract must be advertised on EVERY response, hits included: {warnings:?}"
    );
    assert_eq!(
        body_content(r2).await,
        CLEAN_CSV,
        "hit body is the switched body"
    );
}

/// (f) Control: a matched route WITHOUT the action leaves the upstream
/// request byte-identical to an unrouted dispatch (zero-work default path).
#[tokio::test]
async fn control_route_without_action_is_byte_identical_upstream() {
    let with_route = app_with_format_switch_route(None).await;
    let resp = with_route
        .app
        .oneshot(chat_request(&with_route.key, false, false))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warnings = warnings_of(&resp);
    assert!(
        !warnings.iter().any(|w| w.starts_with("format_switch")),
        "no action → no shaping tokens: {warnings:?}"
    );

    // Unrouted gateway (no routing store at all), same client request.
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let body = Arc::new(Mutex::new(CLEAN_CSV.to_string()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(FsRecordingProvider {
        requests: Arc::clone(&requests),
        calls,
        body,
        finish: Arc::new(Mutex::new("stop".to_string())),
    }));
    let unrouted = build_router(AppState::new(registry));
    let r = unrouted
        .oneshot(chat_request("anything", false, false))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let routed_req = with_route.requests.lock().unwrap()[0].clone();
    let unrouted_req = requests.lock().unwrap()[0].clone();
    assert_eq!(
        routed_req, unrouted_req,
        "a route without the action must not perturb the upstream request"
    );
}
