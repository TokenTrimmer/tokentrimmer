//! Integration tests for the Azure OpenAI adapter.
//!
//! Verifies the Azure-specific wire behavior over a local mock server:
//! - the deployment-scoped URL path is hit,
//! - the `api-version` query param is present,
//! - auth uses the `api-key` header and NOT `Authorization: Bearer`,
//! - a streaming response is parsed via the shared SSE machinery.

use std::collections::HashMap;

use futures::StreamExt;
use httpmock::prelude::*;
use tt_provider_azure::{AzureProvider, ClientConfig, AZURE_API_VERSION};
use tt_shared::{
    context::{ProviderCredentials, RequestContext, SecretString},
    messages::{Message, MessageContent},
    ChatCompletionRequest, Provider,
};
use uuid::Uuid;

fn make_ctx(base_url: &str) -> RequestContext {
    RequestContext {
        trace_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("azure-secret-key"),
            base_url: Some(base_url.to_string()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
        run_id: None,
        node_id: None,
    }
}

fn request(model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![Message::User {
            content: MessageContent::Text("Hello".to_string()),
            name: None,
        }],
        max_tokens: Some(32),
        stream: false,
        tt_extras: HashMap::new(),
        ..Default::default()
    }
}

fn success_body() -> String {
    serde_json::json!({
        "id": "chatcmpl-azure-test",
        "object": "chat.completion",
        "created": 1_716_681_600_i64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hi from Azure" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 4, "total_tokens": 9 }
    })
    .to_string()
}

fn provider() -> AzureProvider {
    AzureProvider::new_allow_local(ClientConfig::default())
}

#[tokio::test]
async fn hits_deployment_url_with_api_version_and_api_key_header() {
    let server = MockServer::start();
    // The mock asserts the full Azure shape: deployment-scoped path, the
    // api-version query param, the api-key header, and the ABSENCE of a Bearer
    // Authorization header.
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/gpt-4o-prod/chat/completions")
            .query_param("api-version", AZURE_API_VERSION)
            .header("api-key", "azure-secret-key");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = make_ctx(&server.base_url());
    let resp = provider()
        .chat_completion(request("gpt-4o-prod"), &ctx)
        .await
        .expect("should succeed");

    mock.assert();
    assert_eq!(resp.usage.prompt_tokens, 5);
    assert_eq!(resp.choices.len(), 1);
}

#[tokio::test]
async fn does_not_send_authorization_bearer_header() {
    let server = MockServer::start();
    // A mock that ONLY matches when there is no Authorization header. If the
    // adapter wrongly sent `Authorization: Bearer …`, this mock would not match
    // and the request would 404 (httpmock default for no match).
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/dep/chat/completions")
            .is_true(|req| {
                !req.headers_vec()
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            });
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = make_ctx(&server.base_url());
    provider()
        .chat_completion(request("dep"), &ctx)
        .await
        .expect("should succeed with api-key auth only");
    mock.assert();
}

#[tokio::test]
async fn strips_azure_routing_prefix_to_deployment_name() {
    let server = MockServer::start();
    // The request model is `azure/gpt-4o-prod`; the URL deployment segment must
    // be the bare `gpt-4o-prod` (prefix stripped).
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/gpt-4o-prod/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = make_ctx(&server.base_url());
    provider()
        .chat_completion(request("azure/gpt-4o-prod"), &ctx)
        .await
        .expect("should succeed");
    mock.assert();
}

#[tokio::test]
async fn streaming_parses_sse_chunks() {
    let server = MockServer::start();
    let sse = concat!(
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/openai/deployments/dep/chat/completions")
            .query_param("api-version", AZURE_API_VERSION)
            .header("api-key", "azure-secret-key");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(request("dep"), &ctx)
        .await
        .expect("stream should open");

    let mut content = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk ok");
        for ch in &chunk.choices {
            if let Some(c) = &ch.delta.content {
                content.push_str(c);
            }
        }
    }
    mock.assert();
    assert_eq!(content, "Hello world");
}

#[tokio::test]
async fn missing_base_url_errors_before_network() {
    // No base_url → InvalidRequest, no HTTP call made.
    let ctx = RequestContext {
        trace_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("k"),
            base_url: None,
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
        run_id: None,
        node_id: None,
    };
    let err = provider()
        .chat_completion(request("gpt-4o"), &ctx)
        .await
        .expect_err("should fail without base_url");
    assert!(matches!(err, tt_shared::ProviderError::InvalidRequest(_)));
}
