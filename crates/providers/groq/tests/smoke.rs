//! Smoke tests for the Groq provider adapter.
//!
//! Verifies: HTTP endpoint is hit correctly, model list is correct,
//! pricing table has expected rates, and error mapping works.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_compat::ClientConfig;
use tt_provider_groq::GroqProvider;
use tt_shared::{
    context::{ProviderCredentials, RequestContext, SecretString},
    messages::{Message, MessageContent},
    ChatCompletionRequest, Provider, ProviderError,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_ctx(base_url: &str) -> RequestContext {
    RequestContext {
        trace_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("test-groq-key"),
            base_url: Some(base_url.to_string()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
    }
}

fn minimal_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "llama-3.3-70b-versatile".to_string(),
        messages: vec![Message::User {
            content: MessageContent::Text("Hello".to_string()),
            name: None,
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(32),
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
        ..Default::default()
    }
}

fn success_body() -> String {
    serde_json::json!({
        "id": "cmpl-groq-test",
        "object": "chat.completion",
        "created": 1716681600_i64,
        "model": "llama-3.3-70b-versatile",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hello from Groq!" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 4,
            "total_tokens": 16
        }
    })
    .to_string()
}

fn provider() -> GroqProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    GroqProvider::new_allow_local(ClientConfig::default())
}

// ---------------------------------------------------------------------------
// 1. 200 success — request hits /chat/completions on the mock base URL
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_200_success_hits_correct_endpoint() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = make_ctx(&server.base_url());
    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed");

    assert_eq!(resp.usage.prompt_tokens, 12);
    assert_eq!(resp.usage.completion_tokens, 4);
    assert_eq!(resp.choices.len(), 1);
}

// ---------------------------------------------------------------------------
// 2. 429 → RateLimited
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_429_rate_limited() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(429)
            .header("Content-Type", "application/json")
            .header("Retry-After", "2")
            .body(
                r#"{"error":{"message":"Rate limit exceeded","type":"requests","code":null,"param":null}}"#,
            );
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: 2_000
            }
        ),
        "expected RateLimited{{2000}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. 500 → ProviderUpstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_500_provider_upstream() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(500)
            .header("Content-Type", "application/json")
            .body(
                r#"{"error":{"message":"Internal error","type":"server_error","code":null,"param":null}}"#,
            );
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::ProviderUpstream { status: 500, .. }),
        "expected ProviderUpstream{{500}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Provider id is "groq"
// ---------------------------------------------------------------------------

#[test]
fn provider_id_is_groq() {
    assert_eq!(provider().id(), "groq");
}

// ---------------------------------------------------------------------------
// 5. Models list contains all expected models
// ---------------------------------------------------------------------------

#[test]
fn models_list_contains_expected_models() {
    let p = provider();
    let models = p.models();
    assert_eq!(models.len(), 4, "expected 4 Groq models");

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&"llama-3.3-70b-versatile"),
        "missing llama-3.3-70b-versatile"
    );
    assert!(
        ids.contains(&"llama-3.1-8b-instant"),
        "missing llama-3.1-8b-instant"
    );
    assert!(
        ids.contains(&"deepseek-r1-distill-llama-70b"),
        "missing deepseek-r1-distill-llama-70b"
    );
    assert!(
        ids.contains(&"mixtral-8x7b-32768"),
        "missing mixtral-8x7b-32768"
    );
}

// ---------------------------------------------------------------------------
// 6. Pricing table has correct rates
// ---------------------------------------------------------------------------

#[test]
fn pricing_table_correct_rates() {
    let p = provider();

    let llama70b = p
        .pricing("llama-3.3-70b-versatile")
        .expect("pricing for llama-3.3-70b");
    assert_eq!(llama70b.input_per_million, 0.59);
    assert_eq!(llama70b.output_per_million, 0.79);

    let llama8b = p
        .pricing("llama-3.1-8b-instant")
        .expect("pricing for llama-3.1-8b");
    assert_eq!(llama8b.input_per_million, 0.05);
    assert_eq!(llama8b.output_per_million, 0.08);

    let deepseek = p
        .pricing("deepseek-r1-distill-llama-70b")
        .expect("pricing for deepseek-r1");
    assert_eq!(deepseek.input_per_million, 0.75);
    assert_eq!(deepseek.output_per_million, 0.99);

    let mixtral = p
        .pricing("mixtral-8x7b-32768")
        .expect("pricing for mixtral");
    assert_eq!(mixtral.input_per_million, 0.24);
    assert_eq!(mixtral.output_per_million, 0.24);

    assert!(p.pricing("unknown-model").is_none());
}

// ---------------------------------------------------------------------------
// 7. deepseek-r1 has Reasoning capability
// ---------------------------------------------------------------------------

#[test]
fn deepseek_r1_has_reasoning_capability() {
    use tt_shared::pricing::Capability;

    let p = provider();
    let models = p.models();
    let deepseek = models
        .iter()
        .find(|m| m.id == "deepseek-r1-distill-llama-70b")
        .expect("deepseek-r1-distill-llama-70b should be in models");

    assert!(
        deepseek.capabilities.contains(&Capability::Reasoning),
        "deepseek-r1-distill-llama-70b should have Reasoning capability"
    );
}
