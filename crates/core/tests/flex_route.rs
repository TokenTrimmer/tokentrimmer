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
    messages::{Choice, Message, MessageContent},
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

fn chat_request(model: &str, bearer: &str) -> Request<Body> {
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
