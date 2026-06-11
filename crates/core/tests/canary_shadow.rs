//! End-to-end (hermetic) tests for canary traffic splits + shadow mode (#454):
//!
//! (a) SHADOW MODE actually dispatches the candidate: a mock provider records
//!     every model it was called with; a route with `shadow_model` set must
//!     produce a call for BOTH the primary AND the shadow model (proving the
//!     shadow provider was really called — not just a flag toggled).
//! (b) The shadow cost is recorded SEPARATELY on the request_logs row
//!     (`shadow_cost_usd` column) and NEVER folded into the primary `cost_usd`.
//! (c) The PRIMARY response is returned unchanged — the client gets the primary
//!     model's response, not the shadow's.
//! (d) Shadow is single-candidate (no failover): exactly ONE shadow call fires.
//! (e) `traffic_split_arm` is recorded on the row for a canary-split route.
//!
//! No network: a mock provider serving two models (`primary-model`,
//! `shadow-model`) records the per-call model + a distinguishable response body,
//! mirroring `flex_route.rs` / `route_rewrite.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
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
use tt_telemetry::request_logs::{InMemoryRequestLogWriter, RequestLogWriter};
use uuid::Uuid;

const PRIMARY_MODEL: &str = "primary-model";
const SHADOW_MODEL: &str = "shadow-model";
const PROMPT_TOKENS: u64 = 1000;
const COMPLETION_TOKENS: u64 = 500;

/// Mock provider serving both the primary and shadow models. Records, per call,
/// the model it was asked to serve, so a test can prove the shadow model was
/// ACTUALLY dispatched (not just configured). Prices the two models differently
/// so the shadow cost is verifiably distinct from the primary cost.
struct RecordingProvider {
    /// Models seen on each `chat_completion` call, in order.
    models_called: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "rec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        [PRIMARY_MODEL, SHADOW_MODEL]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "rec".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 8192,
            })
            .collect()
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        // Primary $10/$30 per 1M, shadow $2/$6 per 1M — distinct so the shadow
        // cost can never be confused with the primary cost.
        let (input, output) = match model {
            SHADOW_MODEL => (2.0, 6.0),
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
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.models_called.lock().unwrap().push(req.model.clone());
        // Distinguishable body so the test can assert the PRIMARY response was
        // returned (never the shadow's).
        let text = format!("response-from-{}", req.model);
        Ok(ChatCompletionResponse {
            id: format!("chatcmpl-{}", req.model),
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
        Err(ProviderError::Unsupported("stream not used".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

fn chat_request(model: &str, bearer: &str, idempotency_key: Option<&str>) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hello world" }],
        "stream": false,
    });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"));
    if let Some(k) = idempotency_key {
        b = b.header("x-idempotency-key", k);
    }
    b.body(Body::from(body.to_string())).unwrap()
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

/// Build a gateway whose org has a single route matching `PRIMARY_MODEL`,
/// carrying the supplied `then` action. Returns the app, the key, the recorded
/// per-call models, the call counter, and the in-memory request-log writer.
async fn app_with_route(
    then: RouteAction,
) -> (
    axum::Router,
    String,
    Arc<Mutex<Vec<String>>>,
    Arc<AtomicUsize>,
    Arc<InMemoryRequestLogWriter>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models_called = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        models_called: Arc::clone(&models_called),
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
            name: "canary-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec![PRIMARY_MODEL.into()],
                ..Default::default()
            },
            then,
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
    (app, plaintext, models_called, calls, log_writer)
}

/// Give the fire-and-forget `spawn_request_log` task a moment to run after the
/// response is returned, then snapshot the rows.
async fn rows_after(
    writer: &Arc<InMemoryRequestLogWriter>,
) -> Vec<tt_telemetry::request_logs::RequestLogRow> {
    for _ in 0..50 {
        let rows = writer.rows();
        if !rows.is_empty() {
            return rows;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    writer.rows()
}

/// (a)+(b)+(c)+(d): a route with `shadow_model` ALSO dispatches the shadow (the
/// provider is really called with the shadow model), records its cost SEPARATELY
/// (never in the primary cost), returns the PRIMARY response unchanged, and fires
/// the shadow exactly once (single candidate, no failover).
#[tokio::test]
async fn shadow_mode_dispatches_candidate_and_records_cost_separately() {
    let then = RouteAction {
        // No model rewrite: serve the primary, shadow the candidate.
        target_model: PRIMARY_MODEL.into(),
        fallbacks: Vec::new(),
        disable_cache: false,
        max_cost_usd: None,
        flex: false,
        compress: false,
        redact: false,
        // shadow with no traffic_pct = 100% shadow, primary still serves.
        traffic_pct: None,
        shadow_model: Some(SHADOW_MODEL.into()),
    };
    let (app, key, models_called, calls, log_writer) = app_with_route(then).await;

    let resp = app
        .oneshot(chat_request(PRIMARY_MODEL, &key, Some("req-1")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // (a) BOTH models were dispatched — the shadow provider was really called.
    let seen = models_called.lock().unwrap().clone();
    assert!(
        seen.contains(&PRIMARY_MODEL.to_string()),
        "primary must be dispatched: {seen:?}"
    );
    assert!(
        seen.contains(&SHADOW_MODEL.to_string()),
        "shadow model must be ACTUALLY dispatched (not just flagged): {seen:?}"
    );
    // (d) Exactly two calls total: one primary + one shadow (no failover storm).
    assert_eq!(
        calls.load(Ordering::Relaxed),
        2,
        "exactly one primary + one shadow call (single candidate, no failover)"
    );
    assert_eq!(
        seen.iter().filter(|m| *m == SHADOW_MODEL).count(),
        1,
        "shadow fires exactly once"
    );

    // (c) The client got the PRIMARY response, not the shadow's.
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["model"], PRIMARY_MODEL);
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(
        content,
        format!("response-from-{PRIMARY_MODEL}"),
        "the PRIMARY response must be returned, never the shadow's"
    );

    // (b) The shadow cost is recorded SEPARATELY on the row, never folded into
    // the primary cost.
    let rows = rows_after(&log_writer).await;
    assert_eq!(rows.len(), 1, "exactly one request_logs row");
    let row = &rows[0];
    // Primary cost = 1000×$10/M + 500×$30/M = 0.025.
    let expected_primary =
        PROMPT_TOKENS as f64 * 10.0 / 1e6 + COMPLETION_TOKENS as f64 * 30.0 / 1e6;
    // Shadow cost = 1000×$2/M + 500×$6/M = 0.005.
    let expected_shadow = PROMPT_TOKENS as f64 * 2.0 / 1e6 + COMPLETION_TOKENS as f64 * 6.0 / 1e6;
    assert!(
        (row.cost_usd - expected_primary).abs() < 1e-9,
        "primary cost must NOT include the shadow cost: got {}",
        row.cost_usd
    );
    assert_eq!(row.shadow_model.as_deref(), Some(SHADOW_MODEL));
    let shadow_cost = row
        .shadow_cost_usd
        .expect("shadow_cost_usd must be recorded");
    assert!(
        (shadow_cost - expected_shadow).abs() < 1e-9,
        "shadow cost must be its own value ({expected_shadow}), got {shadow_cost}"
    );
    // The two are genuinely distinct (the bug being guarded would equal them).
    assert!((shadow_cost - row.cost_usd).abs() > 1e-9);
}

/// The DEFAULT path (no shadow_model) never dispatches a shadow — only the
/// primary is called, and no shadow cost/model is recorded. Guards the opt-in
/// requirement (shadow must NOT fire unless a route enables it).
#[tokio::test]
async fn no_shadow_when_route_does_not_opt_in() {
    let then = RouteAction {
        target_model: PRIMARY_MODEL.into(),
        fallbacks: Vec::new(),
        disable_cache: false,
        max_cost_usd: None,
        flex: false,
        compress: false,
        redact: false,
        traffic_pct: None,
        shadow_model: None,
    };
    let (app, key, _models, calls, log_writer) = app_with_route(then).await;

    let resp = app
        .oneshot(chat_request(PRIMARY_MODEL, &key, Some("req-2")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        calls.load(Ordering::Relaxed),
        1,
        "exactly one (primary) call — no shadow"
    );

    let rows = rows_after(&log_writer).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].shadow_model, None);
    assert_eq!(rows[0].shadow_cost_usd, None);
    assert_eq!(rows[0].traffic_split_arm, None);
}

/// (e) A canary-split route records the assigned `traffic_split_arm` on the row.
/// With `traffic_pct=100` every request is in the canary arm (deterministically),
/// so the row carries `arm="canary"`.
#[tokio::test]
async fn canary_split_records_arm() {
    let then = RouteAction {
        target_model: PRIMARY_MODEL.into(),
        fallbacks: Vec::new(),
        disable_cache: false,
        max_cost_usd: None,
        flex: false,
        compress: false,
        redact: false,
        traffic_pct: Some(100),
        shadow_model: None,
    };
    let (app, key, _models, _calls, log_writer) = app_with_route(then).await;

    let resp = app
        .oneshot(chat_request(PRIMARY_MODEL, &key, Some("req-3")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let rows = rows_after(&log_writer).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].traffic_split_arm.as_deref(),
        Some("canary"),
        "traffic_pct=100 → every request in the canary arm"
    );
}

/// `traffic_pct=0` puts every matched request in the CONTROL arm: the model is
/// reverted to the originally-requested model (served unchanged) and the row
/// records `arm="control"`.
#[tokio::test]
async fn zero_pct_split_serves_control_arm() {
    // Rewrite to the SHADOW_MODEL as the canary target so a control-arm revert is
    // observable (the served model differs between arms).
    let then = RouteAction {
        target_model: SHADOW_MODEL.into(),
        fallbacks: Vec::new(),
        disable_cache: false,
        max_cost_usd: None,
        flex: false,
        compress: false,
        redact: false,
        traffic_pct: Some(0),
        shadow_model: None,
    };
    let (app, key, models_called, _calls, log_writer) = app_with_route(then).await;

    let resp = app
        .oneshot(chat_request(PRIMARY_MODEL, &key, Some("req-4")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Control arm → served the ORIGINAL model, not the canary target.
    let seen = models_called.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![PRIMARY_MODEL.to_string()],
        "0% canary → control arm serves the originally-requested model"
    );

    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["model"], PRIMARY_MODEL, "control arm serves original");

    let rows = rows_after(&log_writer).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].traffic_split_arm.as_deref(), Some("control"));
    assert_eq!(rows[0].model, PRIMARY_MODEL);
}
