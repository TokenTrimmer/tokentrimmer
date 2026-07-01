//! End-to-end (hermetic) tests for the CACHE-SPAN INVARIANT + token-true gate
//! through the chat handler: the compression pipeline must be structurally
//! unable to mutate the cache-stable prefix of a request served by a
//! cache-capable model (`prompt_cache_min_tokens` present), while staying
//! byte-for-byte on its pre-lane behavior for non-cache-capable models.
//!
//! (1) cache-qualified MULTI-TURN request through a compress route → the
//!     upstream request is byte-identical to ingress (the whole conversation
//!     is next turn's cached prefix) and the compression saving books $0;
//! (2) cache-qualified SINGLE-SHOT request on a positional-auto-cache
//!     provider (OpenAI-style — every non-Anthropic provider with a cache
//!     minimum) → the WHOLE prompt is inside the auto-cached prefix, so the
//!     upstream request is byte-identical and the saving books $0;
//! (3) a single-shot BELOW the cache minimum → nothing can cache, so trims
//!     apply everywhere and book a genuine saving;
//! (4) the same requests against a provider with NO `prompt_cache_min_tokens`
//!     → trims everywhere (the pre-lane behavior, pinned).
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

const INPUT_PER_M: f64 = 10.0;
const OUTPUT_PER_M: f64 = 30.0;
const PROMPT_TOKENS: u64 = 1000;
const COMPLETION_TOKENS: u64 = 200;

/// Mock provider that records the upstream `messages` and whose pricing
/// optionally carries a `prompt_cache_min_tokens` — the cache-capability
/// switch the split keys on.
struct RecordingProvider {
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
    /// `Some(min)` = cache-capable (the split protects the stable prefix);
    /// `None` = not cache-capable (all-volatile, pre-lane behavior).
    cache_min_tokens: Option<u32>,
}

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
            cached_input_per_million: Some(1.0),
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
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
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

/// Gateway with a compress route on `rec-model`, served by a provider whose
/// cache capability is `cache_min_tokens`.
async fn app_with_compress_route(
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
            paused: false,
            id: Uuid::now_v7(),
            name: "compress-route".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                model_in: vec!["rec-model".into()],
                ..Default::default()
            },
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
                compress: true,
                doc_compaction: false,
                document_lane: false,
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

/// A MULTI-TURN request (assistant message present) with redundant whitespace
/// in the system, a duplicated tool result, and padded tool-call JSON — every
/// trim the compression pass knows how to make.
fn multi_turn_body() -> serde_json::Value {
    json!({
        "model": "rec-model",
        "messages": [
            { "role": "system", "content": "You are concise.   \n\n\n\n\nFollow the rules.   " },
            { "role": "user", "content": "Please summarize the report." },
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
    })
}

/// Dirty single-shot system prefix (comfortably over a min=8 qualification
/// under the BPE estimator) — bytes inside the provider's auto-cached prefix.
const SINGLE_SHOT_SYSTEM: &str =
    "You are a very concise assistant.   \n\n\n\n\nFollow every rule carefully.   ";

/// A SINGLE-SHOT request (no assistant message): long dirty system + dirty
/// tool content in the volatile tail.
fn single_shot_body() -> serde_json::Value {
    json!({
        "model": "rec-model",
        "messages": [
            { "role": "system", "content": SINGLE_SHOT_SYSTEM },
            { "role": "user", "content": "Summarize the data." },
            { "role": "tool", "tool_call_id": "call_y", "content": "RESULT   \n\n\n\n\nDATA" }
        ]
    })
}

fn request_with_key(body: &serde_json::Value, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn header_f64(resp: &axum::response::Response, name: &str) -> f64 {
    resp.headers()[name]
        .to_str()
        .unwrap()
        .parse::<f64>()
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

/// (1) Cache-qualified multi-turn: the ENTIRE message list is the stable
/// prefix, so the compression pipeline structurally no-ops — the upstream sees
/// the ingress bytes unchanged and the compression saving is exactly $0.
#[tokio::test]
async fn multi_turn_cache_qualified_is_byte_identical_and_books_zero() {
    let (app, key, seen) = app_with_compress_route(Some(8)).await;
    let body = multi_turn_body();

    let resp = app.oneshot(request_with_key(&body, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Upstream messages are byte-identical to the ingress messages.
    let ingress: Vec<Message> =
        serde_json::from_value(body["messages"].clone()).expect("ingress messages parse");
    let msgs = seen.lock().unwrap();
    assert_eq!(
        serde_json::to_string(&msgs[0]).unwrap(),
        serde_json::to_string(&ingress).unwrap(),
        "cache-qualified multi-turn request must reach the upstream unchanged"
    );

    // And the ledger books zero — no saving is claimed for a no-op.
    assert_eq!(
        header_f64(&resp, "x-tokentrimmer-compression-saved-usd"),
        0.0,
        "a structural no-op must book $0"
    );
    assert_eq!(header_f64(&resp, "x-tokentrimmer-saved-usd"), 0.0);
}

/// (2) Cache-qualified single-shot on a positional-auto-cache provider: the
/// WHOLE prompt (system + user + tool) is inside the provider's auto-cached
/// prefix, so nothing is trimmable — the upstream sees the ingress bytes
/// unchanged and the saving books exactly $0. (Trimming "just the tail" would
/// dispatch bytes inside the auto-cached region; a continued conversation
/// would then dispatch the RAW history, diverge from the cached trimmed
/// bytes, and forfeit the ~0.1x cache read on the whole region — a repricing
/// no whitespace trim buys back.)
#[tokio::test]
async fn single_shot_auto_cached_is_byte_identical_and_books_zero() {
    let (app, key, seen) = app_with_compress_route(Some(8)).await;
    let body = single_shot_body();

    let resp = app.oneshot(request_with_key(&body, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let ingress: Vec<Message> =
        serde_json::from_value(body["messages"].clone()).expect("ingress messages parse");
    let msgs = seen.lock().unwrap();
    assert_eq!(
        serde_json::to_string(&msgs[0]).unwrap(),
        serde_json::to_string(&ingress).unwrap(),
        "a cache-qualified single-shot on an auto-cache provider must reach \
         the upstream unchanged (dirty system AND dirty tool tail included)"
    );

    assert_eq!(
        header_f64(&resp, "x-tokentrimmer-compression-saved-usd"),
        0.0,
        "a structural no-op must book $0"
    );
}

/// (3) A single-shot BELOW the cache minimum on the same provider: nothing
/// can cache, so the pre-lane trims apply everywhere — system trimmed, tool
/// trimmed — and the saving is genuine (> 0).
#[tokio::test]
async fn single_shot_below_min_trims_everywhere() {
    // Minimum far above the prompt size — cache-capable model, unqualified
    // request.
    let (app, key, seen) = app_with_compress_route(Some(100_000)).await;

    let resp = app
        .oneshot(request_with_key(&single_shot_body(), &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];

    let system = upstream
        .iter()
        .find(|m| matches!(m, Message::System { .. }))
        .unwrap();
    assert_eq!(
        text_of(system).unwrap(),
        "You are a very concise assistant.\n\nFollow every rule carefully.",
        "below the minimum nothing caches — the system block is trimmed"
    );

    let tool = upstream
        .iter()
        .find(|m| matches!(m, Message::Tool { .. }))
        .unwrap();
    assert_eq!(text_of(tool).unwrap(), "RESULT\n\nDATA");

    let saved = header_f64(&resp, "x-tokentrimmer-compression-saved-usd");
    assert!(saved > 0.0, "below-min trim books a saving, got {saved}");
}

/// (4) No `prompt_cache_min_tokens` → not cache-capable → all-volatile: the
/// exact pre-lane behavior (system trimmed, duplicate dropped) is pinned.
#[tokio::test]
async fn non_cache_capable_provider_trims_everywhere() {
    let (app, key, seen) = app_with_compress_route(None).await;

    let resp = app
        .oneshot(request_with_key(&multi_turn_body(), &key))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let msgs = seen.lock().unwrap();
    let upstream = &msgs[0];

    // Duplicate adjacent tool result dropped: 5 ingress messages → 4 upstream.
    assert_eq!(upstream.len(), 4, "duplicate tool block must be dropped");

    // System trimmed (all-volatile path).
    let system = upstream
        .iter()
        .find(|m| matches!(m, Message::System { .. }))
        .unwrap();
    assert_eq!(
        text_of(system).unwrap(),
        "You are concise.\n\nFollow the rules.",
        "pre-lane behavior: the system block is trimmed when nothing can cache"
    );

    let saved = header_f64(&resp, "x-tokentrimmer-compression-saved-usd");
    assert!(saved > 0.0, "all-volatile trim books its saving");
}
