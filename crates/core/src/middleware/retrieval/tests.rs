//! Middleware alone must never initiate auxiliary I/O. Actual substitution and
//! privacy/isolation are exercised through the full gateway integration tests.
#![allow(clippy::await_holding_lock)]
use super::{DeferredRetrieval, RetrievalState};
use axum::{
    body::{Body, Bytes},
    extract::Extension,
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use httpmock::prelude::*;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tt_auth::ApiKeyContext;
use uuid::Uuid;

static ENV_LOCK: Mutex<()> = Mutex::new(());
const BODY: &str = r#"{"messages":[{"role":"user","content":"<retrievable corpus=\"test\">hello</retrievable>"}]}"#;

async fn echo(
    deferred: Option<Extension<DeferredRetrieval>>,
    body: Bytes,
) -> axum::response::Response {
    let mut response = body.into_response();
    if let Some(Extension(deferred)) = deferred {
        response.headers_mut().insert(
            "x-test-deferred-org",
            deferred.org_id().to_string().parse().unwrap(),
        );
    }
    response
}

async fn middleware_only(org: Option<Uuid>, allow_anon: bool) -> axum::response::Response {
    let server = MockServer::start_async().await;
    let embedding = server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(serde_json::json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    let state = RetrievalState {
        store: Arc::new(tt_retrieval::store::memory::MemoryStore::new()),
        audit: None,
        embedder: Arc::new(tt_retrieval::embed::EmbeddingClient {
            api_key: "test-key".into(),
            base_url: server.base_url(),
            model: "test".into(),
            http: reqwest::Client::new(),
        }),
    };
    let router = Router::new()
        .route("/v1/chat/completions", post(echo))
        .layer(axum::middleware::from_fn_with_state(
            state,
            super::maybe_substitute,
        ));
    let mut request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(org_id) = org {
        request = request.extension(ApiKeyContext {
            key_id: Uuid::now_v7(),
            org_id,
            tier: None,
            skip_shadow: false,
        });
    }
    let prior = std::env::var_os("TT_RETRIEVAL_ALLOW_ANON");
    if allow_anon {
        std::env::set_var("TT_RETRIEVAL_ALLOW_ANON", "1");
    } else {
        std::env::remove_var("TT_RETRIEVAL_ALLOW_ANON");
    }
    let response = router
        .oneshot(request.body(Body::from(BODY)).unwrap())
        .await;
    if let Some(prior) = prior {
        std::env::set_var("TT_RETRIEVAL_ALLOW_ANON", prior);
    } else {
        std::env::remove_var("TT_RETRIEVAL_ALLOW_ANON");
    }
    embedding.assert_calls_async(0).await;
    response.unwrap()
}

#[tokio::test]
async fn unauthenticated_retrieval_is_not_armed() {
    let _guard = ENV_LOCK.lock().unwrap();
    let response = middleware_only(None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-tokentrimmer-retrieval-skipped"],
        "no-auth"
    );
    assert!(!response.headers().contains_key("x-test-deferred-org"));
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), BODY.as_bytes());
}

#[tokio::test]
async fn authenticated_intent_preserves_identity_but_does_not_execute_in_middleware() {
    let _guard = ENV_LOCK.lock().unwrap();
    let org = Uuid::now_v7();
    let response = middleware_only(Some(org), false).await;
    assert_eq!(response.headers()["x-test-deferred-org"], org.to_string());
    assert_eq!(
        response.headers()["x-tokentrimmer-retrieval-skipped"],
        "policy-not-reached"
    );
    assert!(!response
        .headers()
        .contains_key("x-tokentrimmer-retrieval-enabled"));
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), BODY.as_bytes());
}

#[tokio::test]
async fn explicit_anonymous_dev_intent_is_also_deferred() {
    let _guard = ENV_LOCK.lock().unwrap();
    let response = middleware_only(None, true).await;
    assert_eq!(
        response.headers()["x-test-deferred-org"],
        Uuid::nil().to_string()
    );
    assert_eq!(
        response.headers()["x-tokentrimmer-retrieval-skipped"],
        "policy-not-reached"
    );
}

#[tokio::test]
async fn deferred_intent_cannot_cross_request_identity() {
    let server = MockServer::start_async().await;
    let embedding = server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(serde_json::json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    let state = RetrievalState {
        store: Arc::new(tt_retrieval::store::memory::MemoryStore::new()),
        audit: None,
        embedder: Arc::new(tt_retrieval::embed::EmbeddingClient {
            api_key: "test-key".into(),
            base_url: server.base_url(),
            model: "test".into(),
            http: reqwest::Client::new(),
        }),
    };
    let deferred = DeferredRetrieval::new(state, Uuid::now_v7());
    let mut request = tt_shared::ChatCompletionRequest::default();
    let result = deferred.apply(&mut request, Uuid::now_v7(), true).await;
    assert!(matches!(result, Err(crate::ApiError::Forbidden(_))));
    embedding.assert_calls_async(0).await;
}

#[tokio::test]
async fn disabled_path_forwards_body_unbuffered() {
    let big = vec![b'x'; (1 << 20) + 4096];
    let sent_len = big.len();
    let router = Router::new()
        .route("/v1/chat/completions", post(echo))
        .layer(axum::middleware::from_fn(super::maybe_substitute_disabled));
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .body(Body::from(big))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-tokentrimmer-retrieval-enabled"],
        "disabled"
    );
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .len(),
        sent_len
    );
}
