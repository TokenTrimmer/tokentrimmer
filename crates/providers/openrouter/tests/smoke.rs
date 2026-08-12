//! Smoke tests for the OpenRouter provider adapter.
//!
//! Verifies: HTTP endpoint is hit correctly, model list is correct,
//! pricing table has expected rates, and the BYOK fee multiplier is set.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_compat::ClientConfig;
use tt_provider_openrouter::OpenRouterProvider;
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
        budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
        trace_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("test-openrouter-key"),
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
        model: "anthropic/claude-sonnet-4-6".to_string(),
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
        "id": "cmpl-openrouter-test",
        "object": "chat.completion",
        "created": 1716681600_i64,
        "model": "anthropic/claude-sonnet-4-6",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hello from OpenRouter!" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 11,
            "completion_tokens": 5,
            "total_tokens": 16
        }
    })
    .to_string()
}

fn provider() -> OpenRouterProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    OpenRouterProvider::new_allow_local(ClientConfig::default())
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

    assert_eq!(resp.usage.prompt_tokens, 11);
    assert_eq!(resp.usage.completion_tokens, 5);
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
            .header("Retry-After", "5")
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
                retry_after_ms: 5_000
            }
        ),
        "expected RateLimited{{5000}}, got {err:?}"
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
            .body("{invalid json}");
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
// 4. Provider id is "openrouter"
// ---------------------------------------------------------------------------

#[test]
fn provider_id_is_openrouter() {
    assert_eq!(provider().id(), "openrouter");
}

// ---------------------------------------------------------------------------
// 5. Models list contains all expected models
// ---------------------------------------------------------------------------

#[test]
fn models_list_contains_expected_models() {
    let p = provider();
    let models = p.models();
    assert_eq!(
        models.len(),
        5,
        "expected 5 OpenRouter models in static set"
    );

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"anthropic/claude-sonnet-4-6"));
    assert!(ids.contains(&"openai/gpt-5.5"));
    assert!(ids.contains(&"google/gemini-3.1-pro"));
    assert!(ids.contains(&"meta-llama/llama-3.3-70b-instruct"));
    assert!(ids.contains(&"mistralai/mistral-large"));
}

// ---------------------------------------------------------------------------
// 6. Pricing table has correct rates
// ---------------------------------------------------------------------------

#[test]
fn pricing_table_correct_rates() {
    let p = provider();

    let claude = p
        .pricing("anthropic/claude-sonnet-4.6")
        .expect("pricing for claude-sonnet-4.6");
    assert_eq!(claude.input_per_million, 3.00);
    assert_eq!(claude.output_per_million, 15.00);

    let gpt55 = p.pricing("openai/gpt-5.5").expect("pricing for gpt-5.5");
    assert_eq!(gpt55.input_per_million, 5.00);
    assert_eq!(gpt55.output_per_million, 30.00);

    let gemini = p
        .pricing("google/gemini-3.1-pro-preview")
        .expect("pricing for gemini-3.1-pro-preview");
    assert_eq!(gemini.input_per_million, 2.00);
    assert_eq!(gemini.output_per_million, 12.00);

    let llama = p
        .pricing("meta-llama/llama-3.3-70b-instruct")
        .expect("pricing for llama-3.3-70b");
    assert_eq!(llama.input_per_million, 0.10);
    assert_eq!(llama.output_per_million, 0.32);

    let mistral = p
        .pricing("mistralai/mistral-large")
        .expect("pricing for mistral-large");
    assert_eq!(mistral.input_per_million, 2.00);
    assert_eq!(mistral.output_per_million, 6.00);

    assert!(p.pricing("unknown-model").is_none());
}

// ---------------------------------------------------------------------------
// 7. BYOK fee multiplier is 1.05
// ---------------------------------------------------------------------------

#[test]
fn byok_fee_multiplier_is_1_05() {
    // OpenRouter charges a 5% BYOK fee. The multiplier is stored in the
    // CompatConfig and retrieved by the billing layer — it is NOT applied
    // at request time. We verify the value is stored correctly.
    //
    // We access this via the inner OpenAICompatibleProvider's fee_multiplier()
    // method, surfaced through a dedicated accessor on OpenRouterProvider.
    let p = provider();
    assert!(
        (p.fee_multiplier() - 1.05).abs() < f64::EPSILON,
        "expected fee_multiplier=1.05 for OpenRouter BYOK, got {}",
        p.fee_multiplier()
    );
}
