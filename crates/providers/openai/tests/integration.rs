//! Integration tests for the OpenAI adapter using [`httpmock`].
//!
//! Each test spins up a local mock HTTP server, configures the adapter to
//! point at it via `base_url`, and verifies the adapter's behavior including
//! correct error mapping.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_openai::{ClientConfig, OpenAiProvider};
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
        model: "gpt-4o".to_string(),
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

fn success_body(cached_tokens: u64) -> String {
    serde_json::json!({
        "id": "chatcmpl-abc123",
        "object": "chat.completion",
        "created": 1716681600_i64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Hi there!"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15,
            "prompt_tokens_details": {
                "cached_tokens": cached_tokens
            }
        }
    })
    .to_string()
}

fn provider() -> OpenAiProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    OpenAiProvider::new_allow_local(ClientConfig::default())
}

// ---------------------------------------------------------------------------
// 1. 200 success — Usage populated including cached_tokens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn success_200_with_cached_tokens() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(success_body(80));
    });

    let ctx = make_ctx(&server.base_url());
    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed");

    assert_eq!(resp.usage.prompt_tokens, 10);
    assert_eq!(resp.usage.completion_tokens, 5);
    assert_eq!(resp.usage.total_tokens, 15);
    assert_eq!(resp.usage.cached_tokens, 80);
    assert_eq!(resp.choices.len(), 1);
}

// ---------------------------------------------------------------------------
// 2. 200 success — cached_tokens absent → defaults 0
// ---------------------------------------------------------------------------

#[tokio::test]
async fn success_200_without_cached_tokens() {
    let server = MockServer::start();

    let body = serde_json::json!({
        "id": "chatcmpl-abc456",
        "object": "chat.completion",
        "created": 1716681600_i64,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "Hello!" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        }
    })
    .to_string();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(body);
    });

    let ctx = make_ctx(&server.base_url());
    let resp = provider()
        .chat_completion(minimal_request(), &ctx)
        .await
        .expect("should succeed");

    assert_eq!(resp.usage.cached_tokens, 0);
}

// ---------------------------------------------------------------------------
// 3. 401 → Unauthorized
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_401_unauthorized() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(401).header("Content-Type", "application/json").body(
            r#"{"error":{"message":"Invalid API key","type":"invalid_api_key","code":"invalid_api_key","param":null}}"#,
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
// 4. 429 with Retry-After: 5 → RateLimited { retry_after_ms: 5000 }
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_429_with_retry_after() {
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
                retry_after_ms: 5000
            }
        ),
        "expected RateLimited{{5000}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. 429 without Retry-After → default 1000 ms
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_429_without_retry_after_defaults_1000ms() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(429)
            .header("Content-Type", "application/json")
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
                retry_after_ms: 1000
            }
        ),
        "expected RateLimited{{1000}}, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 6. 500 → ProviderUpstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_500_provider_upstream() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(500)
            .header("Content-Type", "application/json")
            .body(
                r#"{"error":{"message":"Internal server error","type":"server_error","code":null,"param":null}}"#,
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
// 7. Malformed JSON → Deserialize error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_malformed_json_deserialize() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "application/json")
            .body("this is not json at all {{{");
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
// 8. 400 with error.type: invalid_request_error → InvalidRequest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_400_invalid_request() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(400)
            .header("Content-Type", "application/json")
            .body(
                r#"{"error":{"message":"Invalid request: max_tokens is required","type":"invalid_request_error","code":null,"param":"max_tokens"}}"#,
            );
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
// 9. Streaming for reasoning models is supported (o3 / o4-mini)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_reasoning_models_supported() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body("data: [DONE]\n\n");
    });
    let ctx = make_ctx(&server.base_url());

    for model in ["o3", "o4-mini"] {
        let mut req = minimal_request();
        req.model = model.to_string();

        let result = provider().chat_completion_stream(req, &ctx).await;

        assert!(
            result.is_ok(),
            "model {model} should now stream, got {:?}",
            result.err()
        );
    }
}

// ---------------------------------------------------------------------------
// 10. Embeddings success — single input, 1536-dim vector
// ---------------------------------------------------------------------------

fn make_embedding_vec(len: usize, seed: f32) -> Vec<f32> {
    (0..len).map(|i| seed + i as f32 * 0.001).collect()
}

fn embeddings_success_body(model: &str, vecs: &[Vec<f32>]) -> String {
    let data: Vec<serde_json::Value> = vecs
        .iter()
        .enumerate()
        .map(|(i, v)| {
            serde_json::json!({
                "object": "embedding",
                "index": i,
                "embedding": v
            })
        })
        .collect();

    let total_tokens: u64 = (vecs.len() * 5) as u64; // synthetic usage
    serde_json::json!({
        "object": "list",
        "data": data,
        "model": model,
        "usage": {
            "prompt_tokens": total_tokens,
            "completion_tokens": 0,
            "total_tokens": total_tokens
        }
    })
    .to_string()
}

#[tokio::test]
async fn embeddings_success() {
    let server = MockServer::start();
    let vec_1536 = make_embedding_vec(1536, 0.001);
    let body = embeddings_success_body("text-embedding-3-small", &[vec_1536.clone()]);

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/embeddings");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(&body);
    });

    let ctx = make_ctx(&server.base_url());
    let req = EmbeddingsRequest {
        model: "text-embedding-3-small".to_string(),
        input: EmbeddingInput::Single("the quick brown fox".to_string()),
        dimensions: None,
        encoding_format: None,
    };

    let resp = provider()
        .embeddings(req, &ctx)
        .await
        .expect("embeddings should succeed");

    assert_eq!(resp.data.len(), 1);
    assert_eq!(resp.data[0].index, 0);
    assert_eq!(resp.data[0].embedding.len(), 1536);
    assert_eq!(resp.data[0].embedding[0], vec_1536[0]);
    assert_eq!(resp.usage.prompt_tokens, 5);
    assert_eq!(resp.usage.total_tokens, 5);
    assert_eq!(resp.model, "text-embedding-3-small");
}

// ---------------------------------------------------------------------------
// 10b. Embeddings batch input — two strings, two EmbeddingData entries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn embeddings_batch_input() {
    let server = MockServer::start();
    let vec_a = make_embedding_vec(1536, 0.001);
    let vec_b = make_embedding_vec(1536, 0.500);
    let body = embeddings_success_body("text-embedding-3-small", &[vec_a.clone(), vec_b.clone()]);

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/embeddings");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(&body);
    });

    let ctx = make_ctx(&server.base_url());
    let req = EmbeddingsRequest {
        model: "text-embedding-3-small".to_string(),
        input: EmbeddingInput::Batch(vec!["a".to_string(), "b".to_string()]),
        dimensions: None,
        encoding_format: None,
    };

    let resp = provider()
        .embeddings(req, &ctx)
        .await
        .expect("batch embeddings should succeed");

    assert_eq!(resp.data.len(), 2);
    assert_eq!(resp.data[0].index, 0);
    assert_eq!(resp.data[1].index, 1);
    assert_eq!(resp.data[0].embedding[0], vec_a[0]);
    assert_eq!(resp.data[1].embedding[0], vec_b[0]);
    assert_eq!(resp.usage.prompt_tokens, 10);
}

// ---------------------------------------------------------------------------
// 10c. Embeddings 401 → Unauthorized
// ---------------------------------------------------------------------------

#[tokio::test]
async fn embeddings_401_unauthorized() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/embeddings");
        then.status(401)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"message":"Invalid API key","type":"invalid_api_key","code":"invalid_api_key","param":null}}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let req = EmbeddingsRequest {
        model: "text-embedding-3-small".to_string(),
        input: EmbeddingInput::Single("hello".to_string()),
        dimensions: None,
        encoding_format: None,
    };

    let err = provider()
        .embeddings(req, &ctx)
        .await
        .expect_err("should fail with 401");

    assert!(
        matches!(err, ProviderError::Unauthorized(_)),
        "expected Unauthorized, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 10d. Embeddings pricing present for text-embedding-3-small
// ---------------------------------------------------------------------------

#[test]
fn embeddings_pricing_present() {
    let p = provider();
    let pricing = p.pricing("text-embedding-3-small");
    assert!(
        pricing.is_some(),
        "text-embedding-3-small should have pricing"
    );
    let pricing = pricing.unwrap();
    assert_eq!(pricing.input_per_million, 0.02, "should be $0.02/1M");
    assert_eq!(pricing.output_per_million, 0.00);

    let large = p.pricing("text-embedding-3-large");
    assert!(
        large.is_some(),
        "text-embedding-3-large should have pricing"
    );
    assert_eq!(large.unwrap().input_per_million, 0.13, "should be $0.13/1M");
}

// ---------------------------------------------------------------------------
// 11. Provider id, models list, pricing
// ---------------------------------------------------------------------------

#[test]
fn provider_id_and_models() {
    let p = provider();
    assert_eq!(p.id(), "openai");

    let models = p.models();
    assert_eq!(models.len(), 9, "expected 9 models (7 chat + 2 embedding)");

    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"gpt-5.5"));
    assert!(ids.contains(&"gpt-5.4"));
    assert!(ids.contains(&"gpt-5.4-mini"));
    assert!(ids.contains(&"gpt-4o"));
    assert!(ids.contains(&"gpt-4o-mini"));
    assert!(ids.contains(&"o3"));
    assert!(ids.contains(&"o4-mini"));
    assert!(ids.contains(&"text-embedding-3-small"));
    assert!(ids.contains(&"text-embedding-3-large"));
}

#[test]
fn pricing_table_all_models_present() {
    let p = provider();

    for model in [
        "gpt-5.5",
        "gpt-5.4",
        "gpt-4o",
        "gpt-4o-mini",
        "o3",
        "o4-mini",
    ] {
        let pricing = p.pricing(model);
        assert!(pricing.is_some(), "missing pricing for model '{model}'");
    }

    // Unknown model returns None.
    assert!(p.pricing("gpt-99").is_none());
}

#[test]
fn pricing_values_match_spec() {
    let p = provider();

    let gpt55 = p.pricing("gpt-5.5").expect("gpt-5.5 pricing");
    assert_eq!(gpt55.input_per_million, 5.00);
    assert_eq!(gpt55.output_per_million, 30.00);
    assert_eq!(gpt55.cached_input_per_million, Some(0.50));

    let o3 = p.pricing("o3").expect("o3 pricing");
    // o3 list price was cut to $2/$8/$0.50 (2026-05-31 catalog entry).
    assert_eq!(o3.input_per_million, 2.00);
    assert_eq!(o3.output_per_million, 8.00);
    assert_eq!(o3.cached_input_per_million, Some(0.50));

    let mini = p.pricing("gpt-4o-mini").expect("gpt-4o-mini pricing");
    assert_eq!(mini.input_per_million, 0.15);
    assert_eq!(mini.output_per_million, 0.60);
    assert_eq!(mini.cached_input_per_million, Some(0.075));
}

// ---------------------------------------------------------------------------
// 12. 503 (5xx not 500) → ProviderUpstream with correct status code
// ---------------------------------------------------------------------------

#[tokio::test]
async fn error_503_provider_upstream() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(503)
            .header("Content-Type", "application/json")
            .body(
                r#"{"error":{"message":"Service temporarily unavailable","type":"server_error","code":null,"param":null}}"#,
            );
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
