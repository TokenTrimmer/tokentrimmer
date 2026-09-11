//! S02: a STRICT structured-output request (`response_format: json_schema`
//! with `strict: true`) must not be rewritten onto a model that only promises
//! the loose `json_object` shape. The dispatch-time downgrade warning fires
//! after a rewrite committed — silently changing the caller's output contract
//! — so the capability guard suppresses the route target at selection
//! instead, while the S01 invariant (privacy effects retained) holds.
//!
//! Hermetic: a recording provider whose catalog distinguishes strict-capable
//! from json_object-only models; a routing store with one rewrite route.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use tower::ServiceExt;
use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore,
};
use tt_cache::memory::InMemoryL1Cache;
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{CachingRoutingStore, Route, RouteAction, RoutingStore};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

const EMAIL: &str = "jane.doe@example.com";

/// One provider exposing three models:
/// - `source` — the caller's model (no strict schema cap listed);
/// - `json-object-only` — JsonMode but NOT StrictJsonSchema;
/// - `strict-capable` — JsonMode + StrictJsonSchema.
#[derive(Default)]
struct RecordingProvider(Mutex<Vec<ChatCompletionRequest>>);

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "s02-test"
    }
    fn models(&self) -> Vec<ModelInfo> {
        [
            ("source", false),
            ("json-object-only", false),
            ("strict-capable", true),
        ]
        .into_iter()
        .map(|(id, strict)| ModelInfo {
            id: id.into(),
            provider: self.id().into(),
            capabilities: if strict {
                vec![
                    Capability::Text,
                    Capability::JsonMode,
                    Capability::StrictJsonSchema,
                ]
            } else if id == "json-object-only" {
                vec![Capability::Text, Capability::JsonMode]
            } else {
                vec![Capability::Text]
            },
            max_input_tokens: 128_000,
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
            "id": "s02-test", "object": "chat.completion", "created": 0, "model": req.model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
        }))
        .unwrap())
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
        _: EmbeddingsRequest,
        _: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        panic!("embedding dispatch must not run in the S02 chat tests")
    }
}

#[derive(Debug)]
struct FixedStore(Route);
#[async_trait]
impl RoutingStore for FixedStore {
    async fn list_for_org(&self, _: Uuid) -> Result<Vec<Route>, tt_routing::RoutingStoreError> {
        Ok(vec![self.0.clone()])
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    provider: Arc<RecordingProvider>,
}

impl Harness {
    async fn new(target_model: &str, redact: bool) -> Self {
        let org = Uuid::now_v7();
        let keys = Arc::new(InMemoryKeyStore::new());
        let key = issue(
            keys.as_ref(),
            &InMemoryAuditWriter::new(),
            org,
            "s02",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap()
        .plaintext;
        let provider = Arc::new(RecordingProvider::default());
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let routing = Arc::new(CachingRoutingStore::with_ttl(
            Arc::new(FixedStore(Route {
                id: Uuid::now_v7(),
                name: "targeted".into(),
                priority: 1,
                enabled: true,
                paused: false,
                when: Default::default(),
                then: RouteAction {
                    target_model: Some(target_model.into()),
                    redact,
                    disable_cache: redact,
                    ..Default::default()
                },
            })),
            Duration::ZERO,
        ));
        let cache = Arc::new(InMemoryL1Cache::new());
        let app = build_router(
            AppState::new(registry)
                .with_key_store(keys)
                .with_routing_store(routing)
                .with_l1(cache, None),
        );
        Self { app, key, provider }
    }

    /// A strict-schema request: json_schema with strict:true over the source
    /// model. The caller's email rides along so the retained privacy effect is
    /// assertable in the dispatch payload.
    fn strict_schema_request(&self) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.key))
            .body(Body::from(
                json!({
                    "model": "source",
                    "messages": [{"role": "user", "content": format!("My email is {EMAIL}; total?")}],
                    "response_format": {
                        "type": "json_schema",
                        "json_schema": {
                            "name": "receipt",
                            "strict": true,
                            "schema": {
                                "type": "object",
                                "properties": {"total": {"type": "number"}},
                                "required": ["total"],
                                "additionalProperties": false
                            }
                        }
                    }
                })
                .to_string(),
            ))
            .unwrap()
    }
}

#[tokio::test]
async fn strict_schema_route_to_json_object_only_target_is_suppressed_with_privacy_retained() {
    let h = Harness::new("json-object-only", true).await;
    let response = h
        .app
        .clone()
        .oneshot(h.strict_schema_request())
        .await
        .unwrap();
    // The request still succeeds — on the CALLER's model with the route's
    // privacy effects (S01 suppression semantics), not the json_object-only
    // rewrite. The output contract is never silently weakened.
    assert_eq!(response.status(), StatusCode::OK);

    let dispatched = h.provider.0.lock().unwrap();
    assert_eq!(
        dispatched.len(),
        1,
        "exactly one dispatch (suppressed route falls back to the caller model)"
    );
    let req = &dispatched[0];
    assert_eq!(
        req.model, "source",
        "the json_object-only target must NOT receive the rewrite"
    );
    // The S01 invariant: privacy effects survive suppression.
    let body = serde_json::to_value(serde_json::to_value(&req.messages).unwrap_or_default())
        .unwrap_or_default();
    let messages = serde_json::to_string(&body).unwrap_or_default();
    assert!(
        !messages.contains(EMAIL),
        "the retained redaction must still strip the caller email: {messages}"
    );
    // The strict response_format must survive the suppression unchanged —
    // the caller's own model keeps the caller's own contract.
    assert_eq!(
        req.response_format.as_ref().unwrap().r#type,
        "json_schema",
        "the caller's strict schema must not be silently downgraded on its own model"
    );
    assert!(
        req.response_format.as_ref().unwrap().json_schema.is_some(),
        "the schema body must be retained"
    );
}

#[tokio::test]
async fn strict_schema_route_to_strict_capable_target_rewrites() {
    let h = Harness::new("strict-capable", true).await;
    let response = h
        .app
        .clone()
        .oneshot(h.strict_schema_request())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let dispatched = h.provider.0.lock().unwrap();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(
        dispatched[0].model, "strict-capable",
        "a strict-capable target receives the rewrite (and its privacy effects)"
    );
    assert_eq!(
        dispatched[0].response_format.as_ref().unwrap().r#type,
        "json_schema",
        "the strict schema is forwarded intact to the strict-capable target"
    );
}

#[tokio::test]
async fn forced_strict_route_to_json_object_only_target_is_refused() {
    // A forced route emulating an operator pin: a suppressed target must
    // refuse before I/O (the existing forced-route semantics for a target
    // that cannot honor the request contract), not silently downgrade.
    let h = Harness::new("json-object-only", true).await;
    let key = h.key.clone();
    let body = json!({
        "model": "source",
        "messages": [{"role": "user", "content": format!("My email is {EMAIL}; total?")}],
        "response_format": {"type": "json_schema", "json_schema": {
            "name": "receipt", "strict": true,
            "schema": {"type": "object", "properties": {"total": {"type": "number"}}}
        }}
    });
    let forced = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-tokentrimmer-route", "targeted")
        .header("authorization", format!("Bearer {key}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = h.app.clone().oneshot(forced).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a forced strict-schema route onto a json_object-only target must be refused before I/O"
    );
    let dispatched = h.provider.0.lock().unwrap();
    assert!(
        dispatched.is_empty(),
        "no dispatch may occur for the refused forced route"
    );
}

#[tokio::test]
async fn non_strict_schema_route_to_json_object_only_target_still_rewrites() {
    // The loose json_schema shape (no strict:true) keeps tolerating the
    // json_object downgrade path: the selection guard only demands
    // StrictJsonSchema for grammar-locked contracts.
    let h = Harness::new("json-object-only", false).await;
    let key = h.key.clone();
    let body = json!({
        "model": "source",
        "messages": [{"role": "user", "content": "total?"}],
        "response_format": {"type": "json_schema", "json_schema": {
            "name": "receipt",
            "schema": {"type": "object", "properties": {"total": {"type": "number"}}}
        }}
    });
    let response = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let dispatched = h.provider.0.lock().unwrap();
    assert_eq!(
        dispatched.len(),
        1,
        "a non-strict schema routes normally to the json_object-capable target"
    );
    assert_eq!(dispatched[0].model, "json-object-only");
}
