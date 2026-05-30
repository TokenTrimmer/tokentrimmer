//! End-to-end streaming tests for the Gemini adapter.
//!
//! Uses [`httpmock`] to serve pre-canned Gemini SSE bodies and verifies that
//! the streaming parser correctly emits [`ChatCompletionChunk`] values.
//!
//! Gemini uses `?alt=sse` for SSE streaming. Events contain partial
//! `generateContent` JSON responses. There is no `[DONE]` terminator.

use std::collections::HashMap;

use futures::StreamExt;
use httpmock::prelude::*;
use insta::assert_json_snapshot;
use tt_provider_gemini::{ClientConfig, GeminiProvider};
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
            api_key: SecretString::new("test-key"),
            base_url: Some(base_url.to_string()),
            extra_headers: vec![],
        },
        tag: None,
        deadline: None,
    }
}

fn stream_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "gemini-3.1-pro".to_string(),
        messages: vec![Message::User {
            content: MessageContent::Text("Hello".to_string()),
            name: None,
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(64),
        stream: false, // adapter handles streaming internally
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

fn provider() -> GeminiProvider {
    GeminiProvider::new(ClientConfig::default())
}

/// Build a Gemini SSE stream from text chunks.
fn text_sse_stream(text_chunks: &[&str], finish_reason: &str, usage_tokens: (u64, u64)) -> String {
    let mut sse = String::new();
    let (prompt_tokens, completion_tokens) = usage_tokens;

    for (i, text) in text_chunks.iter().enumerate() {
        let is_last = i == text_chunks.len() - 1;
        if is_last {
            // Final chunk includes finishReason and usageMetadata
            sse.push_str(&format!(
                "data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"{}\"}}]}},\"finishReason\":\"{}\",\"index\":0}}],\"usageMetadata\":{{\"promptTokenCount\":{},\"candidatesTokenCount\":{},\"totalTokenCount\":{}}}}}\n\n",
                text, finish_reason, prompt_tokens, completion_tokens, prompt_tokens + completion_tokens
            ));
        } else {
            sse.push_str(&format!(
                "data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"{text}\"}}]}},\"index\":0}}]}}\n\n"
            ));
        }
    }

    sse
}

// ---------------------------------------------------------------------------
// 1. Happy path: 3 text chunks + final chunk with finishReason+usage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_happy_path() {
    let server = MockServer::start();

    let sse_body = text_sse_stream(&["Hello", " ", "world!"], "STOP", (10, 3));

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:streamGenerateContent")
            .query_param("alt", "sse");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request(), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    // First chunk carries role=assistant.
    assert!(!chunks.is_empty(), "expected at least one chunk");
    let first_delta = &chunks[0].choices[0].delta;
    assert_eq!(
        first_delta.role.as_deref(),
        Some("assistant"),
        "first chunk should have role=assistant"
    );

    // Last chunk with finishReason should have usage.
    let finish_chunk = chunks
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("should have a chunk with finish_reason");
    assert_eq!(
        finish_chunk.choices[0].finish_reason.as_deref(),
        Some("stop")
    );
    assert!(
        finish_chunk.usage.is_some(),
        "finish chunk should carry usage"
    );
    let usage = finish_chunk.usage.as_ref().unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 3);

    assert_json_snapshot!(
        "stream_happy_path_chunks",
        &chunks,
        { "[].created" => "[timestamp]", "[].id" => "[id]" }
    );
}

// ---------------------------------------------------------------------------
// 2. Chunk fragmentation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_chunk_fragmentation() {
    let server = MockServer::start();

    // Two events with text. The SSE body includes fragmented text.
    let sse_body = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Frag\"}]},\"index\":0}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"mented\"}]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:streamGenerateContent");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request(), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    // Find text content chunks.
    let text_chunks: Vec<&str> = chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.content.as_deref())
        .collect();

    assert!(
        text_chunks.contains(&"Frag"),
        "should have 'Frag' chunk, got: {text_chunks:?}"
    );
    assert!(
        text_chunks.contains(&"mented"),
        "should have 'mented' chunk, got: {text_chunks:?}"
    );

    assert_json_snapshot!(
        "stream_fragmentation_chunks",
        &chunks,
        { "[].created" => "[timestamp]", "[].id" => "[id]" }
    );
}

// ---------------------------------------------------------------------------
// 3. Function-call streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_function_call_in_chunk() {
    let server = MockServer::start();

    // Function call arrives in one chunk.
    let sse_body = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"location\":\"London\"}}}]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":20,\"candidatesTokenCount\":10,\"totalTokenCount\":30}}\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:streamGenerateContent");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request(), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    // Should have tool_call delta chunks.
    let tool_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| !c.choices[0].delta.tool_calls.is_empty())
        .collect();

    assert!(
        !tool_chunks.is_empty(),
        "expected tool call delta chunks, got none"
    );

    let call = &tool_chunks[0].choices[0].delta.tool_calls[0];
    assert_eq!(call.function.name, "get_weather");
    let args: serde_json::Value =
        serde_json::from_str(&call.function.arguments).expect("valid json");
    assert_eq!(args["location"], "London");

    // finish_reason should be "tool_calls" because of the functionCall.
    let finish_chunk = chunks
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("should have finish chunk");
    assert_eq!(
        finish_chunk.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );

    assert_json_snapshot!(
        "stream_function_call_chunks",
        &chunks,
        {
            "[].created" => "[timestamp]",
            "[].id" => "[id]",
            "[].choices[0].delta.tool_calls[0].id" => "[call_id]"
        }
    );
}

// ---------------------------------------------------------------------------
// 4. Mid-stream malformed event → Err yielded, continues
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_midstream_malformed_event_continues() {
    let server = MockServer::start();

    let sse_body = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Before\"}]},\"index\":0}]}\n\n",
        // Malformed event.
        "data: {not valid json at all}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"After\"}]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:streamGenerateContent");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request(), &ctx)
        .await
        .expect("should return stream");

    let mut ok_chunks = Vec::new();
    let mut err_chunks: Vec<String> = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(c) => ok_chunks.push(c),
            Err(e) => err_chunks.push(format!("{e}")),
        }
    }

    // Should have at least one error.
    assert!(
        !err_chunks.is_empty(),
        "expected at least one error, got none"
    );

    // Should still have "Before" and "After" text chunks.
    let texts: Vec<&str> = ok_chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.content.as_deref())
        .collect();

    assert!(
        texts.contains(&"Before"),
        "'Before' chunk expected, got: {texts:?}"
    );
    assert!(
        texts.contains(&"After"),
        "'After' chunk expected, got: {texts:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Early HTTP error (401 before any stream byte) → returns Err directly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_early_http_error_returns_err_directly() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:streamGenerateContent");
        then.status(401)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"code":401,"message":"API key not valid","status":"UNAUTHENTICATED"}}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let result = provider()
        .chat_completion_stream(stream_request(), &ctx)
        .await;

    assert!(result.is_err(), "expected Err for 401 before stream");
    match result.err().expect("error") {
        ProviderError::Unauthorized(_) => {}
        other => panic!("expected Unauthorized, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 6. Server closes without finishReason → stream ends cleanly without panic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_server_closes_without_finish_reason() {
    let server = MockServer::start();

    // Stream ends without finishReason in any event.
    let sse_body = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]},\"index\":0}]}\n\n",
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\" there\"}]},\"index\":0}]}\n\n"
        // No finishReason — server just closes the connection.
    );

    let _mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1beta/models/gemini-3.1-pro:streamGenerateContent");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request(), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    // This must complete without panic.
    while let Some(result) = stream.next().await {
        // Tolerate errors; we only assert the stream completes without panic.
        if let Ok(c) = result {
            chunks.push(c);
        }
    }

    // Stream ended cleanly — we got some chunks.
    let texts: Vec<&str> = chunks
        .iter()
        .filter_map(|c| c.choices[0].delta.content.as_deref())
        .collect();

    assert!(
        texts.contains(&"Hello"),
        "expected 'Hello' chunk, got: {texts:?}"
    );
    assert!(
        texts.contains(&" there"),
        "expected ' there' chunk, got: {texts:?}"
    );

    // No panics — test passes by completing.
}
