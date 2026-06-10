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
use tt_core::quality_sample::{JudgeConfig, JudgeOutcome, JudgeSink};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_plan_core::{JudgeVerdict, RiskBand};
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
    /// The verdict the judge model returns (first line of its reply).
    judge_verdict_word: &'static str,
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
            if let Some(gate) = &self.judge_gate {
                // Block the judge call until the test releases it. Proves the
                // user response is independent of judge progress.
                gate.notified().await;
            }
        } else {
            self.served_calls.fetch_add(1, Ordering::SeqCst);
        }
        let text = if is_judge {
            format!("{}\nreason line", self.judge_verdict_word)
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
}

async fn build_harness(
    rate: f64,
    plant_downgrade_route: bool,
    judge_gate: Option<Arc<Notify>>,
    judge_verdict_word: &'static str,
) -> Harness {
    let judge_calls = Arc::new(AtomicUsize::new(0));
    let served_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(JudgeAwareProvider {
        judge_calls: Arc::clone(&judge_calls),
        served_calls: Arc::clone(&served_calls),
        judge_gate,
        judge_verdict_word,
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
    if plant_downgrade_route {
        routes_backing.set_routes(
            org_id,
            vec![Route {
                id: Uuid::now_v7(),
                name: "downgrade-4o".into(),
                priority: 100,
                enabled: true,
                when: RouteConditions {
                    model_in: vec!["gpt-4o".into()],
                    ..Default::default()
                },
                then: RouteAction {
                    target_model: "gpt-4o-mini".into(),
                    fallbacks: Vec::new(),
                    disable_cache: false,
                    max_cost_usd: None,
                },
            }],
        );
    }
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    let sink = Arc::new(RecordingSink::default());
    let config = JudgeConfig {
        enabled: true,
        sample_rate: rate,
        judge_model: JUDGE_MODEL.to_string(),
    };
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
