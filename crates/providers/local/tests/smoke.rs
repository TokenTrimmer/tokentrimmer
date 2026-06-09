//! HTTP-level smoke tests for the local provider adapter (Ollama/vLLM/LM Studio).
//!
//! Verifies the two behaviors unit tests can't: (1) the backend prefix
//! (`ollama/…`) is stripped BEFORE the request body hits the wire, and (2)
//! `allow_local` lets the adapter reach a loopback base_url (the SSRF guard is
//! bypassed for local backends) — both driven through the real inner client
//! against httpmock.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_local::{LocalBackend, LocalProvider};
use tt_provider_openai::ClientConfig;
use tt_shared::{
    context::{ProviderCredentials, RequestContext, SecretString},
    messages::{Message, MessageContent},
    ChatCompletionRequest, Provider,
};
use uuid::Uuid;

fn ctx() -> RequestContext {
    RequestContext {
        trace_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("local-unused"),
            base_url: None,
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
    }
}

fn request(model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![Message::User {
            content: MessageContent::Text("Hello".to_string()),
            name: None,
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(16),
        stream: false,
        tools: vec![],
        tool_choice: None,
        response_format: None,
        stop: vec![],
        presence_penalty: None,
        frequency_penalty: None,
        n: None,
        seed: None,
        user: None,
        tt_extras: HashMap::new(),
    }
}

fn success_body() -> String {
    serde_json::json!({
        "id": "cmpl-local-test",
        "object": "chat.completion",
        "created": 1716681600_i64,
        "model": "llama3.1:8b",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hi from Ollama!" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
    })
    .to_string()
}

/// The `ollama/` backend prefix must be stripped before dispatch — the mock only
/// matches when the wire body carries the bare model `llama3.1:8b`. A loopback
/// base_url is reached at all only because `allow_local` bypasses the SSRF guard.
#[tokio::test]
async fn strips_backend_prefix_and_reaches_loopback() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .json_body_partial(r#"{"model":"llama3.1:8b"}"#);
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let provider = LocalProvider::with_base_url(
        LocalBackend::Ollama,
        server.base_url(),
        ClientConfig::default(),
    );
    let resp = provider
        .chat_completion(request("ollama/llama3.1:8b"), &ctx())
        .await
        .expect("local chat_completion should succeed against the loopback mock");

    mock.assert(); // matched → the prefixed model was stripped to "llama3.1:8b"
    assert_eq!(resp.usage.total_tokens, 8);
}

/// A bare model (no `ollama/` prefix) is forwarded unchanged.
#[tokio::test]
async fn bare_model_is_forwarded_unchanged() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .json_body_partial(r#"{"model":"llama3.1:8b"}"#);
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let provider = LocalProvider::with_base_url(
        LocalBackend::Ollama,
        server.base_url(),
        ClientConfig::default(),
    );
    provider
        .chat_completion(request("llama3.1:8b"), &ctx())
        .await
        .expect("bare model should succeed");

    mock.assert();
}

#[test]
fn pricing_is_zero_for_any_local_model() {
    let p = LocalProvider::new(LocalBackend::Vllm, ClientConfig::default());
    let pr = p
        .pricing("anything")
        .expect("local pricing returns Some(zero)");
    assert_eq!(pr.input_per_million, 0.0);
    assert_eq!(pr.output_per_million, 0.0);
}
