//! End-to-end auto-pause integration (research Phase 2.3): a regressing
//! down-route — paired judge always Degraded — accrues classified verdicts
//! through the fanout sink until the windowed pass-rate decision trips, the
//! route is sticky-paused in the store, the `route_auto_paused_total` metric
//! is emitted, and the NEXT request passes through on the original model.
//!
//! Hermetic: one recording provider serves the chat models AND the cheap
//! judge model (replying with the paired-comparison token that names the
//! optimized slot as missing → every verdict is Degraded); an
//! [`InMemoryVerdictWindow`] stands in for the durable `quality_verdicts`
//! window, wired as a sink AHEAD of the evaluator exactly like the production
//! `[persistent, auto_pause]` fanout order.

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
use tt_core::quality_sample::{FanoutJudgeSink, JudgeConfig, JudgeOutcome, JudgeSink};
use tt_core::route_autopause::{AutoPauseJudgeSink, InMemoryVerdictWindow, VerdictWindowSource};
use tt_core::{build_router, AppState, ProviderRegistry};
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

const JUDGE_MODEL: &str = "judge-cheap";

/// Serves gpt-4o / gpt-4o-mini / the judge model. The judge reply names
/// whichever blind slot holds the OPTIMIZED (cheaper-model) answer as missing
/// material info → the paired verdict is always **Degraded**.
struct AlwaysDegradedProvider {
    models_called: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

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
impl Provider for AlwaysDegradedProvider {
    fn id(&self) -> &'static str {
        "alwaysdegraded"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["gpt-4o", "gpt-4o-mini", JUDGE_MODEL]
            .into_iter()
            .map(|m| ModelInfo {
                id: m.into(),
                provider: "alwaysdegraded".into(),
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
        let text = if req.model == JUDGE_MODEL {
            // Name the blind slot holding the optimized (gpt-4o-mini) answer.
            let prompt = request_text(&req);
            let a_start = prompt.find("ANSWER A:").unwrap_or(0);
            let b_start = prompt.find("ANSWER B:").unwrap_or(prompt.len());
            if prompt[a_start..b_start].contains("gpt-4o-mini") {
                "A_MISSING — dropped material info".to_string()
            } else {
                "B_MISSING — dropped material info".to_string()
            }
        } else {
            format!("answer from {}", req.model)
        };
        Ok(ChatCompletionResponse {
            id: "chatcmpl-ap".into(),
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

/// Recording sink standing in for the band store (records every outcome).
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

fn chat_request(model: &str, bearer: &str, salt: usize) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(
            json!({
                "model": model,
                // Unique content per request so no cache layer can coalesce.
                "messages": [{"role": "user", "content": format!("question {salt}")}],
                "stream": false,
            })
            .to_string(),
        ))
        .unwrap()
}

/// Drive 3 always-Degraded sampled verdicts through the production fanout
/// (`[recording+window, auto_pause]`, in order) on an `auto_pause: true`
/// route with `pause_min_verdicts: 3` → the route is sticky-paused in the
/// store, `route_auto_paused_total` is emitted, and the next request passes
/// through on the original model.
#[tokio::test]
async fn end_to_end_auto_pause() {
    let calls = Arc::new(AtomicUsize::new(0));
    let models_called = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(AlwaysDegradedProvider {
        models_called: Arc::clone(&models_called),
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
            name: "regressing-downgrade".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                auto_pause: true,
                pause_min_verdicts: Some(3),
                // floor defaults to 0.90; all-Degraded gives 0.0 pass rate.
                ..Default::default()
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(
        routes_backing as Arc<dyn RoutingStore>,
    ));

    // Production-shaped fanout: the window sink (stand-in for the durable
    // PostgresJudgeSink + PgVerdictWindow pair) records BEFORE the appended
    // evaluator consults it — FanoutJudgeSink awaits in order.
    let recording = Arc::new(RecordingSink::default());
    let window = Arc::new(InMemoryVerdictWindow::new());
    let recording_and_window: Arc<dyn JudgeSink> = Arc::new(FanoutJudgeSink::new(vec![
        recording.clone() as Arc<dyn JudgeSink>,
        window.clone() as Arc<dyn JudgeSink>,
    ]));
    let evaluator = Arc::new(AutoPauseJudgeSink::new(
        routing.clone(),
        window.clone() as Arc<dyn VerdictWindowSource>,
    ));

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing.clone())
            .with_quality_judge(
                recording_and_window,
                JudgeConfig {
                    enabled: true,
                    sample_rate: 1.0,
                    judge_model: JUDGE_MODEL.to_string(),
                    ..JudgeConfig::default()
                },
            )
            .with_route_auto_pause(evaluator),
    );

    // ── Drive 3 sampled rerouted-down requests; each records a Degraded
    // verdict through the fanout. ─────────────────────────────────────────
    for i in 0..3 {
        let resp = app
            .clone()
            .oneshot(chat_request("gpt-4o", &key, i))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()["x-tokentrimmer-model-used"]
                .to_str()
                .unwrap(),
            "gpt-4o-mini",
            "request {i}: route still rewrites until the pause trips"
        );
        // The judge task is detached — wait for THIS request's verdict so the
        // window grows deterministically (and the evaluator runs after it).
        for _ in 0..200 {
            if recording.outcomes.lock().unwrap().len() > i {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            recording.outcomes.lock().unwrap().len(),
            i + 1,
            "request {i}: verdict must be recorded"
        );
    }
    assert!(
        recording
            .outcomes
            .lock()
            .unwrap()
            .iter()
            .all(|o| o.route_id == Some(route_id)),
        "verdicts must attribute to the route"
    );

    // ── The evaluator runs inside the same fanout await chain, but poll
    // briefly to be robust to scheduler ordering. ─────────────────────────
    let mut paused = false;
    for _ in 0..200 {
        let route = routing
            .get_route(org_id, route_id)
            .await
            .unwrap()
            .expect("route exists");
        if route.paused {
            paused = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        paused,
        "3 Degraded verdicts at min_verdicts=3 must sticky-pause the route"
    );

    // Metric emitted (global recorder → presence check, like metrics_endpoint).
    let rendered = tt_core::metrics::render().expect("recorder installed");
    assert!(
        rendered.contains("route_auto_paused_total") && rendered.contains("regressing-downgrade"),
        "route_auto_paused_total{{route=\"regressing-downgrade\"}} must be emitted: {rendered}"
    );

    // ── The NEXT request passes through on the ORIGINAL model. ────────────
    let dispatches_before = calls.load(Ordering::SeqCst);
    let resp = app
        .clone()
        .oneshot(chat_request("gpt-4o", &key, 99))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["x-tokentrimmer-model-used"]
            .to_str()
            .unwrap(),
        "gpt-4o",
        "paused route must pass through on the requested model"
    );
    assert!(
        resp.headers()["x-tokentrimmer-warnings"]
            .to_str()
            .unwrap()
            .contains("route_paused:regressing-downgrade"),
        "passthrough must carry the route_paused warning"
    );
    // No judge fires on the passthrough (not a downgrade) — the window is
    // frozen, so the pause is naturally sticky.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        recording.outcomes.lock().unwrap().len(),
        3,
        "no further verdicts after the pause"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        dispatches_before + 1,
        "exactly one upstream dispatch for the passthrough (no judge/baseline)"
    );
}
