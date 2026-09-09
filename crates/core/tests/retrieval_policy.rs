//! S01: no retrieval embedding before policy resolution; queries and retrieved
//! context must respect the selected privacy policy across native ingresses.
use async_trait::async_trait;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use futures::stream::{BoxStream, StreamExt};
use httpmock::prelude::*;
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tower::ServiceExt;
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_core::{build_router_with_retrieval, AppState, ProviderRegistry, RetrievalState};
use tt_retrieval::{
    embed::EmbeddingClient,
    store::{memory::MemoryStore, RetrievalStore},
    types::Chunk,
};
use tt_routing::{
    CachingRoutingStore, Route, RouteAction, RouteConditions, RoutingStore, RoutingStoreError,
};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

const EMAIL: &str = "jane.doe@example.com";
const RETRIEVED_EMAIL: &str = "private.record@example.org";

#[derive(Default)]
struct RecordingProvider(
    Mutex<Vec<ChatCompletionRequest>>,
    Mutex<Vec<EmbeddingsRequest>>,
);
#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "retrieval-policy-test"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["source", "tiny"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: self.id().into(),
                capabilities: vec![Capability::Text, Capability::Streaming],
                max_input_tokens: if id == "tiny" { 1 } else { 128_000 },
                max_output_tokens: 4096,
            })
            .collect()
    }
    fn pricing(&self, _: &str) -> Option<ModelPricing> {
        None
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.0.lock().unwrap().push(req.clone());
        Ok(serde_json::from_value(json!({
            "id": "retrieval-policy-test", "object": "chat.completion", "created": 0, "model": req.model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        })).unwrap())
    }
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.0.lock().unwrap().push(req);
        Ok(futures::stream::empty().boxed())
    }
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        self.1.lock().unwrap().push(req.clone());
        Ok(serde_json::from_value(json!({
            "object": "list", "model": req.model, "data": [],
            "usage": {"prompt_tokens": 1, "completion_tokens": 0, "total_tokens": 1}
        }))
        .unwrap())
    }
}

#[derive(Debug)]
struct PolicyStore {
    fail: AtomicBool,
    route: Route,
}
#[async_trait]
impl RoutingStore for PolicyStore {
    async fn list_for_org(&self, _: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
        if self.fail.load(Ordering::SeqCst) {
            Err(RoutingStoreError::Backend("simulated outage".into()))
        } else {
            Ok(vec![self.route.clone()])
        }
    }
}
struct Harness {
    app: axum::Router,
    key: String,
    org: Uuid,
    provider: Arc<RecordingProvider>,
    backing: Arc<PolicyStore>,
    routing: Arc<CachingRoutingStore>,
}
impl Harness {
    async fn new(server: &MockServer, paused: bool, suppress_target: bool) -> Self {
        let org = Uuid::now_v7();
        let keys = Arc::new(InMemoryKeyStore::new());
        let key = issue(
            keys.as_ref(),
            &InMemoryAuditWriter::new(),
            org,
            "retrieval-safety",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap()
        .plaintext;
        let provider = Arc::new(RecordingProvider::default());
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let backing = Arc::new(PolicyStore {
            fail: AtomicBool::new(false),
            route: Route {
                id: Uuid::now_v7(),
                name: "protected".into(),
                priority: 1,
                enabled: true,
                paused,
                // This predicate vanishes after retrieval. Privacy must not
                // disappear because a transformation changed matching input.
                when: RouteConditions {
                    prompt_contains_any_of: vec!["guarded-payload".into()],
                    ..Default::default()
                },
                then: RouteAction {
                    redact: true,
                    disable_cache: true,
                    target_model: suppress_target.then(|| "tiny".into()),
                    ..Default::default()
                },
            },
        });
        let routing = Arc::new(CachingRoutingStore::with_ttl(
            backing.clone(),
            Duration::ZERO,
        ));
        let store = Arc::new(MemoryStore::new());
        store
            .insert(Chunk {
                id: Uuid::now_v7(),
                org_id: org,
                corpus: "docs".into(),
                doc_id: Uuid::now_v7(),
                chunk_idx: 0,
                text: format!("Retrieved {RETRIEVED_EMAIL}"),
                embedding: vec![1.0, 0.0],
                embedding_model: "test-embedding".into(),
                metadata: json!({}),
            })
            .await
            .unwrap();
        let retrieval = RetrievalState {
            store,
            audit: None,
            embedder: Arc::new(EmbeddingClient {
                api_key: "mock-key".into(),
                base_url: server.base_url(),
                model: "test-embedding".into(),
                http: reqwest::Client::new(),
            }),
        };
        let app = build_router_with_retrieval(
            AppState::new(registry)
                .with_key_store(keys)
                .with_routing_store(routing.clone()),
            Some(retrieval),
        );
        Self {
            app,
            key,
            org,
            provider,
            backing,
            routing,
        }
    }
    fn request(
        &self,
        messages_api: bool,
        stream: bool,
        forced: Option<&str>,
        fallback_query: bool,
    ) -> Request<Body> {
        let payload = format!(
            "guarded-payload {EMAIL} {}",
            "long private context ".repeat(20)
        );
        let outside = if fallback_query {
            String::new()
        } else {
            format!("Contact {EMAIL}: ")
        };
        let content =
            format!("{outside}<retrievable corpus=\"docs\" k=\"1\">{payload}</retrievable>");
        let mut builder = Request::builder()
            .method("POST")
            .uri(if messages_api {
                "/v1/messages"
            } else {
                "/v1/chat/completions"
            })
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.key));
        if let Some(route) = forced {
            builder = builder.header("x-tokentrimmer-route", route);
        }
        builder
            .body(Body::from(
                json!({ "model": "source", "max_tokens": 100, "stream": stream,
            "messages": [{"role": "user", "content": content}] })
                .to_string(),
            ))
            .unwrap()
    }
}

#[tokio::test]
async fn expired_policy_outage_blocks_embedding_and_primary_io_on_both_ingresses() {
    let server = MockServer::start_async().await;
    let embedding = server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    for messages_api in [false, true] {
        for stream in [false, true] {
            for forced in [None, Some("protected")] {
                let h = Harness::new(&server, false, false).await;
                assert_eq!(h.routing.engine_for(h.org).await.unwrap().routes().len(), 1);
                h.backing.fail.store(true, Ordering::SeqCst);
                let resp = h
                    .app
                    .clone()
                    .oneshot(h.request(messages_api, stream, forced, false))
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert!(h.provider.0.lock().unwrap().is_empty());
                embedding.assert_calls_async(0).await;
                assert_ne!(
                    resp.headers()
                        .get("x-tokentrimmer-retrieval-enabled")
                        .and_then(|v| v.to_str().ok()),
                    Some("active")
                );
            }
        }
    }
}

#[tokio::test]
async fn privacy_covers_embedding_queries_retrieved_content_and_original_policy_match() {
    let server = MockServer::start_async().await;
    let leak = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_includes(EMAIL);
            then.status(200)
                .json_body(json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    let safe = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_includes("[REDACTED]");
            then.status(200)
                .json_body(json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    let mut expected_calls = 0;
    for messages_api in [false, true] {
        for stream in [false, true] {
            for (paused, suppress_target) in [(false, false), (true, false), (false, true)] {
                for fallback_query in [false, true] {
                    let h = Harness::new(&server, paused, suppress_target).await;
                    let resp = h
                        .app
                        .clone()
                        .oneshot(h.request(messages_api, stream, None, fallback_query))
                        .await
                        .unwrap();
                    assert_eq!(resp.status(), StatusCode::OK);
                    leak.assert_calls_async(0).await;
                    expected_calls += 1;
                    safe.assert_calls_async(expected_calls).await;
                    assert_eq!(resp.headers()["x-tokentrimmer-route-matched"], "protected");
                    assert_eq!(resp.headers()["x-tokentrimmer-retrieval-enabled"], "active");
                    assert_eq!(
                        resp.headers()["x-tokentrimmer-retrieval-substitutions"],
                        "1"
                    );
                    let _ = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
                    let calls = h.provider.0.lock().unwrap();
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0].model, "source");
                    let wire = serde_json::to_string(&calls[0]).unwrap();
                    assert!(wire.contains("Retrieved"));
                    assert!(
                        !wire.contains("guarded-payload"),
                        "retrieval must actually replace the triggering payload"
                    );
                    assert!(!wire.contains(EMAIL));
                    assert!(!wire.contains(RETRIEVED_EMAIL));
                }
            }
        }
    }
}

#[tokio::test]
async fn direct_embedding_single_and_batch_inputs_retain_selected_redaction() {
    let server = MockServer::start_async().await;
    for (paused, suppress_target) in [(false, false), (true, false), (false, true)] {
        for input in [
            json!(format!("guarded-payload {EMAIL}")),
            json!([format!("guarded-payload {EMAIL}"), RETRIEVED_EMAIL]),
        ] {
            let h = Harness::new(&server, paused, suppress_target).await;
            let request = Request::builder()
                .method("POST")
                .uri("/v1/embeddings")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", h.key))
                .body(Body::from(
                    json!({"model": "source", "input": input}).to_string(),
                ))
                .unwrap();
            let response = h.app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let calls = h.provider.1.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].model, "source");
            let wire = serde_json::to_string(&calls[0]).unwrap();
            assert!(!wire.contains(EMAIL));
            assert!(!wire.contains(RETRIEVED_EMAIL));
            assert!(wire.contains("[REDACTED]"));
            assert_eq!(
                response.headers()["x-tokentrimmer-warnings"],
                "redacted:body"
            );
        }
    }
}

#[tokio::test]
async fn direct_embedding_policy_outage_blocks_provider_io() {
    let server = MockServer::start_async().await;
    let h = Harness::new(&server, false, false).await;
    h.routing.engine_for(h.org).await.unwrap();
    h.backing.fail.store(true, Ordering::SeqCst);
    let request = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(
            json!({"model": "source", "input": EMAIL}).to_string(),
        ))
        .unwrap();
    let response = h.app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(h.provider.1.lock().unwrap().is_empty());
}

fn custom_request(h: &Harness, messages: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", h.key))
        .body(Body::from(
            json!({"model": "source", "messages": messages}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn assistant_queries_and_newly_retrieved_assistant_content_are_guarded() {
    let server = MockServer::start_async().await;
    let safe = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_includes("[REDACTED]");
            then.status(200)
                .json_body(json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    let leak = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_includes(EMAIL);
            then.status(200)
                .json_body(json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    let h = Harness::new(&server, false, false).await;
    let content = format!(
        "Contact {EMAIL} <retrievable corpus=\"docs\">{}</retrievable>",
        "long payload ".repeat(40)
    );
    let response = h
        .app
        .clone()
        .oneshot(custom_request(
            &h,
            json!([
                {"role": "user", "content": "guarded-payload"},
                {"role": "assistant", "content": "Unchanged assistant history"},
                {"role": "assistant", "content": content}
            ]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    safe.assert_calls_async(1).await;
    leak.assert_calls_async(0).await;
    let calls = h.provider.0.lock().unwrap();
    let wire = serde_json::to_string(&calls[0]).unwrap();
    assert!(wire.contains("Unchanged assistant history"));
    assert!(wire.contains("Retrieved"));
    assert!(!wire.contains(EMAIL));
    assert!(!wire.contains(RETRIEVED_EMAIL));
}

#[tokio::test]
async fn partial_substitution_failure_preserves_original_messages_and_privacy() {
    let server = MockServer::start_async().await;
    let first = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_includes("FIRST")
                .body_includes("[REDACTED]");
            then.status(200)
                .json_body(json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    let second = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_includes("SECOND")
                .body_includes("[REDACTED]");
            then.status(503).body("simulated upstream failure");
        })
        .await;
    let h = Harness::new(&server, false, false).await;
    let content = |prefix: &str| {
        format!(
            "{prefix} {EMAIL} <retrievable corpus=\"docs\">guarded-payload {}</retrievable>",
            "long payload ".repeat(40)
        )
    };
    let response = h
        .app
        .clone()
        .oneshot(custom_request(
            &h,
            json!([
                {"role": "user", "content": content("FIRST")},
                {"role": "user", "content": content("SECOND")}
            ]),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-tokentrimmer-retrieval-error"],
        "embedding-error"
    );
    assert!(!response
        .headers()
        .contains_key("x-tokentrimmer-retrieval-substitutions"));
    first.assert_calls_async(1).await;
    second.assert_calls_async(1).await;
    let calls = h.provider.0.lock().unwrap();
    let wire = serde_json::to_string(&calls[0]).unwrap();
    assert!(wire.contains("FIRST"));
    assert!(wire.contains("SECOND"));
    assert!(wire.contains("guarded-payload"));
    assert!(
        !wire.contains("Retrieved"),
        "partial substitution must not escape"
    );
    assert!(!wire.contains(EMAIL));
}

#[tokio::test]
async fn invalid_forced_route_refuses_before_embedding() {
    let server = MockServer::start_async().await;
    let embedding = server
        .mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(json!({"data": [{"embedding": [1.0, 0.0]}]}));
        })
        .await;
    for name in ["missing", "protected"] {
        let h = Harness::new(&server, false, true).await;
        let resp = h
            .app
            .clone()
            .oneshot(h.request(false, false, Some(name), false))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        embedding.assert_calls_async(0).await;
        assert!(h.provider.0.lock().unwrap().is_empty());
    }
}
