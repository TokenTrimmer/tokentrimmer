//! Smoke tests for the Mistral provider adapter.
//!
//! Verifies: HTTP endpoint is hit correctly, model list is correct,
//! pricing table has expected rates, and error mapping works.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_compat::ClientConfig;
use tt_provider_mistral::MistralProvider;
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
            api_key: SecretString::new("test-mistral-key"),
            base_url: Some(base_url.to_string()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
    }
}

fn minimal_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "mistral-large-latest".to_string(),
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
    }
}

fn success_body() -> String {
    serde_json::json!({
        "id": "cmpl-mistral-test",
        "object": "chat.completion",
        "created": 1716681600_i64,
        "model": "mistral-large-latest",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Bonjour!" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 8,
            "completion_tokens": 3,
            "total_tokens": 11
        }
    })
    .to_string()
}

fn provider() -> MistralProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    MistralProvider::new_allow_local(ClientConfig::default())
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

    assert_eq!(resp.usage.prompt_tokens, 8);
    assert_eq!(resp.usage.completion_tokens, 3);
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
            .header("Retry-After", "3")
            .body(
                r#"{"error":{"message":"Too many requests","type":"requests","code":null,"param":null}}"#,
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
                retry_after_ms: 3_000
            }
        ),
        "expected RateLimited{{3000}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. 401 → Unauthorized
// ---------------------------------------------------------------------------

#[tokio::test]
async fn smoke_401_unauthorized() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(401)
            .header("Content-Type", "application/json")
            .body(
                r#"{"error":{"message":"Unauthorized","type":"invalid_api_key","code":"invalid_api_key","param":null}}"#,
            );
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::Unauthorized(_)),
        "expected Unauthorized, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. Provider id is "mistral"
// ---------------------------------------------------------------------------

#[test]
fn provider_id_is_mistral() {
    assert_eq!(provider().id(), "mistral");
}

// ---------------------------------------------------------------------------
// 5. Models list contains all expected models
// ---------------------------------------------------------------------------

#[test]
fn models_list_contains_expected_models() {
    let p = provider();
    let models = p.models();
    assert_eq!(models.len(), 5, "expected 5 Mistral models");

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(
        ids.contains(&"mistral-large-latest"),
        "missing mistral-large-latest"
    );
    assert!(
        ids.contains(&"mistral-medium-latest"),
        "missing mistral-medium-latest"
    );
    assert!(
        ids.contains(&"mistral-small-latest"),
        "missing mistral-small-latest"
    );
    assert!(
        ids.contains(&"codestral-latest"),
        "missing codestral-latest"
    );
    assert!(
        ids.contains(&"pixtral-large-latest"),
        "missing pixtral-large-latest"
    );
}

// ---------------------------------------------------------------------------
// 6. Pricing table has correct rates
// ---------------------------------------------------------------------------

#[test]
fn pricing_table_correct_rates() {
    let p = provider();

    let large = p
        .pricing("mistral-large-latest")
        .expect("pricing for large");
    assert_eq!(large.input_per_million, 2.00);
    assert_eq!(large.output_per_million, 6.00);

    let small = p
        .pricing("mistral-small-latest")
        .expect("pricing for small");
    assert_eq!(small.input_per_million, 0.20);
    assert_eq!(small.output_per_million, 0.60);

    let codestral = p
        .pricing("codestral-latest")
        .expect("pricing for codestral");
    assert_eq!(codestral.input_per_million, 0.30);
    assert_eq!(codestral.output_per_million, 0.90);

    let pixtral = p
        .pricing("pixtral-large-latest")
        .expect("pricing for pixtral");
    assert_eq!(pixtral.input_per_million, 2.00);
    assert_eq!(pixtral.output_per_million, 6.00);

    assert!(p.pricing("unknown-model").is_none());
}

// ---------------------------------------------------------------------------
// 7. pixtral has Vision capability
// ---------------------------------------------------------------------------

#[test]
fn pixtral_has_vision_capability() {
    use tt_shared::pricing::Capability;

    let p = provider();
    let models = p.models();
    let pixtral = models
        .iter()
        .find(|m| m.id == "pixtral-large-latest")
        .expect("pixtral-large-latest should be in models");

    assert!(
        pixtral.capabilities.contains(&Capability::Vision),
        "pixtral-large-latest should have Vision capability"
    );
}
