//! Task 4 — per-leg `panel_legs` persistence integration test.
//!
//! Run with:
//!   cargo test -p tt-core --test panel_legs_persist
//!
//! Verifies that `complete_panel` writes exactly one aggregate `request_logs`
//! row (billing unchanged) AND N+1 `panel_legs` rows (2 member legs + 1
//! arbiter for a 2-member panel) after a happy-path dispatch, with correct
//! field values on the child rows.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tokio_util::task::TaskTracker;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::{panel_legs::InMemoryPanelLegWriter, request_logs::InMemoryRequestLogWriter};

// ---------------------------------------------------------------------------
// Mock providers
// ---------------------------------------------------------------------------

/// A priced mock provider serving a single model. Returns a fixed successful
/// response so every leg prices and records usage.
struct PricedMock {
    id: &'static str,
    model: &'static str,
}

#[async_trait]
impl Provider for PricedMock {
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
                effective_at: chrono::Utc::now(),
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
// App builder
// ---------------------------------------------------------------------------

/// Build a router with:
/// - Panel kill-switch ON.
/// - Two distinct member providers: "vendor-a" (model "model-a") and "vendor-b"
///   (model "model-b"); arbiter uses "vendor-a" / "model-a" (same provider).
/// - Both `InMemoryRequestLogWriter` AND `InMemoryPanelLegWriter` wired.
/// - A `TaskTracker` so telemetry can be drained before assertions.
fn app() -> (
    axum::Router,
    Arc<InMemoryRequestLogWriter>,
    Arc<InMemoryPanelLegWriter>,
    TaskTracker,
) {
    let mut registry = ProviderRegistry::new();
    // Member 0: "vendor-a" / "model-a" — also serves as arbiter model.
    registry.register(Arc::new(PricedMock {
        id: "vendor-a",
        model: "model-a",
    }));
    // Member 1: "vendor-b" / "model-b".
    registry.register(Arc::new(PricedMock {
        id: "vendor-b",
        model: "model-b",
    }));
    let log_writer = Arc::new(InMemoryRequestLogWriter::new());
    let leg_writer = Arc::new(InMemoryPanelLegWriter::new());
    let tracker = TaskTracker::new();
    let state = AppState::new(registry)
        .with_panel_enabled(true)
        .with_request_log_writer(log_writer.clone())
        .with_panel_leg_writer(leg_writer.clone())
        .with_telemetry_tracker(tracker.clone());
    (build_router(state), log_writer, leg_writer, tracker)
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// After a happy 2-member panel:
/// 1. Billing row is exactly ONE row, `provider == "panel"`, `cached == false`.
/// 2. Three `panel_legs` rows (2 member legs + 1 arbiter).
/// 3. All child rows share the parent `request_logs.id` as `request_log_id`.
/// 4. `leg_index` values are 0, 1, 2 (unique, enumeration-order).
/// 5. Roles are `["leg", "leg", "arbiter"]`.
/// 6. Per-leg `provider` / `model` are populated.
/// 7. `cost_usd` is `Some` for priced mocks.
#[tokio::test]
async fn happy_panel_writes_one_billing_row_and_three_leg_rows() {
    let (app, log_writer, leg_writer, tracker) = app();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        // Generous ceiling so the budget gate passes.
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "model-a",
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": false,
                "tt_extras": {
                    "panel": {
                        "members": ["model-a", "model-b"],
                        "arbiter_model": "model-a"
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
        "happy panel should return 200"
    );
    // Consume the body (not the focus of this test).
    let _ = to_bytes(resp.into_body(), 256 * 1024).await.unwrap();

    // Drain all fire-and-forget telemetry writes.
    tracker.close();
    tracker.wait().await;

    // ── Assertion 1: billing row unchanged ───────────────────────────────────
    let log_rows = log_writer.rows();
    assert_eq!(
        log_rows.len(),
        1,
        "INVARIANT: exactly ONE request_logs row per panel request, got {}",
        log_rows.len()
    );
    let log_row = &log_rows[0];
    assert_eq!(
        log_row.provider, "panel",
        "billing row must use 'panel' sentinel"
    );
    assert!(
        !log_row.cached,
        "INVARIANT: panel billing row is cached=false"
    );

    // ── Assertion 2: three panel_legs rows ───────────────────────────────────
    let leg_rows = leg_writer.rows();
    assert_eq!(
        leg_rows.len(),
        3,
        "2 member legs + 1 arbiter = 3 panel_legs rows, got {}",
        leg_rows.len()
    );

    // ── Assertion 3: all child rows share the parent id ──────────────────────
    let parent_id = log_row.id;
    for row in &leg_rows {
        assert_eq!(
            row.request_log_id, parent_id,
            "leg row request_log_id must match the parent request_logs.id"
        );
    }

    // ── Assertion 4: leg_index values are 0, 1, 2 (unique, in order) ─────────
    let mut indices: Vec<i32> = leg_rows.iter().map(|r| r.leg_index).collect();
    indices.sort_unstable();
    assert_eq!(indices, vec![0, 1, 2], "leg_index must be 0, 1, 2");

    // ── Assertion 5: roles are leg, leg, arbiter ──────────────────────────────
    let roles: Vec<&str> = leg_rows.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["leg", "leg", "arbiter"],
        "roles must be [leg, leg, arbiter] in dispatch order"
    );

    // ── Assertion 6: per-leg provider/model populated ────────────────────────
    for row in &leg_rows {
        assert!(
            !row.provider.is_empty(),
            "leg row provider must be non-empty, got empty on leg_index {}",
            row.leg_index
        );
        assert!(
            !row.model.is_empty(),
            "leg row model must be non-empty, got empty on leg_index {}",
            row.leg_index
        );
    }

    // ── Assertion 7: cost_usd is Some for priced mocks ───────────────────────
    for row in &leg_rows {
        assert!(
            row.cost_usd.is_some(),
            "cost_usd must be Some for priced mock on leg_index {}",
            row.leg_index
        );
    }
}

// ---------------------------------------------------------------------------
// MockPanic provider — panics in chat_completion; JoinSet catches as JoinError
// ---------------------------------------------------------------------------

/// A mock provider whose `chat_completion` panics unconditionally. The
/// `tokio::task::JoinSet` in `panel_fanout.rs` catches the panic as a
/// `JoinError` — the process does NOT abort and the test keeps running.
struct PanicMock {
    id: &'static str,
    model: &'static str,
}

#[async_trait]
impl Provider for PanicMock {
    fn id(&self) -> &'static str {
        self.id
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.into(),
            provider: self.id.into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
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
                effective_at: chrono::Utc::now(),
            })
        } else {
            None
        }
    }
    async fn chat_completion(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        panic!("deliberate leg panic in panel_legs_persist");
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
// App builder for panic test
// ---------------------------------------------------------------------------

/// Build a router with:
/// - Panel kill-switch ON.
/// - Member 0: panicking provider ("vendor-panic" / "model-panic").
/// - Member 1: normal priced provider ("vendor-ok" / "model-ok") — also
///   serves as the arbiter so quorum=1 is achievable.
/// - Both writers wired + a TaskTracker for deterministic draining.
fn app_with_panic_member() -> (
    axum::Router,
    Arc<InMemoryRequestLogWriter>,
    Arc<InMemoryPanelLegWriter>,
    TaskTracker,
) {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(PanicMock {
        id: "vendor-panic",
        model: "model-panic",
    }));
    registry.register(Arc::new(PricedMock {
        id: "vendor-ok",
        model: "model-ok",
    }));
    let log_writer = Arc::new(InMemoryRequestLogWriter::new());
    let leg_writer = Arc::new(InMemoryPanelLegWriter::new());
    let tracker = TaskTracker::new();
    let state = AppState::new(registry)
        .with_panel_enabled(true)
        .with_request_log_writer(log_writer.clone())
        .with_panel_leg_writer(leg_writer.clone())
        .with_telemetry_tracker(tracker.clone());
    (build_router(state), log_writer, leg_writer, tracker)
}

// ---------------------------------------------------------------------------
// Test: panicked member leg — no usize::MAX sentinel leaks into persisted rows
// ---------------------------------------------------------------------------

/// Guards the invariant that the `usize::MAX` sentinel used internally for a
/// panicked `LegResult.leg_index` NEVER leaks into the persisted `panel_legs`
/// rows. The `complete_panel` enumeration loop assigns indices from the output
/// slice order (`enumerate()`), so every row must have a contiguous i32 index
/// in `0..N` even when one member panicked.
///
/// Panel config: one panicking member + one normal member (quorum=1 met by the
/// normal leg) + arbiter from the normal provider.  Panel returns 200 OK with
/// synthesized response.
#[tokio::test]
async fn panicked_member_leg_index_is_contiguous_not_sentinel() {
    let (app, log_writer, leg_writer, tracker) = app_with_panic_member();

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test")
        .header("x-tokentrimmer-panel", "synthesize")
        // Generous ceiling so the budget gate passes.
        .header("x-tokentrimmer-cost-limit-usd", "10.0")
        .body(Body::from(
            json!({
                "model": "model-ok",
                "messages": [{ "role": "user", "content": "deep question" }],
                "stream": false,
                "tt_extras": {
                    "panel": {
                        "members": ["model-panic", "model-ok"],
                        "arbiter_model": "model-ok"
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
        "panel with one panicking member and quorum=1 must return 200"
    );
    // Consume the body.
    let _ = to_bytes(resp.into_body(), 256 * 1024).await.unwrap();

    // Drain all fire-and-forget telemetry writes (including the leg write now
    // routed through the tracker by Fix 1).
    tracker.close();
    tracker.wait().await;

    // ── Billing row: exactly one, unchanged ──────────────────────────────────
    let log_rows = log_writer.rows();
    assert_eq!(
        log_rows.len(),
        1,
        "INVARIANT: exactly ONE request_logs row per panel request, got {}",
        log_rows.len()
    );

    // ── Leg rows: contiguous indices, no sentinel leakage ───────────────────
    let leg_rows = leg_writer.rows();
    let n = leg_rows.len() as i32;
    assert!(
        n >= 2,
        "must have at least 2 leg rows (one error + arbiter), got {n}"
    );

    let mut indices: Vec<i32> = leg_rows.iter().map(|r| r.leg_index).collect();
    indices.sort_unstable();
    let expected: Vec<i32> = (0..n).collect();
    assert_eq!(
        indices, expected,
        "leg_index values must be contiguous 0..N, got {indices:?}"
    );

    // Explicitly guard against known sentinel values leaking through.
    for row in &leg_rows {
        assert_ne!(
            row.leg_index,
            i32::MAX,
            "i32::MAX sentinel must NOT appear in persisted leg_index (leg role={})",
            row.role
        );
        assert_ne!(
            row.leg_index, -1,
            "-1 must NOT appear in persisted leg_index (leg role={})",
            row.role
        );
    }

    // ── There must be exactly one error leg (from the panic) ─────────────────
    let error_legs: Vec<_> = leg_rows.iter().filter(|r| r.status == "error").collect();
    assert_eq!(
        error_legs.len(),
        1,
        "panicked member must produce exactly one 'error' leg row, got {}",
        error_legs.len()
    );

    // ── There must be exactly one arbiter row ────────────────────────────────
    let arbiter_legs: Vec<_> = leg_rows.iter().filter(|r| r.role == "arbiter").collect();
    assert_eq!(
        arbiter_legs.len(),
        1,
        "must have exactly one arbiter row, got {}",
        arbiter_legs.len()
    );
}
