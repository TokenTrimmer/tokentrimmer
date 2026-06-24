//! End-to-end (hermetic) tests for the conservative compression pass
//! (COMPRESS-PASS1), driven by the `RouteAction::compress` route flag:
//!
//! (a) a request with redundant whitespace + a duplicated tool-result block →
//!     the UPSTREAM request the provider sees has fewer estimated tokens AND the
//!     semantic content is preserved (user prose byte-identical; tool args /
//!     instruction content preserved modulo the safe trims);
//! (b) the pass is OFF by default — the same request through a route WITHOUT
//!     `compress` reaches the upstream byte-for-byte unchanged;
//! (c) savings are attributed to the new `compression` source
//!     (`X-TokenTrimmer-Compression-Saved-Usd`, folded into `saved_usd`);
//! (d) user prose / a normal message is never modified by the pass.
//!
//! No network: a mock provider records the exact upstream request it receives
//! and returns a fixed response whose `prompt_tokens` reflect the (compressed)
//! prompt, mirroring `flex_route.rs` / `route_rewrite.rs`.

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
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutingStore,
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
/// can assert what the compression pass did/didn't trim) and returns a fixed
/// response. Standard input/output rates make the compression saving math exact.
struct RecordingProvider {
    /// The `messages` of each upstream request, captured verbatim.
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
}

const INPUT_PER_M: f64 = 10.0;
const OUTPUT_PER_M: f64 = 30.0;
// Token usage the upstream reports back (post-compression prompt).
const PROMPT_TOKENS: u64 = 1000;
const COMPLETION_TOKENS: u64 = 200;

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "rec"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "rec-model".into(),
            provider: "rec".into(),
            // Tools capability is required: the test request carries an
            // assistant `tool_calls` + tool-result messages, so the routing
            // capability guard skips a model that does not advertise Tools.
            capabilities: vec![Capability::Text, Capability::Tools],
            max_input_tokens: 100_000,
            max_output_tokens: 8192,
        }]
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

/// Build a gateway whose org has a single route matching `rec-model` with
/// `compress` set to `compress_flag`, returning the app, key, and the recorded
/// upstream-messages handle.
async fn app_with_compress_route(
    compress_flag: bool,
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
            name: "compress-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["rec-model".into()],
                ..Default::default()
            },
            // No model rewrite — compression is a pure request-pass action.
            then: RouteAction {
                format_switch: None,
                diff: false,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                agentic_budget: None,
                target_model: Some("rec-model".into()),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: compress_flag,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                panel: None,
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
    (app, plaintext, seen)
}

/// A request carrying:
/// - a SYSTEM block with redundant trailing whitespace + blank-line runs,
/// - a USER prose message (with its OWN trailing whitespace that must survive),
/// - an ASSISTANT tool_call with whitespace-padded JSON arguments,
/// - TWO byte-identical adjacent TOOL results for the same call (a duplicate).
fn redundant_request(stream: bool) -> Request<Body> {
    let body = json!({
        "model": "rec-model",
        "stream": stream,
        "messages": [
            { "role": "system", "content": "You are concise.   \n\n\n\n\nFollow the rules.   " },
            { "role": "user", "content": "Please summarize.   \n\n\n\n\nKeep my spacing." },
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_x",
                    "type": "function",
                    "function": { "name": "lookup", "arguments": "{  \"q\" :  \"weather\"  }" }
                }]
            },
            { "role": "tool", "tool_call_id": "call_x", "content": "RESULT   \n\n\n\n\nDATA" },
            { "role": "tool", "tool_call_id": "call_x", "content": "RESULT   \n\n\n\n\nDATA" }
        ]
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header(
            "authorization",
            format!("Bearer {bearer}", bearer = "PLACEHOLDER"),
        )
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn request_with_key(stream: bool, key: &str) -> Request<Body> {
    let r = redundant_request(stream);
    let (mut parts, body) = r.into_parts();
    parts
        .headers
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    Request::from_parts(parts, body)
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

/// (a)+(d) With the compress route ON, the upstream request is trimmed on
/// non-prose blocks while USER PROSE is byte-identical and tool-call arguments
/// are preserved (lossless), and the duplicate tool block is dropped.
#[tokio::test]
async fn compress_route_trims_nonprose_preserves_prose() {
    let (app, key, seen) = app_with_compress_route(true).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];

    // The duplicate adjacent tool result for the same call was dropped: the
    // original request had 5 messages; the upstream sees 4.
    assert_eq!(
        upstream.len(),
        4,
        "duplicate adjacent tool block should be de-duplicated"
    );

    // System (non-prose) block: trailing whitespace stripped + blank-line run
    // collapsed — lossless.
    let system = upstream
        .iter()
        .find(|m| matches!(m, Message::System { .. }))
        .unwrap();
    assert_eq!(
        text_of(system).unwrap(),
        "You are concise.\n\nFollow the rules.",
        "system block must be conservatively trimmed"
    );

    // (d) USER PROSE is byte-identical — its trailing whitespace + blank lines
    // are NOT touched.
    let user = upstream
        .iter()
        .find(|m| matches!(m, Message::User { .. }))
        .unwrap();
    assert_eq!(
        text_of(user).unwrap(),
        "Please summarize.   \n\n\n\n\nKeep my spacing.",
        "USER PROSE must never be modified by the compression pass"
    );

    // Tool-call arguments: canonicalized JSON, but the decoded value is
    // identical (lossless).
    let assistant = upstream
        .iter()
        .find(|m| matches!(m, Message::Assistant { .. }))
        .unwrap();
    let Message::Assistant { tool_calls, .. } = assistant else {
        unreachable!()
    };
    let args = &tool_calls[0].function.arguments;
    let decoded: serde_json::Value = serde_json::from_str(args).unwrap();
    assert_eq!(
        decoded,
        json!({ "q": "weather" }),
        "tool-call arguments value must be preserved exactly"
    );

    // The remaining tool result is trimmed (non-prose), still semantically the
    // same content.
    let tool = upstream
        .iter()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap();
    assert_eq!(text_of(tool).unwrap(), "RESULT\n\nDATA");
}

/// (b) With the compress route OFF (flag false), the upstream request reaches
/// the provider BYTE-FOR-BYTE unchanged — the pass never ran.
#[tokio::test]
async fn compress_off_by_default_leaves_request_unchanged() {
    let (app, key, seen) = app_with_compress_route(false).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];

    // All 5 original messages survive (no de-dup, no trim).
    assert_eq!(upstream.len(), 5, "no pass should run when compress is off");

    let system = upstream
        .iter()
        .find(|m| matches!(m, Message::System { .. }))
        .unwrap();
    assert_eq!(
        text_of(system).unwrap(),
        "You are concise.   \n\n\n\n\nFollow the rules.   ",
        "system block unchanged when compress is off"
    );

    // Both duplicate tool blocks survive unchanged.
    let tools: Vec<String> = upstream
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .map(|m| text_of(m).unwrap())
        .collect();
    assert_eq!(
        tools,
        vec![
            "RESULT   \n\n\n\n\nDATA".to_string(),
            "RESULT   \n\n\n\n\nDATA".to_string()
        ],
        "tool blocks unchanged + not de-duplicated when compress is off"
    );
}

/// A route with NO compress flag at all (default RouteAction) must also leave
/// the request unchanged — proving OFF BY DEFAULT at the type level.
#[tokio::test]
async fn no_route_means_no_compression() {
    // Build an app with a route that matches but uses the DEFAULT action (no
    // compress). Equivalent to (b) but via Default to lock in the default.
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
            name: "plain".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["rec-model".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: Some("rec-model".into()),
                ..Default::default()
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

    let resp = app
        .oneshot(request_with_key(false, &plaintext))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        seen.lock().unwrap()[0].len(),
        5,
        "default RouteAction must not compress"
    );
}

/// (c) The compression saving is attributed to the dedicated `compression`
/// source header AND folded into the headline `saved_usd`, equal to the removed
/// tokens × served input rate. With the upstream reporting the post-compression
/// prompt count, the saving must be strictly positive and consistent.
#[tokio::test]
async fn compression_saving_is_attributed() {
    let (app, key, _seen) = app_with_compress_route(true).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let header_f64 = |name: &str| -> f64 {
        resp.headers()[name]
            .to_str()
            .unwrap()
            .parse::<f64>()
            .unwrap()
    };

    let compression_saved = header_f64("x-tokentrimmer-compression-saved-usd");
    let saved = header_f64("x-tokentrimmer-saved-usd");
    let cost = header_f64("x-tokentrimmer-cost-usd");
    let baseline = header_f64("x-tokentrimmer-baseline-cost-usd");

    // The pass removed at least one token, so the compression saving is > 0.
    assert!(
        compression_saved > 0.0,
        "compression saving must be positive, got {compression_saved}"
    );

    // The headline saving includes the compression saving (no routing rewrite,
    // no flex, no cache → the headline IS the compression saving).
    assert!(
        (saved - compression_saved).abs() < 1e-9,
        "headline saved ({saved}) should equal the compression saving ({compression_saved})"
    );

    // baseline = cost + saved (the no-TT baseline is the served cost on the
    // uncompressed prompt; the delta is exactly the compression saving).
    assert!(
        (baseline - cost - compression_saved).abs() < 1e-9,
        "baseline ({baseline}) must equal cost ({cost}) + compression_saved ({compression_saved})"
    );

    // The realized cost is metered on the (reduced) prompt the upstream
    // reported: PROMPT_TOKENS × input + COMPLETION_TOKENS × output.
    let expected_cost = (PROMPT_TOKENS as f64) * INPUT_PER_M / 1e6
        + (COMPLETION_TOKENS as f64) * OUTPUT_PER_M / 1e6;
    assert!(
        (cost - expected_cost).abs() < 1e-9,
        "cost ({cost}) should meter the reported (compressed) usage ({expected_cost})"
    );
}

/// Without any compression (route off), the compression-saved header is exactly
/// 0 — no phantom saving on the default path.
#[tokio::test]
async fn no_compression_means_zero_compression_saving() {
    let (app, key, _seen) = app_with_compress_route(false).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let compression_saved: f64 = resp.headers()["x-tokentrimmer-compression-saved-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(compression_saved, 0.0, "off → zero compression saving");
}
