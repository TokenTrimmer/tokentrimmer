//! End-to-end (hermetic) tests for the ALWAYS-ON cache classifier
//! (`CacheClassifierPass`): a volatile marker (UUID) inside a cache-qualified
//! system prefix surfaces a `cache_dynamic_prefix:<kind>` token in the
//! `x-tokentrimmer-warnings` header on a PLAIN request — no route opt-in
//! required (observability-only, default-on) — while a clean system prompt
//! and a non-cache-capable provider stay silent, and the request itself is
//! never mutated.

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
use tt_shared::messages::{Message, MessageContent};
use tt_shared::{messages::Choice, pricing::Capability};
use tt_shared::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

struct RecordingProvider {
    seen_messages: Arc<Mutex<Vec<Vec<Message>>>>,
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
            capabilities: vec![Capability::Text],
            max_input_tokens: 100_000,
            max_output_tokens: 8192,
        }]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
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
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
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

/// A plain gateway — NO routing store at all, so nothing is opted into any
/// route action. The classifier must still run (default-on diagnostics).
async fn plain_app(
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
    let audit = InMemoryAuditWriter::new();
    let plaintext = issue(
        &raw_store,
        &audit,
        org_id,
        "test-key",
        Environment::Live,
        Actor::System,
    )
    .await
    .expect("issue tt_live_ key")
    .plaintext;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let app = build_router(AppState::new(registry).with_key_store(key_store));
    (app, plaintext, seen)
}

const UUID_SYSTEM: &str =
    "Session 550e8400-e29b-41d4-a716-446655440000: you are a helpful, concise assistant.";
const CLEAN_SYSTEM: &str =
    "You are a helpful, concise assistant. Always answer in plain prose and cite sources.";

fn chat_request(system: &str, key: &str) -> Request<Body> {
    let body = json!({
        "model": "rec-model",
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": "What is the capital of France?" }
        ]
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn warnings_of(resp: &axum::response::Response) -> String {
    resp.headers()
        .get("x-tokentrimmer-warnings")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

/// A UUID inside a cache-qualified system prefix on a PLAIN request (no route
/// needed) surfaces `cache_dynamic_prefix:uuid` — and the request reaches the
/// upstream byte-identical (diagnostics only, never mutates).
#[tokio::test]
async fn uuid_in_cached_system_prefix_emits_warning_on_plain_request() {
    let (app, key, seen) = plain_app(Some(8)).await;

    let resp = app.oneshot(chat_request(UUID_SYSTEM, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let warnings = warnings_of(&resp);
    assert!(
        warnings.contains("cache_dynamic_prefix:uuid"),
        "default-on classifier must flag the volatile prefix: {warnings:?}"
    );

    // Diagnostics only: the upstream sees the system prompt unchanged.
    let msgs = seen.lock().unwrap();
    let Message::System { content } = &msgs[0][0] else {
        panic!("expected system message");
    };
    let MessageContent::Text(s) = content else {
        panic!("expected text content");
    };
    assert_eq!(
        s, UUID_SYSTEM,
        "the classifier must never mutate the request"
    );
}

/// A clean system prompt emits no `cache_dynamic_prefix` token.
#[tokio::test]
async fn clean_system_prefix_emits_no_warning() {
    let (app, key, _seen) = plain_app(Some(8)).await;

    let resp = app.oneshot(chat_request(CLEAN_SYSTEM, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !warnings_of(&resp).contains("cache_dynamic_prefix"),
        "clean prefix must stay silent: {:?}",
        warnings_of(&resp)
    );
}

/// A provider with no `prompt_cache_min_tokens` cannot cache, so there is no
/// busted cache to report — silent even with a UUID in the system prompt.
#[tokio::test]
async fn non_cache_capable_provider_emits_no_warning() {
    let (app, key, _seen) = plain_app(None).await;

    let resp = app.oneshot(chat_request(UUID_SYSTEM, &key)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !warnings_of(&resp).contains("cache_dynamic_prefix"),
        "nothing cacheable means nothing to report: {:?}",
        warnings_of(&resp)
    );
}
