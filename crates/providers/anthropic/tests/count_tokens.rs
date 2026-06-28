//! Integration tests for `AnthropicProvider::count_tokens` (COST-6) using
//! [`httpmock`] — request shape, response parse, memo cache, and the proxy
//! fallback. No network is touched: every call targets a local mock server.

use std::collections::HashMap;

use httpmock::prelude::*;
use tt_provider_anthropic::{AnthropicProvider, ClientConfig};
use tt_shared::{
    context::{ProviderCredentials, RequestContext, SecretString},
    messages::{Message, MessageContent},
    ChatCompletionRequest,
};
use uuid::Uuid;

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
        run_id: None,
        node_id: None,
    }
}

fn request(text: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "claude-sonnet-4-6".to_string(),
        messages: vec![Message::User {
            content: MessageContent::Text(text.to_string()),
            name: None,
        }],
        max_tokens: Some(64),
        stream: false,
        tt_extras: HashMap::new(),
        ..Default::default()
    }
}

fn provider() -> AnthropicProvider {
    AnthropicProvider::new_allow_local(ClientConfig::default())
}

/// A 200 with `input_tokens` is parsed and returned verbatim, and the request
/// goes to `/v1/messages/count_tokens` with the `x-api-key` header and a
/// `messages` body (the count endpoint's shape).
#[tokio::test]
async fn returns_authoritative_count() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/messages/count_tokens")
            .header("x-api-key", "test-key")
            // The count endpoint's body carries the model + messages (and, per
            // the unit test in src/count_tokens.rs, NOT max_tokens).
            .json_body_includes(r#"{"model": "claude-sonnet-4-6"}"#);
        then.status(200)
            .header("Content-Type", "application/json")
            .body(r#"{"input_tokens": 1234}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let count = provider().count_tokens(&request("hello world"), &ctx).await;

    assert_eq!(count, 1234, "must return Anthropic's reported input_tokens");
    mock.assert();
}

/// Repeated counts of the SAME prompt hit the network once; the second is
/// served from the in-memory memo cache.
#[tokio::test]
async fn caches_identical_prompts() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages/count_tokens");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(r#"{"input_tokens": 77}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let p = provider();
    let req = request("cache me");

    let a = p.count_tokens(&req, &ctx).await;
    let b = p.count_tokens(&req, &ctx).await;

    assert_eq!(a, 77);
    assert_eq!(b, 77);
    assert_eq!(
        mock.calls(),
        1,
        "second identical count must be cache-served"
    );
}

/// On an upstream error the call NEVER fails — it falls back to the corrected
/// `tt_tokenize` proxy estimate (the model-aware path, which carries +18%).
#[tokio::test]
async fn falls_back_to_proxy_estimate_on_error() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages/count_tokens");
        then.status(500)
            .header("Content-Type", "application/json")
            .body(r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let req = request("fall back to the proxy estimate please");
    let count = provider().count_tokens(&req, &ctx).await;

    let text = tt_shared::message_text_for_estimation(&req);
    let expected = u64::from(tt_tokenize::estimate_tokens_for_model(
        "anthropic",
        &req.model,
        &text,
    ));
    assert_eq!(
        count, expected,
        "a 500 must degrade to the corrected proxy estimate"
    );
}

/// A failed count is not cached: a later call retries the API (and succeeds).
#[tokio::test]
async fn error_result_is_not_cached() {
    let server = MockServer::start();
    let mut fail = server.mock(|when, then| {
        when.method(POST).path("/v1/messages/count_tokens");
        then.status(500).body("nope");
    });

    let ctx = make_ctx(&server.base_url());
    let p = provider();
    let req = request("retry after failure");

    let _ = p.count_tokens(&req, &ctx).await; // falls back, does not cache
    fail.delete();

    let ok = server.mock(|when, then| {
        when.method(POST).path("/v1/messages/count_tokens");
        then.status(200)
            .header("Content-Type", "application/json")
            .body(r#"{"input_tokens": 555}"#);
    });

    let count = p.count_tokens(&req, &ctx).await;
    assert_eq!(count, 555, "a prior fallback must not poison the cache");
    ok.assert();
}
