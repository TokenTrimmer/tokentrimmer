//! Hermetic route-level safety regressions for Document Lane D4c.
//!
//! A document route may target a text-only model only after every inline media
//! part has been losslessly converted. These tests keep the sidecar, routing,
//! cross-provider credential, and panel paths in one place so a future change
//! cannot accidentally send raw documents to the text target (or its fallback)
//! when the optional sidecar is disabled or incomplete.

use std::{
    ffi::OsString,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use axum::{body::Body, http::Request};
use chrono::Utc;
use futures::stream::{BoxStream, StreamExt};
use httpmock::prelude::*;
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use tower::util::ServiceExt;

use tt_auth::{
    keys::{issue, Environment},
    InMemoryKeyStore, InMemoryProviderCredentialStore, KeyStore, ProviderCredentialStore,
};
use tt_core::{build_router, AppState, ProviderRegistry};
use tt_routing::{
    CachingRoutingStore, InMemoryRoutingStore, Route, RouteAction, RouteConditions, RoutePanel,
    RoutingStore,
};
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{Choice, Message, MessageContent},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext, Usage,
};
use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
use uuid::Uuid;

const SIDECAR_ENV: &str = "TT_DOC_SIDECAR_URL";
const SOURCE_MODEL: &str = "vision-source";
const TEXT_TARGET: &str = "text-target";
const TEXT_FALLBACK: &str = "text-fallback";
const ARBITER_MODEL: &str = "panel-arbiter";
const INLINE_PDF_B64: &str = "JVBERi0xLjQKJWRvY3VtZW50";

// Each integration-test file builds into its own test process. This lock
// serializes the tests below that change the process-local sidecar URL, and
// the RAII guard restores a user/developer value even when an assertion panics.
// Keeping the guard alive across `oneshot(...).await` is intentional: the
// gateway reads the environment during request preparation, not app creation.
static SIDECAR_ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

struct SidecarEnvGuard {
    _lock: MutexGuard<'static, ()>,
    prior: Option<OsString>,
}

impl SidecarEnvGuard {
    async fn set(url: Option<&str>) -> Self {
        let lock = SIDECAR_ENV_LOCK.lock().await;
        let prior = std::env::var_os(SIDECAR_ENV);
        match url {
            Some(url) => std::env::set_var(SIDECAR_ENV, url),
            None => std::env::remove_var(SIDECAR_ENV),
        }
        Self { _lock: lock, prior }
    }
}

impl Drop for SidecarEnvGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(value) => std::env::set_var(SIDECAR_ENV, value),
            None => std::env::remove_var(SIDECAR_ENV),
        }
    }
}

#[derive(Debug, Clone)]
struct Dispatch {
    provider: String,
    request: Value,
}

/// Records the exact gateway request received by one provider. The source
/// provider offers the raw-document/vision model; the text provider offers the
/// route target plus a fallback so fail-open tests catch either accidental use.
struct RecordingProvider {
    id: &'static str,
    models: &'static [&'static str],
    capabilities: Vec<Capability>,
    dispatches: Arc<Mutex<Vec<Dispatch>>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|model| ModelInfo {
                id: (*model).into(),
                provider: self.id.into(),
                capabilities: self.capabilities.clone(),
                max_input_tokens: 32_768,
                max_output_tokens: 4_096,
            })
            .collect()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.models
            .iter()
            .any(|candidate| *candidate == model)
            .then(|| ModelPricing {
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
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.dispatches.lock().unwrap().push(Dispatch {
            provider: self.id.into(),
            request: serde_json::to_value(&req).expect("record dispatched request"),
        });
        Ok(ChatCompletionResponse {
            id: "chatcmpl-document-lane".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("ok".into())),
                    tool_calls: Vec::new(),
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 20,
                completion_tokens: 5,
                total_tokens: 25,
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
        Ok(futures::stream::iter(Vec::new()).boxed())
    }

    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported("mock has no embeddings".into()))
    }
}

fn credentials(api_key: &str) -> ProviderCredentials {
    ProviderCredentials {
        api_key: SecretString::new(api_key.to_string()),
        base_url: None,
        extra_headers: Vec::new(),
    }
}

async fn issue_key_for(store: &InMemoryKeyStore, org_id: Uuid) -> String {
    let audit = InMemoryAuditWriter::new();
    issue(
        store,
        &audit,
        org_id,
        "document-lane-test-key",
        Environment::Live,
        Actor::System,
    )
    .await
    .expect("issue live key")
    .plaintext
}

struct Harness {
    app: axum::Router,
    key: String,
    source_dispatches: Arc<Mutex<Vec<Dispatch>>>,
    text_dispatches: Arc<Mutex<Vec<Dispatch>>>,
}

async fn app_with_route(route: Route) -> Harness {
    let source_dispatches = Arc::new(Mutex::new(Vec::new()));
    let text_dispatches = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        id: "source-provider",
        models: &[SOURCE_MODEL, ARBITER_MODEL],
        capabilities: vec![Capability::Text, Capability::Vision],
        dispatches: Arc::clone(&source_dispatches),
    }));
    registry.register(Arc::new(RecordingProvider {
        id: "text-provider",
        models: &[TEXT_TARGET, TEXT_FALLBACK],
        capabilities: vec![Capability::Text],
        dispatches: Arc::clone(&text_dispatches),
    }));

    let raw_keys = InMemoryKeyStore::new();
    let org_id = Uuid::now_v7();
    let key = issue_key_for(&raw_keys, org_id).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw_keys);

    // The target has a real, distinct provider credential. If the safety guard
    // regresses, the test therefore reaches the target and fails by observation
    // instead of being masked by a missing-credential 400.
    let credential_store = InMemoryProviderCredentialStore::new();
    credential_store.insert(org_id, "source-provider", credentials("source-secret"));
    credential_store.insert(org_id, "text-provider", credentials("text-secret"));
    let credential_store: Arc<dyn ProviderCredentialStore> = Arc::new(credential_store);

    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(org_id, vec![route]);
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));

    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_credential_store(credential_store)
            .with_routing_store(routing)
            .with_panel_enabled(true),
    );
    Harness {
        app,
        key,
        source_dispatches,
        text_dispatches,
    }
}

fn lane_route(target: Option<&str>, fallbacks: Vec<String>, panel: Option<RoutePanel>) -> Route {
    Route {
        paused: false,
        id: Uuid::now_v7(),
        name: "document-lane".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions {
            model_in: vec![SOURCE_MODEL.into()],
            has_documents: Some(true),
            ..Default::default()
        },
        then: RouteAction {
            workflow: None,
            target_model: target.map(str::to_string),
            fallbacks,
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: false,
            doc_compaction: false,
            document_lane: true,
            content_compress: false,
            redact: false,
            format_switch: None,
            diff: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            agentic_budget: None,
            panel,
        },
    }
}

fn route_selected_panel() -> Route {
    lane_route(
        None,
        Vec::new(),
        Some(RoutePanel {
            strategy: "synthesize".into(),
            members: vec![SOURCE_MODEL.into(), TEXT_TARGET.into()],
            arbiter: Some(ARBITER_MODEL.into()),
            quorum: None,
            max_cost_usd: Some(10.0),
        }),
    )
}

fn direct_lane_route() -> Route {
    lane_route(Some(TEXT_TARGET), vec![TEXT_FALLBACK.into()], None)
}

fn document_request(key: &str, panel_header: Option<&str>) -> Request<Body> {
    let mut body = json!({
        "model": SOURCE_MODEL,
        "max_tokens": 64,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "summarize the attached memo"},
                {
                    "type": "document",
                    "document": {
                        "source": {
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": INLINE_PDF_B64
                        },
                        "filename": "memo.pdf"
                    }
                }
            ]
        }],
        "stream": false,
    });
    if panel_header.is_some() {
        body["tt_extras"] = json!({
            "panel": {
                "members": [SOURCE_MODEL, TEXT_TARGET],
                "arbiter_model": ARBITER_MODEL
            }
        });
    }

    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .header("x-tokentrimmer-cost-limit-usd", "10.0");
    if let Some(strategy) = panel_header {
        builder = builder.header("x-tokentrimmer-panel", strategy);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn warning_tokens(response: &axum::response::Response) -> Vec<String> {
    response
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(',')
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn assert_doc_lane_zero(response: &axum::response::Response) {
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-doc-vision-saved-est-usd")
            .and_then(|value| value.to_str().ok()),
        Some("0.000000"),
        "a non-applied lane must never book a vision-avoided estimate"
    );
}

fn request_has_raw_document(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .any(|part| part["type"] == "document")
}

fn request_has_text(request: &Value, expected: &str) -> bool {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .any(|part| part["type"] == "text" && part["text"] == expected)
}

async fn assert_incomplete_stays_on_source(harness: &Harness) {
    let response = harness
        .app
        .clone()
        .oneshot(document_request(&harness.key, None))
        .await
        .expect("router response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-model-used")
            .and_then(|value| value.to_str().ok()),
        Some(SOURCE_MODEL),
        "incomplete conversion must restore the caller's source model"
    );
    assert!(
        warning_tokens(&response)
            .iter()
            .any(|token| token == "document_lane_not_applied:incomplete"),
        "caller must be told the optional lane did not apply"
    );
    assert_doc_lane_zero(&response);

    let source = harness.source_dispatches.lock().unwrap().clone();
    assert_eq!(
        source.len(),
        1,
        "source provider must receive the fallback request"
    );
    assert_eq!(source[0].provider, "source-provider");
    assert_eq!(source[0].request["model"], SOURCE_MODEL);
    assert!(
        request_has_raw_document(&source[0].request),
        "fail-open dispatch must preserve the raw document"
    );
    assert!(
        harness.text_dispatches.lock().unwrap().is_empty(),
        "incomplete conversion must never call the text target or its fallback"
    );
}

fn assert_panel_keeps_raw_members(harness: &Harness) {
    let mut all = harness.source_dispatches.lock().unwrap().clone();
    all.extend(harness.text_dispatches.lock().unwrap().clone());
    let raw_members: Vec<&Dispatch> = all
        .iter()
        .filter(|dispatch| {
            matches!(
                dispatch.request["model"].as_str(),
                Some(SOURCE_MODEL | TEXT_TARGET)
            ) && request_has_raw_document(&dispatch.request)
        })
        .collect();
    assert_eq!(
        raw_members.len(),
        2,
        "the two member legs must receive the raw caller document"
    );
    assert!(
        raw_members
            .iter()
            .any(|dispatch| dispatch.request["model"] == SOURCE_MODEL),
        "source member must retain the raw document"
    );
    assert!(
        raw_members
            .iter()
            .any(|dispatch| dispatch.request["model"] == TEXT_TARGET),
        "header/route panel text member must receive raw media rather than lane text"
    );
}

#[tokio::test]
async fn lossless_inline_pdf_conversion_can_reach_the_text_target() {
    let server = MockServer::start();
    let sidecar = server.mock(|when, then| {
        when.method(POST)
            .path("/extract")
            .body_includes(INLINE_PDF_B64);
        then.status(200)
            .header("content-type", "application/json")
            .body(
                json!({
                    "text": "Lossless memo text",
                    "pages": 1,
                    "spans": [{"kind": "lossless", "page": 0, "chars": 18}]
                })
                .to_string(),
            );
    });
    let sidecar_url = server.base_url();
    let _env = SidecarEnvGuard::set(Some(&sidecar_url)).await;
    let harness = app_with_route(direct_lane_route()).await;

    let response = harness
        .app
        .clone()
        .oneshot(document_request(&harness.key, None))
        .await
        .expect("router response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-tokentrimmer-model-used")
            .and_then(|value| value.to_str().ok()),
        Some(TEXT_TARGET),
        "a complete lossless conversion makes the text target safe"
    );
    sidecar.assert_calls(1);

    let target = harness.text_dispatches.lock().unwrap().clone();
    assert_eq!(target.len(), 1, "only the text target should dispatch");
    assert_eq!(target[0].provider, "text-provider");
    assert_eq!(target[0].request["model"], TEXT_TARGET);
    assert!(
        !request_has_raw_document(&target[0].request),
        "the text target must not receive the original document part"
    );
    assert!(
        request_has_text(&target[0].request, "Lossless memo text"),
        "the extracted text must replace the document part before target dispatch"
    );
    assert!(
        harness.source_dispatches.lock().unwrap().is_empty(),
        "complete conversion should take the text target rather than source"
    );
}

#[tokio::test]
async fn unset_sidecar_keeps_raw_document_on_source_with_incomplete_warning() {
    let _env = SidecarEnvGuard::set(None).await;
    let harness = app_with_route(direct_lane_route()).await;

    assert_incomplete_stays_on_source(&harness).await;
}

#[tokio::test]
async fn sidecar_500_keeps_raw_document_on_source_with_incomplete_warning() {
    let server = MockServer::start();
    let sidecar = server.mock(|when, then| {
        when.method(POST).path("/extract");
        then.status(500);
    });
    let sidecar_url = server.base_url();
    let _env = SidecarEnvGuard::set(Some(&sidecar_url)).await;
    let harness = app_with_route(direct_lane_route()).await;

    assert_incomplete_stays_on_source(&harness).await;
    sidecar.assert_calls(1);
}

#[tokio::test]
async fn empty_sidecar_200_keeps_raw_document_on_source_with_incomplete_warning() {
    let server = MockServer::start();
    let sidecar = server.mock(|when, then| {
        when.method(POST).path("/extract");
        then.status(200)
            .header("content-type", "application/json")
            .body("{}");
    });
    let sidecar_url = server.base_url();
    let _env = SidecarEnvGuard::set(Some(&sidecar_url)).await;
    let harness = app_with_route(direct_lane_route()).await;

    assert_incomplete_stays_on_source(&harness).await;
    sidecar.assert_calls(1);
}

#[tokio::test]
async fn route_selected_panel_suppresses_document_lane_and_keeps_raw_members() {
    let server = MockServer::start();
    let sidecar = server.mock(|when, then| {
        when.method(POST).path("/extract");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                json!({
                    "text": "must not be used",
                    "pages": 1,
                    "spans": [{"kind": "lossless", "page": 0, "chars": 16}]
                })
                .to_string(),
            );
    });
    let sidecar_url = server.base_url();
    let _env = SidecarEnvGuard::set(Some(&sidecar_url)).await;
    let harness = app_with_route(route_selected_panel()).await;

    let response = harness
        .app
        .clone()
        .oneshot(document_request(&harness.key, None))
        .await
        .expect("router response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(
        warning_tokens(&response)
            .iter()
            .any(|token| token == "document_lane_not_applied:panel"),
        "route-selected panel must make the lane suppression explicit"
    );
    assert_doc_lane_zero(&response);
    sidecar.assert_calls(0);
    assert_panel_keeps_raw_members(&harness);
}

#[tokio::test]
async fn header_selected_panel_suppresses_document_lane_and_keeps_raw_members() {
    let server = MockServer::start();
    let sidecar = server.mock(|when, then| {
        when.method(POST).path("/extract");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                json!({
                    "text": "must not be used",
                    "pages": 1,
                    "spans": [{"kind": "lossless", "page": 0, "chars": 16}]
                })
                .to_string(),
            );
    });
    let sidecar_url = server.base_url();
    let _env = SidecarEnvGuard::set(Some(&sidecar_url)).await;
    let harness = app_with_route(direct_lane_route()).await;

    let response = harness
        .app
        .clone()
        .oneshot(document_request(&harness.key, Some("synthesize")))
        .await
        .expect("router response");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(
        warning_tokens(&response)
            .iter()
            .any(|token| token == "document_lane_not_applied:panel"),
        "header-selected panel must make the lane suppression explicit"
    );
    assert_doc_lane_zero(&response);
    sidecar.assert_calls(0);
    assert_panel_keeps_raw_members(&harness);
}
