//! End-to-end (hermetic) tests for wiring the agentic-context-budget planner
//! into the chat handler (plan `2026-06-13-agentic-cost-context-budget`, Task 9),
//! driven by the `RouteAction::agentic_budget` opt-in:
//!
//! (a) OFF BY DEFAULT — a request through a route with the DEFAULT action
//!     (`agentic_budget: None`) reaches the upstream BYTE-FOR-BYTE unchanged and
//!     emits no new spend headers (the planner is never constructed). This is the
//!     load-bearing back-compat invariant: the default path must be byte-identical.
//! (b) RUNS AFTER REDACTION (R5) — a route with BOTH `redact` and an
//!     `agentic_budget` set redacts the outbound request FIRST, so the planner's
//!     split (and any field-drop) is built from the POST-redaction bytes: the
//!     upstream sees the secret stripped AND the droppable field elided on the
//!     SAME tool block, proving the planner operated on the redacted request.
//!
//! No network: a mock provider records the exact upstream request it receives
//! and returns a fixed response, mirroring `compression_route.rs` /
//! `redaction_route.rs`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
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
    AgenticBudget, CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions,
    RoutingStore,
};
use tt_shared::messages::{Message, MessageContent};
use tt_shared::{messages::Choice, pricing::Capability};
use tt_shared::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Mock provider that records the upstream `messages` it received (so the test
/// can assert what the planner / redaction did or didn't change) and returns a
/// fixed response. Standard input/output rates keep any saving math exact.
struct RecordingProvider {
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
}

const INPUT_PER_M: f64 = 10.0;
const OUTPUT_PER_M: f64 = 30.0;
const PROMPT_TOKENS: u64 = 1000;
const COMPLETION_TOKENS: u64 = 200;

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "rec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        let caps = vec![Capability::Text, Capability::Tools];
        vec![
            ModelInfo {
                id: "rec-model".into(),
                provider: "rec".into(),
                // The test request carries tool_calls + tool-result messages, so
                // the routing capability guard skips a model lacking Tools.
                capabilities: caps.clone(),
                max_input_tokens: 100_000,
                max_output_tokens: 8192,
            },
            // A second model so a `Some`-target route can rewrite FROM one model
            // TO another (the regression guard needs requested != target).
            ModelInfo {
                id: "rec-source".into(),
                provider: "rec".into(),
                capabilities: caps,
                max_input_tokens: 100_000,
                max_output_tokens: 8192,
            },
        ]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: INPUT_PER_M,
            output_per_million: OUTPUT_PER_M,
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
        self.seen_messages
            .lock()
            .unwrap()
            .push(req.messages.clone());
        Ok(ChatCompletionResponse {
            id: "chatcmpl-rec".into(),
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
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        _req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        Err(ProviderError::Unsupported("no stream".into()))
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

/// Build a gateway whose org has a single route matching `rec-model` carrying
/// the supplied `RouteAction` (caller sets `agentic_budget` / `redact`),
/// returning the app, key, and the recorded upstream-messages handle.
async fn app_with_action(
    then: RouteAction,
) -> (axum::Router, String, Arc<Mutex<Vec<Vec<Message>>>>) {
    app_with_route(vec!["rec-model".into()], then).await
}

/// Like [`app_with_action`] but lets the caller set the route's `when.model_in`
/// (so a `Some`-target route can match a request for one model and rewrite it to
/// another).
async fn app_with_route(
    model_in: Vec<String>,
    then: RouteAction,
) -> (axum::Router, String, Arc<Mutex<Vec<Vec<Message>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        seen_messages: Arc::clone(&seen),
    }));

    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let routes_backing = Arc::new(InMemoryRoutingStore::new());
    routes_backing.set_routes(
        org_id,
        vec![Route {
            paused: false,
            id: Uuid::now_v7(),
            name: "agentic-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in,
                ..Default::default()
            },
            then,
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
    (app, plaintext, seen)
}

const TOOL_SSN: &str = "123-45-6789";
const TMP_FILE: &str = "/tmp/.tt-scan-8c1f2-with-a-long-noisy-tempfile-path.py";

/// A multi-step agent request whose OLDER `inspect_diff` tool result carries
/// BOTH a secret (a US SSN) and a droppable per-finding `file` tempfile path,
/// plus a RECENT tool result (kept verbatim by `keep_recent_pairs`).
fn agent_request() -> serde_json::Value {
    json!({
        "model": "rec-model",
        "stream": false,
        "messages": [
            { "role": "user", "content": "scan my diff" },
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "inspect_diff", "arguments": "{}" }
                }]
            },
            { "role": "tool", "tool_call_id": "c1", "content": json!({
                "scanned": true,
                "detected_language": "python",
                "note": format!("found ssn {TOOL_SSN} in db"),
                "findings": [{
                    "rule_id": "cache-missing",
                    "severity": "high",
                    "file": TMP_FILE,
                    "line": 3,
                    "message": "m",
                    "confidence": 0.9,
                    "fix_hint": "h"
                }]
            }).to_string() },
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "c2",
                    "type": "function",
                    "function": { "name": "inspect_diff", "arguments": "{}" }
                }]
            },
            { "role": "tool", "tool_call_id": "c2", "content": json!({
                "scanned": true,
                "detected_language": "python",
                "findings": [{
                    "rule_id": "cache-missing",
                    "severity": "high",
                    "file": "/tmp/.tt-scan-RECENT.py",
                    "line": 9,
                    "message": "m2",
                    "confidence": 0.8,
                    "fix_hint": "h2"
                }]
            }).to_string() }
        ]
    })
}

fn request_with_key(body: serde_json::Value, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn text_of(m: &Message) -> Option<String> {
    match m {
        Message::System { content }
        | Message::User { content, .. }
        | Message::Tool { content, .. } => match content {
            MessageContent::Text(s) => Some(s.clone()),
            MessageContent::Parts(_) => None,
        },
        Message::Assistant { content, .. } => match content {
            Some(MessageContent::Text(s)) => Some(s.clone()),
            _ => None,
        },
    }
}

/// (a) OFF BY DEFAULT — a route with the DEFAULT `RouteAction`
/// (`agentic_budget: None`) leaves the request BYTE-FOR-BYTE unchanged at the
/// upstream: the planner is never constructed, so no field-drop, no
/// cache-prefix annotation, no new tokens. The default path is byte-identical.
#[tokio::test]
async fn agentic_budget_off_by_default_is_byte_identical() {
    let (app, key, seen) = app_with_action(RouteAction {
        target_model: Some("rec-model".into()),
        ..Default::default()
    })
    .await;

    let original = agent_request();
    let resp = app
        .oneshot(request_with_key(original.clone(), &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // No new spend headers on the default path: the planner books no field-drop
    // / summary / tax / cache-bust, so those headers are absent or exactly zero.
    let header_zero_or_absent = |name: &str| match resp.headers().get(name) {
        None => true,
        Some(v) => v
            .to_str()
            .unwrap()
            .parse::<f64>()
            .map(|x| x == 0.0)
            .unwrap_or(false),
    };
    assert!(
        header_zero_or_absent("x-tokentrimmer-summarizer-tax-usd"),
        "no summarizer tax on the default path"
    );
    assert!(
        header_zero_or_absent("x-tokentrimmer-cache-bust-usd"),
        "no cache-bust penalty on the default path"
    );

    // The upstream messages are byte-for-byte the request's messages: the JSON
    // serialization of the dispatched tool blocks equals the original input.
    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];
    assert_eq!(
        upstream.len(),
        5,
        "all five messages survive unchanged when agentic_budget is off"
    );

    // Both tool results reach the upstream byte-identical to what was sent (the
    // droppable `file` path survives, the secret survives — nothing ran).
    let original_tools: Vec<String> = original["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap().to_string())
        .collect();
    let upstream_tools: Vec<String> = upstream
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .map(|m| text_of(m).unwrap())
        .collect();
    assert_eq!(
        upstream_tools, original_tools,
        "tool blocks must reach the upstream byte-identical when agentic_budget is off"
    );
}

/// (a') A route that matches but carries an explicit `agentic_budget: None`
/// (rather than relying on `..Default::default()`) is also a no-op — locking in
/// the off-by-default contract at the field level.
#[tokio::test]
async fn agentic_budget_none_is_byte_identical() {
    let (app, key, seen) = app_with_action(RouteAction {
        target_model: Some("rec-model".into()),
        agentic_budget: None,
        ..Default::default()
    })
    .await;

    let original = agent_request();
    let resp = app
        .oneshot(request_with_key(original.clone(), &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];
    assert_eq!(
        upstream.len(),
        5,
        "no message dropped when agentic_budget is None"
    );
    let upstream_tools: Vec<String> = upstream
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .map(|m| text_of(m).unwrap())
        .collect();
    // The droppable tempfile path survives → field-drop never ran.
    assert!(
        upstream_tools[0].contains(TMP_FILE),
        "the droppable file path must survive when agentic_budget is None"
    );
}

/// (b) RUNS AFTER REDACTION (R5) — with BOTH `redact` and `agentic_budget`
/// (`elide_stale_tools`) set, redaction strips the secret from the outbound
/// request FIRST, then the planner's field-drop runs on the POST-redaction
/// bytes. The upstream's OLDER tool block shows the secret GONE *and* the
/// droppable `file` path elided — proving the planner operated on the redacted
/// request (a pre-redaction clone would have leaked the secret).
#[tokio::test]
async fn agentic_budget_runs_after_redaction() {
    let (app, key, seen) = app_with_action(RouteAction {
        target_model: Some("rec-model".into()),
        redact: true,
        agentic_budget: Some(AgenticBudget {
            elide_stale_tools: true,
            // Keep only the most-recent pair verbatim so the OLDER tool block
            // (carrying the secret + droppable field) is eligible for field-drop.
            keep_recent_pairs: 1,
            ..Default::default()
        }),
        ..Default::default()
    })
    .await;

    let resp = app
        .oneshot(request_with_key(agent_request(), &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];

    // NO upstream message may carry the plaintext secret — redaction ran on the
    // outbound request BEFORE the planner saw it.
    for m in upstream.iter() {
        if let Some(t) = text_of(m) {
            assert!(
                !t.contains(TOOL_SSN),
                "the secret SSN must never reach the upstream: {t}"
            );
        }
    }

    // The OLDER tool block (idx 2) also had its droppable tempfile `file` path
    // elided by the planner's field-drop — proving the planner RAN, and ran on
    // the redacted bytes (same `req`, after redaction).
    let older_tool = text_of(&upstream[2]).expect("older tool block is text");
    assert!(
        !older_tool.contains(TMP_FILE),
        "the planner's field-drop must elide the older block's tempfile path: {older_tool}"
    );
    assert!(
        !older_tool.contains(TOOL_SSN),
        "the redacted secret must be gone from the elided older block: {older_tool}"
    );

    // The RECENT tool block (idx 4) is kept verbatim by keep_recent_pairs=1 —
    // its tempfile path survives (only the OLDER block is field-dropped).
    let recent_tool = text_of(&upstream[4]).expect("recent tool block is text");
    assert!(
        recent_tool.contains("/tmp/.tt-scan-RECENT.py"),
        "the most-recent tool block must be kept verbatim: {recent_tool}"
    );
}

/// Read the upstream-dispatched model: the `RecordingProvider` records the model
/// it received as the first dispatched request's response model (it echoes
/// `req.model`), but the most direct signal is the dispatched messages plus the
/// response body's `model` field, which the mock sets to `req.model`. We read it
/// from the response JSON.
async fn response_model(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["model"].as_str().unwrap().to_string()
}

/// MODIFIER-ONLY ROUTE — a route with `target_model: None` (omitted) keeps the
/// CALLER's chosen model (no rewrite) AND still applies its `agentic_budget`
/// effects. The dispatched/echoed model equals the request model, and the
/// planner runs (the older block's droppable tempfile path is elided) — proving
/// a modifier-only route applies its effects without re-targeting the model.
#[tokio::test]
async fn modifier_only_route_keeps_caller_model_and_runs_planner() {
    let (app, key, seen) = app_with_action(RouteAction {
        // Modifier-only: NO rewrite target.
        target_model: None,
        agentic_budget: Some(AgenticBudget {
            cache_prefix: true,
            elide_stale_tools: true,
            keep_recent_pairs: 1,
            ..Default::default()
        }),
        ..Default::default()
    })
    .await;

    let resp = app
        .oneshot(request_with_key(agent_request(), &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // The model is UNCHANGED — the modifier-only route echoes the caller's model.
    assert_eq!(
        response_model(resp).await,
        "rec-model",
        "a modifier-only route must NOT rewrite the model"
    );

    // The planner RAN: the older tool block's droppable tempfile path is elided
    // (the same observable the redaction-ordering wiring test asserts).
    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];
    let older_tool = text_of(&upstream[2]).expect("older tool block is text");
    assert!(
        !older_tool.contains(TMP_FILE),
        "the planner's field-drop must run on a modifier-only route: {older_tool}"
    );
    // The recent block is kept verbatim (keep_recent_pairs=1) — proving the
    // planner ran the keep-recent logic, not that all blocks were dropped.
    let recent_tool = text_of(&upstream[4]).expect("recent tool block is text");
    assert!(
        recent_tool.contains("/tmp/.tt-scan-RECENT.py"),
        "the most-recent tool block must be kept verbatim: {recent_tool}"
    );
}

/// REGRESSION GUARD — a normal route (`target_model: Some(..)`) still rewrites
/// `req.model` to the target exactly as before. A request for `rec-source`
/// matches a route that rewrites to `rec-model`; the dispatched/echoed model is
/// the TARGET, proving the optional-target change did NOT alter `Some`-route
/// behavior.
#[tokio::test]
async fn some_target_route_still_rewrites_model() {
    let (app, key, _seen) = app_with_route(
        vec!["rec-source".into()],
        RouteAction {
            target_model: Some("rec-model".into()),
            ..Default::default()
        },
    )
    .await;

    let mut req = agent_request();
    req["model"] = json!("rec-source");
    let resp = app.oneshot(request_with_key(req, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        response_model(resp).await,
        "rec-model",
        "a Some-target route must rewrite req.model from rec-source to the target rec-model"
    );
}
