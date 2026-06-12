//! Isolation tests for the retrieval middleware: verifies that two different
//! organisations cannot see each other's retrieval data.
//!
//! The core property under test is:
//!   - Data stored under org A is only returned when a request is authenticated
//!     as org A.
//!   - A request authenticated as org B does NOT receive org A's chunks, even
//!     when both orgs store documents in a corpus with the same name.
//!
//! Each test builds a full Axum application (retrieval + auth + echo provider),
//! pre-inserts an `ApiKeyContext` into the request extensions so the retrieval
//! middleware can read the real org_id, and checks the echoed body to confirm
//! each org sees only its own data.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use httpmock::prelude::*;
use serde_json::json;
use tower::util::ServiceExt;
use uuid::Uuid;

use futures::stream::BoxStream;
use tt_auth::ApiKeyContext;
use tt_core::{build_router_with_retrieval, AppState, ProviderRegistry, RetrievalState};
use tt_retrieval::{
    embed::EmbeddingClient,
    store::{memory::MemoryStore, RetrievalStore},
    types::Chunk,
};
use tt_shared::{
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};

// ── Echo provider ─────────────────────────────────────────────────────────────
// Returns the last user message text as the assistant reply, so we can inspect
// exactly what the retrieval middleware injected into the body.

struct EchoProvider;

#[async_trait]
impl Provider for EchoProvider {
    fn id(&self) -> &'static str {
        "echo"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "echo-1".into(),
            provider: "echo".into(),
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
            effective_at: chrono::Utc::now(),
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let user_content = req
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m, tt_shared::messages::Message::User { .. }))
            .and_then(|m| {
                if let tt_shared::messages::Message::User { content, .. } = m {
                    match content {
                        tt_shared::messages::MessageContent::Text(t) => Some(t.clone()),
                        _ => None,
                    }
                } else {
                    None
                }
            })
            .unwrap_or_default();

        Ok(ChatCompletionResponse {
            id: "chatcmpl-echo".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(user_content)),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
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
        Err(ProviderError::Unsupported("n/a".into()))
    }
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("n/a".into()))
    }
}

/// Build the Axum router wired with an EchoProvider and the given RetrievalState.
fn app(retrieval: RetrievalState) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(EchoProvider));
    build_router_with_retrieval(AppState::new(registry), Some(retrieval))
}

/// Build a chat POST request that carries an `ApiKeyContext` extension so the
/// retrieval middleware uses the caller's real org_id.
///
/// The auth middleware does not overwrite extensions that are already set, so
/// pre-inserting the context here is sufficient to supply an org_id without
/// requiring a real key store or a live bearer token.
fn chat_request_with_auth(body: serde_json::Value, auth_ctx: ApiKeyContext) -> Request<Body> {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut().insert(auth_ctx);
    req
}

// ── Test: org A data is NOT visible to org B ──────────────────────────────────

/// Two orgs each pre-load a document into the same-named corpus.  A request
/// authenticated as org A must only surface org A's chunk and must never
/// include org B's chunk, and vice versa.
#[tokio::test]
async fn different_orgs_do_not_share_retrieval_data() {
    // ── Setup: shared mock embedding server ───────────────────────────────
    let emb_server = MockServer::start_async().await;
    let _m = emb_server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(json!({ "data": [{ "embedding": [1.0, 0.0] }] }));
        })
        .await;

    let store = Arc::new(MemoryStore::new());

    let org_a = Uuid::new_v4();
    let org_b = Uuid::new_v4();

    // Different org IDs must be distinct — guard against UUID collision.
    assert_ne!(
        org_a, org_b,
        "test precondition: org_a and org_b must differ"
    );

    // Insert chunk exclusively for org A.
    store
        .insert(Chunk {
            id: Uuid::new_v4(),
            org_id: org_a,
            corpus: "shared-corpus".into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: "ORG_A_EXCLUSIVE_CONTENT".into(),
            embedding: vec![1.0, 0.0],
            embedding_model: "text-embedding-3-small".into(),
            metadata: json!({}),
        })
        .await
        .unwrap();

    // Insert chunk exclusively for org B.
    store
        .insert(Chunk {
            id: Uuid::new_v4(),
            org_id: org_b,
            corpus: "shared-corpus".into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: "ORG_B_EXCLUSIVE_CONTENT".into(),
            embedding: vec![1.0, 0.0],
            embedding_model: "text-embedding-3-small".into(),
            metadata: json!({}),
        })
        .await
        .unwrap();

    let embedder = Arc::new(EmbeddingClient {
        api_key: "test-key".into(),
        base_url: emb_server.base_url(),
        model: "text-embedding-3-small".into(),
        http: reqwest::Client::new(),
    });

    let retrieval_state = RetrievalState {
        store: store as Arc<dyn RetrievalStore + Send + Sync>,
        embedder,
        audit: None,
    };
    let application = app(retrieval_state);

    let body = json!({
        "model": "echo-1",
        "messages": [{
            "role": "user",
            "content": "Context: <retrievable corpus=\"shared-corpus\" k=\"2\">placeholder</retrievable>"
        }]
    });

    // ── Request from org A ────────────────────────────────────────────────
    let resp_a = application
        .clone()
        .oneshot(chat_request_with_auth(
            body.clone(),
            ApiKeyContext {
                key_id: Uuid::new_v4(),
                org_id: org_a,
                tier: None,
            },
        ))
        .await
        .unwrap();

    assert_eq!(
        resp_a.status(),
        StatusCode::OK,
        "org_a request should succeed"
    );
    assert_eq!(
        resp_a
            .headers()
            .get("x-tokentrimmer-retrieval-enabled")
            .and_then(|v| v.to_str().ok()),
        Some("active"),
        "retrieval should be active for an authenticated org_a request"
    );

    let bytes_a = to_bytes(resp_a.into_body(), 8 * 1024).await.unwrap();
    let json_a: serde_json::Value = serde_json::from_slice(&bytes_a).unwrap();
    let text_a = json_a["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");

    assert!(
        text_a.contains("ORG_A_EXCLUSIVE_CONTENT"),
        "org A should see its own chunk; assistant content: {text_a}"
    );
    assert!(
        !text_a.contains("ORG_B_EXCLUSIVE_CONTENT"),
        "org A must NOT see org B's chunk; assistant content: {text_a}"
    );

    // ── Request from org B ────────────────────────────────────────────────
    let resp_b = application
        .oneshot(chat_request_with_auth(
            body,
            ApiKeyContext {
                key_id: Uuid::new_v4(),
                org_id: org_b,
                tier: None,
            },
        ))
        .await
        .unwrap();

    assert_eq!(
        resp_b.status(),
        StatusCode::OK,
        "org_b request should succeed"
    );
    assert_eq!(
        resp_b
            .headers()
            .get("x-tokentrimmer-retrieval-enabled")
            .and_then(|v| v.to_str().ok()),
        Some("active"),
        "retrieval should be active for an authenticated org_b request"
    );

    let bytes_b = to_bytes(resp_b.into_body(), 8 * 1024).await.unwrap();
    let json_b: serde_json::Value = serde_json::from_slice(&bytes_b).unwrap();
    let text_b = json_b["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");

    assert!(
        text_b.contains("ORG_B_EXCLUSIVE_CONTENT"),
        "org B should see its own chunk; assistant content: {text_b}"
    );
    assert!(
        !text_b.contains("ORG_A_EXCLUSIVE_CONTENT"),
        "org B must NOT see org A's chunk; assistant content: {text_b}"
    );
}
