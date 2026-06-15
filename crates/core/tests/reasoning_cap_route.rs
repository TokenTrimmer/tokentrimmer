//! End-to-end (hermetic) tests for the class-gated reasoning-token cap route
//! action (`RouteAction::reasoning_max_effort` / `reasoning_budget_tokens`,
//! research Phase 3.2):
//!
//! (a) a capped route lowers the `reasoning_effort` the mock provider sees,
//!     emits the warning token, increments `reasoning_capped_total`, and books
//!     $0 — cost/baseline/saved identical to an uncapped control request;
//! (b) a math-classified request passes through UNCAPPED with the class token
//!     (the HARD gate — capping where reasoning is the work is refused);
//! (c) a sticky-PAUSED route suppresses BOTH new output-shaping actions
//!     (minify + reasoning cap are cost levers);
//! (d) judge eligibility: with the judge at sample rate 1.0, an action-only
//!     shaped route (same target model — `is_downgrade` false) spawns a judge
//!     job whose baseline re-dispatch is the UN-shaped request.
//!
//! No network: a recording mock provider, mirroring
//! `route_pause_passthrough.rs`.

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
use uuid::Uuid;

const MODEL: &str = "reasoner-1";
const JUDGE_MODEL: &str = "judge-cheap";
const ROUTE_NAME: &str = "cap-reasoning";

/// One recorded upstream dispatch: model + reasoning_effort + system text.
#[derive(Clone, Debug)]
struct Dispatch {
    model: String,
    reasoning_effort: Option<String>,
    system_text: String,
}

/// Recording provider serving a Reasoning-capable model + the judge model.
struct CapRecordingProvider {
    dispatches: Arc<Mutex<Vec<Dispatch>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for CapRecordingProvider {
    fn id(&self) -> &'static str {
        "caprec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: MODEL.into(),
                provider: "caprec".into(),
                capabilities: vec![Capability::Text, Capability::Reasoning],
                max_input_tokens: 100_000,
                max_output_tokens: 100_000,
            },
            ModelInfo {
                id: JUDGE_MODEL.into(),
                provider: "caprec".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 100_000,
                max_output_tokens: 100_000,
            },
        ]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (input, output) = match model {
            JUDGE_MODEL => (0.1, 0.4),
            _ => (10.0, 30.0),
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
        let system_text = req
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::System {
                    content: MessageContent::Text(t),
                } => Some(t.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.dispatches.lock().unwrap().push(Dispatch {
            model: req.model.clone(),
            reasoning_effort: req.reasoning_effort.clone(),
            system_text,
        });
        Ok(ChatCompletionResponse {
            id: "chatcmpl-cap".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    // A judge-parsable verdict AND a plain answer in one: the
                    // judge prompt asks for a verdict word; "ACCEPTABLE" keeps
                    // the paired judge classification deterministic.
                    content: Some(MessageContent::Text("ACCEPTABLE".into())),
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
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// In-memory judge sink recording every outcome.
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
    dispatches: Arc<Mutex<Vec<Dispatch>>>,
    calls: Arc<AtomicUsize>,
    judge_sink: Arc<RecordingSink>,
}

struct Opts {
    then: RouteAction,
    paused: bool,
    with_judge: bool,
}

async fn harness(opts: Opts) -> Harness {
    let dispatches = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CapRecordingProvider {
        dispatches: Arc::clone(&dispatches),
        calls: Arc::clone(&calls),
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
            paused: false,
            id: route_id,
            name: ROUTE_NAME.into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec![MODEL.into()],
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
        assert!(newly);
    }

    let judge_sink = Arc::new(RecordingSink::default());
    let mut state = AppState::new(registry)
        .with_key_store(key_store)
        .with_routing_store(routing);
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
        dispatches,
        calls,
        judge_sink,
    }
}

fn cap_action() -> RouteAction {
    RouteAction {
        target_model: Some(MODEL.into()), // action-only: no rewrite
        reasoning_max_effort: Some("low".into()),
        ..Default::default()
    }
}

fn chat_request(prompt: &str, effort: Option<&str>, bearer: &str) -> Request<Body> {
    let mut body = json!({
        "model": MODEL,
        "messages": [
            {"role": "system", "content": "You are an API."},
            {"role": "user", "content": prompt}
        ],
        "stream": false,
    });
    if let Some(e) = effort {
        body["reasoning_effort"] = json!(e);
    }
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn warnings_of(resp: &axum::http::Response<Body>) -> String {
    resp.headers()
        .get("x-tokentrimmer-warnings")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

fn header_f64(resp: &axum::http::Response<Body>, name: &str) -> f64 {
    resp.headers()[name]
        .to_str()
        .unwrap()
        .parse::<f64>()
        .unwrap()
}

/// A neutral prompt that classifies as NONE of math/code/legal/medical.
const NEUTRAL_PROMPT: &str = "Summarize this paragraph in one sentence.";

/// (a) Capped route: the provider sees the LOWERED effort, the warning token
/// is emitted, `reasoning_capped_total` increments, and the booked figures
/// are identical to an uncapped control request ($0 booked).
#[tokio::test]
async fn capped_route_lowers_effort_books_zero_and_meters() {
    // Capped arm.
    let h = harness(Opts {
        then: cap_action(),
        paused: false,
        with_judge: false,
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(NEUTRAL_PROMPT, Some("high"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let d = h.dispatches.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        d.reasoning_effort.as_deref(),
        Some("low"),
        "the provider must see the LOWERED effort"
    );
    let warnings = warnings_of(&resp);
    assert!(
        warnings
            .split(',')
            .any(|w| w == "reasoning_capped:reasoning_effort:low"),
        "expected the capped token, got {warnings:?}"
    );

    // Metered: the global Prometheus recorder carries the per-route counter.
    let rendered = tt_core::metrics::render().expect("recorder installed by build_router");
    assert!(
        rendered.contains("reasoning_capped_total"),
        "reasoning_capped_total must be exported"
    );
    assert!(
        rendered.contains(&format!("route=\"{ROUTE_NAME}\"")),
        "the counter must be labeled with the route name"
    );

    let capped_cost = header_f64(&resp, "x-tokentrimmer-cost-usd");
    let capped_baseline = header_f64(&resp, "x-tokentrimmer-baseline-cost-usd");
    let capped_saved = header_f64(&resp, "x-tokentrimmer-saved-usd");

    // Control arm: the same request through a gateway whose route has NO cap.
    let control = harness(Opts {
        then: RouteAction {
            target_model: Some(MODEL.into()),
            ..Default::default()
        },
        paused: false,
        with_judge: false,
    })
    .await;
    let control_resp = control
        .app
        .clone()
        .oneshot(chat_request(NEUTRAL_PROMPT, Some("high"), &control.key))
        .await
        .unwrap();
    assert_eq!(control_resp.status(), StatusCode::OK);
    let d = control.dispatches.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        d.reasoning_effort.as_deref(),
        Some("high"),
        "control uncapped"
    );

    // $0 booked: every figure identical to the uncapped control.
    assert_eq!(
        capped_cost,
        header_f64(&control_resp, "x-tokentrimmer-cost-usd")
    );
    assert_eq!(
        capped_baseline,
        header_f64(&control_resp, "x-tokentrimmer-baseline-cost-usd")
    );
    assert_eq!(
        capped_saved,
        header_f64(&control_resp, "x-tokentrimmer-saved-usd")
    );
    assert_eq!(capped_saved, 0.0, "no fabricated saving for the cap");
}

/// (b) A math-classified request passes through UNCAPPED with the class token
/// — the HARD gate refuses before any lever is touched.
#[tokio::test]
async fn math_classified_request_is_never_capped() {
    let h = harness(Opts {
        then: cap_action(),
        paused: false,
        with_judge: false,
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(
            "Prove that the square root of 2 is irrational.",
            Some("high"),
            &h.key,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let d = h.dispatches.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        d.reasoning_effort.as_deref(),
        Some("high"),
        "math request must keep its full reasoning effort"
    );
    let warnings = warnings_of(&resp);
    assert!(
        warnings
            .split(',')
            .any(|w| w == "reasoning_cap_skipped:class:math"),
        "expected the class refusal token, got {warnings:?}"
    );
    assert!(
        !warnings.contains("reasoning_capped:"),
        "no capped token may accompany a class refusal: {warnings:?}"
    );
}

/// (c) A sticky-PAUSED route suppresses BOTH new output-shaping actions —
/// they are cost levers, and a pause fails safe in the expensive direction.
#[tokio::test]
async fn paused_route_suppresses_output_shaping_actions() {
    let h = harness(Opts {
        then: RouteAction {
            target_model: Some(MODEL.into()),
            minify_json: true,
            reasoning_max_effort: Some("low".into()),
            reasoning_budget_tokens: Some(8192),
            ..Default::default()
        },
        paused: true,
        with_judge: false,
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(NEUTRAL_PROMPT, Some("high"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let d = h.dispatches.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        d.reasoning_effort.as_deref(),
        Some("high"),
        "paused route must not cap"
    );
    assert!(
        !d.system_text.contains("emit it minified"),
        "paused route must not inject the minify instruction"
    );
    let warnings = warnings_of(&resp);
    assert!(
        warnings
            .split(',')
            .any(|w| w == format!("route_paused:{ROUTE_NAME}")),
        "expected route_paused token, got {warnings:?}"
    );
    assert!(
        !warnings.contains("output_minified") && !warnings.contains("reasoning_capped"),
        "no shaping token on a paused route: {warnings:?}"
    );
    let est = header_f64(&resp, "x-tokentrimmer-minify-saved-est-usd");
    assert_eq!(est, 0.0, "paused route books no estimate");
}

/// (d) Judge eligibility: an ACTION-ONLY shaped route (same target model — no
/// price downgrade) at sample rate 1.0 spawns a paired judge job whose
/// baseline re-dispatch is the UN-shaped request (full effort, no minify
/// instruction).
#[tokio::test]
async fn shaped_route_spawns_judge_with_unshaped_baseline() {
    let h = harness(Opts {
        then: RouteAction {
            target_model: Some(MODEL.into()),
            minify_json: true,
            reasoning_max_effort: Some("low".into()),
            ..Default::default()
        },
        paused: false,
        with_judge: true,
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request(NEUTRAL_PROMPT, Some("high"), &h.key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The judge runs detached: wait for (1) the served dispatch, (2) the
    // baseline reference re-dispatch on MODEL, (3) the judge-model call.
    let mut outcome_recorded = false;
    for _ in 0..200 {
        if !h.judge_sink.outcomes.lock().unwrap().is_empty() {
            outcome_recorded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        outcome_recorded,
        "an output-shaped same-model route must be judge-eligible (calls={}, dispatches={:?})",
        h.calls.load(Ordering::SeqCst),
        h.dispatches.lock().unwrap().len()
    );

    let dispatches = h.dispatches.lock().unwrap().clone();
    // First dispatch = the SERVED (shaped) request.
    assert_eq!(dispatches[0].reasoning_effort.as_deref(), Some("low"));
    assert!(dispatches[0].system_text.contains("emit it minified"));
    // The baseline reference re-dispatch on the SAME model is UN-shaped: the
    // pre-routing capture precedes all pre-dispatch shaping.
    let baseline = dispatches[1..]
        .iter()
        .find(|d| d.model == MODEL)
        .expect("baseline reference re-dispatch on the original model");
    assert_eq!(
        baseline.reasoning_effort.as_deref(),
        Some("high"),
        "baseline must carry the ORIGINAL effort"
    );
    assert!(
        !baseline.system_text.contains("emit it minified"),
        "baseline must NOT carry the minify instruction"
    );
    // And the judge model itself was called.
    assert!(
        dispatches.iter().any(|d| d.model == JUDGE_MODEL),
        "judge model must be dispatched: {dispatches:?}"
    );
}
