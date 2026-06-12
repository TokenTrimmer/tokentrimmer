//! End-to-end (hermetic) tests for the request-redaction guardrail
//! (Guardrails MVP #461), driven by the `RouteAction::redact` route flag:
//!
//! (a) with the redact route ON, PII/secrets in the USER body, SYSTEM block,
//!     and TOOL-RESULT content are replaced with `[REDACTED]` in the UPSTREAM
//!     request the provider sees — the secret never leaves the gateway;
//! (b) the guardrail is OFF by default — the same request through a route
//!     WITHOUT `redact` reaches the upstream byte-for-byte unchanged;
//! (c) when redaction fires, the `x-tokentrimmer-warnings` header carries a
//!     `redacted:<class>` entry for each field class that was redacted, and the
//!     matched secret VALUES never appear in any header;
//! (d) redaction is a SAFETY transform, not a savings feature — no redaction
//!     cost/savings header is emitted;
//! (e) a cache HIT serves the cached response without re-running redaction
//!     (the pass is a no-op on the hit path).
//!
//! No network: a mock provider records the exact upstream request it receives
//! and returns a fixed response, mirroring `compression_route.rs`.

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
use tt_cache::memory::InMemoryL1Cache;
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
/// can assert what the redaction guardrail did/didn't strip) and returns a
/// fixed response. It also counts how many upstream calls it received, so the
/// cache-hit test can assert the provider was hit exactly once.
struct RecordingProvider {
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
    /// `Some(min)` makes the model cache-capable: the request-pass split then
    /// computes a stable prefix. Redaction is DETERMINISTIC on the ingress
    /// bytes, so even a hit inside that prefix books NO cache-bust penalty
    /// (the dispatched prefix is byte-identical every turn — the provider
    /// cache keeps hitting). `None` (the default everywhere below) keeps the
    /// pre-lane all-volatile behavior.
    cache_min_tokens: Option<u32>,
}

const INPUT_PER_M: f64 = 10.0;
const OUTPUT_PER_M: f64 = 30.0;
const CACHED_INPUT_PER_M: f64 = 1.0;
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
            capabilities: vec![Capability::Text, Capability::Tools],
            max_input_tokens: 100_000,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: INPUT_PER_M,
            output_per_million: OUTPUT_PER_M,
            cached_input_per_million: Some(CACHED_INPUT_PER_M),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: self.cache_min_tokens,
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
/// `redact` set to `redact_flag`, returning the app, key, and the recorded
/// upstream-messages handle. `None` cache minimum — the pre-lane shape.
async fn app_with_redact_route(
    redact_flag: bool,
) -> (axum::Router, String, Arc<Mutex<Vec<Vec<Message>>>>) {
    app_with_redact_route_and_cache_min(redact_flag, None).await
}

/// Like [`app_with_redact_route`] but with a configurable
/// `prompt_cache_min_tokens` so the bust-booking tests can make the model
/// cache-capable.
async fn app_with_redact_route_and_cache_min(
    redact_flag: bool,
    cache_min_tokens: Option<u32>,
) -> (axum::Router, String, Arc<Mutex<Vec<Vec<Message>>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        seen_messages: Arc::clone(&seen),
        cache_min_tokens,
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
            name: "redact-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["rec-model".into()],
                ..Default::default()
            },
            // No model rewrite — redaction is a pure request-pass guardrail.
            then: RouteAction {
                target_model: "rec-model".into(),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                redact: redact_flag,
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
            .with_routing_store(routing)
            // L1 wired so the cache-hit test can prove redaction does not re-run
            // on a hit. The other tests are unaffected (each sends one request).
            .with_l1(Arc::new(InMemoryL1Cache::new()), None),
    );
    (app, plaintext, seen)
}

// Plaintext secrets/PII embedded in the request. Captured as constants so the
// assertions can prove NONE of them survive to the upstream or leak into headers.
const SECRET_KEY: &str = "sk-ant-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const USER_EMAIL: &str = "jane.doe@example.com";
const TOOL_SSN: &str = "123-45-6789";

/// A request carrying PII/secrets in three field classes:
/// - a SYSTEM block leaking an Anthropic API key (agent-config secret),
/// - a USER prose message with an email address,
/// - a TOOL result with a US SSN scraped from upstream.
fn pii_request(stream: bool) -> serde_json::Value {
    json!({
        "model": "rec-model",
        "stream": stream,
        "messages": [
            { "role": "system", "content": format!("Internal config: ANTHROPIC_API_KEY={SECRET_KEY}") },
            { "role": "user", "content": format!("My email is {USER_EMAIL}, please help.") },
            {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_x",
                    "type": "function",
                    "function": { "name": "lookup", "arguments": "{\"q\":\"records\"}" }
                }]
            },
            { "role": "tool", "tool_call_id": "call_x", "content": format!("found ssn {TOOL_SSN} in db") }
        ]
    })
}

fn request_with_key(stream: bool, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(pii_request(stream).to_string()))
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

/// (a) With the redact route ON, every PII/secret span in the system/user/tool
/// blocks is replaced with `[REDACTED]` in the UPSTREAM request — no plaintext
/// secret reaches the provider.
#[tokio::test]
async fn redact_route_strips_pii_from_all_field_classes() {
    let (app, key, seen) = app_with_redact_route(true).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];

    let system = upstream
        .iter()
        .find(|m| matches!(m, Message::System { .. }))
        .unwrap();
    assert_eq!(
        text_of(system).unwrap(),
        "Internal config: ANTHROPIC_API_KEY=[REDACTED]",
        "system secret must be redacted"
    );

    let user = upstream
        .iter()
        .find(|m| matches!(m, Message::User { .. }))
        .unwrap();
    assert_eq!(
        text_of(user).unwrap(),
        "My email is [REDACTED], please help.",
        "user PII must be redacted"
    );

    let tool = upstream
        .iter()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap();
    assert_eq!(
        text_of(tool).unwrap(),
        "found ssn [REDACTED] in db",
        "tool-result PII must be redacted"
    );

    // No plaintext secret value survives in ANY upstream message.
    let all_upstream_text: String = upstream.iter().filter_map(text_of).collect();
    assert!(!all_upstream_text.contains(SECRET_KEY));
    assert!(!all_upstream_text.contains(USER_EMAIL));
    assert!(!all_upstream_text.contains(TOOL_SSN));
}

/// (c) When redaction fires, the warnings header lists a `redacted:<class>`
/// entry per redacted field class — and never the secret values.
#[tokio::test]
async fn redact_route_emits_field_class_warnings_without_leaking_values() {
    let (app, key, _seen) = app_with_redact_route(true).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let warnings = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .expect("warnings header must be present when redaction fires")
        .to_str()
        .unwrap()
        .to_string();

    assert!(
        warnings.contains("redacted:system"),
        "warnings must name the system class: {warnings}"
    );
    assert!(
        warnings.contains("redacted:body"),
        "warnings must name the body class: {warnings}"
    );
    assert!(
        warnings.contains("redacted:tool_result"),
        "warnings must name the tool_result class: {warnings}"
    );

    // The secret VALUES must never appear in the warnings header.
    assert!(!warnings.contains(SECRET_KEY));
    assert!(!warnings.contains(USER_EMAIL));
    assert!(!warnings.contains(TOOL_SSN));

    // (d) Redaction is a SAFETY transform, not a savings feature: there is no
    // redaction-saved cost header, and no secret leaks into any header value.
    assert!(
        resp.headers()
            .get("x-tokentrimmer-redaction-saved-usd")
            .is_none(),
        "redaction must NOT emit a savings header"
    );
    for (_name, value) in resp.headers() {
        let v = value.to_str().unwrap_or("");
        assert!(!v.contains(SECRET_KEY));
        assert!(!v.contains(USER_EMAIL));
        assert!(!v.contains(TOOL_SSN));
    }
}

/// (b) With the redact route OFF (flag false), the upstream request reaches the
/// provider BYTE-FOR-BYTE unchanged and no `redacted:*` warning is emitted.
#[tokio::test]
async fn redact_off_by_default_leaves_request_unchanged() {
    let (app, key, seen) = app_with_redact_route(false).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // No redacted:* warning on the default path.
    let warnings = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        !warnings.contains("redacted:"),
        "no redaction warning when redact is off: {warnings}"
    );

    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];

    // The plaintext secrets reach the upstream unchanged (redaction did NOT run).
    let system = upstream
        .iter()
        .find(|m| matches!(m, Message::System { .. }))
        .unwrap();
    assert_eq!(
        text_of(system).unwrap(),
        format!("Internal config: ANTHROPIC_API_KEY={SECRET_KEY}"),
        "system block unchanged when redact is off"
    );
    let user = upstream
        .iter()
        .find(|m| matches!(m, Message::User { .. }))
        .unwrap();
    assert_eq!(
        text_of(user).unwrap(),
        format!("My email is {USER_EMAIL}, please help.")
    );
}

/// A route with NO redact flag at all (default `RouteAction`) must also leave
/// the request unchanged — proving OFF BY DEFAULT at the type level.
#[tokio::test]
async fn no_redact_flag_means_no_redaction() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        seen_messages: Arc::clone(&seen),
        cache_min_tokens: None,
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
            name: "plain".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["rec-model".into()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: "rec-model".into(),
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

    let msgs = seen.lock().unwrap();
    let all_upstream_text: String = msgs[0].iter().filter_map(text_of).collect();
    assert!(
        all_upstream_text.contains(SECRET_KEY),
        "default RouteAction must not redact"
    );
}

/// (e) A cache HIT serves the cached response without re-running the redaction
/// pass: the provider is dispatched exactly once (the miss), and the second
/// identical request is served from L1.
#[tokio::test]
async fn cache_hit_does_not_re_run_redaction() {
    let (app, key, seen) = app_with_redact_route(true).await;

    // First request: a cache MISS — dispatches to the provider (which redacts).
    let resp1 = app
        .clone()
        .oneshot(request_with_key(false, &key))
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    // Second identical request: served from L1 — the provider is NOT hit again,
    // so the redaction pass does not run on the hit path.
    let resp2 = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    assert_eq!(
        resp2.headers().get("x-tokentrimmer-cache").unwrap(),
        "hit-l1",
        "second identical request must be served from L1"
    );

    // The provider saw exactly one upstream request — the redaction pass ran
    // only on the miss, never on the hit.
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "redaction must not re-run on a cache hit (provider hit once)"
    );
}

/// NO PHANTOM BUST: on a cache-capable model (min present) the pii_request is
/// multi-turn (assistant present) and comfortably over the tiny minimum, so
/// the WHOLE message list is the cache-stable prefix — and the system secret
/// redaction fires INSIDE it. But redaction is DETERMINISTIC on the ingress
/// bytes (fixed regexes, fixed `[REDACTED]` placeholder): the dispatched
/// prefix is byte-identical on every request/turn, the provider's
/// exact-prefix cache keeps hitting, and NO bust occurs. Booking one anyway
/// would wipe the savings headline of every redact+cache-capable customer on
/// every turn for a cost no provider invoice would ever show (the phantom
/// recurring penalty this lane's review caught). So:
/// `x-tokentrimmer-cache-bust-usd` reads 0.000000, no `cache_bust:` token is
/// emitted, and the redaction itself still proceeds — safety beats cost.
#[tokio::test]
async fn deterministic_redaction_in_stable_prefix_books_no_bust() {
    let (app, key, seen) = app_with_redact_route_and_cache_min(true, Some(8)).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The redaction still fired — the secret never reaches the upstream.
    let msgs = seen.lock().unwrap();
    let all_upstream_text: String = msgs[0].iter().filter_map(text_of).collect();
    assert!(!all_upstream_text.contains(SECRET_KEY));

    // No bust is booked: deterministic egress ⇒ the provider cache still hits.
    assert_eq!(
        resp.headers()["x-tokentrimmer-cache-bust-usd"]
            .to_str()
            .unwrap(),
        "0.000000",
        "deterministic redaction must never book a cache bust"
    );

    let warnings = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .expect("warnings header present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        !warnings.contains("cache_bust:"),
        "no bust token for a deterministic mutation: {warnings}"
    );
    assert!(
        warnings.contains("redacted:system"),
        "the redaction class tokens are unchanged: {warnings}"
    );

    // The headline is NOT suppressed by a phantom penalty: with baseline ==
    // cost (same model, no cache, no compression) it is exactly 0 because
    // there is nothing saved — not because a penalty wiped it.
    let saved: f64 = resp.headers()["x-tokentrimmer-saved-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(saved, 0.0);

    // And the realized cost figure is untouched (estimates are never folded
    // into the invoice-reconcilable cost).
    let cost: f64 = resp.headers()["x-tokentrimmer-cost-usd"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let expected_cost = (PROMPT_TOKENS as f64) * INPUT_PER_M / 1e6
        + (COMPLETION_TOKENS as f64) * OUTPUT_PER_M / 1e6;
    assert!(
        (cost - expected_cost).abs() < 1e-9,
        "cost ({cost}) must stay the realized usage cost ({expected_cost})"
    );
}

/// Outside the cache regime (no `prompt_cache_min_tokens`) the same redaction
/// books NOTHING: the bust header reads 0.000000 and no `cache_bust:` token
/// appears — there was no warm prefix to bust.
#[tokio::test]
async fn redaction_outside_cache_regime_books_zero() {
    let (app, key, _seen) = app_with_redact_route(true).await;

    let resp = app.oneshot(request_with_key(false, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        resp.headers()["x-tokentrimmer-cache-bust-usd"]
            .to_str()
            .unwrap(),
        "0.000000",
        "no cache regime, no bust"
    );

    let warnings = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    assert!(
        !warnings.contains("cache_bust:"),
        "no bust token outside the cache regime: {warnings}"
    );
    // The redaction class tokens still fire as before.
    assert!(warnings.contains("redacted:system"), "{warnings}");
}
