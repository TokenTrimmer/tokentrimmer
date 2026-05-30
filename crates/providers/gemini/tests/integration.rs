//! Integration tests for the Gemini adapter using [`httpmock`].
//!
//! Each test spins up a local mock HTTP server, configures the adapter to
//! point at it via `base_url`, and verifies the adapter's behavior including
//! correct error mapping.
//!
//! The Gemini API key is passed as a query parameter (`?key=...`), not a
//! header — tests verify this behavior.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_gemini::{ClientConfig, GeminiProvider};
use tt_shared::{
    context::{ProviderCredentials, RequestContext, SecretString},
    messages::{EmbeddingInput, EmbeddingsRequest, Message, MessageContent},
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

fn minimal_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "gemini-3.1-pro".to_string(),
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
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [{ "text": "Hi there!" }]
                },
                "finishReason": "STOP",
                "index": 0
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 5,
            "totalTokenCount": 15,
            "cachedContentTokenCount": 3
        },
        "modelVersion": "gemini-3.1-pro-002"
    })
    .to_string()
}

fn provider() -> GeminiProvider {
    GeminiProvider::new(ClientConfig::default())
}

// ---------------------------------------------------------------------------
// 1. 200 success including cachedContentTokenCount in usage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn success_200_with_cached_content_token_count() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent")
            .header("x-goog-api-key", "test-key");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = make_ctx(&server.base_url());
    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed");

    // Usage with cached tokens.
    assert_eq!(resp.usage.prompt_tokens, 10);
    assert_eq!(resp.usage.completion_tokens, 5);
    assert_eq!(resp.usage.total_tokens, 15);
    assert_eq!(resp.usage.cached_tokens, 3);

    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    assert_eq!(resp.model, "gemini-3.1-pro-002");

    // id should be auto-generated.
    assert!(resp.id.starts_with("chatcmpl-gem-"));
}

// ---------------------------------------------------------------------------
// 2. 401 → Unauthorized
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_401_unauthorized() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent");
        then.status(401)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"code":401,"message":"API key not valid","status":"UNAUTHENTICATED"}}"#);
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
// 3. 429 → RateLimited (default 1000ms if no Retry-After)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_429_rate_limited_default_retry_after() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent");
        then.status(429)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}"#);
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
// 4. 400 INVALID_ARGUMENT → InvalidRequest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_400_invalid_argument() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent");
        then.status(400)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"code":400,"message":"Invalid argument: contents is empty","status":"INVALID_ARGUMENT"}}"#);
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
// 5. 404 → ModelNotFound
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_404_model_not_found() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent");
        then.status(404)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"code":404,"message":"models/gemini-3.1-pro is not found","status":"NOT_FOUND"}}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let err = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect_err("should fail");

    assert!(
        matches!(err, ProviderError::ModelNotFound { .. }),
        "expected ModelNotFound, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. 500 INTERNAL → ProviderUpstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_500_internal_provider_upstream() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent");
        then.status(500)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"code":500,"message":"Internal error","status":"INTERNAL"}}"#);
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
// 7. 503 UNAVAILABLE → ProviderUpstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_503_unavailable_provider_upstream() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent");
        then.status(503)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"code":503,"message":"The service is currently unavailable","status":"UNAVAILABLE"}}"#);
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
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent");
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
// Provider metadata tests
// ---------------------------------------------------------------------------

#[test]
fn provider_id_and_models() {
    let p = provider();
    assert_eq!(p.id(), "gemini");

    let models = p.models();
    assert_eq!(models.len(), 3, "expected 3 models in pricing table");

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"gemini-3.1-flash-lite"));
    assert!(ids.contains(&"gemini-3.5-flash"));
    assert!(ids.contains(&"gemini-3.1-pro"));
}

#[test]
fn pricing_table_all_models_present() {
    let p = provider();

    for model in [
        "gemini-3.1-flash-lite",
        "gemini-3.5-flash",
        "gemini-3.1-pro",
    ] {
        let pricing = p.pricing(model);
        assert!(pricing.is_some(), "missing pricing for model '{model}'");
    }

    assert!(p.pricing("gemini-1.0-pro").is_none());
}

#[test]
fn pricing_values_match_spec() {
    let p = provider();

    let flash_lite = p
        .pricing("gemini-3.1-flash-lite")
        .expect("flash-lite pricing");
    assert_eq!(flash_lite.input_per_million, 0.25);
    assert_eq!(flash_lite.output_per_million, 1.50);
    assert_eq!(flash_lite.cached_input_per_million, Some(0.025));

    let flash = p.pricing("gemini-3.5-flash").expect("flash pricing");
    assert_eq!(flash.input_per_million, 1.50);
    assert_eq!(flash.output_per_million, 9.00);
    assert_eq!(flash.cached_input_per_million, Some(0.15));

    let pro = p.pricing("gemini-3.1-pro").expect("pro pricing");
    assert_eq!(pro.input_per_million, 2.00);
    assert_eq!(pro.output_per_million, 12.00);
    assert_eq!(pro.cached_input_per_million, Some(0.20));
}

#[tokio::test]
async fn embeddings_returns_unsupported() {
    let server = MockServer::start();
    let ctx = make_ctx(&server.base_url());

    let req = EmbeddingsRequest {
        model: "text-embedding-004".to_string(),
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
// Auth: API key passed in the x-goog-api-key header, never the URL (§5.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_uses_api_key_header() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent")
            .header("x-goog-api-key", "my-specific-test-key");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body());
    });

    let ctx = RequestContext {
        trace_id: Uuid::new_v4(),
        org_id: Uuid::new_v4(),
        api_key_id: Uuid::new_v4(),
        credentials: ProviderCredentials {
            api_key: SecretString::new("my-specific-test-key"),
            base_url: Some(server.base_url()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
    };

    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed with x-goog-api-key header");

    assert!(resp.id.starts_with("chatcmpl-gem-"));
}

// ---------------------------------------------------------------------------
// Response: stop reason mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn response_stop_reason_maps_correctly() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:generateContent");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body()); // finishReason: STOP
    });

    let ctx = make_ctx(&server.base_url());
    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed");

    assert_eq!(
        resp.choices[0].finish_reason.as_deref(),
        Some("stop"),
        "STOP should map to 'stop'"
    );
}
