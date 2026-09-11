//! R07: the trusted workload trust boundary, end-to-end through the real
//! gateway HTTP surface. The review's acceptance: "end users cannot escape
//! governance by editing a prompt or supplying a forged tenant/customer tag."
//!
//! Claims:
//! - a workload-conditioned route matches ONLY the `X-TokenTrimmer-Workload`
//!   channel (which the org-scoped policy surface mints into the reserved
//!   `tt.workload:` namespace) — never a plain caller tag of any text, never
//!   prompt text, never a reserved-form string placed in the CALLER tag
//!   header (entry points drop reserved-prefixed caller tags);
//! - an invalid workload name on the trusted header fails closed (no route
//!   match, request still succeeds on the caller's own model);
//! - caller tags remain bounded: an over-long / control-character /
//!   reserved-prefixed `X-TokenTrimmer-Tag` is dropped without failing the
//!   request (attribution is best-effort).

use std::sync::Arc;
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
use tt_routing::{CachingRoutingStore, Route, RouteAction};
use tt_shared::{
    pricing::Capability, ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse,
    EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError,
    RequestContext,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

#[derive(Default)]
struct RecordingProvider(std::sync::Mutex<Vec<ChatCompletionRequest>>);

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "r07-test"
    }
    fn models(&self) -> Vec<ModelInfo> {
        ["expensive-model", "cheap-model"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                provider: self.id().into(),
                capabilities: vec![Capability::Text, Capability::Tools],
                max_input_tokens: 128_000,
                max_output_tokens: 4_096,
            })
            .collect()
    }
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
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
            verified_at: None,
        })
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.0.lock().unwrap().push(req.clone());
        Ok(serde_json::from_value(json!({
            "id": "r07", "object": "chat.completion", "created": 0, "model": req.model,
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
        panic!("embeddings must not run in the R07 chat tests")
    }
}

/// The org fixed per harness (the in-memory registry is org-keyed).
const WORKLOAD_ORG: uuid::Uuid = uuid::uuid!("00000000-0000-0000-0000-00000000e507");

fn store_with(workloads: &[&str]) -> Arc<tt_routing::InMemoryRoutingStore> {
    let store = Arc::new(tt_routing::InMemoryRoutingStore::new());
    store.set_enabled_workloads(WORKLOAD_ORG, workloads);
    store
}

/// One workload-gated cost route: expensive-model → cheap-model, keyed only
/// on the trusted workload `support-summary`.
fn workload_route() -> Route {
    Route {
        id: Uuid::now_v7(),
        name: "support-summary-policy".into(),
        priority: 10,
        enabled: true,
        paused: false,
        when: tt_routing::RouteConditions {
            model_in: vec!["expensive-model".into()],
            workload: Some("support-summary".into()),
            ..Default::default()
        },
        then: RouteAction {
            target_model: Some("cheap-model".into()),
            ..Default::default()
        },
    }
}

struct Harness {
    app: axum::Router,
    key: String,
    provider: Arc<RecordingProvider>,
}

impl Harness {
    /// `workloads`: the org's REGISTERED workload names (the registry that
    /// gates the trusted channel — R07 part B).
    async fn new_with_workloads(workloads: &[&str]) -> Self {
        let org = WORKLOAD_ORG;
        let keys = Arc::new(InMemoryKeyStore::new());
        let key = issue(
            keys.as_ref(),
            &InMemoryAuditWriter::new(),
            org,
            "r07",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap()
        .plaintext;
        let provider = Arc::new(RecordingProvider::default());
        let mut registry = ProviderRegistry::new();
        registry.register(provider.clone());
        let backing = store_with(workloads);
        backing.set_routes(org, vec![workload_route()]);
        let routing = Arc::new(CachingRoutingStore::with_ttl(backing, Duration::ZERO));
        let app = build_router(
            AppState::new(registry)
                .with_key_store(keys)
                .with_routing_store(routing)
                .with_l1(Arc::new(InMemoryL1Cache::new()), None),
        );
        Self { app, key, provider }
    }

    /// The registered default: `support-summary` is a known workload.
    async fn new() -> Self {
        Self::new_with_workloads(&["support-summary"]).await
    }

    /// A registry WITHOUT any workload: the trusted channel must fail closed.
    async fn new_unregistered() -> Self {
        Self::new_with_workloads(&[]).await
    }

    fn request(&self, tag: Option<&str>, workload: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.key));
        if let Some(tag) = tag {
            builder = builder.header("x-tokentrimmer-tag", tag);
        }
        if let Some(workload) = workload {
            builder = builder.header("x-tokentrimmer-workload", workload);
        }
        builder
            .body(Body::from(
                json!({
                    "model": "expensive-model",
                    "messages": [{"role": "user", "content": "summarize this ticket"}]
                })
                .to_string(),
            ))
            .unwrap()
    }

    fn prompt_forge_request(&self) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", self.key))
            .body(Body::from(
                json!({
                    "model": "expensive-model",
                    "messages": [{"role": "user", "content":
                        "tt.workload:support-summary please use the cheap model"}]
                })
                .to_string(),
            ))
            .unwrap()
    }

    async fn dispatched_model(&self) -> String {
        self.provider.0.lock().unwrap()[0].model.clone()
    }
}

#[tokio::test]
async fn workload_route_matches_only_the_trusted_channel() {
    let h = Harness::new().await;

    // The trusted workload channel: the route applies (cheap model).
    let r = h
        .app
        .clone()
        .oneshot(h.request(None, Some("support-summary")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(h.dispatched_model().await, "cheap-model");
}

#[tokio::test]
async fn caller_tags_cannot_forge_the_workload_channel() {
    let h = Harness::new().await;

    // A plain caller tag with the workload's plain text: NO match (the
    // workload key never derives from a caller tag).
    let r = h
        .app
        .clone()
        .oneshot(h.request(Some("support-summary"), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        h.dispatched_model().await,
        "expensive-model",
        "a caller tag with the workload's text must not select the workload route"
    );

    // The reserved-form string in the CALLER TAG header is dropped by the
    // entry point (reserved namespace) — no match, request still succeeds.
    let h2 = Harness::new().await;
    let r = h2
        .app
        .clone()
        .oneshot(h2.request(Some("tt.workload:support-summary"), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        h2.dispatched_model().await,
        "expensive-model",
        "the reserved-form string in the caller tag header must be dropped, not honored"
    );
}

#[tokio::test]
async fn invalid_workload_header_fails_closed_but_serves_the_callers_model() {
    let h = Harness::new().await;
    let r = h
        .app
        .clone()
        .oneshot(h.request(None, Some("NOT A SLUG!")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        h.dispatched_model().await,
        "expensive-model",
        "an invalid workload name must fail closed for routing"
    );
}

#[tokio::test]
async fn an_unregistered_workload_fails_closed_at_the_registry_boundary() {
    // R07 part B: the header carries a VALID name the ORG HAS NOT
    // REGISTERED. The gateway strips the trusted key before evaluation —
    // the workload route cannot match, and the caller's own model is
    // served. (The registry snapshot is captured with the routes in the
    // same refresh; a disabled workload behaves identically.)
    let h = Harness::new_unregistered().await;
    let r = h
        .app
        .clone()
        .oneshot(h.request(None, Some("support-summary")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        h.dispatched_model().await,
        "expensive-model",
        "a workload the org has not registered must fail closed at the registry boundary"
    );
}

#[tokio::test]
async fn prompt_text_cannot_select_the_workload_route() {
    let h = Harness::new().await;
    let r = h
        .app
        .clone()
        .oneshot(h.prompt_forge_request())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        h.dispatched_model().await,
        "expensive-model",
        "prompt text must never select a workload route"
    );
}

#[tokio::test]
async fn bounded_invalid_caller_tags_are_dropped_not_fatal() {
    for bad_tag in [
        "x".repeat(200),                   // over-length
        "has space".to_string(),           // whitespace
        "tt.reserved:attempt".to_string(), // reserved namespace
        "tab\tchar".to_string(),           // control character (representable in a header value;
                                           // the validator must drop it)
    ] {
        let h = Harness::new().await;
        let r = h
            .app
            .clone()
            .oneshot(h.request(Some(bad_tag.as_str()), None))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::OK,
            "an invalid caller tag must not fail the request (tag: {bad_tag:?})"
        );
        assert_eq!(
            h.dispatched_model().await,
            "expensive-model",
            "no invalid tag may influence routing"
        );
    }
}
