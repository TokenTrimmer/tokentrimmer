//! End-to-end test for the `x-tokentrimmer-captured` response header (#451
//! gap-fill): present ONLY when the gateway handed the trace's body to the
//! per-org opt-in encrypted capture sink (writer armed + resolved org), and
//! absent on the default capture-off path. Covers both non-streaming and
//! streaming responses.

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
    InMemoryKeyStore, KeyStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use tt_telemetry::body_capture::{BodyCaptureError, BodyCaptureRecord, BodyCaptureWriter};
use uuid::Uuid;

/// Minimal provider that returns a fixed completion (and a one-chunk stream).
struct StubProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "gpt-4o-mini".into(),
            provider: "stub".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 4096,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.15,
            output_per_million: 0.6,
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
        Ok(ChatCompletionResponse {
            id: "chatcmpl-stub".into(),
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
                cache_read_input_tokens: None,
            },
        })
    }
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let chunk = ChatCompletionChunk {
            id: "chatcmpl-stub".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: req.model,
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            extra: Default::default(),
        };
        Ok(futures::stream::iter(vec![Ok(chunk)]).boxed())
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("no".into()))
    }
}

/// In-memory capture sink that records everything it is handed (acts like an
/// org that has opted in: it never no-ops on `enabled`).
#[derive(Default)]
struct RecordingCaptureWriter {
    records: Arc<Mutex<Vec<BodyCaptureRecord>>>,
}

#[async_trait]
impl BodyCaptureWriter for RecordingCaptureWriter {
    async fn record(&self, record: BodyCaptureRecord) -> Result<(), BodyCaptureError> {
        self.records.lock().unwrap().push(record);
        Ok(())
    }
}

fn registry() -> (ProviderRegistry, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(StubProvider {
        calls: Arc::clone(&calls),
    }));
    (registry, calls)
}

fn chat_request(stream: bool, bearer: &str) -> Request<Body> {
    let body = json!({
        "model": "gpt-4o-mini",
        "messages": [{ "role": "user", "content": "hello world" }],
        "stream": stream,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
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

#[tokio::test]
async fn captured_header_present_when_writer_armed_nonstreaming() {
    let (registry, _calls) = registry();
    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let writer = Arc::new(RecordingCaptureWriter::default());
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_body_capture_writer(writer.clone()),
    );

    let resp = app.oneshot(chat_request(false, &plaintext)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-captured")
            .map(|v| v.to_str().unwrap().to_string()),
        Some("true".to_string()),
        "header present when capture is armed for a resolved org"
    );
    // `spawn_body_capture` is fire-and-forget (detached `tokio::spawn`), so the
    // sink may not have observed the record by the time the response returns —
    // the header is the synchronous contract under test here. The full
    // writer-received-and-stored round-trip is covered by `body_capture_pg`.
    let _ = &writer;
}

#[tokio::test]
async fn captured_header_present_when_writer_armed_streaming() {
    let (registry, _calls) = registry();
    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let writer = Arc::new(RecordingCaptureWriter::default());
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_body_capture_writer(writer.clone()),
    );

    let resp = app.oneshot(chat_request(true, &plaintext)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-tokentrimmer-captured")
            .map(|v| v.to_str().unwrap().to_string()),
        Some("true".to_string()),
        "header present on the streaming response when capture is armed"
    );
}

#[tokio::test]
async fn captured_header_absent_when_capture_off_nonstreaming() {
    // No `with_body_capture_writer(...)` — the default path.
    let (registry, _calls) = registry();
    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let app = build_router(AppState::new(registry).with_key_store(key_store));

    let resp = app.oneshot(chat_request(false, &plaintext)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("x-tokentrimmer-captured").is_none(),
        "header MUST be absent on the default capture-off path"
    );
}

#[tokio::test]
async fn captured_header_absent_when_capture_off_streaming() {
    let (registry, _calls) = registry();
    let raw_store = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let plaintext = issue_key_for(&raw_store, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_store);

    let app = build_router(AppState::new(registry).with_key_store(key_store));

    let resp = app.oneshot(chat_request(true, &plaintext)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("x-tokentrimmer-captured").is_none(),
        "header MUST be absent on the default streaming capture-off path"
    );
}
