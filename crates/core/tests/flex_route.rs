//! End-to-end (hermetic) tests for the OpenAI Flex route action (FLEX-REWRITE):
//!
//! (a) a route with `flex: true` + an ELIGIBLE model → the upstream request
//!     carries `service_tier: "flex"`;
//! (b) an INELIGIBLE model → flex NOT applied (+ a `flex_not_applied:<model>`
//!     warning on the `X-TokenTrimmer-Warnings` header);
//! (c) a flex-served response attributes savings == standard_baseline − flex_cost
//!     for the token usage, to the cent, surfaced as the dedicated
//!     `X-TokenTrimmer-Flex-Saved-Usd` header (and folded into `saved_usd`).
//!
//! No network: a mock provider records the upstream request and returns a fixed
//! response, mirroring `route_rewrite.rs`. The mock prices an eligible model
//! with flex rates (50% of standard) and an ineligible model without, so the
//! gateway's catalog-driven eligibility gate (`ModelPricing::flex_eligible`) and
//! the standard−flex savings delta are exercised exactly.

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
use uuid::Uuid;

/// Mock provider that records the `service_tier` value carried on each upstream
/// request (None when absent). Prices a flex-eligible model (`flex-eligible`)
/// with flex rates and an ineligible model (`flex-ineligible`) without.
struct FlexRecordingProvider {
    /// `Some(service_tier)` per call, or `None` when the field was absent.
    service_tiers: Arc<Mutex<Vec<Option<String>>>>,
    calls: Arc<AtomicUsize>,
}

const PROMPT_TOKENS: u64 = 1000;
const COMPLETION_TOKENS: u64 = 500;

#[async_trait]
impl Provider for FlexRecordingProvider {
    fn id(&self) -> &'static str {
        "flexrec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["flex-eligible", "flex-ineligible"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "flexrec".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 8192,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        // Standard $10/$30 per 1M for both; the eligible model also carries flex
        // rates at exactly 50% ($5/$15) — eligibility is the presence of the
        // flex rate, mirroring the real catalog gate.
        let (flex_in, flex_out) = match model {
            "flex-eligible" => (Some(5.0), Some(15.0)),
            _ => (None, None),
        };
        Some(ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: flex_in,
            flex_output_per_million: flex_out,
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
        // `service_tier` rides in the serde-flatten `extra` map.
        let tier = req
            .extra
            .get("service_tier")
            .and_then(|v| v.as_str())
            .map(String::from);
        self.service_tiers.lock().unwrap().push(tier);
        Ok(ChatCompletionResponse {
            id: "chatcmpl-flex".into(),
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        // Record service_tier exactly as the non-streaming path does, so the
        // streaming test can assert the same flex rewrite reached the upstream.
        let tier = req
            .extra
            .get("service_tier")
            .and_then(|v| v.as_str())
            .map(String::from);
        self.service_tiers.lock().unwrap().push(tier);
        // Emit a clean stream (role, content, finish+usage) carrying the same
        // PROMPT/COMPLETION token usage the non-streaming path returns, so the
        // terminal `tokentrimmer.usage` event prices a real (non-zero) cost.
        let chunks = vec![
            Ok(ChatCompletionChunk {
                id: "chatcmpl-flex-stream".into(),
                object: "chat.completion.chunk".into(),
                created: 0,
                model: req.model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: Some("assistant".into()),
                        ..Default::default()
                    },
                    finish_reason: None,
                    extra: Default::default(),
                }],
                usage: None,
                extra: Default::default(),
            }),
            Ok(ChatCompletionChunk {
                id: "chatcmpl-flex-stream".into(),
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
                id: "chatcmpl-flex-stream".into(),
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
    chat_request_streaming(model, bearer, false)
}

/// Like [`chat_request`] but lets the caller pick the `stream` flag so the same
/// flex route can be exercised over the streaming surface.
fn chat_request_streaming(model: &str, bearer: &str, stream: bool) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello world" }],
        "stream": stream,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
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

/// Build a gateway whose org has a single route matching `model_in` with the
/// flex action set, returning the app + the recorded-service-tier handle.
async fn app_with_flex_route(
    model_in: &str,
) -> (
    axum::Router,
    String,
    Arc<Mutex<Vec<Option<String>>>>,
    Arc<AtomicUsize>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let tiers = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(FlexRecordingProvider {
        service_tiers: Arc::clone(&tiers),
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
            id: Uuid::now_v7(),
            name: "flex-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec![model_in.into()],
                ..Default::default()
            },
            // No model rewrite — flex is a pure request-parameter action.
            then: RouteAction {
                target_model: model_in.into(),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: true,
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

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );
    (app, plaintext, tiers, calls)
}

/// (a) Eligible model + flex route → upstream request carries service_tier=flex.
#[tokio::test]
async fn flex_applied_to_eligible_model_sets_service_tier() {
    let (app, key, tiers, calls) = app_with_flex_route("flex-eligible").await;

    let resp = app
        .oneshot(chat_request("flex-eligible", &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // The provider observed service_tier=flex on the upstream request.
    assert_eq!(
        tiers.lock().unwrap().as_slice(),
        [Some("flex".to_string())],
        "eligible model must dispatch with service_tier=flex"
    );
}

/// (b) Ineligible model + flex route → flex NOT applied + warning surfaced.
#[tokio::test]
async fn flex_not_applied_to_ineligible_model_warns() {
    let (app, key, tiers, calls) = app_with_flex_route("flex-ineligible").await;

    let resp = app
        .oneshot(chat_request("flex-ineligible", &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // service_tier MUST NOT be set on an ineligible model.
    assert_eq!(
        tiers.lock().unwrap().as_slice(),
        [None],
        "ineligible model must not carry service_tier"
    );

    // A `flex_not_applied:<model>` warning is surfaced via the warnings header.
    let warnings = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        warnings
            .split(',')
            .any(|w| w == "flex_not_applied:flex-ineligible"),
        "expected flex_not_applied warning, got: {warnings:?}"
    );

    // And no phantom flex saving is claimed.
    let flex_saved: f64 = resp.headers()["x-tokentrimmer-flex-saved-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(flex_saved, 0.0, "ineligible → zero flex saving");
}

/// (c) Flex-served response attributes savings == standard_baseline − flex_cost,
/// to the cent, as the dedicated flex source (and inside `saved_usd`).
#[tokio::test]
async fn flex_saving_equals_standard_minus_flex_to_the_cent() {
    let (app, key, _tiers, _calls) = app_with_flex_route("flex-eligible").await;

    let resp = app
        .oneshot(chat_request("flex-eligible", &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let header_f64 = |name: &str| -> f64 {
        resp.headers()[name]
            .to_str()
            .unwrap()
            .parse::<f64>()
            .unwrap()
    };

    // Expected figures for usage prompt=1000, completion=500:
    //   standard = 1000×$10/M + 500×$30/M = 0.01 + 0.015 = 0.025
    //   flex     = 1000×$5/M  + 500×$15/M = 0.005 + 0.0075 = 0.0125
    //   flex_saved = standard − flex = 0.0125
    let standard = (PROMPT_TOKENS as f64) * 10.0 / 1e6 + (COMPLETION_TOKENS as f64) * 30.0 / 1e6;
    let flex = (PROMPT_TOKENS as f64) * 5.0 / 1e6 + (COMPLETION_TOKENS as f64) * 15.0 / 1e6;
    let expected_saved = standard - flex;

    let cost = header_f64("x-tokentrimmer-cost-usd");
    let flex_saved = header_f64("x-tokentrimmer-flex-saved-usd");
    let saved = header_f64("x-tokentrimmer-saved-usd");

    // Cost is billed at flex rates.
    assert!(
        (cost - flex).abs() < 1e-9,
        "cost ({cost}) should be the flex cost ({flex})"
    );
    // The dedicated flex-saved figure equals standard − flex to the cent.
    assert!(
        (flex_saved - expected_saved).abs() < 0.005,
        "flex_saved ({flex_saved}) should equal standard − flex ({expected_saved}) to the cent"
    );
    assert!(
        (flex_saved - expected_saved).abs() < 1e-9,
        "flex_saved ({flex_saved}) should equal standard − flex ({expected_saved}) exactly"
    );
    // With no routing rewrite, the headline TT saving IS the flex saving.
    assert!(
        (saved - expected_saved).abs() < 1e-9,
        "headline saved ({saved}) should equal the flex saving ({expected_saved})"
    );
}

/// Extract the JSON payload of the terminal `tokentrimmer.usage` SSE event from
/// a drained event-stream body (`event: tokentrimmer.usage\ndata: {json}`).
fn parse_tokentrimmer_usage(body: &str) -> serde_json::Value {
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        if line.trim() == "event: tokentrimmer.usage" {
            let data_line = lines
                .find(|l| l.starts_with("data:"))
                .expect("tokentrimmer.usage event missing data line");
            let json = data_line.trim_start_matches("data:").trim();
            return serde_json::from_str(json).expect("tokentrimmer.usage data is valid JSON");
        }
    }
    panic!("no tokentrimmer.usage event found in stream body:\n{body}");
}

/// (a+c, streaming surface) A STREAMING request matched by the same flex route
/// on an eligible model must (1) carry `service_tier=flex` on the upstream
/// streaming dispatch and (2) attribute the standard−flex saving on the
/// terminal `tokentrimmer.usage` event — cost at flex rates, `saved_usd` ==
/// standard − flex. Without threading `flex_applied` into `StreamLogContext`
/// the streaming cost path prices at STANDARD rates (≈2× over-report) and drops
/// the entire flex saving, so this guards FLEX-REWRITE requirement (2) for
/// streaming.
#[tokio::test]
async fn flex_streaming_attributes_savings_on_usage_event() {
    let (app, key, tiers, calls) = app_with_flex_route("flex-eligible").await;

    let resp = app
        .oneshot(chat_request_streaming("flex-eligible", &key, true))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // (1) The streaming dispatch carried service_tier=flex upstream — the same
    // rewrite the non-streaming path applies.
    assert_eq!(
        tiers.lock().unwrap().as_slice(),
        [Some("flex".to_string())],
        "streaming eligible model must dispatch with service_tier=flex"
    );

    // Drain the SSE body and pull the terminal tokentrimmer.usage event.
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body = String::from_utf8_lossy(&bytes).to_string();
    let usage = parse_tokentrimmer_usage(&body);

    // Expected figures for usage prompt=1000, completion=500 (same as the
    // non-streaming (c) test):
    //   standard = 1000×$10/M + 500×$30/M = 0.025
    //   flex     = 1000×$5/M  + 500×$15/M = 0.0125
    //   flex_saved = standard − flex = 0.0125
    let standard = (PROMPT_TOKENS as f64) * 10.0 / 1e6 + (COMPLETION_TOKENS as f64) * 30.0 / 1e6;
    let flex = (PROMPT_TOKENS as f64) * 5.0 / 1e6 + (COMPLETION_TOKENS as f64) * 15.0 / 1e6;
    let expected_saved = standard - flex;

    let cost = usage["cost_usd"].as_f64().expect("cost_usd is a number");
    let saved = usage["saved_usd"].as_f64().expect("saved_usd is a number");

    // (2) Cost is billed at flex rates (NOT standard — would be ≈2× here).
    assert!(
        (cost - flex).abs() < 1e-9,
        "streaming cost ({cost}) should be the flex cost ({flex}), not standard ({standard})"
    );
    // The headline streaming saving IS the flex saving (no routing rewrite).
    assert!(
        (saved - expected_saved).abs() < 1e-9,
        "streaming saved ({saved}) should equal the flex saving ({expected_saved})"
    );
    // Sanity: the bug being guarded would report cost==standard and saved==0.
    assert!(
        (cost - standard).abs() > 1e-9,
        "regression guard: streaming cost must not be the standard cost ({standard})"
    );
}
