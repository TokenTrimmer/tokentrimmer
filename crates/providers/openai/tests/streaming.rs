//! End-to-end streaming tests for the OpenAI adapter.
//!
//! Uses [`httpmock`] to serve pre-canned SSE bodies and verifies that
//! the streaming parser reassembles fragments, skips comments, handles
//! `[DONE]`, and propagates errors correctly.

use std::collections::HashMap;

use futures::StreamExt;
use httpmock::prelude::*;
use insta::assert_json_snapshot;
use tt_provider_openai::{ClientConfig, OpenAiProvider};
use tt_shared::{
    context::{ProviderCredentials, RequestContext, SecretString},
    messages::{Message, MessageContent},
    ChatCompletionRequest, Provider, ProviderError,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers (mirrors integration.rs)
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

fn stream_request(model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![Message::User {
            content: MessageContent::Text("Hello".to_string()),
            name: None,
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(64),
        stream: false, // adapter sets this to true automatically
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

fn provider() -> OpenAiProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    OpenAiProvider::new_allow_local(ClientConfig::default())
}

/// Build a minimal `ChatCompletionChunk` JSON line (as a `data:` SSE event).
fn chunk_event(id: &str, content: &str, finish_reason: Option<&str>) -> String {
    let finish = finish_reason
        .map(|r| format!(r#""{r}""#))
        .unwrap_or_else(|| "null".to_string());
    format!(
        "data: {{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,\
         \"model\":\"gpt-4o\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}},\
         \"finish_reason\":{finish}}}]}}\n\n"
    )
}

/// Build a role-only first chunk (assistant delta with role).
fn role_chunk_event(id: &str) -> String {
    format!(
        "data: {{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,\
         \"model\":\"gpt-4o\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}},\
         \"finish_reason\":null}}]}}\n\n"
    )
}

/// Build the final usage chunk (empty choices, usage object).
fn usage_chunk_event(id: &str) -> String {
    format!(
        "data: {{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,\
         \"model\":\"gpt-4o\",\"choices\":[],\
         \"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12,\"cached_tokens\":0}}}}\n\n"
    )
}

fn done_event() -> &'static str {
    "data: [DONE]\n\n"
}

// ---------------------------------------------------------------------------
// Test 1: Happy path — role chunk + content chunks + usage chunk + [DONE]
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_happy_path() {
    let server = MockServer::start();

    let sse_body = format!(
        "{}{}{}{}{}{}",
        role_chunk_event("chatcmpl-1"),
        chunk_event("chatcmpl-1", "Hello", None),
        chunk_event("chatcmpl-1", " world", None),
        chunk_event("chatcmpl-1", "!", Some("stop")),
        usage_chunk_event("chatcmpl-1"),
        done_event(),
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    // 1 role + 3 content + 1 usage = 5 chunks
    assert_eq!(chunks.len(), 5, "expected 5 chunks, got {}", chunks.len());

    // First chunk has role delta.
    assert_eq!(
        chunks[0].choices[0].delta.role.as_deref(),
        Some("assistant")
    );

    // Second chunk has first content.
    assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("Hello"));

    // Fourth chunk has finish_reason = stop.
    assert_eq!(chunks[3].choices[0].finish_reason.as_deref(), Some("stop"));

    // Last chunk (index 4) is the usage-only chunk: no choices content, usage present.
    assert!(chunks[4].usage.is_some(), "last chunk should carry usage");
    let usage = chunks[4].usage.as_ref().unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 2);
    assert_eq!(usage.total_tokens, 12);

    // Insta snapshot of the full collected chunks.
    assert_json_snapshot!("stream_happy_path_chunks", &chunks);
}

// ---------------------------------------------------------------------------
// Test 2: Chunk fragmentation — SSE events arrive split across TCP frames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_chunk_fragmentation() {
    // Build the SSE body and then split it into 3 unequal byte chunks so the
    // httpmock delivers them one at a time. httpmock sends the body as one
    // response, but we can test the parser with a manual bytes approach by
    // splitting the body ourselves and verifying reassembly produces the right
    // number of parsed chunks.
    //
    // httpmock doesn't provide chunked transfer encoding control at this API
    // level, so we verify fragmentation by constructing a body where the
    // double-\n\n boundary deliberately falls at different split points.

    let server = MockServer::start();

    // Two events that we will split manually in a unit style assertion,
    // but here we verify the HTTP round-trip correctly reconstitutes both events.
    let part1 = chunk_event("chatcmpl-2", "Frag", None);
    let part2 = format!(
        "{}{}",
        chunk_event("chatcmpl-2", "mented", Some("stop")),
        done_event()
    );

    let sse_body = format!("{part1}{part2}");

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("Frag"));
    assert_eq!(
        chunks[1].choices[0].delta.content.as_deref(),
        Some("mented")
    );
    assert_eq!(chunks[1].choices[0].finish_reason.as_deref(), Some("stop"));
}

// ---------------------------------------------------------------------------
// Test 3: Heartbeat comments are skipped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_heartbeat_comments_skipped() {
    let server = MockServer::start();

    let sse_body = format!(
        "{}:keep-alive\n\n{}:keep-alive\n\n{}{}",
        chunk_event("chatcmpl-3", "A", None),
        chunk_event("chatcmpl-3", "B", Some("stop")),
        usage_chunk_event("chatcmpl-3"),
        done_event(),
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    // Comments must not produce extra chunks.
    assert_eq!(chunks.len(), 3, "expected 3 chunks, got {}", chunks.len());
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("A"));
    assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("B"));
    assert!(chunks[2].usage.is_some());

    assert_json_snapshot!("stream_heartbeat_chunks", &chunks);
}

// ---------------------------------------------------------------------------
// Test 4: [DONE] terminates the stream cleanly — no error chunk emitted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_done_terminates_cleanly() {
    let server = MockServer::start();

    let sse_body = format!(
        "{}{}",
        chunk_event("chatcmpl-4", "Hi", Some("stop")),
        done_event(),
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    let mut errors: Vec<ProviderError> = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(c) => chunks.push(c),
            Err(e) => errors.push(e),
        }
    }

    assert!(errors.is_empty(), "no errors expected, got: {errors:?}");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("Hi"));
}

// ---------------------------------------------------------------------------
// Test 5: Early HTTP error (401) — Err returned before stream opens
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_early_http_401_error() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(401)
            .header("Content-Type", "application/json")
            .body(r#"{"error":{"message":"Invalid API key","type":"invalid_api_key","code":"invalid_api_key","param":null}}"#);
    });

    let ctx = make_ctx(&server.base_url());
    let result = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await;

    // Must fail before returning a stream.
    assert!(result.is_err(), "expected Err for 401");
    match result.err().expect("error") {
        ProviderError::Unauthorized(_) => {}
        other => panic!("expected Unauthorized, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 6: Mid-stream malformed JSON — yields Err, then continues
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_midstream_malformed_json_continues() {
    let server = MockServer::start();

    let sse_body = format!(
        "{}data: {{not valid json}}\n\n{}{}",
        chunk_event("chatcmpl-6", "Before", None),
        chunk_event("chatcmpl-6", "After", Some("stop")),
        done_event(),
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(c) => chunks.push(c),
            Err(e) => errors.push(format!("{e}")),
        }
    }

    // Should have received: "Before" chunk, one Deserialize error, "After" chunk.
    assert_eq!(chunks.len(), 2, "expected 2 valid chunks");
    assert_eq!(errors.len(), 1, "expected 1 deserialize error");
    assert!(
        errors[0].contains("deserialize"),
        "error should mention deserialize: {}",
        errors[0]
    );
    assert_eq!(
        chunks[0].choices[0].delta.content.as_deref(),
        Some("Before")
    );
    assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("After"));
}

// ---------------------------------------------------------------------------
// Test 7: Tool-call delta chunks pass through correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_tool_call_delta() {
    let server = MockServer::start();

    // Real OpenAI shape: first frag carries id/type/name + empty args; the
    // continuations carry ONLY index + an `arguments` fragment (no id/name).
    let sse_body = concat!(
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,",
        "\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":",
        "[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,",
        "\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":",
        "[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-7\",\"object\":\"chat.completion.chunk\",\"created\":1716681600,",
        "\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":",
        "[{\"index\":0,\"function\":{\"arguments\":\"\\\"NYC\\\"}\"}}]},",
        "\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    // Fragments are accumulated and emitted as ONE complete tool-call chunk.
    let tool_chunks: Vec<_> = chunks
        .iter()
        .filter(|c| {
            c.choices
                .first()
                .is_some_and(|ch| !ch.delta.tool_calls.is_empty())
        })
        .collect();
    assert_eq!(
        tool_chunks.len(),
        1,
        "expected one reassembled tool-call chunk"
    );

    let tc = &tool_chunks[0].choices[0].delta.tool_calls;
    assert_eq!(tc.len(), 1);
    assert_eq!(tc[0].id, "call_abc");
    assert_eq!(tc[0].r#type, "function");
    assert_eq!(tc[0].function.name, "get_weather");
    assert_eq!(tc[0].function.arguments, "{\"city\":\"NYC\"}");
    assert_eq!(
        tool_chunks[0].choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );
}

#[tokio::test]
async fn stream_two_tool_calls_by_index() {
    let server = MockServer::start();
    let sse_body = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"fa\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"fb\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });
    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("stream");
    let mut tool_calls = Vec::new();
    while let Some(r) = stream.next().await {
        let c = r.expect("no error");
        if let Some(ch) = c.choices.first() {
            tool_calls.extend(ch.delta.tool_calls.clone());
        }
    }
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0].id, "call_a"); // index 0 first
    assert_eq!(tool_calls[1].id, "call_b");
}

#[tokio::test]
async fn stream_content_then_tool_call() {
    let server = MockServer::start();
    let sse_body = concat!(
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Let me check\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",",
        "\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });
    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("stream");
    let mut content = String::new();
    let mut tool_call_count = 0;
    while let Some(r) = stream.next().await {
        let c = r.expect("no error");
        if let Some(ch) = c.choices.first() {
            if let Some(t) = &ch.delta.content {
                content.push_str(t);
            }
            tool_call_count += ch.delta.tool_calls.len();
        }
    }
    assert_eq!(content, "Let me check", "content chunk still streams");
    assert_eq!(tool_call_count, 1, "tool call reassembled");
}

// ---------------------------------------------------------------------------
// Test 8: Reasoning models (o3 / o4-mini) stream like any other model
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_reasoning_models_supported() {
    // o3 / o4-mini DO support streaming via the chat-completions API — the
    // adapter must no longer short-circuit them with Unsupported.
    let server = MockServer::start();
    let body = format!(
        "{}{}{}data: [DONE]\n\n",
        role_chunk_event("chatcmpl-o3"),
        chunk_event("chatcmpl-o3", "Hi", Some("stop")),
        usage_chunk_event("chatcmpl-o3"),
    );
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(&body);
    });
    let ctx = make_ctx(&server.base_url());

    for model in ["o3", "o4-mini"] {
        let stream = provider()
            .chat_completion_stream(stream_request(model), &ctx)
            .await
            .unwrap_or_else(|e| panic!("model {model} should stream, got {e:?}"));
        let chunks: Vec<_> = stream.collect().await;
        assert!(
            chunks.iter().any(std::result::Result::is_ok),
            "model {model} should yield at least one chunk"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 9: Empty body / immediate connection close — no chunks, no error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_empty_body_no_chunks() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body("");
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream for empty body");

    let mut chunks = Vec::new();
    let mut errors: Vec<ProviderError> = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(c) => chunks.push(c),
            Err(e) => errors.push(e),
        }
    }

    assert!(chunks.is_empty(), "no chunks expected for empty body");
    assert!(errors.is_empty(), "no errors expected for empty body");
}

// ---------------------------------------------------------------------------
// Test 10: Multiple heartbeats interspersed — all skipped, correct chunk count
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_multiple_heartbeats() {
    let server = MockServer::start();

    let sse_body = format!(
        ":heartbeat\n\n\
         {}\
         :heartbeat\n\n\
         :heartbeat\n\n\
         {}\
         {}",
        chunk_event("chatcmpl-10", "X", None),
        chunk_event("chatcmpl-10", "Y", Some("stop")),
        done_event(),
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("Content-Type", "text/event-stream")
            .body(sse_body);
    });

    let ctx = make_ctx(&server.base_url());
    let mut stream = provider()
        .chat_completion_stream(stream_request("gpt-4o"), &ctx)
        .await
        .expect("should return stream");

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.expect("no stream error"));
    }

    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].choices[0].delta.content.as_deref(), Some("X"));
    assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some("Y"));
}
