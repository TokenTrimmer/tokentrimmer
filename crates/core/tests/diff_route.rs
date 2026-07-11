//! End-to-end (hermetic) tests for the opt-in DELTA/DIFF route action
//! (`RouteAction::diff`, research Phase 3.4):
//!
//! (a) applied: exactly ONE upstream call; the caller receives the FULL
//!     reconstructed artifact while the response `usage` stays the
//!     provider-billed PATCH usage (invoice-honest); the measured-saving
//!     header is > 0; the row records `diff_applied`; the response is never
//!     L1-cached (an identical follow-up dispatches again);
//! (b) fail-closed: an invalid patch → exactly TWO upstream calls; the caller
//!     receives the full re-emit; `cost_usd` = patch + re-emit (the retry is
//!     real invoice spend); the row records `diff_failed` with a nonzero
//!     `diff_failed_cost_usd`; the saved headline stays 0;
//! (c) degraded: the re-emit dispatch itself errors → 200 with the raw patch
//!     body + `diff_degraded` (never a 5xx after a successful upstream call),
//!     and the single billed dispatch is NOT double-counted;
//! (d) no identifiable prior → single normal dispatch + `diff_skipped:no_prior`;
//! (e) the explicit `tt_extras["diff_prior"]` echo beats the assistant
//!     history AND never rides to the upstream provider;
//! (f) a PAUSED route suppresses the lever entirely (no instruction, no diff
//!     tokens beyond `route_paused:*`).
//!
//! No network: a scripted mock provider records every upstream request
//! VERBATIM and plays back per-call responses, mirroring `batch_route.rs`.

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
use tt_cache::memory::InMemoryL1Cache;
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

/// Patch-dispatch billed completion tokens (the SHORT patch — the point).
const PATCH_COMPLETION_TOKENS: u64 = 20;
/// Full re-emit billed completion tokens.
const FULL_COMPLETION_TOKENS: u64 = 500;
const PROMPT_TOKENS: u64 = 1000;

/// $10/M input, $30/M output.
fn patch_cost() -> f64 {
    (PROMPT_TOKENS as f64) * 10.0 / 1e6 + (PATCH_COMPLETION_TOKENS as f64) * 30.0 / 1e6
}
fn full_cost() -> f64 {
    (PROMPT_TOKENS as f64) * 10.0 / 1e6 + (FULL_COMPLETION_TOKENS as f64) * 30.0 / 1e6
}

/// The prior document (well over MIN_PRIOR_CHARS).
fn prior_doc() -> String {
    let mut s = String::from("# Release Notes\n\n");
    for i in 0..12 {
        s.push_str(&format!(
            "Item {i}: the quick brown fox jumps over the lazy dog once more.\n"
        ));
    }
    s
}

const GOOD_PATCH: &str =
    "<<<<<<< SEARCH\nItem 3: the quick brown fox jumps over the lazy dog once more.\n=======\nItem 3: the SWIFT brown fox vaults over the energetic dog.\n>>>>>>> REPLACE";
const PROSE_NO_PATCH: &str = "Sorry, here is the full document rewritten from scratch instead.";
const FULL_REEMIT_BODY: &str = "the full re-emitted document the caller originally asked for";

/// One scripted upstream behavior.
enum Script {
    /// Respond with this assistant body and completion-token count
    /// (finish_reason "stop").
    Body(&'static str, u64),
    /// Respond with this assistant body, completion-token count, and
    /// finish_reason (e.g. "length" — a token-limit truncation).
    BodyFinish(&'static str, u64, &'static str),
    /// Fail with a NON-retryable error (InvalidRequest is not retried).
    Fail,
}

struct ScriptedProvider {
    requests: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    script: Arc<Mutex<Vec<Script>>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "diffrec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "diff-1".into(),
            provider: "diffrec".into(),
            capabilities: vec![Capability::Text, Capability::JsonMode],
            max_input_tokens: 8192,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
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
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&req).expect("serialize upstream request"));
        let next = {
            let mut s = self.script.lock().unwrap();
            if s.is_empty() {
                Script::Body("unscripted", PATCH_COMPLETION_TOKENS)
            } else {
                s.remove(0)
            }
        };
        let (body, completion, finish) = match next {
            Script::Body(b, c) => (b.to_string(), c, "stop"),
            Script::BodyFinish(b, c, f) => (b.to_string(), c, f),
            Script::Fail => return Err(ProviderError::InvalidRequest("scripted failure".into())),
        };
        Ok(ChatCompletionResponse {
            id: "chatcmpl-diff".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(body)),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some(finish.into()),
            }],
            usage: Usage {
                prompt_tokens: PROMPT_TOKENS,
                completion_tokens: completion,
                total_tokens: PROMPT_TOKENS + completion,
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
        Err(ProviderError::Unsupported(
            "no streaming in this mock".into(),
        ))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
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

struct Harness {
    app: axum::Router,
    key: String,
    requests: Arc<Mutex<Vec<String>>>,
    calls: Arc<AtomicUsize>,
    script: Arc<Mutex<Vec<Script>>>,
    log_writer: Arc<InMemoryRequestLogWriter>,
}

/// The default diff-route action; tests tweak individual levers on top.
fn diff_action() -> RouteAction {
    RouteAction {
        workflow: None,
        target_model: Some("diff-1".into()),
        fallbacks: Vec::new(),
        disable_cache: false,
        max_cost_usd: None,
        flex: false,
        batch: false,
        compress: false,
        doc_compaction: false,
        document_lane: false,
        content_compress: false,
        redact: false,
        format_switch: None,
        diff: true,
        minify_json: false,
        reasoning_max_effort: None,
        reasoning_budget_tokens: None,
        agentic_budget: None,
        traffic_pct: None,
        shadow_model: None,
        auto_pause: false,
        pause_floor_pass_rate: None,
        pause_min_verdicts: None,
        panel: None,
    }
}

async fn app_with_diff_route(paused: bool) -> Harness {
    app_with_route(paused, diff_action()).await
}

async fn app_with_route(paused: bool, then: RouteAction) -> Harness {
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let script: Arc<Mutex<Vec<Script>>> = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(ScriptedProvider {
        requests: Arc::clone(&requests),
        calls: Arc::clone(&calls),
        script: Arc::clone(&script),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(
        org_id,
        vec![Route {
            paused,
            id: Uuid::now_v7(),
            name: "diff-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["diff-1".into()],
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
            .with_l1(Arc::new(InMemoryL1Cache::new()), None)
            .with_request_log_writer(log_writer.clone() as Arc<dyn RequestLogWriter>),
    );
    Harness {
        app,
        key: plaintext,
        requests,
        calls,
        script,
        log_writer,
    }
}

/// The common edit-loop request shape: [user ask][assistant doc][user edit].
fn edit_request(bearer: &str, prior: &str, extras: Option<serde_json::Value>) -> Request<Body> {
    let mut body = json!({
        "model": "diff-1",
        "messages": [
            { "role": "user", "content": "write the release notes" },
            { "role": "assistant", "content": prior },
            { "role": "user", "content": "punch up Item 3" }
        ],
        "stream": false,
    });
    if let Some(extras) = extras {
        body["tt_extras"] = extras;
    }
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn wait_for_rows(
    writer: &InMemoryRequestLogWriter,
    n: usize,
) -> Vec<tt_telemetry::request_logs::RequestLogRow> {
    for _ in 0..100 {
        if writer.rows().len() >= n {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    writer.rows()
}

fn header_str<'r>(resp: &'r axum::response::Response, name: &str) -> &'r str {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

fn header_f64(resp: &axum::response::Response, name: &str) -> f64 {
    header_str(resp, name)
        .parse::<f64>()
        .unwrap_or_else(|_| panic!("header {name} missing or not a number"))
}

fn warnings_of(resp: &axum::response::Response) -> Vec<String> {
    header_str(resp, "x-tokentrimmer-warnings")
        .split(',')
        .map(str::to_string)
        .collect()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).expect("response is JSON")
}

/// (a) Applied: one dispatch, full artifact to the caller, billed patch
/// usage, measured saving header > 0, row marked, never cached.
#[tokio::test]
async fn applied_patch_reconstructs_full_artifact() {
    let h = app_with_diff_route(false).await;
    let prior = prior_doc();
    h.script
        .lock()
        .unwrap()
        .push(Script::Body(GOOD_PATCH, PATCH_COMPLETION_TOKENS));

    let resp = h
        .app
        .clone()
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        h.calls.load(Ordering::Relaxed),
        1,
        "exactly one upstream call"
    );

    // The dispatched request dropped nothing but gained the instruction.
    {
        let recorded = h.requests.lock().unwrap();
        assert!(
            recorded[0].contains("anchored search/replace patch"),
            "patch instruction must ride as a System message: {}",
            recorded[0]
        );
    }

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "diff_applied"),
        "expected diff_applied, got {warnings:?}"
    );
    let saved = header_f64(&resp, "x-tokentrimmer-diff-saved-usd");
    assert!(saved > 0.0, "measured saving must be > 0, got {saved}");
    // Cost = the billed PATCH dispatch only.
    let cost = header_f64(&resp, "x-tokentrimmer-cost-usd");
    assert!(
        (cost - patch_cost()).abs() < 1e-9,
        "cost ({cost}) must equal the patch dispatch ({})",
        patch_cost()
    );
    // The measured saving rides the TT headline (baseline fold).
    let headline = header_f64(&resp, "x-tokentrimmer-saved-usd");
    assert!(
        (headline - saved).abs() < 1e-9,
        "headline ({headline}) must include exactly the diff saving ({saved})"
    );

    let body = body_json(resp).await;
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("SWIFT brown fox") && content.contains("Item 11"),
        "caller must receive the FULL reconstructed artifact, got: {content}"
    );
    assert!(
        !content.contains("SEARCH"),
        "the raw patch must never leak to the caller"
    );
    // usage stays the provider-billed PATCH usage — the invoice truth.
    assert_eq!(
        body["usage"]["completion_tokens"].as_u64(),
        Some(PATCH_COMPLETION_TOKENS)
    );

    let rows = wait_for_rows(&h.log_writer, 1).await;
    assert!(rows[0].diff_applied, "row must record the applied diff");
    assert!(rows[0].diff_saved_usd > 0.0);
    assert!(!rows[0].diff_failed);

    // Diff traffic never participates in L1: the identical follow-up
    // dispatches again (the reconstructed body depends on the prior, and
    // cache keys ignore tt_extras).
    h.script
        .lock()
        .unwrap()
        .push(Script::Body(GOOD_PATCH, PATCH_COMPLETION_TOKENS));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let r2 = h
        .app
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(
        h.calls.load(Ordering::Relaxed),
        2,
        "diff traffic must never be served from cache"
    );
}

/// (b) Fail-closed: invalid patch → full re-emit, both dispatches billed,
/// zero claimed saving, row marks the failure + retry cost.
#[tokio::test]
async fn failed_patch_fails_closed_to_full_reemit() {
    let h = app_with_diff_route(false).await;
    let prior = prior_doc();
    {
        let mut s = h.script.lock().unwrap();
        s.push(Script::Body(PROSE_NO_PATCH, PATCH_COMPLETION_TOKENS));
        s.push(Script::Body(FULL_REEMIT_BODY, FULL_COMPLETION_TOKENS));
    }

    let resp = h
        .app
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "fail-closed is never a 5xx");
    assert_eq!(
        h.calls.load(Ordering::Relaxed),
        2,
        "fail-closed = patch attempt + full re-emit"
    );

    // The re-emit dispatched the caller's EXACT original request: no patch
    // instruction on the second recorded upstream call.
    {
        let recorded = h.requests.lock().unwrap();
        assert!(recorded[0].contains("anchored search/replace patch"));
        assert!(
            !recorded[1].contains("anchored search/replace patch"),
            "the re-emit must be the caller's original request: {}",
            recorded[1]
        );
    }

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "diff_failed:no_blocks"),
        "expected diff_failed:no_blocks, got {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w == "diff_applied"),
        "a failed diff must not also claim success: {warnings:?}"
    );

    // The retry is REAL spend: cost = patch + re-emit, headline clamps to 0,
    // and the failed attempt is unpickable in its own header.
    let cost = header_f64(&resp, "x-tokentrimmer-cost-usd");
    let expected = patch_cost() + full_cost();
    assert!(
        (cost - expected).abs() < 1e-9,
        "cost ({cost}) must carry BOTH dispatches ({expected})"
    );
    assert_eq!(header_str(&resp, "x-tokentrimmer-saved-usd"), "0.000000");
    assert_eq!(
        header_str(&resp, "x-tokentrimmer-diff-saved-usd"),
        "0.000000"
    );
    let failed_cost = header_f64(&resp, "x-tokentrimmer-diff-failed-cost-usd");
    assert!(
        (failed_cost - patch_cost()).abs() < 1e-9,
        "failed-attempt cost ({failed_cost}) must equal the patch dispatch"
    );

    let body = body_json(resp).await;
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some(FULL_REEMIT_BODY),
        "the caller receives the full re-emit"
    );
    assert_eq!(
        body["usage"]["completion_tokens"].as_u64(),
        Some(FULL_COMPLETION_TOKENS)
    );

    let rows = wait_for_rows(&h.log_writer, 1).await;
    assert!(rows[0].diff_failed);
    assert!(rows[0].diff_failed_cost_usd > 0.0);
    assert!(!rows[0].diff_applied);
    assert_eq!(rows[0].diff_saved_usd, 0.0);
}

/// (c) Degraded: the re-emit dispatch errors → 200 with the raw patch body +
/// `diff_degraded`; the single billed dispatch is metered exactly once.
#[tokio::test]
async fn reemit_error_degrades_to_raw_patch_response() {
    let h = app_with_diff_route(false).await;
    let prior = prior_doc();
    {
        let mut s = h.script.lock().unwrap();
        s.push(Script::Body(PROSE_NO_PATCH, PATCH_COMPLETION_TOKENS));
        s.push(Script::Fail);
    }

    let resp = h
        .app
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "never a 5xx after a successful upstream call"
    );
    assert_eq!(h.calls.load(Ordering::Relaxed), 2);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "diff_failed:no_blocks"),
        "expected diff_failed:no_blocks, got {warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w == "diff_degraded"),
        "expected diff_degraded, got {warnings:?}"
    );

    // Exactly one dispatch billed (the errored re-emit billed nothing) — the
    // patch response is metered via its own usage, never double-counted.
    let cost = header_f64(&resp, "x-tokentrimmer-cost-usd");
    assert!(
        (cost - patch_cost()).abs() < 1e-9,
        "degraded trace must meter the single billed dispatch once, got {cost}"
    );

    let body = body_json(resp).await;
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some(PROSE_NO_PATCH),
        "the raw patch response passes through marked degraded"
    );
}

/// (d) No identifiable prior → single dispatch + diff_skipped:no_prior, no
/// instruction injected.
#[tokio::test]
async fn no_prior_skips_with_warning() {
    let h = app_with_diff_route(false).await;
    h.script
        .lock()
        .unwrap()
        .push(Script::Body("a normal answer", FULL_COMPLETION_TOKENS));

    // No assistant history, no tt_extras echo.
    let body = json!({
        "model": "diff-1",
        "messages": [{ "role": "user", "content": "write the release notes" }],
        "stream": false,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = h.app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.calls.load(Ordering::Relaxed), 1);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "diff_skipped:no_prior"),
        "expected diff_skipped:no_prior, got {warnings:?}"
    );
    let recorded = h.requests.lock().unwrap();
    assert!(
        !recorded[0].contains("anchored search/replace patch"),
        "no prior → no instruction: {}",
        recorded[0]
    );
}

/// (e) The explicit tt_extras echo WINS over the assistant history and never
/// reaches the upstream provider.
#[tokio::test]
async fn tt_extras_prior_wins_and_never_rides_upstream() {
    let h = app_with_diff_route(false).await;
    // The history doc does NOT contain the anchor; only the echoed doc does.
    let history_doc = "H".repeat(300);
    let mut echoed = prior_doc();
    echoed.push_str("Echo marker line.\n");
    h.script
        .lock()
        .unwrap()
        .push(Script::Body(GOOD_PATCH, PATCH_COMPLETION_TOKENS));

    let resp = h
        .app
        .oneshot(edit_request(
            &h.key,
            &history_doc,
            Some(json!({ "diff_prior": echoed })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "diff_applied"),
        "anchor exists only in the echoed prior — applying proves precedence: {warnings:?}"
    );
    let body = body_json(resp).await;
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("SWIFT brown fox") && content.contains("Echo marker line."),
        "the artifact must be the patched ECHOED doc: {content}"
    );

    let recorded = h.requests.lock().unwrap();
    assert!(
        !recorded[0].contains("diff_prior"),
        "the consumed tt_extras echo must never ride upstream: {}",
        recorded[0]
    );
}

/// (g) Truncation gate: a patch emission cut by the provider's token limit
/// (`finish_reason: "length"`) fails CLOSED even when the truncated text
/// parses as a structurally valid patch — a cut landing exactly on a
/// `>>>>>>> REPLACE` boundary would otherwise silently serve a PARTIALLY
/// edited artifact.
#[tokio::test]
async fn truncated_patch_fails_closed_to_full_reemit() {
    let h = app_with_diff_route(false).await;
    let prior = prior_doc();
    {
        let mut s = h.script.lock().unwrap();
        // GOOD_PATCH is fully parseable — only the finish_reason says the
        // emission was cut (the missing-trailing-block case is invisible to
        // every structural check, which is the point of the gate).
        s.push(Script::BodyFinish(
            GOOD_PATCH,
            PATCH_COMPLETION_TOKENS,
            "length",
        ));
        s.push(Script::Body(FULL_REEMIT_BODY, FULL_COMPLETION_TOKENS));
    }

    let resp = h
        .app
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        h.calls.load(Ordering::Relaxed),
        2,
        "truncated patch = failed attempt + full re-emit"
    );

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "diff_failed:truncated"),
        "expected diff_failed:truncated, got {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w == "diff_applied"),
        "a truncated patch must never claim success: {warnings:?}"
    );
    // Both dispatches billed; headline clamps to 0 (no fabricated saving).
    let cost = header_f64(&resp, "x-tokentrimmer-cost-usd");
    let expected = patch_cost() + full_cost();
    assert!(
        (cost - expected).abs() < 1e-9,
        "cost ({cost}) must carry BOTH dispatches ({expected})"
    );
    assert_eq!(header_str(&resp, "x-tokentrimmer-saved-usd"), "0.000000");

    let body = body_json(resp).await;
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some(FULL_REEMIT_BODY),
        "the caller receives the full re-emit, never the truncated patch's partial apply"
    );
}

/// (h) redact × diff: the fail-closed re-emit must dispatch the REDACTED
/// bytes — it is derived from the dispatched request, never a pre-pipeline
/// clone. A secret stripped from the patch dispatch must not reappear on
/// the re-emit (the measurement/retry path must never out-leak the dispatch
/// path).
#[tokio::test]
async fn redact_diff_reemit_never_leaks_unredacted_content() {
    let h = app_with_route(
        false,
        RouteAction {
            workflow: None,
            redact: true,
            ..diff_action()
        },
    )
    .await;
    let prior = prior_doc();
    let secret = format!("sk-ant-{}", "a".repeat(40));
    {
        let mut s = h.script.lock().unwrap();
        s.push(Script::Body(PROSE_NO_PATCH, PATCH_COMPLETION_TOKENS));
        s.push(Script::Body(FULL_REEMIT_BODY, FULL_COMPLETION_TOKENS));
    }

    // The edit ask leaks an API key in the user turn; the assistant prior is
    // untouched by redaction (Assistant messages are skipped), so the diff
    // planning itself is unaffected.
    let body = json!({
        "model": "diff-1",
        "messages": [
            { "role": "user", "content": "write the release notes" },
            { "role": "assistant", "content": prior },
            { "role": "user", "content": format!("punch up Item 3, auth with {secret}") }
        ],
        "stream": false,
    });
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = h.app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        h.calls.load(Ordering::Relaxed),
        2,
        "failed patch + fail-closed re-emit"
    );

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "redacted:body"),
        "the redaction guardrail must fire on the user turn: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w == "diff_failed:no_blocks"),
        "expected diff_failed:no_blocks, got {warnings:?}"
    );

    let recorded = h.requests.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    // The patch dispatch was redacted (pre-existing guarantee)…
    assert!(
        !recorded[0].contains(&secret) && recorded[0].contains("[REDACTED]"),
        "patch dispatch must carry redacted bytes: {}",
        recorded[0]
    );
    // …and the RE-EMIT must be too: the regression under test is a
    // pre-pipeline clone re-dispatching the raw secret upstream.
    assert!(
        !recorded[1].contains(&secret),
        "the fail-closed re-emit must NEVER leak the unredacted secret upstream: {}",
        recorded[1]
    );
    assert!(
        recorded[1].contains("[REDACTED]"),
        "the re-emit must carry the same redacted bytes the dispatch path carried: {}",
        recorded[1]
    );
    // And it is still the instruction-free original ask.
    assert!(
        !recorded[1].contains("anchored search/replace patch"),
        "the re-emit must not carry the patch instruction: {}",
        recorded[1]
    );
}

/// (i) Canary traffic split × shaping: the CONTROL arm's contract is "served
/// unchanged" — the contract-changing diff lever must not fire there
/// (traffic_pct: 0 ⇒ every request is control), while the canary arm
/// (traffic_pct: 100) keeps the lever.
#[tokio::test]
async fn canary_control_arm_never_receives_shaping() {
    // Control arm: pct=0 puts ALL traffic on control.
    let h = app_with_route(
        false,
        RouteAction {
            workflow: None,
            traffic_pct: Some(0),
            ..diff_action()
        },
    )
    .await;
    let prior = prior_doc();
    h.script
        .lock()
        .unwrap()
        .push(Script::Body("a normal answer", FULL_COMPLETION_TOKENS));

    let resp = h
        .app
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.calls.load(Ordering::Relaxed), 1);

    let warnings = warnings_of(&resp);
    assert!(
        !warnings.iter().any(|w| w.starts_with("diff")),
        "the control arm must emit NO diff tokens (not even a skip): {warnings:?}"
    );
    {
        let recorded = h.requests.lock().unwrap();
        assert!(
            !recorded[0].contains("anchored search/replace patch"),
            "no instruction may be injected on the control arm: {}",
            recorded[0]
        );
    }

    // Canary arm: pct=100 puts ALL traffic on canary — the lever still fires.
    let h = app_with_route(
        false,
        RouteAction {
            workflow: None,
            traffic_pct: Some(100),
            ..diff_action()
        },
    )
    .await;
    h.script
        .lock()
        .unwrap()
        .push(Script::Body(GOOD_PATCH, PATCH_COMPLETION_TOKENS));
    let resp = h
        .app
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "diff_applied"),
        "the canary arm keeps the lever: {warnings:?}"
    );
}

/// (j) Defense-in-depth conflict arm: a route carrying BOTH levers (only
/// reachable by a writer that bypasses `validate_output_shaping` — the
/// in-memory store here) lets diff win and skips the switch with the
/// `conflict` token.
#[tokio::test]
async fn conflicting_levers_defend_diff_wins_switch_skipped() {
    let h = app_with_route(
        false,
        RouteAction {
            workflow: None,
            format_switch: Some("csv".into()),
            ..diff_action()
        },
    )
    .await;
    let prior = prior_doc();
    h.script
        .lock()
        .unwrap()
        .push(Script::Body(GOOD_PATCH, PATCH_COMPLETION_TOKENS));

    let resp = h
        .app
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.calls.load(Ordering::Relaxed), 1);

    let warnings = warnings_of(&resp);
    assert!(
        warnings
            .iter()
            .any(|w| w == "format_switch_skipped:conflict"),
        "the switch must be skipped with the conflict token: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w == "diff_applied"),
        "diff wins the conflict: {warnings:?}"
    );
    let body = body_json(resp).await;
    let content = body["choices"][0]["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("SWIFT brown fox"),
        "the diff lever must still reconstruct the artifact: {content}"
    );
}

/// (f) A PAUSED route suppresses the cost lever entirely.
#[tokio::test]
async fn paused_route_suppresses_diff() {
    let h = app_with_diff_route(true).await;
    let prior = prior_doc();
    h.script
        .lock()
        .unwrap()
        .push(Script::Body("a normal answer", FULL_COMPLETION_TOKENS));

    let resp = h
        .app
        .oneshot(edit_request(&h.key, &prior, None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(h.calls.load(Ordering::Relaxed), 1);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.iter().any(|w| w == "route_paused:diff-route"),
        "expected route_paused:diff-route, got {warnings:?}"
    );
    assert!(
        !warnings.iter().any(|w| w.starts_with("diff")),
        "a paused route must emit NO diff tokens: {warnings:?}"
    );
    let recorded = h.requests.lock().unwrap();
    assert!(
        !recorded[0].contains("anchored search/replace patch"),
        "no instruction may be injected on a paused route: {}",
        recorded[0]
    );
}
