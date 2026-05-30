//! Integration tests for [`OpenAICompatibleProvider`].
//!
//! Parameterised over two compat configs (Mistral + Groq shapes) to verify
//! the shared base handles success, error, and malformed-JSON cases correctly
//! regardless of which provider config is plugged in.

use std::collections::HashMap;

use chrono::Utc;
use httpmock::prelude::*;
use tt_provider_compat::{ClientConfig, CompatConfig, OpenAICompatibleProvider};
use tt_shared::{
    context::{ProviderCredentials, RequestContext, SecretString},
    messages::{Message, MessageContent},
    pricing::{Capability, ModelInfo, ModelPricing},
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
            api_key: SecretString::new("test-key"),
            base_url: Some(base_url.to_string()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
    }
}

fn minimal_request(model: &str) -> ChatCompletionRequest {
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

fn success_body(model: &str) -> String {
    serde_json::json!({
        "id": "chatcmpl-test-abc",
        "object": "chat.completion",
        "created": 1716681600_i64,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hi!" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        }
    })
    .to_string()
}

/// Build a minimal compat config pointing at the given mock base URL.
///
/// The `id` is used to differentiate provider instances in parameterised tests.
fn make_compat_provider(id: &'static str, model_id: &str) -> OpenAICompatibleProvider {
    let now = Utc::now();
    let mut pricing_table = HashMap::new();
    pricing_table.insert(
        model_id.to_string(),
        ModelPricing {
            input_per_million: 1.00,
            output_per_million: 3.00,
            cached_input_per_million: None,
            effective_at: now,
        },
    );

    let cfg = CompatConfig {
        id,
        default_base_url: "https://unused.example.com/v1".to_string(),
        models: vec![ModelInfo {
            id: model_id.to_string(),
            provider: id.to_string(),
            capabilities: vec![Capability::Text, Capability::Streaming],
            max_input_tokens: 128_000,
            max_output_tokens: 4_096,
        }],
        pricing_table,
        fee_multiplier: 1.0,
        // Tests use a local httpmock server on 127.0.0.1 — allow_local bypasses
        // the SSRF guard so the mock URL passes validation.
        allow_local: true,
    };

    OpenAICompatibleProvider::new(ClientConfig::default(), cfg)
}

// ---------------------------------------------------------------------------
// Parameterised 200 success tests
// ---------------------------------------------------------------------------

/// Mistral-shaped config: 200 response is correctly deserialised.
#[tokio::test]
async fn compat_mistral_config_200_success() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body("mistral-large-latest"));
    });

    let provider = make_compat_provider("mistral", "mistral-large-latest");
    let ctx = make_ctx(&server.base_url());
    let resp = provider
        .chat_completion(minimal_request("mistral-large-latest"), &ctx)
        .await
        .expect("should succeed");

    assert_eq!(resp.usage.prompt_tokens, 10);
    assert_eq!(resp.usage.completion_tokens, 5);
    assert_eq!(resp.choices.len(), 1);
}

/// Groq-shaped config: 200 response is correctly deserialised.
#[tokio::test]
async fn compat_groq_config_200_success() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body("llama-3.3-70b-versatile"));
    });

    let provider = make_compat_provider("groq", "llama-3.3-70b-versatile");
    let ctx = make_ctx(&server.base_url());
    let resp = provider
        .chat_completion(minimal_request("llama-3.3-70b-versatile"), &ctx)
        .await
        .expect("should succeed");

    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.usage.total_tokens, 15);
}

// ---------------------------------------------------------------------------
// 429 rate-limited (shared base, provider-agnostic)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compat_mistral_429_rate_limited() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(429)
            .header("Content-Type", "application/json")
            .header("Retry-After", "10")
            .body(
                r#"{"error":{"message":"Rate limit exceeded","type":"requests","code":null,"param":null}}"#,
            );
    });

    let provider = make_compat_provider("mistral", "mistral-large-latest");
    let ctx = make_ctx(&server.base_url());
    let err = provider
        .chat_completion(minimal_request("mistral-large-latest"), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: 10_000
            }
        ),
        "expected RateLimited{{10000}}, got {err:?}"
    );
}

#[tokio::test]
async fn compat_groq_429_rate_limited() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(429)
            .header("Content-Type", "application/json")
            .body(
                r#"{"error":{"message":"Rate limit exceeded","type":"requests","code":null,"param":null}}"#,
            );
    });

    let provider = make_compat_provider("groq", "llama-3.3-70b-versatile");
    let ctx = make_ctx(&server.base_url());
    let err = provider
        .chat_completion(minimal_request("llama-3.3-70b-versatile"), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: 1_000
            }
        ),
        "expected RateLimited{{1000}} (no header → default), got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Malformed JSON → Deserialize error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compat_mistral_malformed_json() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body("not json {{{{");
    });

    let provider = make_compat_provider("mistral", "mistral-large-latest");
    let ctx = make_ctx(&server.base_url());
    let err = provider
        .chat_completion(minimal_request("mistral-large-latest"), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::Deserialize(_)),
        "expected Deserialize, got {err:?}"
    );
}

#[tokio::test]
async fn compat_groq_malformed_json() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body("{bad json}");
    });

    let provider = make_compat_provider("groq", "llama-3.3-70b-versatile");
    let ctx = make_ctx(&server.base_url());
    let err = provider
        .chat_completion(minimal_request("llama-3.3-70b-versatile"), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::Deserialize(_)),
        "expected Deserialize, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Provider metadata
// ---------------------------------------------------------------------------

#[test]
fn compat_provider_id_and_models() {
    let provider = make_compat_provider("my-test-provider", "test-model-1");
    assert_eq!(provider.id(), "my-test-provider");

    let models = provider.models();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "test-model-1");
    assert_eq!(models[0].provider, "my-test-provider");
}

#[test]
fn compat_pricing_table_lookup() {
    let provider = make_compat_provider("my-test-provider", "test-model-1");

    let pricing = provider.pricing("test-model-1");
    assert!(pricing.is_some(), "test-model-1 should have pricing");
    let p = pricing.unwrap();
    assert_eq!(p.input_per_million, 1.00);
    assert_eq!(p.output_per_million, 3.00);

    assert!(provider.pricing("unknown-model").is_none());
}

#[test]
fn compat_fee_multiplier_stored() {
    let now = Utc::now();
    let cfg = CompatConfig {
        id: "openrouter",
        default_base_url: "https://openrouter.ai/api/v1".to_string(),
        models: vec![],
        pricing_table: HashMap::new(),
        fee_multiplier: 1.05,
        allow_local: false,
    };
    let provider = OpenAICompatibleProvider::new(ClientConfig::default(), cfg);
    // fee_multiplier is 1.05 — verify it is stored (billing layer reads it).
    assert!((provider.fee_multiplier() - 1.05).abs() < f64::EPSILON);
    let _ = now; // suppress lint
}

// ---------------------------------------------------------------------------
// Embeddings success for compat provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compat_embeddings_success() {
    use tt_shared::messages::{EmbeddingInput, EmbeddingsRequest};

    let server = MockServer::start();

    // Build a synthetic 8-dim vector for brevity (real models use 1536).
    let embedding_vec: Vec<f32> = (0..8).map(|i| i as f32 * 0.1).collect();
    let body = serde_json::json!({
        "object": "list",
        "data": [{
            "object": "embedding",
            "index": 0,
            "embedding": embedding_vec
        }],
        "model": "mistral-embed",
        "usage": {
            "prompt_tokens": 4,
            "completion_tokens": 0,
            "total_tokens": 4
        }
    })
    .to_string();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/embeddings");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(&body);
    });

    let provider = make_compat_provider("mistral", "mistral-large-latest");
    let ctx = make_ctx(&server.base_url());

    let req = EmbeddingsRequest {
        model: "mistral-embed".to_string(),
        input: EmbeddingInput::Single("hello world".to_string()),
        dimensions: None,
        encoding_format: None,
    };

    let resp = provider
        .embeddings(req, &ctx)
        .await
        .expect("compat embeddings should succeed");

    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].index, 0);
    assert_eq!(resp.data[0].embedding.len(), 8);
    assert_eq!(resp.usage.prompt_tokens, 4);
}

// ---------------------------------------------------------------------------
// extra_headers denylist: Authorization/Host are dropped; custom headers kept
// ---------------------------------------------------------------------------

/// Verify that `authorization` and `host` extra_headers are stripped and that a
/// legitimate custom header is forwarded to the upstream server.
#[tokio::test]
async fn extra_headers_denylist_drops_auth_and_host() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions")
            // The custom header MUST arrive at the server.
            .header("x-custom-org", "org-123")
            // Authorization should NOT be overridden by extra_headers.
            // httpmock allows us to assert what was NOT sent by checking
            // that the canonical Authorization header (set by the adapter)
            // equals the adapter-set value (Bearer test-key), not "Bearer evil".
            .header("authorization", "Bearer test-key");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body("mistral-large-latest"));
    });

    let provider = make_compat_provider("mistral", "mistral-large-latest");

    // Build a context with denied headers plus a legitimate custom header.
    let ctx = RequestContext {
        trace_id: uuid::Uuid::new_v4(),
        org_id: uuid::Uuid::new_v4(),
        api_key_id: uuid::Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("test-key"),
            base_url: Some(server.base_url()),
            extra_headers: vec![
                // Should be dropped — would override the adapter-set auth.
                ("Authorization".to_string(), "Bearer evil".to_string()),
                // Should be dropped — routing header.
                ("Host".to_string(), "internal.corp".to_string()),
                // Should be forwarded.
                ("X-Custom-Org".to_string(), "org-123".to_string()),
            ],
        },
        tag: None,
        deadline: None,
    };

    // The mock asserts that Authorization = "Bearer test-key" (not "Bearer evil")
    // and that X-Custom-Org = "org-123" is present.
    let resp = provider
        .chat_completion(minimal_request("mistral-large-latest"), &ctx)
        .await
        .expect("request should succeed with filtered headers");

    assert_eq!(resp.usage.prompt_tokens, 10);
}
