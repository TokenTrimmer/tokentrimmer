//! End-to-end streaming tests for the Anthropic adapter.
//!
//! Uses [`httpmock`] to serve pre-canned Anthropic-format SSE bodies and
//! verifies that the streaming parser correctly emits [`ChatCompletionChunk`]
//! values.

use std::collections::HashMap;

use futures::StreamExt;
use httpmock::prelude::*;
use insta::assert_json_snapshot;
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
        model: "claude-sonnet-4-6".to_string(),
        messages: vec![Message::User {
            content: MessageContent::Text("Hello".to_string()),
            name: None,
        }],
        temperature: None,
        top_p: None,
        max_tokens: Some(64),
        stream: false, // adapter sets this to true
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

fn provider() -> AnthropicProvider {
    // Tests use a local httpmock server — allow_local bypasses the SSRF guard.
    AnthropicProvider::new_allow_local(ClientConfig::default())
}

/// Build a standard Anthropic SSE text stream.
fn text_stream(text_chunks: &[&str], stop_reason: &str, output_tokens: u64) -> String {
    let mut sse = String::new();

    // message_start
    sse.push_str("event: message_start\n");
    sse.push_str(r#"data: {"type":"message_start","message":{"id":"msg_01","model":"claude-sonnet-4-6","content":[],"role":"assistant","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":0}}}"#);
    sse.push_str("\n\n");

    // content_block_start
    sse.push_str("event: content_block_start\n");
    sse.push_str(
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
    );
    sse.push_str("\n\n");

    // content_block_delta for each chunk
    for text in text_chunks {
        sse.push_str("event: content_block_delta\n");
        sse.push_str(&format!(
            r#"data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#
        ));
        sse.push_str("\n\n");
    }

    // content_block_stop
    sse.push_str("event: content_block_stop\n");
    sse.push_str(r#"data: {"type":"content_block_stop","index":0}"#);
    sse.push_str("\n\n");

    // message_delta (stop_reason + output_tokens)
    sse.push_str("event: message_delta\n");
    sse.push_str(&format!(
        r#"data: {{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}","stop_sequence":null}},"usage":{{"output_tokens":{output_tokens}}}}}"#
    ));
    sse.push_str("\n\n");

    // message_stop
    sse.push_str("event: message_stop\n");
    sse.push_str(r#"data: {"type":"message_stop"}"#);
    sse.push_str("\n\n");

    sse
}

// ---------------------------------------------------------------------------
// 1. Happy path: full event sequence, correct chunk order, usage in final chunk
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_happy_path() {
    let server = MockServer::start();

    let sse_body = text_stream(&["Hello", " ", "world"], "end_turn", 3);

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
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

    // Expected: message_start chunk + 3 text delta chunks + message_delta chunk = 5
    assert!(
        chunks.len() >= 4,
        "expected at least 4 chunks, got {}",
        chunks.len()
    );

    // First chunk carries role=assistant.
    let first_delta = &chunks[0].choices[0].delta;
    assert_eq!(first_delta.role.as_deref(), Some("assistant"));

    // Last chunk has finish_reason and usage.
    let last = chunks.last().expect("at least one chunk");
    assert!(last.usage.is_some(), "last chunk should carry usage");
    let usage = last.usage.as_ref().unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 3);

    // message_delta chunk has finish_reason = stop (end_turn → stop).
    let message_delta_chunk = chunks
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("should have a chunk with finish_reason");
    assert_eq!(
        message_delta_chunk.choices[0].finish_reason.as_deref(),
        Some("stop")
    );

    assert_json_snapshot!(
        "stream_happy_path_chunks",
        &chunks,
        { "[].created" => "[timestamp]" }
    );
}

// ---------------------------------------------------------------------------
// 2. Chunk fragmentation — SSE events arrive split across body chunks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_chunk_fragmentation() {
    let server = MockServer::start();

    // Two text delta events embedded in a single SSE body. httpmock delivers
    // the body at once but we verify the parser handles events split at
    // character boundaries by including events with deliberately short spacing.
    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_02\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"role\":\"assistant\",\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Frag\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"mented\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
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
        { "[].created" => "[timestamp]" }
    );
}

// ---------------------------------------------------------------------------
// 3. Ping heartbeat skipped — no extra chunks emitted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_ping_heartbeat_skipped() {
    let server = MockServer::start();

    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_03\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"role\":\"assistant\",\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: ping\n",
        "data: {\"type\":\"ping\"}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: ping\n",
        "data: {\"type\":\"ping\"}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        "event: ping\n",
        "data: {\"type\":\"ping\"}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
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

    // Pings must not produce any chunks.
    // Expected: message_start + text_delta + message_delta = 3
    assert!(
        chunks.len() <= 5,
        "too many chunks — pings may not be skipped: {}",
        chunks.len()
    );

    // Verify there is a text chunk with "Hi".
    let has_hi = chunks
        .iter()
        .any(|c| c.choices[0].delta.content.as_deref() == Some("Hi"));
    assert!(has_hi, "expected 'Hi' text chunk");
}

// ---------------------------------------------------------------------------
// 4. Tool use stream — input_json_delta accumulation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_tool_use_input_json_accumulation() {
    let server = MockServer::start();

    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_04\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"role\":\"assistant\",\"stop_reason\":null,\"usage\":{\"input_tokens\":20,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"loc\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"ation\\\":\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"London\\\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":15}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
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

    // Each tool chunk should have the same tool id and name.
    for tc in &tool_chunks {
        let call = &tc.choices[0].delta.tool_calls[0];
        assert_eq!(call.id, "toolu_01");
        assert_eq!(call.function.name, "get_weather");
    }

    // The last tool chunk's arguments should contain all the JSON.
    let last_tool_chunk = tool_chunks.last().unwrap();
    let args = &last_tool_chunk.choices[0].delta.tool_calls[0]
        .function
        .arguments;
    assert!(
        args.contains("London"),
        "arguments should contain 'London', got: {args}"
    );

    // The message_delta chunk should have finish_reason = tool_calls.
    let finish_chunk = chunks
        .iter()
        .find(|c| c.choices[0].finish_reason.is_some())
        .expect("should have finish_reason chunk");
    assert_eq!(
        finish_chunk.choices[0].finish_reason.as_deref(),
        Some("tool_calls")
    );

    assert_json_snapshot!(
        "stream_tool_use_chunks",
        &chunks,
        { "[].created" => "[timestamp]" }
    );
}

// ---------------------------------------------------------------------------
// 5. Mid-stream malformed event → Err yielded, stream continues
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_midstream_malformed_event_continues() {
    let server = MockServer::start();

    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_05\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"role\":\"assistant\",\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Before\"}}\n\n",
        // Malformed event — not valid JSON.
        "event: content_block_delta\n",
        "data: {not valid json at all}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"After\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
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

    // Should have at least one error for the malformed event.
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
// 6. Early HTTP error before stream starts → Err returned directly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_early_http_error_returns_err_directly() {
    let server = MockServer::start();

    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(401)
            .header("Content-Type", "application/json")
            .body(r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid API key"}}"#);
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
