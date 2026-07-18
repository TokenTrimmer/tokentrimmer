//! Task 6 — `complete_panel` wiring integration tests.
//!
//! Run with:
//!   cargo test -p tt-core --test panel_dispatch
//!
//! These two focused tests pin the Task-6 wiring invariants end-to-end through
//! the router. The full seven-test invariant matrix (fail-closed budget, quorum
//! unmet, multi-provider, kill-switch) is Task 7's `panel_engine.rs`.
//!
//!   1. Off-by-default: a request with NO `X-TokenTrimmer-Panel` header is
//!      completed on the single-model path — 200, served model in the body, and
//!      NO `tokentrimmer.panel` object. Exactly ONE non-cached `request_logs`
//!      row with the real provider stamp (not `"panel"`).
//!   2. Panel happy path: `X-TokenTrimmer-Panel: synthesize` + a member list +
//!      sufficient `X-TokenTrimmer-Cost-Limit-Usd` ⇒ 200 with a
//!      `tokentrimmer.panel` body (legs + quorum + total) AND exactly ONE
//!      non-cached `request_logs` row stamped `provider = "panel"` (the
//!      served==rows + one-aggregate-row billing discipline).

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
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

/// A priced provider serving two chat models (`gpt-4o`, `gpt-4o-mini`) so a
/// panel can fan out across distinct members + arbiter and every leg prices.
struct PanelProvider;

#[async_trait]
impl Provider for PanelProvider {
    fn id(&self) -> &'static str {
        "openai"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["gpt-4o", "gpt-4o-mini"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: "openai".into(),
                capabilities: vec![Capability::Text],
                max_input_tokens: 8192,
                max_output_tokens: 8192,
            })
            .collect()
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
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
            effective_at: chrono::Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
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

/// Build a router with the panel-capable provider, the panel kill-switch ON,
/// and an in-memory `request_logs` writer + telemetry tracker so the spawned
/// row write can be drained and asserted. Returns the router, the writer, and
/// the tracker.
fn app() -> (axum::Router, Arc<InMemoryRequestLogWriter>, TaskTracker) {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(PanelProvider));
    let writer = Arc::new(InMemoryRequestLogWriter::new());
    let tracker = TaskTracker::new();
    let state = AppState::new(registry)
        .with_panel_enabled(true)
        .with_request_log_writer(writer.clone())
        .with_telemetry_tracker(tracker.clone());
    (build_router(state), writer, tracker)
}

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

/// INVARIANT 1 (off-by-default): no panel header ⇒ the single-model path is
/// taken — 200, served model in the body, NO `tokentrimmer.panel` object, and
/// exactly one non-cached row stamped with the REAL provider (`"openai"`).
#[tokio::test]
async fn no_panel_header_is_single_model_path() {
    let (app, writer, tracker) = app();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "messages": [{ "role": "user", "content": "hi" }],
                "stream": false,
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    // Single-model body shape: served model present, NO panel attribution.
    assert_eq!(body["model"], "gpt-4o");
    assert!(
        body.get("tokentrimmer").is_none(),
        "off-by-default: a no-panel request must NOT carry a tokentrimmer.panel body, got {body}"
    );

    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(rows.len(), 1, "exactly one request_logs row");
    assert!(!rows[0].cached, "the dispatched row is cached=false");
    assert_eq!(
        rows[0].provider, "openai",
        "single-model row keeps the REAL provider stamp (not the panel sentinel)"
    );
}

/// INVARIANT 2 (panel happy path + one-row billing): a synthesize panel with a
/// member list and a sufficient budget returns 200 with a `tokentrimmer.panel`
/// body AND writes exactly ONE non-cached row stamped `provider = "panel"`
/// (served==rows: one billable request regardless of N legs).
#[tokio::test]
async fn panel_header_dispatches_and_bills_one_aggregate_row() {
    let (app, writer, tracker) = app();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        // Generous ceiling — the fail-closed budget gate must pass.
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "gpt-4o",
                "max_tokens": 64,
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
        StatusCode::OK,
        "panel within budget should dispatch and return 200"
    );
    let body = body_json(resp).await;

    // The arbiter's answer is the served body; the panel attribution rides in
    // `tokentrimmer.panel`.
    let panel = &body["tokentrimmer"]["panel"];
    assert!(
        panel.is_object(),
        "panel body must carry tokentrimmer.panel, got {body}"
    );
    assert_eq!(panel["strategy"], "synthesize");
    // legs = 2 members + 1 arbiter = 3 leg records.
    let legs = panel["legs"].as_array().expect("legs array");
    assert_eq!(legs.len(), 3, "two members + one arbiter leg recorded");
    assert_eq!(panel["quorum"]["met"], 2, "both member legs succeeded");
    assert!(
        panel["total_cost_usd"].as_f64().unwrap() > 0.0,
        "aggregate cost is the summed leg+arbiter spend"
    );

    // Billing discipline: EXACTLY ONE non-cached row, stamped with the panel
    // sentinel provider and the arbiter model (served==rows, one billable
    // request, never cached).
    let rows = drain_rows(&writer, tracker).await;
    assert_eq!(
        rows.len(),
        1,
        "a panel writes EXACTLY ONE request_logs row regardless of N legs"
    );
    let row = &rows[0];
    assert!(!row.cached, "INVARIANT: every panel row is cached=false");
    assert_eq!(row.provider, "panel", "decision-A sentinel provider stamp");
    assert_eq!(row.requested_model.as_deref(), Some("gpt-4o"));
    assert_eq!(row.model, "gpt-4o", "row.model == arbiter model");
    assert!(
        row.cost_usd > 0.0,
        "the row carries the aggregate panel cost, got {}",
        row.cost_usd
    );
    // The recorded aggregate equals the body's reported total (no double count).
    assert!(
        (row.cost_usd - panel["total_cost_usd"].as_f64().unwrap()).abs() < 1e-9,
        "row cost_usd must equal the body's total_cost_usd (one aggregate, no double-count)"
    );
}
