//! Route-triggered deep-research panel — Task 2 integration suite.
//!
//! A matched route whose `then.panel` is set triggers the panel WITHOUT a
//! caller `X-TokenTrimmer-Panel` header. The header still WINS when present
//! (D2). A route-triggered panel goes through the SAME kill-switch / entitlement
//! / budget gates and the SAME credential resolution as a header-triggered one
//! (D3). A paused panel route does NOT trigger the panel. A route with no panel
//! and no header is byte-identical single-model (off-by-default).
//!
//! Combines the routing harness (real key store + routing store, org derived
//! from the `tt_live_` key) from `route_header.rs` with the mock-panel app
//! harness (panel-enabled, request-log writer, counted dispatch) from
//! `panel_engine.rs`.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{json, Value};
use tokio_util::task::TaskTracker;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, KeyStore,
};
use tt_core::{
    build_router,
    tier_resolver::{ResolvedTier, TierResolver, TierResolverError},
    AppState, ProviderRegistry,
};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutePanel,
    RoutingStore,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    CallerTier, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext, Usage,
};
use tt_telemetry::{
    audit::{Actor, InMemoryAuditWriter},
    request_logs::InMemoryRequestLogWriter,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Counted mock provider — one model per provider id, shared call counter.
// ---------------------------------------------------------------------------

/// A priced, counted provider serving a single model. The call counter lets
/// tests assert ZERO upstream dispatches on early-reject paths (kill-switch,
/// entitlement, budget). One `CountedMock` per provider id; tests register
/// several (distinct ids, distinct models) so a panel config can name multiple
/// members — mirrors `panel_engine.rs::app_two_providers`.
struct CountedMock {
    id: &'static str,
    model: &'static str,
    calls: Arc<AtomicUsize>,
}

impl CountedMock {
    fn new(id: &'static str, model: &'static str, calls: Arc<AtomicUsize>) -> Self {
        Self { id, model, calls }
    }
}

#[async_trait]
impl Provider for CountedMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        if model == self.model {
            Some(ModelPricing {
                input_per_million: 1.0,
                output_per_million: 2.0,
                cached_input_per_million: None,
                cache_write_per_million: None,
                batch_input_per_million: None,
                batch_output_per_million: None,
                flex_input_per_million: None,
                flex_output_per_million: None,
                prompt_cache_min_tokens: None,
                effective_at: Utc::now(),
            })
        } else {
            None
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("answer".into())),
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

// ---------------------------------------------------------------------------
// Fixed tier resolver (for the entitlement-gate test).
// ---------------------------------------------------------------------------

/// Always resolves to the given `CallerTier` with uncapped limits.
struct FixedTierResolver {
    tier: CallerTier,
}

#[async_trait]
impl TierResolver for FixedTierResolver {
    async fn resolve(&self, _org_id: Uuid) -> Result<ResolvedTier, TierResolverError> {
        use tt_core::budget::BudgetLimits;
        Ok(ResolvedTier {
            caller_tier: self.tier,
            limits: BudgetLimits {
                monthly_cap_usd: None,
                max_requests_per_min: None,
                monthly_request_cap: None,
                monthly_served_cap: None,
                l2_cache: self.tier != CallerTier::Free,
            },
            semantic_cache_disabled: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Harness — routing store + panel + a real key (org derives from the key).
// ---------------------------------------------------------------------------

/// Register two member models + one arbiter model, each on its own provider id,
/// all sharing `calls`. Models: "model-a", "model-b" (members), "model-arb"
/// (arbiter). The request's `model` is also "model-a" so the route can match on
/// `model_equals`.
fn registry_with_panel_models(calls: &Arc<AtomicUsize>) -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountedMock::new(
        "vendor-a",
        "model-a",
        Arc::clone(calls),
    )));
    registry.register(Arc::new(CountedMock::new(
        "vendor-b",
        "model-b",
        Arc::clone(calls),
    )));
    registry.register(Arc::new(CountedMock::new(
        "vendor-arb",
        "model-arb",
        Arc::clone(calls),
    )));
    registry
}

async fn issue_key(store: &InMemoryKeyStore, org: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(store, &audit, org, "k", Environment::Live, Actor::System)
        .await
        .unwrap()
        .plaintext
}

/// Build an app with `routes` for the caller-org, panel-enabled (configurable),
/// optional min-tier, optional tier resolver. Returns
/// (app, key, request_log_writer, telemetry_tracker, call_counter).
async fn app_with_routes(
    routes: Vec<Route>,
    panel_enabled: bool,
    min_tier: Option<CallerTier>,
    tier_resolver: Option<Arc<dyn TierResolver>>,
) -> (
    axum::Router,
    String,
    Arc<InMemoryRequestLogWriter>,
    TaskTracker,
    Arc<AtomicUsize>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let registry = registry_with_panel_models(&calls);

    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org, routes);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));

    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let tracker = TaskTracker::new();

    let mut state = AppState::new(registry)
        .with_key_store(key_store)
        .with_routing_store(routing)
        .with_panel_enabled(panel_enabled)
        .with_request_log_writer(writer.clone())
        .with_telemetry_tracker(tracker.clone());
    if let Some(t) = min_tier {
        state = state.with_panel_min_tier(t);
    }
    if let Some(tr) = tier_resolver {
        state = state.with_tier_resolver(tr);
    }
    (build_router(state), key, writer, tracker, calls)
}

/// A modifier-only route (no target_model) whose `then.panel` triggers the
/// panel. `strategy` is the route's panel strategy. Matches `model_equals`
/// "model-a" so a request on that model routes to it.
fn panel_route(name: &str, strategy: &str, paused: bool, max_cost_usd: Option<f64>) -> Route {
    Route {
        paused,
        id: Uuid::now_v7(),
        name: name.into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["model-a".into()],
            ..Default::default()
        },
        then: RouteAction {
            // modifier-only: no rewrite, the panel is the effect.
            target_model: None,
            fallbacks: vec![],
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: false,
            doc_compaction: false,
            document_lane: false,
            redact: false,
            format_switch: None,
            diff: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            agentic_budget: None,
            panel: Some(RoutePanel {
                strategy: strategy.into(),
                members: vec!["model-a".into(), "model-b".into()],
                arbiter: Some("model-arb".into()),
                quorum: None,
                max_cost_usd,
            }),
        },
    }
}

/// A non-panel modifier-only route (compress) — for the off-by-default test.
fn non_panel_route(name: &str) -> Route {
    Route {
        paused: false,
        id: Uuid::now_v7(),
        name: name.into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec!["model-a".into()],
            ..Default::default()
        },
        then: RouteAction {
            target_model: None,
            fallbacks: vec![],
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: true, // a benign effect so the route is not a no-op.
            doc_compaction: false,
            document_lane: false,
            redact: false,
            format_switch: None,
            diff: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            agentic_budget: None,
            panel: None,
        },
    }
}

/// A chat request on "model-a" with optional `X-TokenTrimmer-Panel` header and
/// optional `X-TokenTrimmer-Cost-Limit-Usd` ceiling. No `tt_extras.panel`.
fn chat_req(key: &str, panel_header: Option<&str>, cost_limit: Option<&str>) -> Request<Body> {
    let body = json!({
        "model": "model-a",
        "messages": [{ "role": "user", "content": "deep question" }],
        "stream": false
    });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"));
    if let Some(h) = panel_header {
        b = b.header("x-tokentrimmer-panel", h);
    }
    if let Some(c) = cost_limit {
        b = b.header("x-tokentrimmer-cost-limit-usd", c);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 256 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn drain_rows(
    writer: &Arc<InMemoryRequestLogWriter>,
    tracker: TaskTracker,
) -> Vec<tt_telemetry::request_logs::RequestLogRow> {
    tracker.close();
    tracker.wait().await;
    writer.rows()
}

fn hdr(resp: &axum::http::Response<Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

// ===========================================================================
// TEST 1: Route triggers panel (NO header) ⇒ 200 + tokentrimmer.panel +
// one provider='panel' request_logs row with matched_route_id set.
// ===========================================================================

#[tokio::test]
async fn route_triggers_panel_without_header() {
    let route = panel_route("research", "synthesize", false, Some(10.0));
    let route_id = route.id;
    let (app, key, writer, tracker, _calls) = app_with_routes(vec![route], true, None, None).await;

    // NO panel header — the route's `then.panel` is the only trigger.
    let resp = app
        .clone()
        .oneshot(chat_req(&key, None, Some("10.0")))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "route-triggered panel must return 200"
    );
    // The route matched and attributes.
    assert_eq!(
        hdr(&resp, "x-tokentrimmer-route-matched").as_deref(),
        Some("research"),
        "route must attribute on the response"
    );

    let body = body_json(resp).await;
    let panel = &body["tokentrimmer"]["panel"];
    assert!(
        panel.is_object(),
        "route-triggered panel must carry tokentrimmer.panel, got {body}"
    );
    assert_eq!(
        panel["legs"].as_array().unwrap().len(),
        3,
        "2 member legs + 1 arbiter leg"
    );
    assert_eq!(
        panel["arbiter"]["strategy"], "synthesize",
        "route strategy must drive the arbiter, got {panel}"
    );

    // One aggregate billing row, provider == "panel", matched_route_id == route.id.
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        1,
        "route-triggered panel must write exactly ONE request_logs row"
    );
    assert_eq!(
        rows[0].provider, "panel",
        "the single aggregate row must carry the panel sentinel provider"
    );
    assert_eq!(
        rows[0].route_id,
        Some(route_id),
        "ATTRIBUTION: the panel row must stamp matched_route_id = the route's id"
    );
}

// ===========================================================================
// TEST 2: Header WINS over a matched route's panel (D2).
//
// The route declares `synthesize`; the request carries
// `X-TokenTrimmer-Panel: best-of-n`. The header must drive the strategy.
// (No tt_extras.panel ⇒ the header path uses env defaults, which are empty, so
//  the panel would fail to resolve members. To keep the header path meaningful
//  WITHOUT relying on env defaults, the header test asserts the strategy that
//  ran is the HEADER's — best-of-n — not the route's synthesize. The members
//  come from tt_extras.panel.)
// ===========================================================================

#[tokio::test]
async fn header_wins_over_route_panel() {
    let route = panel_route("research", "synthesize", false, Some(10.0));
    let (app, key, _writer, _tracker, _calls) =
        app_with_routes(vec![route], true, None, None).await;

    // Header best-of-n + tt_extras.panel members (header path reads tt_extras).
    let body = json!({
        "model": "model-a",
        "messages": [{ "role": "user", "content": "which is best?" }],
        "stream": false,
        "tt_extras": {
            "panel": {
                "members": ["model-a", "model-b"],
                "arbiter_model": "model-arb"
            }
        }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .header("x-tokentrimmer-panel", "best-of-n")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "header panel must return 200"
    );

    let body = body_json(resp).await;
    let panel = &body["tokentrimmer"]["panel"];
    assert!(panel.is_object(), "panel must run, got {body}");
    assert_eq!(
        panel["arbiter"]["strategy"], "best-of-n",
        "HEADER WINS: the header strategy (best-of-n) must override the route's (synthesize), got {panel}"
    );
}

// ===========================================================================
// TEST 3a: Kill-switch off ⇒ a route-triggered panel 403s (D3), zero dispatch.
// ===========================================================================

#[tokio::test]
async fn route_panel_kill_switch_returns_403() {
    let route = panel_route("research", "synthesize", false, Some(10.0));
    let (app, key, writer, tracker, calls) =
        app_with_routes(vec![route], false /* panel disabled */, None, None).await;

    let resp = app
        .clone()
        .oneshot(chat_req(&key, None, Some("10.0")))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "kill-switch must 403 a route-triggered panel too"
    );
    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "panel_disabled",
        "error code must be panel_disabled, got {body}"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "kill-switch must fire before any dispatch (zero calls)"
    );
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(rows.len(), 0, "kill-switch reject writes zero rows");
}

// ===========================================================================
// TEST 3b: Below-min-tier ⇒ a route-triggered panel 403s (D3), zero dispatch.
// ===========================================================================

#[tokio::test]
async fn route_panel_entitlement_blocks_free_caller() {
    let route = panel_route("research", "synthesize", false, Some(10.0));
    // min_tier = Pro; the issued key's org has no FixedTierResolver → Free fallback.
    let (app, key, _writer, _tracker, calls) =
        app_with_routes(vec![route], true, Some(CallerTier::Pro), None).await;

    let resp = app
        .clone()
        .oneshot(chat_req(&key, None, Some("10.0")))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "min_tier=Pro must 403 a Free caller's route-triggered panel"
    );
    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "operation_not_permitted",
        "error code must be operation_not_permitted (entitlement), got {body}"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "entitlement gate must fire before any dispatch (zero calls)"
    );
}

// ===========================================================================
// TEST 3c: Over-budget ⇒ a route-triggered panel 402s (D3), zero dispatch.
//
// The route declares max_cost_usd = 0.000001 (one microdollar), below any
// 3-leg estimate for the prompt — the budget gate fires before dispatch.
// ===========================================================================

#[tokio::test]
async fn route_panel_over_budget_returns_402() {
    // No header cost-limit — the route's own max_cost_usd is the ceiling.
    let route = panel_route("research", "synthesize", false, Some(0.000001));
    let (app, key, writer, tracker, calls) = app_with_routes(vec![route], true, None, None).await;

    let resp = app
        .clone()
        .oneshot(chat_req(&key, None, None))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::PAYMENT_REQUIRED,
        "over-budget route-triggered panel must 402"
    );
    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "cost_limit_exceeded",
        "error code must be cost_limit_exceeded, got {body}"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "budget gate must fire before any dispatch (zero calls)"
    );
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(rows.len(), 0, "budget reject writes zero rows");
}

// ===========================================================================
// TEST 4: Paused panel route ⇒ NO panel (single-model path).
// ===========================================================================

#[tokio::test]
async fn paused_panel_route_does_not_trigger_panel() {
    let route = panel_route("research", "synthesize", true /* paused */, Some(10.0));
    let (app, key, writer, tracker, _calls) = app_with_routes(vec![route], true, None, None).await;

    let resp = app
        .clone()
        .oneshot(chat_req(&key, None, Some("10.0")))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "paused route still 200s");
    // The route still attributes (paused routes match + attribute).
    assert_eq!(
        hdr(&resp, "x-tokentrimmer-route-matched").as_deref(),
        Some("research"),
        "a paused route still attributes"
    );

    let body = body_json(resp).await;
    assert!(
        body["tokentrimmer"]
            .get("panel")
            .map(|p| p.is_null())
            .unwrap_or(true),
        "PAUSED ⇒ NO PANEL: a paused panel route must NOT carry tokentrimmer.panel, got {body}"
    );

    // Single-model billing: one row, provider != "panel".
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(rows.len(), 1, "paused single-model path writes one row");
    assert_ne!(
        rows[0].provider, "panel",
        "a paused route must NOT write a panel sentinel row"
    );
}

// ===========================================================================
// TEST 5: Off-by-default — a route with NO panel + NO header ⇒ single-model.
// ===========================================================================

#[tokio::test]
async fn no_panel_route_no_header_is_single_model() {
    let route = non_panel_route("compress-only");
    let (app, key, writer, tracker, _calls) = app_with_routes(vec![route], true, None, None).await;

    let resp = app
        .clone()
        .oneshot(chat_req(&key, None, None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "single-model path 200s");
    assert_eq!(
        hdr(&resp, "x-tokentrimmer-route-matched").as_deref(),
        Some("compress-only"),
        "the non-panel route still matches + attributes"
    );

    let body = body_json(resp).await;
    assert!(
        body["tokentrimmer"]
            .get("panel")
            .map(|p| p.is_null())
            .unwrap_or(true),
        "OFF-BY-DEFAULT: no panel route + no header ⇒ no tokentrimmer.panel, got {body}"
    );

    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(rows.len(), 1, "single-model path writes one row");
    assert_ne!(
        rows[0].provider, "panel",
        "single-model path must NOT write a panel sentinel row"
    );
}

// ===========================================================================
// Mark the unused FixedTierResolver constructor live for the Pro-allow path if
// it is ever exercised — kept minimal: the entitlement test above uses the
// Free fallback (no resolver), so we only need the struct's resolve impl. A
// silenced helper avoids a dead-code warning when the constructor is unused.
// ===========================================================================
#[allow(dead_code)]
fn _pro_resolver() -> Arc<dyn TierResolver> {
    Arc::new(FixedTierResolver {
        tier: CallerTier::Pro,
    })
}
