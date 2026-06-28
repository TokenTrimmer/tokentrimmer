//! Smoke tests for the Together AI provider adapter.
//!
//! Verifies: HTTP endpoint is hit correctly, model list is correct,
//! pricing table has expected rates, and error mapping works.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_compat::ClientConfig;
use tt_provider_together::TogetherProvider;
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
            api_key: SecretString::new("test-together-key"),
            base_url: Some(base_url.to_string()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
        run_id: None,
        node_id: None,
    }
}

fn minimal_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo".to_string(),
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
        "id": "cmpl-together-test",
        "object": "chat.completion",
        "created": 1716681600_i64,
        "model": "meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hello from Together!" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 9,
            "completion_tokens": 4,
            "total_tokens": 13
        }
    })
    .to_string()
}

fn provider() -> TogetherProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    TogetherProvider::new_allow_local(ClientConfig::default())
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

    assert_eq!(resp.usage.prompt_tokens, 9);
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
            .header("Retry-After", "7")
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
                retry_after_ms: 7_000
            }
        ),
        "expected RateLimited{{7000}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Malformed JSON → Deserialize error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_malformed_json_deserialize() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body("not valid json >>>>");
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::Deserialize(_)),
        "expected Deserialize, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Provider id is "together"
// ---------------------------------------------------------------------------

#[test]
fn provider_id_is_together() {
    assert_eq!(provider().id(), "together");
}

// ---------------------------------------------------------------------------
// 5. Models list contains all expected models
// ---------------------------------------------------------------------------

#[test]
fn models_list_contains_expected_models() {
    let p = provider();
    let models = p.models();
    assert_eq!(models.len(), 4, "expected 4 Together AI models");

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo"));
    assert!(ids.contains(&"meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo"));
    assert!(ids.contains(&"Qwen/Qwen2.5-72B-Instruct-Turbo"));
    assert!(ids.contains(&"deepseek-ai/DeepSeek-V3"));
}

// ---------------------------------------------------------------------------
// 6. Pricing table has correct rates
// ---------------------------------------------------------------------------

#[test]
fn pricing_table_correct_rates() {
    let p = provider();

    let llama70b = p
        .pricing("meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo")
        .expect("pricing for llama-3.3-70b");
    assert_eq!(llama70b.input_per_million, 1.04);
    assert_eq!(llama70b.output_per_million, 1.04);

    let llama405b = p
        .pricing("meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo")
        .expect("pricing for llama-3.1-405b");
    assert_eq!(llama405b.input_per_million, 3.50);
    assert_eq!(llama405b.output_per_million, 3.50);

    let qwen = p
        .pricing("Qwen/Qwen2.5-72B-Instruct-Turbo")
        .expect("pricing for Qwen2.5-72B");
    assert_eq!(qwen.input_per_million, 1.20);
    assert_eq!(qwen.output_per_million, 1.20);

    let deepseek = p
        .pricing("deepseek-ai/DeepSeek-V3")
        .expect("pricing for DeepSeek-V3");
    assert_eq!(deepseek.input_per_million, 1.25);
    assert_eq!(deepseek.output_per_million, 1.25);

    assert!(p.pricing("unknown-model").is_none());
}

// ---------------------------------------------------------------------------
// 7. All models belong to the "together" provider
// ---------------------------------------------------------------------------

#[test]
fn all_models_belong_to_together_provider() {
    let p = provider();
    for model in p.models() {
        assert_eq!(
            model.provider, "together",
            "model {} has wrong provider field",
            model.id
        );
    }
}
