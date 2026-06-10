//! End-to-end tests for the API key auth middleware.
//!
//! Covers:
//! 1. No Bearer header → request still serves (no auth wired path).
//! 2. `tt_test_*` Bearer → sandbox short-circuit fires regardless of key store.
//! 3. `tt_live_*` Bearer with no key store → request passes through.
//! 4. `tt_live_*` Bearer with valid key store entry → request reaches handler
//!    with `ApiKeyContext` populated and the credential store supplies the
//!    upstream key the provider receives.
//! 5. `tt_live_*` Bearer with key store but no matching key → 401.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, InMemoryProviderCredentialStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::context::{ProviderCredentials, SecretString};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

/// Provider that records the api_key it was handed on every request, so tests
/// can assert credential substitution happened.
struct RecordingProvider {
    last_api_key: Arc<Mutex<Option<String>>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "rec-1".into(),
            provider: "recording".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
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
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        *self.last_api_key.lock().unwrap() = Some(ctx.credentials.api_key.expose().to_string());
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
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
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

fn chat_request(model: &str, bearer: Option<&str>) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
        "stream": false,
    });
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(b_val) = bearer {
        b = b.header("authorization", format!("Bearer {b_val}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn build_app_with_state(state: AppState) -> axum::Router {
    build_router(state)
}

#[tokio::test]
async fn missing_bearer_passes_through() {
    let calls = Arc::new(AtomicUsize::new(0));
    let last_key = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        last_api_key: Arc::clone(&last_key),
        calls: Arc::clone(&calls),
    }));
    let app = build_app_with_state(AppState::new(registry));

    let resp = app.oneshot(chat_request("rec-1", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    // No Bearer → provider receives an empty api_key, which is the legacy
    // synthetic-ctx behavior.
    assert_eq!(last_key.lock().unwrap().as_deref(), Some(""));
}

#[tokio::test]
async fn tt_test_bypasses_verify_and_credential_substitution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let last_key = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        last_api_key: Arc::clone(&last_key),
        calls: Arc::clone(&calls),
    }));
    // Configure key store and credential store — sandbox path must still
    // short-circuit before either is consulted.
    let key_store: Arc<dyn KeyStore> = Arc::new(InMemoryKeyStore::new());
    let cred_store = Arc::new(InMemoryProviderCredentialStore::new());
    let app = build_app_with_state(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(cred_store),
    );

    let resp = app
        .oneshot(chat_request("rec-1", Some("tt_test_abc")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["x-tokentrimmer-provider"].to_str().unwrap(),
        "sandbox"
    );
    // Provider was never called — recording counter stays 0.
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn tt_live_with_no_key_store_passes_through() {
    let calls = Arc::new(AtomicUsize::new(0));
    let last_key = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        last_api_key: Arc::clone(&last_key),
        calls: Arc::clone(&calls),
    }));
    let app = build_app_with_state(AppState::new(registry));

    let resp = app
        .oneshot(chat_request("rec-1", Some("tt_live_devmode")))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    // No credential store either → provider receives the raw Bearer verbatim
    // (legacy fallback path).
    assert_eq!(last_key.lock().unwrap().as_deref(), Some("tt_live_devmode"));
}

#[tokio::test]
async fn tt_live_invalid_returns_401() {
    let calls = Arc::new(AtomicUsize::new(0));
    let last_key = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        last_api_key: Arc::clone(&last_key),
        calls: Arc::clone(&calls),
    }));
    let key_store: Arc<dyn KeyStore> = Arc::new(InMemoryKeyStore::new());
    let app = build_app_with_state(AppState::new(registry).with_key_store(key_store));

    let resp = app
        .oneshot(chat_request(
            "rec-1",
            // Long enough to pass the format check, but not in the store.
            Some("tt_live_ffffffffffffffffffffffffffffffff"),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

/// BYO-only (P0 #9): a VERIFIED org with a credential store configured but no
/// stored credential for the serving provider gets an actionable
/// `missing_provider_credential` 400 — the gateway must neither forward the
/// org's TokenTrimmer key upstream nor serve any operator key.
#[tokio::test]
async fn tt_live_valid_without_stored_credential_is_actionable_400() {
    let calls = Arc::new(AtomicUsize::new(0));
    let last_key = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        last_api_key: Arc::clone(&last_key),
        calls: Arc::clone(&calls),
    }));

    let raw_store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = Uuid::now_v7();
    let issued = issue(
        &raw_store,
        &audit,
        org_id,
        "byo-only",
        Environment::Live,
        Actor::System,
    )
    .await
    .expect("issue tt_live_ key");

    // Credential store configured but EMPTY for this org.
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);
    let app = build_app_with_state(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(Arc::new(InMemoryProviderCredentialStore::new())),
    );

    let resp = app
        .oneshot(chat_request("rec-1", Some(&issued.plaintext)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(
        v["error"]["code"], "missing_provider_credential",
        "error must tell the org to add its provider credential, got: {v}"
    );
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "provider must never be called"
    );
}

#[tokio::test]
async fn tt_live_valid_substitutes_upstream_credential() {
    let calls = Arc::new(AtomicUsize::new(0));
    let last_key = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        last_api_key: Arc::clone(&last_key),
        calls: Arc::clone(&calls),
    }));

    let raw_store = InMemoryKeyStore::new();
    let audit = InMemoryAuditWriter::new();
    let org_id = Uuid::now_v7();
    let issued = issue(
        &raw_store,
        &audit,
        org_id,
        "test-live-key",
        Environment::Live,
        Actor::System,
    )
    .await
    .expect("issue tt_live_ key");
    let plaintext = issued.plaintext;

    let cred_store = InMemoryProviderCredentialStore::new();
    cred_store.insert(
        org_id,
        "recording",
        ProviderCredentials {
            api_key: SecretString::new("upstream-sk-XYZ"),
            base_url: None,
            extra_headers: Vec::new(),
        },
    );

    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);
    let cred_store_arc = Arc::new(cred_store);
    let app = build_app_with_state(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(cred_store_arc),
    );

    let resp = app
        .oneshot(chat_request("rec-1", Some(&plaintext)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        last_key.lock().unwrap().as_deref(),
        Some("upstream-sk-XYZ"),
        "credential store must substitute the upstream key, not forward the TokenTrimmer key"
    );
}
