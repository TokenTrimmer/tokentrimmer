//! Hermetic integration tests for the sampled async quality judge on rerouted
//! (downgraded) traffic. No network — a single recording provider stands in for
//! both the served models and the cheap judge model; an in-memory sink records
//! the judge outcome.
//!
//! Behaviors asserted (task JUDGE-SAMPLE a–d):
//!   (a) a rerouted-DOWN request IN the sampled set triggers a judge call and
//!       records a SampleScore/RiskBand;
//!   (b) a NON-rerouted request never triggers the judge;
//!   (c) sampling respects the rate (rate 1.0 → all judged, 0.0 → none);
//!   (d) the user response returns WITHOUT waiting for the judge — the judge
//!       runs AFTER / off the response path (proven with a gated judge call:
//!       the HTTP response lands while the judge is still blocked).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tokio::sync::Notify;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::quality_sample::{InMemoryJudgeBandStore, JudgeConfig, JudgeOutcome, JudgeSink};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_plan_core::{JudgeVerdict, RiskBand, SampleScore};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// The cheap judge model id this test uses.
const JUDGE_MODEL: &str = "judge-cheap";

/// A provider that serves the chat models (gpt-4o / gpt-4o-mini) AND the cheap
/// judge model. It separately counts judge-model calls and can optionally block
/// the judge call on a [`Notify`] gate so a test can prove the user response
/// returns before the judge runs.
struct JudgeAwareProvider {
    /// Count of judge-model (`JUDGE_MODEL`) chat completions.
    judge_calls: Arc<AtomicUsize>,
    /// Count of NON-judge (served-model) chat completions.
    served_calls: Arc<AtomicUsize>,
    /// When set, the judge call awaits this gate before returning — lets a test
    /// hold the judge mid-flight while asserting the HTTP response already landed.
    judge_gate: Option<Arc<Notify>>,
    /// The verdict intent the judge model expresses with pair tokens:
    /// `"ACCEPTABLE"` → `EQUIVALENT`; `"DEGRADED"` → `A_MISSING`/`B_MISSING`
    /// naming whichever blind slot holds the optimized (cheaper-model) answer.
    judge_verdict_word: &'static str,
    /// When true, every judge-model call errors (fail-open test).
    judge_fail: bool,
    /// Full prompt text (system + user) of every judge-model request, captured
    /// for the blind-prompt assertions.
    judge_prompts: Arc<Mutex<Vec<String>>>,
    /// Fixed assistant body for NON-judge dispatches (`None` → the default
    /// `answer from <model>`). The output-shaping tests script a patch here.
    served_body: Option<&'static str>,
}

/// Build the judge model's pair-token reply. For a "DEGRADED" intent the mock
/// must name whichever blind slot holds the OPTIMIZED (cheaper-model) answer —
/// the slot is randomized per trace, so the mock inspects the prompt rather
/// than assuming a fixed position.
fn pair_reply(user_text: &str, verdict_word: &'static str) -> String {
    match verdict_word {
        "ACCEPTABLE" => "EQUIVALENT\nsame material information".to_string(),
        "DEGRADED" => {
            let a_start = user_text.find("ANSWER A:").unwrap_or(0);
            let b_start = user_text.find("ANSWER B:").unwrap_or(user_text.len());
            let slot_a = &user_text[a_start..b_start];
            if slot_a.contains("gpt-4o-mini") {
                "A_MISSING — dropped material info".to_string()
            } else {
                "B_MISSING — dropped material info".to_string()
            }
        }
        other => format!("{other}\nreason line"),
    }
}

/// Concatenated text content of a request's messages (system + user), used to
/// capture and assert on the judge prompt.
fn request_text(req: &ChatCompletionRequest) -> String {
    req.messages
        .iter()
        .filter_map(|m| match m {
            Message::System { content } | Message::User { content, .. } => match content {
                MessageContent::Text(s) => Some(s.clone()),
                MessageContent::Parts(_) => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Provider for JudgeAwareProvider {
    fn id(&self) -> &'static str {
        "judgeaware"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["gpt-4o", "gpt-4o-mini", JUDGE_MODEL]
            .into_iter()
            .map(|m| ModelInfo {
                id: m.into(),
                provider: "judgeaware".into(),
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
            prompt_cache_min_tokens: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let is_judge = req.model == JUDGE_MODEL;
        if is_judge {
            self.judge_calls.fetch_add(1, Ordering::SeqCst);
            self.judge_prompts.lock().unwrap().push(request_text(&req));
            if self.judge_fail {
                return Err(ProviderError::Unsupported("judge down".into()));
            }
            if let Some(gate) = &self.judge_gate {
                // Block the judge call until the test releases it. Proves the
                // user response is independent of judge progress.
                gate.notified().await;
            }
        } else {
            self.served_calls.fetch_add(1, Ordering::SeqCst);
        }
        let text = if is_judge {
            pair_reply(&request_text(&req), self.judge_verdict_word)
        } else if let Some(body) = self.served_body {
            body.to_string()
        } else {
            // Served + reference (original-model) answers.
            format!("answer from {}", req.model)
        };
        Ok(ChatCompletionResponse {
            id: "chatcmpl-x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(text)),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 100,
                total_tokens: 200,
                cached_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
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

/// In-memory sink that records every judged outcome.
#[derive(Default)]
struct RecordingSink {
    outcomes: Mutex<Vec<JudgeOutcome>>,
    /// Notified once after each record so a test can await the first outcome
    /// without polling-sleep races.
    recorded: Notify,
}

#[async_trait]
impl JudgeSink for RecordingSink {
    async fn record(&self, outcome: JudgeOutcome) {
        self.outcomes.lock().unwrap().push(outcome);
        self.recorded.notify_one();
    }
}

struct Harness {
    app: axum::Router,
    judge_calls: Arc<AtomicUsize>,
    served_calls: Arc<AtomicUsize>,
    sink: Arc<RecordingSink>,
    plaintext: String,
    judge_prompts: Arc<Mutex<Vec<String>>>,
}

/// Harness knobs beyond the original four (new tests opt in via
/// [`build_harness_opts`]; the existing call sites keep [`build_harness`]).
struct HarnessOpts {
    rate: f64,
    plant_downgrade_route: bool,
    judge_gate: Option<Arc<Notify>>,
    judge_verdict_word: &'static str,
    judge_fail: bool,
    both_orders: bool,
    /// The planted route opts into the redaction guardrail (`redact: true`) —
    /// the judge must then be skipped wholesale (the captured pre-redaction
    /// request must never ride the measurement path).
    redact_route: bool,
    /// Override `JudgeConfig::baseline_timeout` (also the per-judge-call
    /// timeout the production spawn wires via `with_call_timeout`).
    baseline_timeout: Option<std::time::Duration>,
<<<<<<< HEAD
    /// Plant a SAME-MODEL, action-less route (`target_model == "gpt-4o"`,
    /// every action off) instead of the downgrade route — matched traffic is
    /// neither a downgrade nor output-shaped, so the judge must NOT spawn
    /// (guards the #3.x eligibility widening against over-widening).
    same_model_route: bool,
=======
    /// Plant a SAME-MODEL route (gpt-4o-mini → gpt-4o-mini; never a
    /// downgrade) instead of the downgrade route — the output-shaping judge
    /// tests use it to isolate the `response_shaped` gate arm.
    plant_same_model_route: bool,
    /// The planted same-model route opts into delta/diff responses
    /// (`diff: true`).
    same_model_route_diff: bool,
    /// Fixed assistant body for NON-judge dispatches (both the served call
    /// and the reference re-dispatch); `None` keeps the default
    /// `answer from <model>`.
    served_body: Option<&'static str>,
>>>>>>> d9f635e (feat(core): contract-changing output shaping — format-switch (CSV/bare) + delta/diff responses with fail-closed re-emit)
}

impl Default for HarnessOpts {
    fn default() -> Self {
        Self {
            rate: 1.0,
            plant_downgrade_route: true,
            judge_gate: None,
            judge_verdict_word: "ACCEPTABLE",
            judge_fail: false,
            both_orders: false,
            redact_route: false,
            baseline_timeout: None,
<<<<<<< HEAD
            same_model_route: false,
=======
            plant_same_model_route: false,
            same_model_route_diff: false,
            served_body: None,
>>>>>>> d9f635e (feat(core): contract-changing output shaping — format-switch (CSV/bare) + delta/diff responses with fail-closed re-emit)
        }
    }
}

async fn build_harness(
    rate: f64,
    plant_downgrade_route: bool,
    judge_gate: Option<Arc<Notify>>,
    judge_verdict_word: &'static str,
) -> Harness {
    build_harness_opts(HarnessOpts {
        rate,
        plant_downgrade_route,
        judge_gate,
        judge_verdict_word,
        ..Default::default()
    })
    .await
}

async fn build_harness_opts(opts: HarnessOpts) -> Harness {
    let judge_calls = Arc::new(AtomicUsize::new(0));
    let served_calls = Arc::new(AtomicUsize::new(0));
    let judge_prompts = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(JudgeAwareProvider {
        judge_calls: Arc::clone(&judge_calls),
        served_calls: Arc::clone(&served_calls),
        judge_gate: opts.judge_gate,
        judge_verdict_word: opts.judge_verdict_word,
        judge_fail: opts.judge_fail,
        judge_prompts: Arc::clone(&judge_prompts),
        served_body: opts.served_body,
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let audit = InMemoryAuditWriter::new();
    let plaintext = issue(
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
    if opts.plant_same_model_route {
        // SAME-model route: matched_route_id is stamped but the served model
        // equals the requested one (same pricing → never a downgrade) — the
        // output-shaping tests use it to isolate the `response_shaped` arm
        // of the judge gate.
        routes_backing.set_routes(
            org_id,
            vec![Route {
                paused: false,
                id: Uuid::now_v7(),
                name: "same-model-shaping".into(),
                priority: 100,
                enabled: true,
                when: RouteConditions {
                    model_in: vec!["gpt-4o-mini".into()],
                    ..Default::default()
                },
                then: RouteAction {
                    target_model: "gpt-4o-mini".into(),
                    fallbacks: Vec::new(),
                    disable_cache: false,
                    max_cost_usd: None,
                    flex: false,
                    batch: false,
                    compress: false,
                    redact: false,
                    format_switch: None,
                    diff: opts.same_model_route_diff,
                    traffic_pct: None,
                    shadow_model: None,
                    auto_pause: false,
                    pause_floor_pass_rate: None,
                    pause_min_verdicts: None,
                },
            }],
        );
    } else if opts.plant_downgrade_route {
        routes_backing.set_routes(
            org_id,
            vec![Route {
                paused: false,
                id: Uuid::now_v7(),
                name: "downgrade-4o".into(),
                priority: 100,
                enabled: true,
                when: RouteConditions {
                    model_in: vec!["gpt-4o".into()],
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
                    target_model: if opts.same_model_route {
                        "gpt-4o".into()
                    } else {
                        "gpt-4o-mini".into()
                    },
                    fallbacks: Vec::new(),
                    disable_cache: false,
                    max_cost_usd: None,
                    flex: false,
                    batch: false,
                    compress: false,
                    redact: opts.redact_route,
                    traffic_pct: None,
                    shadow_model: None,
                },
            }],
        );
    }
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let sink = Arc::new(RecordingSink::default());
    let mut config = JudgeConfig {
        enabled: true,
        sample_rate: opts.rate,
        judge_model: JUDGE_MODEL.to_string(),
        both_orders: opts.both_orders,
        ..JudgeConfig::default()
    };
    if let Some(t) = opts.baseline_timeout {
        config.baseline_timeout = t;
    }
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing)
            .with_quality_judge(sink.clone() as Arc<dyn JudgeSink>, config),
    );

    Harness {
        app,
        judge_calls,
        served_calls,
        sink,
        plaintext,
        judge_prompts,
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

/// (a) A rerouted-DOWN request in the sampled set (rate 1.0) triggers a judge
/// call and records a SampleScore + RiskBand.
#[tokio::test]
async fn rerouted_sampled_request_triggers_judge_and_records_score() {
    let h = build_harness(1.0, true, None, "DEGRADED").await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Served model is the cheap one (the downgrade fired).
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o-mini"
    );

    // Wait for the detached judge task to record (bounded).
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("judge should record within timeout");

    assert_eq!(
        h.judge_calls.load(Ordering::SeqCst),
        1,
        "exactly one judge call for one sampled rerouted-down request"
    );
    let outcomes = h.sink.outcomes.lock().unwrap();
    assert_eq!(outcomes.len(), 1, "one recorded outcome");
    let o = &outcomes[0];
    assert_eq!(o.requested_model, "gpt-4o");
    assert_eq!(o.served_model, "gpt-4o-mini");
    assert_eq!(o.score.verdict, JudgeVerdict::Degraded);
    assert_eq!(o.risk_band, RiskBand::High, "Degraded → High risk band");
    assert!(o.route_id.is_some(), "outcome carries the matched route id");
}

/// (a') Acceptable verdict maps to a Low risk band.
#[tokio::test]
async fn acceptable_verdict_records_low_band() {
    let h = build_harness(1.0, true, None, "ACCEPTABLE").await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("judge should record");
    let outcomes = h.sink.outcomes.lock().unwrap();
    assert_eq!(outcomes[0].score.verdict, JudgeVerdict::Acceptable);
    assert_eq!(outcomes[0].risk_band, RiskBand::Low);
}

/// (b) A NON-rerouted request never triggers the judge (no route planted, so the
/// model passes through unchanged — not a downgrade).
#[tokio::test]
async fn non_rerouted_request_never_triggers_judge() {
    let h = build_harness(1.0, false, None, "DEGRADED").await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o",
        "no route → model unchanged"
    );

    // Give any (erroneously) spawned judge task a chance to run.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        h.judge_calls.load(Ordering::SeqCst),
        0,
        "non-rerouted request must never call the judge"
    );
    assert_eq!(h.sink.outcomes.lock().unwrap().len(), 0);
    // The served model WAS dispatched once (the actual user request).
    assert_eq!(h.served_calls.load(Ordering::SeqCst), 1);
}

<<<<<<< HEAD
/// A route that MATCHES but neither downgrades nor output-shapes (same
/// target model, every action off) never judges — the output-shaping
/// eligibility widening is shaped-only, not matched-only.
#[tokio::test]
async fn matched_unshaped_same_model_route_never_triggers_judge() {
    let h = build_harness_opts(HarnessOpts {
        same_model_route: true,
=======
/// The prior document for the output-shaping judge tests (> 200 chars so the
/// diff planner accepts it) and a patch whose anchor is unique within it.
const SHAPING_PRIOR_LINE: &str =
    "Item line: the quick brown fox jumps over the lazy dog yet again today.\n";
const SHAPING_PATCH: &str =
    "<<<<<<< SEARCH\nItem 0 marker line.\n=======\nItem ZERO marker line.\n>>>>>>> REPLACE";

fn shaping_edit_request(bearer: &str) -> Request<Body> {
    let mut prior = String::from("Item 0 marker line.\n");
    for _ in 0..4 {
        prior.push_str(SHAPING_PRIOR_LINE);
    }
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(
            json!({
                "model": "gpt-4o-mini",
                "messages": [
                    {"role": "user", "content": "write the list"},
                    {"role": "assistant", "content": prior},
                    {"role": "user", "content": "capitalize the marker"}
                ],
                "stream": false,
            })
            .to_string(),
        ))
        .unwrap()
}

/// A SHAPED response (applied diff) on a NON-downgrade route samples the
/// paired judge — output shaping is exactly what the #155 gate exists to
/// police, even when the served model's price did not change.
#[tokio::test]
async fn shaped_response_on_non_downgrade_route_samples_judge() {
    let h = build_harness_opts(HarnessOpts {
        rate: 1.0,
        plant_downgrade_route: false,
        plant_same_model_route: true,
        same_model_route_diff: true,
        served_body: Some(SHAPING_PATCH),
>>>>>>> d9f635e (feat(core): contract-changing output shaping — format-switch (CSV/bare) + delta/diff responses with fail-closed re-emit)
        ..Default::default()
    })
    .await;

    let resp = h
        .app
        .clone()
<<<<<<< HEAD
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o",
        "same-model route → model unchanged"
    );

    // Give any (erroneously) spawned judge task a chance to run.
=======
        .oneshot(shaping_edit_request(&h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warnings = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        warnings.split(',').any(|w| w == "diff_applied"),
        "precondition: the diff must have applied, got {warnings}"
    );

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("a shaped response on a non-downgrade route must sample the judge");
    assert!(h.judge_calls.load(Ordering::SeqCst) >= 1);
}

/// Control: the SAME same-model route WITHOUT shaping still never samples —
/// the `response_shaped` arm widened the gate for shaped traffic only.
#[tokio::test]
async fn unshaped_non_downgrade_route_still_never_samples_judge() {
    let h = build_harness_opts(HarnessOpts {
        rate: 1.0,
        plant_downgrade_route: false,
        plant_same_model_route: true,
        same_model_route_diff: false,
        ..Default::default()
    })
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(shaping_edit_request(&h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

>>>>>>> d9f635e (feat(core): contract-changing output shaping — format-switch (CSV/bare) + delta/diff responses with fail-closed re-emit)
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        h.judge_calls.load(Ordering::SeqCst),
        0,
<<<<<<< HEAD
        "matched-but-unshaped same-model route must never call the judge"
    );
    assert_eq!(h.sink.outcomes.lock().unwrap().len(), 0);
    assert_eq!(h.served_calls.load(Ordering::SeqCst), 1);
=======
        "an unshaped non-downgrade route must not sample"
    );
    assert_eq!(h.sink.outcomes.lock().unwrap().len(), 0);
>>>>>>> d9f635e (feat(core): contract-changing output shaping — format-switch (CSV/bare) + delta/diff responses with fail-closed re-emit)
}

/// (c) At rate 0.0 a rerouted-down request is never judged.
#[tokio::test]
async fn sample_rate_zero_judges_nothing() {
    let h = build_harness(0.0, true, None, "DEGRADED").await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(
        h.judge_calls.load(Ordering::SeqCst),
        0,
        "rate 0.0 must judge nothing"
    );
    assert_eq!(h.sink.outcomes.lock().unwrap().len(), 0);
}

/// (c') At rate 1.0 every rerouted-down request is judged.
#[tokio::test]
async fn sample_rate_one_judges_every_rerouted_request() {
    let h = build_harness(1.0, true, None, "ACCEPTABLE").await;

    for _ in 0..3 {
        let resp = h
            .app
            .clone()
            .oneshot(chat_request("gpt-4o", &h.plaintext))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // Wait until all three outcomes are recorded (short poll; bounded).
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if h.sink.outcomes.lock().unwrap().len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all three rerouted-down requests should be judged");
    assert_eq!(h.judge_calls.load(Ordering::SeqCst), 3);
}

/// (d) The user response returns WITHOUT waiting for the judge. The judge call
/// is gated; we assert the HTTP response lands while the judge is still blocked,
/// then release the gate and confirm the judge records afterwards.
#[tokio::test]
async fn response_returns_before_judge_completes() {
    let gate = Arc::new(Notify::new());
    let h = build_harness(1.0, true, Some(Arc::clone(&gate)), "ACCEPTABLE").await;

    // The user request must complete promptly even though the judge is blocked
    // on the gate (never notified yet).
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.app.clone().oneshot(chat_request("gpt-4o", &h.plaintext)),
    )
    .await
    .expect("user response must return without waiting for the gated judge")
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The judge has NOT recorded yet — it's parked on the gate.
    assert_eq!(
        h.sink.outcomes.lock().unwrap().len(),
        0,
        "judge must not have recorded before the user response returned"
    );

    // Release the gate; the judge can now finish and record. `notify_one`
    // stores a permit even if the judge task parks on the gate slightly later,
    // so this can't race-lose the wakeup.
    gate.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("judge records after the gate is released");
    assert_eq!(h.sink.outcomes.lock().unwrap().len(), 1);
    assert!(h.judge_calls.load(Ordering::SeqCst) >= 1);
}

/// End-to-end production join: a recorded judge outcome flows through the
/// `InMemoryJudgeBandStore` and the `tt_preview` enrichment seam to populate the
/// `quality_risk_band` on the live `/v1/preview` response — lifting the judged
/// swap off the hard-coded `Unknown`, while an unjudged swap stays `UNKNOWN`.
#[tokio::test]
async fn preview_enriches_quality_band_from_recorded_judge_outcome() {
    // A band store with a Degraded outcome recorded for gpt-4o → gpt-4o-mini.
    let store = Arc::new(InMemoryJudgeBandStore::new());
    store
        .record(JudgeOutcome {
            org_id: Uuid::now_v7(),
            route_id: Some(Uuid::now_v7()),
            requested_model: "gpt-4o".into(),
            served_model: "gpt-4o-mini".into(),
            score: SampleScore {
                request_id: Uuid::now_v7(),
                verdict: JudgeVerdict::Degraded,
                reason: "lost nuance".into(),
            },
            risk_band: RiskBand::High,
            judge_model: JUDGE_MODEL.to_string(),
            judge_cost_usd: Some(0.000_05),
            baseline_cost_usd: Some(0.002),
            baseline_dispatched: true,
            optimized_position: tt_core::AbOrder::OptimizedA,
            orders_judged: 1,
            orders_agreed: None,
            cache_entry_id: None,
            hit_similarity: None,
        })
        .await;

    // Minimal authed app with the SAME store wired as sink + read-side.
    let registry = ProviderRegistry::new();
    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let audit = InMemoryAuditWriter::new();
    let plaintext = issue(
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

    let config = JudgeConfig {
        enabled: true,
        sample_rate: 1.0,
        judge_model: JUDGE_MODEL.to_string(),
        ..JudgeConfig::default()
    };
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_quality_judge_band_store(store, config),
    );

    let req = Request::builder()
        .method("POST")
        .uri("/v1/preview")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {plaintext}"))
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": "hello world"}],
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let suggestions = body["route_suggestions"]
        .as_array()
        .expect("route_suggestions array");
    let mini = suggestions
        .iter()
        .find(|s| s["model"] == "gpt-4o-mini")
        .expect("preview should suggest gpt-4o-mini for a gpt-4o chat request");
    // Judged swap → lifted off UNKNOWN to the recorded HIGH band.
    assert_eq!(
        mini["quality_risk_band"], "HIGH",
        "judged swap band must be populated from the recorded outcome"
    );
    // Any OTHER (unjudged) suggestion stays honestly UNKNOWN.
    for s in suggestions {
        if s["model"] != "gpt-4o-mini" {
            assert_eq!(
                s["quality_risk_band"], "UNKNOWN",
                "unjudged swap must stay UNKNOWN"
            );
        }
    }
}

// ── Paired A/B sampler (research Phase 0.4) ─────────────────────────────────

/// The judge prompt that actually reaches the judge model is BLIND: the two
/// answers appear only as ANSWER A / ANSWER B, with no ORIGINAL/CHEAPER role
/// labels (the #128 label-bias regression guard, end-to-end).
#[tokio::test]
async fn judge_prompt_is_blind_end_to_end() {
    let h = build_harness_opts(HarnessOpts::default()).await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("judge should record");

    let prompts = h.judge_prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1, "one judge prompt captured");
    let prompt = &prompts[0];
    assert!(prompt.contains("ANSWER A"), "blind slot labels: {prompt}");
    assert!(prompt.contains("ANSWER B"), "blind slot labels: {prompt}");
    assert!(
        !prompt.contains("ORIGINAL ANSWER"),
        "the role-leaking #128 label must be gone: {prompt}"
    );
    assert!(
        !prompt.contains("CHEAPER"),
        "the role-leaking #128 label must be gone: {prompt}"
    );
}

/// The baseline reference dispatch is METERED: the recorded outcome carries
/// the baseline model's cost on its own pricing (the measurement tax Phase 2
/// nets into attribution) plus a non-zero judge tax.
#[tokio::test]
async fn paired_baseline_dispatch_is_metered() {
    let h = build_harness_opts(HarnessOpts::default()).await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("judge should record");

    let outcomes = h.sink.outcomes.lock().unwrap();
    assert_eq!(outcomes.len(), 1);
    let o = &outcomes[0];
    // gpt-4o pricing (5.0 in / 15.0 out per M) on the mock's fixed 100/100
    // usage: (100*5 + 100*15) / 1e6 = 0.002.
    let baseline = o
        .baseline_cost_usd
        .expect("a re-dispatched baseline must be metered");
    assert!(
        (baseline - 0.002).abs() < 1e-9,
        "baseline cost should be 0.002, got {baseline}"
    );
    assert!(
        o.baseline_dispatched,
        "the baseline reference dispatch must be marked as billed"
    );
    // Judge pricing (0.1 / 0.4 per M) on 100/100 usage: 0.00005.
    let judge_cost = o
        .judge_cost_usd
        .expect("the judge tax must be metered (judge model is priced)");
    assert!(
        (judge_cost - 0.000_05).abs() < 1e-9,
        "judge cost should be 0.00005, got {judge_cost}"
    );
    assert_eq!(o.judge_model, JUDGE_MODEL);
    assert_eq!(o.orders_judged, 1);
    assert_eq!(o.orders_agreed, None);
}

/// Both-orders mode makes exactly TWO judge calls per sampled request (and
/// sums the judge tax); single-order mode makes exactly one.
#[tokio::test]
async fn both_orders_makes_two_judge_calls() {
    let h = build_harness_opts(HarnessOpts {
        both_orders: true,
        ..Default::default()
    })
    .await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("judge should record");
    assert_eq!(
        h.judge_calls.load(Ordering::SeqCst),
        2,
        "both-orders mode judges the pair twice (slots swapped)"
    );
    {
        let outcomes = h.sink.outcomes.lock().unwrap();
        let o = &outcomes[0];
        assert_eq!(o.orders_judged, 2);
        assert_eq!(
            o.orders_agreed,
            Some(true),
            "EQUIVALENT agrees across orders"
        );
        let judge_cost = o.judge_cost_usd.expect("metered judge tax");
        assert!(
            (judge_cost - 2.0 * 0.000_05).abs() < 1e-9,
            "judge tax sums over both orders, got {judge_cost}"
        );
    }

    // Single-order control: exactly one judge call.
    let h1 = build_harness_opts(HarnessOpts::default()).await;
    let resp = h1
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h1.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h1.sink.recorded.notified(),
    )
    .await
    .expect("judge should record");
    assert_eq!(
        h1.judge_calls.load(Ordering::SeqCst),
        1,
        "single-order mode judges once"
    );
}

/// A judge-model failure is FAIL-OPEN for the user (200 + served body
/// untouched) — but the measurement spend ALREADY BILLED before the failure
/// (the baseline reference dispatch) is ledgered as an `unclear` row carrying
/// the incurred tax, so Phase-2 netting stays invoice-complete. No quality
/// signal is fabricated: `unclear` has no valence and is excluded from every
/// quality aggregate.
#[tokio::test]
async fn judge_failure_is_fail_open() {
    let h = build_harness_opts(HarnessOpts {
        judge_fail: true,
        ..Default::default()
    })
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a judge failure must never touch the user response"
    );
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o-mini"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        body["choices"][0]["message"]["content"], "answer from gpt-4o-mini",
        "the served body is intact"
    );

    // Give the detached judge task time to attempt + fail.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while h.judge_calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the judge call should at least be attempted");
    // The baseline reference dispatch was billed BEFORE the judge failed, so
    // the incurred tax must be ledgered (never silently dropped).
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("the incurred measurement spend must be ledgered");
    let outcomes = h.sink.outcomes.lock().unwrap();
    assert_eq!(outcomes.len(), 1, "exactly one spend-ledger row");
    let o = &outcomes[0];
    assert_eq!(
        o.score.verdict,
        JudgeVerdict::Unclear,
        "a failed judge never fabricates a verdict — unclear has no valence"
    );
    assert!(
        o.score.reason.starts_with("unjudged:"),
        "the reason names the failure: {}",
        o.score.reason
    );
    assert!(o.baseline_dispatched);
    // gpt-4o (5/15 per M) on the mock's 100/100 usage = 0.002.
    let baseline = o
        .baseline_cost_usd
        .expect("the billed baseline dispatch is metered");
    assert!(
        (baseline - 0.002).abs() < 1e-9,
        "baseline tax must be ledgered, got {baseline}"
    );
    assert_eq!(
        o.judge_cost_usd, None,
        "the failed judge call was ATTEMPTED (possibly billed upstream) — \
         its tax is unknown (NULL), never a fabricated $0"
    );
    assert_eq!(o.orders_judged, 0);
}

/// A route that opted into the redaction guardrail (`redact: true`) skips the
/// judge WHOLESALE: the judge captures hold the pre-redaction request, and the
/// judge job would re-dispatch it verbatim (baseline reference) and embed its
/// text in the judge prompt — potentially to a third vendor. The redaction
/// promise ("the secret never reaches the upstream provider") must hold on the
/// measurement path too, so no judge call fires and nothing is recorded even
/// at sample rate 1.0.
#[tokio::test]
async fn redact_route_skips_judge_entirely() {
    let h = build_harness_opts(HarnessOpts {
        redact_route: true,
        ..Default::default()
    })
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o-mini",
        "the downgrade route still fires — only the judge is skipped"
    );

    // Give any (erroneously) spawned judge task a chance to run.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        h.judge_calls.load(Ordering::SeqCst),
        0,
        "a redact route must never spawn the judge (the pre-redaction \
         request must not ride the measurement path)"
    );
    assert_eq!(h.sink.outcomes.lock().unwrap().len(), 0);
    // Exactly ONE upstream dispatch: the user's own request. No baseline
    // reference re-dispatch of the unredacted original.
    assert_eq!(
        h.served_calls.load(Ordering::SeqCst),
        1,
        "no baseline reference dispatch of the unredacted request"
    );
}

/// The production judge wiring bounds each judge call with
/// `with_call_timeout` (the baseline timeout): a judge upstream that HANGS
/// (gate never released) times out instead of pinning the detached task, and
/// the attempted (possibly billed) call + the billed baseline dispatch are
/// ledgered as an `unclear` spend row. Deleting the `with_call_timeout` wiring
/// in `maybe_spawn_quality_judge` makes this test hang past its bound and
/// fail.
#[tokio::test]
async fn hung_judge_call_is_bounded_and_ledgered() {
    let gate = Arc::new(Notify::new()); // never released — the judge hangs
    let h = build_harness_opts(HarnessOpts {
        judge_gate: Some(gate),
        baseline_timeout: Some(std::time::Duration::from_millis(100)),
        ..Default::default()
    })
    .await;

    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "user path untouched");

    // The judge call times out after ~100ms; the spend row must land well
    // within the 5s bound. Without the call timeout the gated judge call
    // would hang forever and nothing would ever be recorded.
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("a timed-out judge call must still ledger its measurement spend");

    let outcomes = h.sink.outcomes.lock().unwrap();
    assert_eq!(outcomes.len(), 1);
    let o = &outcomes[0];
    assert_eq!(o.score.verdict, JudgeVerdict::Unclear);
    assert!(
        o.score.reason.contains("deadline exceeded"),
        "the reason names the timeout: {}",
        o.score.reason
    );
    assert!(o.baseline_dispatched, "the baseline dispatch was billed");
    assert!(
        o.baseline_cost_usd.is_some(),
        "the completed baseline dispatch is metered"
    );
    assert_eq!(
        o.judge_cost_usd, None,
        "a timed-out judge call's tax is unknown — NULL, never $0"
    );
}

// ── Cross-provider judge credentials (fail closed, never cross-sent) ────────

/// A judge-only provider on a DIFFERENT provider id than the source request,
/// recording the api key of every call so the test can prove which credential
/// was transmitted.
struct RemoteJudgeProvider {
    keys_seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Provider for RemoteJudgeProvider {
    fn id(&self) -> &'static str {
        "judgeprov"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "judge-remote".into(),
            provider: "judgeprov".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.1,
            output_per_million: 0.4,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            effective_at: Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.keys_seen
            .lock()
            .unwrap()
            .push(ctx.credentials.api_key.expose().to_string());
        Ok(ChatCompletionResponse {
            id: "chatcmpl-remote".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(
                        "EQUIVALENT\nsame material information".into(),
                    )),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 100,
                total_tokens: 200,
                cached_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
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

struct CrossProviderHarness {
    app: axum::Router,
    /// Api key of every call that reached the REMOTE judge provider. Emptiness
    /// proves the judge vendor was never called at all (the fail-closed
    /// assertion); contents prove WHICH credential was transmitted.
    judge_keys_seen: Arc<Mutex<Vec<String>>>,
    sink: Arc<RecordingSink>,
    plaintext: String,
}

/// Harness with the judge model on a SECOND provider (`judgeprov`) and a
/// per-org credential store. `with_judge_credential` controls whether the org
/// has a stored credential for the judge provider.
async fn build_cross_provider_harness(with_judge_credential: bool) -> CrossProviderHarness {
    use tt_auth::{InMemoryProviderCredentialStore, ProviderCredentialStore};
    use tt_shared::context::{ProviderCredentials, SecretString};

    let served_calls = Arc::new(AtomicUsize::new(0));
    let judge_keys_seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(JudgeAwareProvider {
        judge_calls: Arc::new(AtomicUsize::new(0)), // unused: judge lives remote
        served_calls: Arc::clone(&served_calls),
        judge_gate: None,
        judge_verdict_word: "ACCEPTABLE",
        judge_fail: false,
        judge_prompts: Arc::new(Mutex::new(Vec::new())),
        served_body: None,
    }));
    registry.register(Arc::new(RemoteJudgeProvider {
        keys_seen: Arc::clone(&judge_keys_seen),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let audit = InMemoryAuditWriter::new();
    let plaintext = issue(
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

    // Per-org credential store: the source provider always has a key; the
    // judge provider's key is present only when `with_judge_credential`.
    let creds = InMemoryProviderCredentialStore::new();
    let cred = |key: &str| ProviderCredentials {
        api_key: SecretString::new(key.to_string()),
        base_url: None,
        extra_headers: Vec::new(),
    };
    creds.insert(org_id, "judgeaware", cred("sk-source"));
    if with_judge_credential {
        creds.insert(org_id, "judgeprov", cred("sk-judge"));
    }
    let cred_store: Arc<dyn ProviderCredentialStore> = Arc::new(creds);

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(
        org_id,
        vec![Route {
            paused: false,
            id: Uuid::now_v7(),
            name: "downgrade-4o".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
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
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let sink = Arc::new(RecordingSink::default());
    let config = JudgeConfig {
        enabled: true,
        sample_rate: 1.0,
        judge_model: "judge-remote".to_string(),
        ..JudgeConfig::default()
    };
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(cred_store)
            .with_routing_store(routing)
            .with_quality_judge(sink.clone() as Arc<dyn JudgeSink>, config),
    );

    CrossProviderHarness {
        app,
        judge_keys_seen,
        sink,
        plaintext,
    }
}

/// A judge model on a DIFFERENT provider uses the judge provider's OWN stored
/// credential — the source provider's key is never transmitted to the judge
/// vendor (the #128 cross-provider credential leak, fixed).
#[tokio::test]
async fn cross_provider_judge_uses_judge_providers_own_credential() {
    let h = build_cross_provider_harness(true).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.sink.recorded.notified(),
    )
    .await
    .expect("judge should record");

    let keys = h.judge_keys_seen.lock().unwrap();
    assert!(
        !keys.is_empty(),
        "the remote judge provider must have been called"
    );
    for key in keys.iter() {
        assert_eq!(
            key, "sk-judge",
            "the judge call must carry the JUDGE provider's credential, \
             never the source provider's key"
        );
    }
    let outcomes = h.sink.outcomes.lock().unwrap();
    assert_eq!(outcomes[0].score.verdict, JudgeVerdict::Acceptable);
}

/// A verified org with NO stored credential for the judge provider fails
/// CLOSED: the judge is skipped entirely (no call, nothing recorded, no
/// cross-provider key leak) and the user response is untouched.
#[tokio::test]
async fn cross_provider_judge_without_credential_fails_closed() {
    let h = build_cross_provider_harness(false).await;
    let resp = h
        .app
        .clone()
        .oneshot(chat_request("gpt-4o", &h.plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "user path untouched");

    // Give the detached task time to (not) fire.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        h.judge_keys_seen.lock().unwrap().is_empty(),
        "no credential for the judge provider → the judge vendor must never \
         be called (fail closed, no key leak)"
    );
    assert_eq!(h.sink.outcomes.lock().unwrap().len(), 0);
}
