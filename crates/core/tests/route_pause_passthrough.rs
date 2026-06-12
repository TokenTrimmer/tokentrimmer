//! Handler integration tests for sticky route pauses (research Phase 2.3).
//!
//! A PAUSED route still MATCHES — the request attributes to it (route_id
//! stamped, `x-tokentrimmer-warnings: route_paused:<name>`, the
//! `request_logs.route_paused` marker) — but the rewrite and every other COST
//! lever are suppressed: requests flow to the originally-requested model (the
//! EXPENSIVE, quality-safe direction), no shadow fires, and the routing
//! saving honestly books 0 (cost == baseline). SAFETY levers (`redact`,
//! `disable_cache`) stay live: pausing a quality gate must never disable a
//! privacy guardrail. A forced `X-TokenTrimmer-Route` header does NOT bypass
//! a pause, and a paused route is never judged (no downgrade → window
//! freezes → the pause is naturally sticky).

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
use tt_cache::memory::InMemoryL1Cache;
use tt_core::quality_sample::{JudgeConfig, JudgeOutcome, JudgeSink};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, NewRoutePause, PausedBy, Route, RouteAction,
    RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use tt_telemetry::request_logs::{InMemoryRequestLogWriter, RequestLogRow, RequestLogWriter};
use uuid::Uuid;

const ROUTE_NAME: &str = "downgrade-4o";
const JUDGE_MODEL: &str = "judge-cheap";

/// Provider that records every model it serves (primary, shadow, judge alike).
struct RecordingProvider {
    models_called: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    /// Last upstream messages, for the redaction assertion.
    last_messages: Arc<Mutex<Vec<Message>>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["gpt-4o", "gpt-4o-mini", JUDGE_MODEL]
            .into_iter()
            .map(|m| ModelInfo {
                id: m.into(),
                provider: "recording".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 4096,
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (input, output) = match model {
            "gpt-4o" => (5.0, 15.0),
            "gpt-4o-mini" => (0.15, 0.6),
            JUDGE_MODEL => (0.1, 0.4),
            _ => (1.0, 2.0),
        };
        Some(ModelPricing {
            input_per_million: input,
            output_per_million: output,
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.models_called.lock().unwrap().push(req.model.clone());
        *self.last_messages.lock().unwrap() = req.messages.clone();
        Ok(ChatCompletionResponse {
            id: "chatcmpl-pause".into(),
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
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
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
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// In-memory judge sink recording every outcome (must stay EMPTY on a paused
/// route).
#[derive(Default)]
struct RecordingSink {
    outcomes: Mutex<Vec<JudgeOutcome>>,
}

#[async_trait]
impl JudgeSink for RecordingSink {
    async fn record(&self, outcome: JudgeOutcome) {
        self.outcomes.lock().unwrap().push(outcome);
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    models_called: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    last_messages: Arc<Mutex<Vec<Message>>>,
    log_writer: Arc<InMemoryRequestLogWriter>,
    route_id: Uuid,
    judge_sink: Arc<RecordingSink>,
}

struct HarnessOpts {
    then: RouteAction,
    paused: bool,
    with_l1: bool,
    /// Wire the sampled judge at rate 1.0 (every eligible request judged).
    with_judge: bool,
}

impl Default for HarnessOpts {
    fn default() -> Self {
        Self {
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                ..Default::default()
            },
            paused: true,
            with_l1: false,
            with_judge: false,
        }
    }
}

/// Build a gateway whose org has one `gpt-4o`-matching route carrying `then`,
/// optionally sticky-paused through the production [`CachingRoutingStore`]
/// path (delegate + invalidate).
async fn harness(opts: HarnessOpts) -> Harness {
    let calls = Arc::new(AtomicUsize::new(0));
    let models_called = Arc::new(Mutex::new(Vec::new()));
    let last_messages = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        models_called: Arc::clone(&models_called),
        calls: Arc::clone(&calls),
        last_messages: Arc::clone(&last_messages),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let audit = InMemoryAuditWriter::new();
    let key = issue(
        &raw_store,
        &audit,
        org_id,
        "k",
        Environment::Live,
        Actor::System,
    )
    .await
    .unwrap()
    .plaintext;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    let route_id = Uuid::now_v7();
    routes_backing.set_routes(
        org_id,
        vec![Route {
            paused: false, // populated by the store, never by callers
            id: route_id,
            name: ROUTE_NAME.into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            then: opts.then,
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));
    if opts.paused {
        let newly = routing
            .pause_route(
                org_id,
                route_id,
                NewRoutePause {
                    paused_by: PausedBy::Manual,
                    reason: "test".into(),
                    pass_rate: None,
                    verdicts_in_window: None,
                },
            )
            .await
            .expect("pause");
        assert!(newly, "first pause must report newly-paused");
    }

    let log_writer = Arc::new(InMemoryRequestLogWriter::new());
    let mut state = AppState::new(registry)
        .with_key_store(key_store)
        .with_routing_store(routing)
        .with_request_log_writer(log_writer.clone() as Arc<dyn RequestLogWriter>);
    if opts.with_l1 {
        state = state.with_l1(Arc::new(InMemoryL1Cache::new()), None);
    }
    let judge_sink = Arc::new(RecordingSink::default());
    if opts.with_judge {
        state = state.with_quality_judge(
            judge_sink.clone() as Arc<dyn JudgeSink>,
            JudgeConfig {
                enabled: true,
                sample_rate: 1.0,
                judge_model: JUDGE_MODEL.to_string(),
                ..JudgeConfig::default()
            },
        );
    }

    Harness {
        app: build_router(state),
        key,
        models_called,
        calls,
        last_messages,
        log_writer,
        route_id,
        judge_sink,
    }
}

fn chat_request(model: &str, bearer: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(
            json!({
                "model": model,
                "messages": [{"role": "user", "content": "hello world"}],
                "stream": false,
            })
            .to_string(),
        ))
        .unwrap()
}

fn warnings_of(resp: &axum::http::Response<Body>) -> String {
    resp.headers()
        .get("x-tokentrimmer-warnings")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

/// Wait for the fire-and-forget request-log task, then snapshot the rows.
async fn rows_after(writer: &Arc<InMemoryRequestLogWriter>, n: usize) -> Vec<RequestLogRow> {
    for _ in 0..50 {
        let rows = writer.rows();
        if rows.len() >= n {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    writer.rows()
}

/// A paused downgrade route passes through on the ORIGINAL model: warnings
/// token, route attribution + route_paused marker on the row, cost ==
/// baseline (no fabricated saving), and NO shadow dispatch even though the
/// paused route configures `shadow_model`.
#[tokio::test]
async fn paused_route_passes_through() {
    let h = harness(HarnessOpts {
        then: RouteAction {
            target_model: "gpt-4o-mini".into(),
            shadow_model: Some("gpt-4o-mini".into()),
            ..Default::default()
        },
        paused: true,
        ..Default::default()
    })
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Pause still surfaces the match for attribution…
    assert_eq!(
        resp.headers()["x-tokentrimmer-route-matched"]
            .to_str()
            .unwrap(),
        ROUTE_NAME
    );
    // …with the caller-visible pause token.
    assert!(
        warnings_of(&resp).contains(&format!("route_paused:{ROUTE_NAME}")),
        "warnings must carry route_paused:<name>, got {:?}",
        warnings_of(&resp)
    );
    // The provider received the ORIGINAL (expensive) model — fail-safe.
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o"
    );

    // No shadow fires on a paused route: exactly one dispatch, on gpt-4o.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(h.calls.load(Ordering::SeqCst), 1, "no shadow dispatch");
    assert_eq!(
        h.models_called.lock().unwrap().as_slice(),
        ["gpt-4o"],
        "passthrough must serve the requested model only"
    );

    // The durable marker: route attributed, route_paused=true, cost ==
    // baseline (the routing saving honestly books 0), no shadow columns.
    let rows = rows_after(&h.log_writer, 1).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.route_id, Some(h.route_id), "route still attributes");
    assert!(row.route_paused, "request_logs.route_paused must be set");
    assert!(
        (row.cost_usd - row.baseline_cost_usd).abs() < 1e-12,
        "paused passthrough must book ZERO saving: cost {} vs baseline {}",
        row.cost_usd,
        row.baseline_cost_usd
    );
    assert_eq!(row.shadow_model, None);
    assert_eq!(row.shadow_cost_usd, None);
}

/// Regression for the 99% path: the SAME route unpaused still rewrites, and
/// its row carries route_paused = false.
#[tokio::test]
async fn unpaused_route_unaffected_regression() {
    let h = harness(HarnessOpts {
        paused: false,
        ..Default::default()
    })
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o-mini",
        "unpaused route must still rewrite"
    );
    assert!(
        !warnings_of(&resp).contains("route_paused:"),
        "no pause token on an unpaused route"
    );

    let rows = rows_after(&h.log_writer, 1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].route_id, Some(h.route_id));
    assert!(
        !rows[0].route_paused,
        "unpaused row must not carry the marker"
    );
}

/// A forced `X-TokenTrimmer-Route` header naming a paused route does NOT
/// bypass the pause — the quality gate wins over the caller's preference.
#[tokio::test]
async fn forced_route_header_does_not_bypass_pause() {
    let h = harness(HarnessOpts::default()).await;

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .header("x-tokentrimmer-route", ROUTE_NAME)
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": "hello world"}],
                "stream": false,
            })
            .to_string(),
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o",
        "forcing a paused route must still pass through"
    );
    assert!(
        warnings_of(&resp).contains(&format!("route_paused:{ROUTE_NAME}")),
        "forced match must still warn route_paused"
    );
    assert_eq!(h.models_called.lock().unwrap().as_slice(), ["gpt-4o"]);
}

/// SAFETY levers stay ON while paused: `redact: true` still strips PII from
/// the upstream request (and warns `redacted:<class>`); `disable_cache: true`
/// still skips the L1 cache.
#[tokio::test]
async fn paused_route_keeps_safety_levers() {
    // ── redact stays live ───────────────────────────────────────────────────
    let h = harness(HarnessOpts {
        then: RouteAction {
            target_model: "gpt-4o-mini".into(),
            redact: true,
            ..Default::default()
        },
        paused: true,
        ..Default::default()
    })
    .await;
    const USER_EMAIL: &str = "jane.doe@example.com";
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": format!("My email is {USER_EMAIL}, please help.")}],
                "stream": false,
            })
            .to_string(),
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warnings = warnings_of(&resp);
    assert!(
        warnings.contains(&format!("route_paused:{ROUTE_NAME}")),
        "paused token present: {warnings:?}"
    );
    assert!(
        warnings.contains("redacted:body"),
        "redaction must still fire while paused: {warnings:?}"
    );
    let upstream_text: String = h
        .last_messages
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| match m {
            Message::User {
                content: MessageContent::Text(s),
                ..
            } => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !upstream_text.contains(USER_EMAIL),
        "PII must not reach the provider while paused"
    );
    assert!(upstream_text.contains("[REDACTED]"));

    // ── disable_cache stays live ────────────────────────────────────────────
    let h = harness(HarnessOpts {
        then: RouteAction {
            target_model: "gpt-4o-mini".into(),
            disable_cache: true,
            ..Default::default()
        },
        paused: true,
        with_l1: true,
        ..Default::default()
    })
    .await;
    for _ in 0..2 {
        let resp = h
            .app
            .clone()
            .oneshot(chat_request("gpt-4o", &h.key))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        2,
        "disable_cache must keep skipping L1 while paused (no hit)"
    );

    // Control: the same paused route WITHOUT disable_cache serves the second
    // request from L1 — proving the skip above came from the safety lever,
    // not from the pause itself.
    let h = harness(HarnessOpts {
        paused: true,
        with_l1: true,
        ..Default::default()
    })
    .await;
    for _ in 0..2 {
        let resp = h
            .app
            .clone()
            .oneshot(chat_request("gpt-4o", &h.key))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        h.calls.load(Ordering::SeqCst),
        1,
        "paused route without disable_cache still uses L1"
    );
    // The L1-hit row must carry the pause marker too (row-builder threading).
    let rows = rows_after(&h.log_writer, 2).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|r| r.route_paused),
        "both miss and L1-hit rows must carry route_paused"
    );
}

/// A paused route is never judged: no rewrite → not a downgrade → the #155
/// sampler skips it even at rate 1.0. (This is what freezes the verdict
/// window and makes the pause naturally sticky.)
#[tokio::test]
async fn paused_route_no_judge_sample() {
    let h = harness(HarnessOpts {
        paused: true,
        with_judge: true,
        ..Default::default()
    })
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Give any (wrongly) spawned judge task ample time to run.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        h.judge_sink.outcomes.lock().unwrap().is_empty(),
        "paused route must record no judge outcome"
    );
    assert!(
        !h.models_called
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == JUDGE_MODEL),
        "judge model must never be dispatched for a paused route"
    );
}

/// Regression (hit-path token attachment): pre-dispatch warning tokens —
/// here `route_paused:<name>` — must survive on an L1-HIT response, not just
/// on the miss/dispatch path. Before the `attach_warning_tokens` helper, hit
/// early-returns silently dropped every pre-dispatch token.
#[tokio::test]
async fn l1_hit_response_carries_route_paused_token() {
    let h = harness(HarnessOpts {
        paused: true,
        with_l1: true,
        ..Default::default()
    })
    .await;

    // Miss → populates L1 (paused route, no rewrite).
    let r1 = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.key))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert!(
        warnings_of(&r1).contains(&format!("route_paused:{ROUTE_NAME}")),
        "miss path carries the token (precondition)"
    );
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Hit → the SAME pre-dispatch token must still be advertised.
    let r2 = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.key))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(h.calls.load(Ordering::SeqCst), 1, "second request hit L1");
    assert!(
        warnings_of(&r2).contains(&format!("route_paused:{ROUTE_NAME}")),
        "hit responses must carry pre-dispatch warning tokens too, got {:?}",
        warnings_of(&r2)
    );
}
