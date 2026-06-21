//! Deep-research panel — integration invariant suite.
//!
//! Phase 1 / Task 1 seeds this file with a real assertion of the kill-switch
//! env parser. Task 7 extends it with the router-level invariant tests
//! (off-by-default golden, fail-closed budget, quorum, served==rows,
//! multi-provider) built over the crate's mock-provider harness.

use tt_core::panel_enabled_from_env;

/// The `TT_PANEL_ENABLED` kill-switch parser: truthy only for `"1"` or a
/// case-insensitive `"true"`; absent or anything else is off (the panel is
/// off-by-default). Saves and restores the prior env value so adding more
/// tests to this binary later does not leak global state.
#[test]
fn tt_panel_enabled_env_parsing() {
    let key = "TT_PANEL_ENABLED";
    let prior = std::env::var(key).ok();

    std::env::remove_var(key);
    assert!(!panel_enabled_from_env(), "absent => off by default");

    std::env::set_var(key, "1");
    assert!(panel_enabled_from_env(), "\"1\" => on");

    std::env::set_var(key, "TRUE");
    assert!(panel_enabled_from_env(), "case-insensitive true => on");

    std::env::set_var(key, "yes");
    assert!(!panel_enabled_from_env(), "non-truthy => off");

    match prior {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

// ============================================================================
// Task 7 — router-level invariant suite
// ============================================================================

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

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::request_logs::InMemoryRequestLogWriter;

// ---------------------------------------------------------------------------
// Shared helpers (mirrors panel_dispatch.rs)
// ---------------------------------------------------------------------------

/// Read the JSON body of a response.
async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 256 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Drain all spawned telemetry writes, then return the captured rows.
async fn drain_rows(
    writer: &Arc<InMemoryRequestLogWriter>,
    tracker: TaskTracker,
) -> Vec<tt_telemetry::request_logs::RequestLogRow> {
    tracker.close();
    tracker.wait().await;
    writer.rows()
}

// ---------------------------------------------------------------------------
// Mock providers
// ---------------------------------------------------------------------------

/// A priced, counted provider serving a single model. The call counter lets
/// tests assert ZERO upstream dispatches on early-reject paths (budget gate,
/// kill-switch) — if `calls` increments, the fail-closed invariant is broken.
struct CountedMock {
    id: &'static str,
    model: &'static str,
    calls: Arc<AtomicUsize>,
    /// When `true`, `chat_completion` returns a 503 error instead of a
    /// successful response. Used to manufacture quorum-unmet scenarios.
    fail: bool,
}

impl CountedMock {
    fn new(id: &'static str, model: &'static str, calls: Arc<AtomicUsize>) -> Self {
        Self {
            id,
            model,
            calls,
            fail: false,
        }
    }

    fn failing(id: &'static str, model: &'static str, calls: Arc<AtomicUsize>) -> Self {
        Self {
            id,
            model,
            calls,
            fail: true,
        }
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
        if self.fail {
            return Err(ProviderError::ProviderUpstream {
                status: 503,
                message: "mock member failure".into(),
            });
        }
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
// App builder helpers
// ---------------------------------------------------------------------------

/// Build a router with panel-enabled=true, two models on the SAME mock
/// provider ("openai": "gpt-4o" + "gpt-4o-mini"), plus an AtomicUsize counter
/// that records ALL chat_completion calls.
///
/// Used for: fail-closed budget, quorum-unmet, served==rows, kill-switch.
///
/// Returns (router, request_log_writer, telemetry_tracker, call_counter).
fn app_single_provider(
    fail_members: bool,
    panel_enabled: bool,
) -> (
    axum::Router,
    Arc<InMemoryRequestLogWriter>,
    TaskTracker,
    Arc<AtomicUsize>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    // Both models are on "openai"; arbiter ("gpt-4o") also lives here.
    registry.register(Arc::new(if fail_members {
        CountedMock::failing("openai", "gpt-4o", Arc::clone(&calls))
    } else {
        CountedMock::new("openai", "gpt-4o", Arc::clone(&calls))
    }));
    // A second "openai" registration would conflict; instead serve "gpt-4o-mini"
    // via a second provider slot so the panel config can reference two models.
    // We use a separate provider id "openai-mini" (distinct id, same pricing).
    registry.register(Arc::new(if fail_members {
        CountedMock::failing("openai-mini", "gpt-4o-mini", Arc::clone(&calls))
    } else {
        CountedMock::new("openai-mini", "gpt-4o-mini", Arc::clone(&calls))
    }));
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let tracker = TaskTracker::new();
    let state = AppState::new(registry)
        .with_panel_enabled(panel_enabled)
        .with_request_log_writer(writer.clone())
        .with_telemetry_tracker(tracker.clone());
    (build_router(state), writer, tracker, calls)
}

/// Build a router with panel-enabled=true and TWO DISTINCT mock providers
/// ("vendor-a" serving "model-a", "vendor-b" serving "model-b" + "model-arb")
/// each with their own call counter.
///
/// Used for: multi-provider invariant.
///
/// Returns (router, writer, tracker, calls_a, calls_b).
fn app_two_providers() -> (
    axum::Router,
    Arc<InMemoryRequestLogWriter>,
    TaskTracker,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let calls_a = Arc::new(AtomicUsize::new(0));
    let calls_b = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(CountedMock::new(
        "vendor-a",
        "model-a",
        Arc::clone(&calls_a),
    )));
    // vendor-b serves "model-b" (member) and "model-arb" (arbiter).
    // Since a single CountedMock only serves one model id, we register two
    // CountedMock instances for vendor-b sharing the same calls_b counter.
    registry.register(Arc::new(CountedMock::new(
        "vendor-b",
        "model-b",
        Arc::clone(&calls_b),
    )));
    // Arbiter model on vendor-c (third, so we can distinguish from members).
    let calls_arb = Arc::clone(&calls_b); // share b's counter for simplicity
    registry.register(Arc::new(CountedMock::new(
        "vendor-c",
        "model-arb",
        calls_arb,
    )));
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let tracker = TaskTracker::new();
    let state = AppState::new(registry)
        .with_panel_enabled(true)
        .with_request_log_writer(writer.clone())
        .with_telemetry_tracker(tracker.clone());
    (build_router(state), writer, tracker, calls_a, calls_b)
}

// ============================================================================
// INVARIANT 3 (7.3 fail-closed budget): over-budget ⇒ 402 BEFORE any dispatch.
//
// The budget gate in chat.rs fires BEFORE `Prepared` is built and BEFORE any
// call to a provider. The call counter must remain at ZERO — if it increments,
// the gate is broken.
//
// Decision on the `x-tokentrimmer-cost-limit-usd` ceiling: we set 0.000001
// (one microdollar) which is less than any real 3-leg estimate for 1M tokens,
// triggering the budget gate. Using a priced provider ensures the estimate
// path is exercised (not the "unpriceable ⇒ fail-closed" path).
// ============================================================================

#[tokio::test]
async fn fail_closed_budget_returns_402_with_zero_dispatches() {
    let (app, writer, tracker, calls) = app_single_provider(false, true);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        // Microscopic ceiling → estimate will exceed it → 402 before any dispatch.
        .header("x-tokentrimmer-cost-limit-usd", "0.000001")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": false,
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    // Must be 402 (CostLimitExceeded) — gate fired before dispatch.
    assert_eq!(
        resp.status(),
        StatusCode::PAYMENT_REQUIRED,
        "over-budget panel must return 402 PAYMENT_REQUIRED"
    );

    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "cost_limit_exceeded",
        "error code must be cost_limit_exceeded, got {body}"
    );

    // CRITICAL: zero upstream calls — the gate is fail-closed.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "INVARIANT: budget gate must fire BEFORE any provider dispatch (zero calls)"
    );

    // Also zero request_logs rows — no billing side effect on early reject.
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        0,
        "INVARIANT: a budget-gate reject must write ZERO request_logs rows"
    );
}

// ============================================================================
// INVARIANT 4 (7.4 quorum unmet): all members fail ⇒ 502 BAD_GATEWAY,
// no billing row written.
//
// `fail_members = true` makes both CountedMock providers return a 503
// ProviderUpstream error, so run_panel returns Err(PanelQuorumUnmet).
// complete_panel propagates the error BEFORE the billing tail — no row, no
// spend_sink().record().
//
// Harness note: the request_logs writer is injectable; asserting `rows.len()==0`
// is the observable check. The spend_sink is not injectable in tests (it is
// the NoopSpendSink in AppState::new), so spend counter assertions are
// structurally N/A here (the sink is always a no-op in unit/integration tests).
// ============================================================================

#[tokio::test]
async fn quorum_unmet_returns_502_with_zero_rows() {
    let (app, writer, tracker, _calls) = app_single_provider(true, true);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": false,
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    // Both members failed → quorum unmet → 502.
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "quorum-unmet must return 502 BAD_GATEWAY"
    );

    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "panel_quorum_unmet",
        "error code must be panel_quorum_unmet, got {body}"
    );

    // Billing invariant: no request_logs row when the panel fails.
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        0,
        "INVARIANT: a quorum-unmet panel must write ZERO request_logs rows (no billing side effect)"
    );
}

// ============================================================================
// INVARIANT 5 (7.5 served==rows + tt_panel_legs_total): a happy-path panel
// through the HTTP router increments tt_requests_served_total by 1 AND writes
// 1 request_logs row; tt_panel_legs_total carries at least 3 increments
// (2 member legs + 1 arbiter, all successful).
//
// Because the global Prometheus recorder is shared across the test binary
// (counters accumulate across tests), we assert metric PRESENCE with expected
// labels, not exact absolute values — the same discipline as metrics_endpoint.rs.
//
// HTTP path note: `record_request_served` is bumped by the chat handler consumer
// after `complete_panel` returns `CompletionOutcome::Dispatched`. The agent-loop
// path (agent_run.rs) has a pre-existing served-metric gap unrelated to panels;
// this test drives the HTTP handler path only, which correctly bumps the counter.
// ============================================================================

#[tokio::test]
async fn happy_path_served_counter_and_panel_legs_metric_present() {
    let (app, writer, tracker, _calls) = app_single_provider(false, true);

    // Drive one happy-path panel request.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": false,
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "happy-path panel must return 200");

    // Drain rows.
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(rows.len(), 1, "served==rows: exactly ONE request_logs row");
    assert!(!rows[0].cached, "panel row must be cached=false");
    assert_eq!(rows[0].provider, "panel", "panel sentinel provider stamp");

    // Scrape metrics — presence-only (shared recorder accumulates across tests).
    let metrics_body = tt_core::metrics::render()
        .expect("Prometheus recorder must be installed after build_router");

    // tt_requests_served_total must have a 'chat' path label (HTTP handler path).
    assert!(
        metrics_body
            .lines()
            .any(|l| l.starts_with("tt_requests_served_total") && l.contains("path=\"chat\"")),
        "tt_requests_served_total{{path=\"chat\"}} must be present after a panel dispatch:\n{metrics_body}"
    );

    // tt_panel_legs_total must be present with a 'ok' status label —
    // the `LegStatus::Ok.as_str()` wire value is "ok" (not "success").
    // All 3 legs (2 members + 1 arbiter) dispatch and succeed on the happy path.
    assert!(
        metrics_body
            .lines()
            .any(|l| l.starts_with("tt_panel_legs_total") && l.contains("status=\"ok\"")),
        "tt_panel_legs_total{{status=\"ok\"}} must be present after a happy-path panel:\n{metrics_body}"
    );
}

// ============================================================================
// INVARIANT 6 (7.6 kill-switch): panel-DISABLED gateway + panel header ⇒ 403
// FORBIDDEN, zero dispatches.
//
// `panel_enabled=false` forces `AppState::panel_enabled = false`. The handler
// checks this before building Prepared or calling any provider.
// ============================================================================

#[tokio::test]
async fn kill_switch_disabled_panel_returns_403_with_zero_dispatches() {
    let (app, writer, tracker, calls) = app_single_provider(false, false /* disabled */);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": false,
                "tt_extras": {
                    "panel": {
                        "members": ["gpt-4o", "gpt-4o-mini"],
                        "arbiter_model": "gpt-4o"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "panel-disabled gateway must return 403 FORBIDDEN"
    );

    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "panel_disabled",
        "error code must be panel_disabled, got {body}"
    );

    // Zero dispatches — the kill-switch fires before any provider call.
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "INVARIANT: kill-switch must fire before any provider dispatch (zero calls)"
    );

    // Zero rows — no billing on a kill-switch reject.
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(rows.len(), 0, "kill-switch reject must write ZERO request_logs rows");
}

// ============================================================================
// INVARIANT 7 (7.7 multi-provider): two members on DIFFERENT mock providers ⇒
// both dispatch; aggregate cost sums across providers; request_logs row has
// provider == "panel".
//
// Provider layout:
//   - "vendor-a" serves "model-a"  (member 1)
//   - "vendor-b" serves "model-b"  (member 2)
//   - "vendor-c" serves "model-arb" (arbiter)
//
// calls_a confirms vendor-a was dispatched; calls_b also counts vendor-b AND
// vendor-c (arbiter), so calls_b >= 2 after a successful panel.
// ============================================================================

#[tokio::test]
async fn multi_provider_panel_dispatches_both_and_bills_one_aggregate_row() {
    let (app, writer, tracker, calls_a, calls_b) = app_two_providers();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "model-a",
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": false,
                "tt_extras": {
                    "panel": {
                        "members": ["model-a", "model-b"],
                        "arbiter_model": "model-arb"
                    }
                }
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "multi-provider panel must return 200"
    );

    let body = body_json(resp).await;
    let panel = &body["tokentrimmer"]["panel"];
    assert!(panel.is_object(), "response must carry tokentrimmer.panel, got {body}");

    // Verify both member providers were dispatched.
    assert!(
        calls_a.load(Ordering::Relaxed) >= 1,
        "vendor-a (model-a) must have been dispatched, calls_a={}",
        calls_a.load(Ordering::Relaxed)
    );
    // calls_b counts vendor-b (model-b member) AND vendor-c (model-arb arbiter).
    assert!(
        calls_b.load(Ordering::Relaxed) >= 2,
        "vendor-b (model-b member) + vendor-c (arbiter) must both have been dispatched, calls_b={}",
        calls_b.load(Ordering::Relaxed)
    );

    // Aggregate cost must be positive (all 3 legs priced).
    let total_cost = panel["total_cost_usd"].as_f64().unwrap_or(0.0);
    assert!(
        total_cost > 0.0,
        "aggregate panel cost must be positive (3 priced legs), got {total_cost}"
    );

    // One aggregate billing row, provider == "panel".
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        1,
        "multi-provider panel must write exactly ONE request_logs row"
    );
    let row = &rows[0];
    assert_eq!(row.provider, "panel", "aggregate row provider must be the panel sentinel");
    assert!(!row.cached, "aggregate row must be cached=false");
    assert!(
        (row.cost_usd - total_cost).abs() < 1e-9,
        "row.cost_usd must equal body total_cost_usd, got {} vs {total_cost}",
        row.cost_usd
    );
}
