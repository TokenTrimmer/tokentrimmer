//! Integration tests for the retrieval middleware activation.
//!
//! Test 1: retrieval disabled (no state) — request forwarded unchanged,
//!         `x-tt-retrieval-enabled: disabled` header present.
//!
//! Test 2: retrieval enabled (injected MemoryStore + mock embedding server)
//!         with a request body containing `<retrievable ...>` — forwarded body
//!         has chunk text substituted in, response carries
//!         `x-tt-retrieval-tokens-saved` header.
//!
//! Test 3: retrieval enabled but no `<retrievable` tag present — forwarded
//!         unchanged, header `x-tt-retrieval-enabled: ready`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use httpmock::prelude::*;
use serde_json::json;
use tower::util::ServiceExt;
use uuid::Uuid;

use futures::stream::BoxStream;
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

// ── Simple pass-through provider ───────────────────────────────────────────
// Returns the first user message content as the assistant reply text.
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

fn app_with_echo(retrieval: Option<RetrievalState>) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(EchoProvider));
    build_router_with_retrieval(AppState::new(registry), retrieval)
}

// ── Test 1 ─────────────────────────────────────────────────────────────────

/// When no retrieval state is provided (disabled path), the request is forwarded
/// unchanged and the response carries `x-tt-retrieval-enabled: disabled`.
#[tokio::test]
async fn retrieval_disabled_sets_disabled_header() {
    let app = app_with_echo(None);

    let body = json!({
        "model": "echo-1",
        "messages": [{ "role": "user", "content": "hello world" }]
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-tt-retrieval-enabled")
            .and_then(|v| v.to_str().ok()),
        Some("disabled"),
        "disabled header must be present when retrieval is not configured"
    );
}

// ── Test 2 ─────────────────────────────────────────────────────────────────

/// When retrieval is enabled and the request body has a `<retrievable>` tag,
/// the middleware substitutes the tag with the retrieved chunk text and sets
/// `x-tt-retrieval-tokens-saved` in the response.
#[tokio::test]
async fn retrieval_enabled_substitutes_retrievable_tag() {
    // Start a mock embedding server.
    let emb_server = MockServer::start_async().await;
    let _m = emb_server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(json!({ "data": [{ "embedding": [1.0, 0.0] }] }));
        })
        .await;

    // Build MemoryStore and pre-insert a chunk for org_id = Uuid::nil().
    let store = Arc::new(MemoryStore::new());
    let org_id = Uuid::nil();
    store
        .insert(Chunk {
            id: Uuid::new_v4(),
            org_id,
            corpus: "my-docs".into(),
            doc_id: Uuid::new_v4(),
            chunk_idx: 0,
            text: "Retrieved chunk content here.".into(),
            embedding: vec![1.0, 0.0],
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

    let retrieval = RetrievalState {
        store: store as Arc<dyn RetrievalStore + Send + Sync>,
        embedder,
        audit: None,
    };
    let app = app_with_echo(Some(retrieval));

    let body = json!({
        "model": "echo-1",
        "messages": [{
            "role": "user",
            "content": "Please review: <retrievable corpus=\"my-docs\" k=\"1\">verbose payload the LLM never sees</retrievable> and summarize."
        }]
    });

    // Attach an ApiKeyContext so the middleware takes the authenticated path
    // (not the fail-closed unauthenticated path).  We use the same nil org_id
    // that was seeded into the MemoryStore above.
    let ctx = tt_auth::ApiKeyContext {
        key_id: Uuid::new_v4(),
        org_id,
    };

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .extension(ctx)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Response must carry tokens-saved header.
    assert!(
        resp.headers().contains_key("x-tt-retrieval-tokens-saved"),
        "x-tt-retrieval-tokens-saved header must be present on successful substitution"
    );
    assert_eq!(
        resp.headers()
            .get("x-tt-retrieval-enabled")
            .and_then(|v| v.to_str().ok()),
        Some("active"),
        "x-tt-retrieval-enabled must be 'active' when substitution ran"
    );
    assert_eq!(
        resp.headers()
            .get("x-tt-retrieval-substitutions")
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "x-tt-retrieval-substitutions must be '1'"
    );

    // The echo provider returns the (now-substituted) user content as the
    // assistant message. Verify the retrieved chunk text is present and
    // the original verbose payload is gone.
    let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let assistant_text = body_json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(
        assistant_text.contains("Retrieved chunk content here."),
        "assistant reply should contain the retrieved chunk; got: {assistant_text}"
    );
    assert!(
        !assistant_text.contains("verbose payload the LLM never sees"),
        "original tag payload should have been replaced; got: {assistant_text}"
    );
}

// ── Test 3 ─────────────────────────────────────────────────────────────────

/// When retrieval is enabled but the request has no `<retrievable` tag,
/// the request is forwarded unchanged with `x-tt-retrieval-enabled: ready`.
#[tokio::test]
async fn retrieval_enabled_no_tag_sets_ready_header() {
    let emb_server = MockServer::start_async().await;
    let store = Arc::new(MemoryStore::new());
    let embedder = Arc::new(EmbeddingClient {
        api_key: "k".into(),
        base_url: emb_server.base_url(),
        model: "text-embedding-3-small".into(),
        http: reqwest::Client::new(),
    });
    let retrieval = RetrievalState {
        store: store as Arc<dyn RetrievalStore + Send + Sync>,
        embedder,
        audit: None,
    };
    let app = app_with_echo(Some(retrieval));

    let body = json!({
        "model": "echo-1",
        "messages": [{ "role": "user", "content": "no tags here" }]
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-tt-retrieval-enabled")
            .and_then(|v| v.to_str().ok()),
        Some("ready"),
        "header must be 'ready' when enabled but no tag present"
    );
    assert!(
        !resp.headers().contains_key("x-tt-retrieval-tokens-saved"),
        "tokens-saved header must not be set when no substitution ran"
    );
}
