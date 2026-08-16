//! Integration tests for the Anthropic adapter using [`httpmock`].
//!
//! Each test spins up a local mock HTTP server, configures the adapter to
//! point at it via `base_url`, and verifies the adapter's behavior including
//! correct error mapping.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_anthropic::{AnthropicProvider, ClientConfig};
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
            api_key: SecretString::new("test-key"),
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
        model: "claude-sonnet-4-6".to_string(),
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
        ..Default::default()
    }
}

fn success_body() -> String {
    serde_json::json!({
        "id": "msg_01abc",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20251022",
        "content": [
            { "type": "text", "text": "Hi there!" }
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation_input_tokens": 20,
            "cache_read_input_tokens": 80
        }
    })
    .to_string()
}

fn provider() -> AnthropicProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    AnthropicProvider::new_allow_local(ClientConfig::default())
}

// ---------------------------------------------------------------------------
// 1. 200 success with usage including cached tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn success_200_with_cached_tokens() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = make_ctx(&server.base_url());
    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed");

    assert_eq!(resp.id, "msg_01abc");
    // prompt_tokens is the FULL input: 10 fresh + 80 cache-read + 20 cache-create = 110.
    assert_eq!(resp.usage.prompt_tokens, 110);
    assert_eq!(resp.usage.completion_tokens, 5);
    assert_eq!(resp.usage.total_tokens, 115);
    assert_eq!(resp.usage.cached_tokens, 80);
    assert_eq!(resp.usage.cache_creation_input_tokens, Some(20));
    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
}

// ---------------------------------------------------------------------------
// 2. 401 → Unauthorized
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_401_unauthorized() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(401)
            .header("Content-Type", "application/json")
            .body(r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid API key"}}"#);
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
// 3. 429 with retry-after: 3 → RateLimited { retry_after_ms: 3000 }
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_429_with_retry_after() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(429)
            .header("Content-Type", "application/json")
            .header("Retry-After", "3")
            .body(r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limit exceeded"}}"#);
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
                retry_after_ms: 3000
            }
        ),
        "expected RateLimited{{3000}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. 429 without retry-after → default 1000ms
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_429_without_retry_after_defaults_1000ms() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(429)
            .header("Content-Type", "application/json")
            .body(r#"{"type":"error","error":{"type":"rate_limit_error","message":"Too many requests"}}"#);
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
                retry_after_ms: 1000
            }
        ),
        "expected RateLimited{{1000}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. 400 invalid_request_error → InvalidRequest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_400_invalid_request() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(400)
            .header("Content-Type", "application/json")
            .body(r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens is required"}}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::InvalidRequest(_)),
        "expected InvalidRequest, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. 500 → ProviderUpstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_500_provider_upstream() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(500)
            .header("Content-Type", "application/json")
            .body(r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#);
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
// 7. 529 overloaded_error → ProviderUpstream (503)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_529_overloaded_maps_to_503() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(529)
            .header("Content-Type", "application/json")
            .body(r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::ProviderUpstream { status: 503, .. }),
        "expected ProviderUpstream{{503}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 8. Malformed JSON → Deserialize error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_malformed_json_deserialize() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(200)
            .header("Content-Type", "application/json")
            .body("this is definitely not json {{{");
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
// 9. Embeddings returns Unsupported
// ---------------------------------------------------------------------------

#[tokio::test]
async fn embeddings_returns_unsupported() {
    use tt_shared::messages::{EmbeddingInput, EmbeddingsRequest};

    let server = MockServer::start();
    let ctx = make_ctx(&server.base_url());

    let req = EmbeddingsRequest {
        model: "some-embed-model".to_string(),
        input: EmbeddingInput::Single("hello".to_string()),
        dimensions: None,
        encoding_format: None,
    };

    let err = provider()
        .embeddings(req, &ctx)
        .await
        .expect_err("should return Unsupported");

    assert!(
        matches!(err, ProviderError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 10. Provider id, models list, pricing
// ---------------------------------------------------------------------------

#[test]
fn provider_id_and_models() {
    let p = provider();
    assert_eq!(p.id(), "anthropic");

    let models = p.models();
    assert!(models.len() >= 4, "expected at least 4 models in pricing table");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"claude-haiku-4-5"));
    assert!(ids.contains(&"claude-sonnet-4-6"));
    assert!(ids.contains(&"claude-opus-4-7"));
    assert!(ids.contains(&"claude-opus-4-8"));
}

#[test]
fn pricing_table_all_models_present() {
    let p = provider();

    for model in ["claude-haiku-4-5", "claude-sonnet-4-6", "claude-opus-4-7"] {
        let pricing = p.pricing(model);
        assert!(pricing.is_some(), "missing pricing for model '{model}'");
    }

    assert!(p.pricing("claude-3-opus").is_none());
}

#[test]
fn pricing_values_match_spec() {
    let p = provider();

    let haiku = p.pricing("claude-haiku-4-5").expect("haiku pricing");
    assert_eq!(haiku.input_per_million, 1.00);
    assert_eq!(haiku.output_per_million, 5.00);
    assert_eq!(haiku.cached_input_per_million, Some(0.10));

    let sonnet = p.pricing("claude-sonnet-4-6").expect("sonnet pricing");
    assert_eq!(sonnet.input_per_million, 3.00);
    assert_eq!(sonnet.output_per_million, 15.00);
    assert_eq!(sonnet.cached_input_per_million, Some(0.30));

    let opus = p.pricing("claude-opus-4-7").expect("opus pricing");
    assert_eq!(opus.input_per_million, 5.00);
    assert_eq!(opus.output_per_million, 25.00);
    assert_eq!(opus.cached_input_per_million, Some(0.50));
}

// ---------------------------------------------------------------------------
// 11. Request uses x-api-key header (not Authorization: Bearer)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_uses_x_api_key_header() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/messages")
            .header("x-api-key", "my-test-key");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = RequestContext {
        budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
        trace_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("my-test-key"),
            base_url: Some(server.base_url()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
        run_id: None,
        node_id: None,
    };

    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed with x-api-key");
    assert_eq!(resp.id, "msg_01abc");
}

// ---------------------------------------------------------------------------
// 12. Response stop_reason mapping: end_turn → stop
// ---------------------------------------------------------------------------

#[tokio::test]
async fn response_stop_reason_end_turn_maps_to_stop() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = make_ctx(&server.base_url());
    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed");

    assert_eq!(
        resp.choices[0].finish_reason.as_deref(),
        Some("stop"),
        "end_turn should map to 'stop'"
    );
}
