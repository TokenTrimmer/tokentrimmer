//! `X-TokenTrimmer-Warnings: response_format_downgrade` + json_schema→json_object rewrite.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use futures::stream::BoxStream;
use serde_json::json;
use tower::util::ServiceExt;

use tt_core::{build_router, AppState, ProviderRegistry};
use tt_shared::{
    messages::{Choice, Message, MessageContent, ResponseFormat},
    pricing::Capability,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext, Usage,
};

struct RfMock {
    schema: bool,
    drops_rf: bool,
    seen: Arc<Mutex<Option<ResponseFormat>>>,
}

#[async_trait]
impl Provider for RfMock {
    fn id(&self) -> &'static str {
        "rfmock"
    }
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "rf-1".into(),
            provider: "rfmock".into(),
            capabilities: vec![Capability::Text, Capability::JsonMode],
            max_input_tokens: 128_000,
            max_output_tokens: 4096,
        }]
    }
    fn pricing(&self, _m: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.1,
            output_per_million: 0.1,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }
    fn supports_response_schema(&self) -> bool {
        self.schema
    }
    fn dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String> {
        if self.drops_rf && req.response_format.is_some() {
            vec!["response_format".to_string()]
        } else {
            Vec::new()
        }
    }
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        _ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        *self.seen.lock().unwrap() = req.response_format.clone();
        Ok(ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: req.model,
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("hi".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
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
        Err(ProviderError::Unsupported("no stream".into()))
    }
}

fn app_with(mock: RfMock) -> axum::Router {
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(mock));
    build_router(AppState::new(registry))
}

fn schema_req() -> String {
    json!({
        "model": "rf-1",
        "messages": [{"role":"user","content":"hi"}],
        "response_format": {"type":"json_schema","json_schema":{"name":"x","schema":{"type":"object"}}}
    })
    .to_string()
}

async fn post(app: axum::Router, body: String) -> axum::http::Response<Body> {
    app.oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn downgrades_schema_for_non_schema_provider() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(RfMock {
            schema: false,
            drops_rf: false,
            seen: Arc::clone(&seen),
        }),
        schema_req(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(warn.contains("response_format_downgrade"), "got: {warn}");
    // The dispatched request was rewritten to json_object with no schema.
    let rf = seen.lock().unwrap().clone().expect("response_format seen");
    assert_eq!(rf.r#type, "json_object");
    assert!(rf.json_schema.is_none());
}

#[tokio::test]
async fn no_downgrade_for_schema_capable_provider() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(RfMock {
            schema: true,
            drops_rf: false,
            seen: Arc::clone(&seen),
        }),
        schema_req(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-tokentrimmer-warnings").is_none());
    let rf = seen.lock().unwrap().clone().expect("response_format seen");
    assert_eq!(rf.r#type, "json_schema");
}

#[tokio::test]
async fn dropped_response_format_is_not_downgraded() {
    let seen = Arc::new(Mutex::new(None));
    let resp = post(
        app_with(RfMock {
            schema: false,
            drops_rf: true,
            seen: Arc::clone(&seen),
        }),
        schema_req(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let warn = resp
        .headers()
        .get("x-tokentrimmer-warnings")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        warn.contains("param_dropped:response_format"),
        "got: {warn}"
    );
    assert!(!warn.contains("response_format_downgrade"), "got: {warn}");
}
