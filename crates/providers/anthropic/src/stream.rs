//! SSE parser and streaming chat completion for the Anthropic adapter.
//!
//! Anthropic uses typed SSE events rather than OpenAI's simple `data:` lines.
//! Each event has both an `event:` line and a `data:` line with a JSON object
//! whose `type` field matches the event name.
//!
//! # Event sequence
//!
//! ```text
//! event: message_start
//! data: {"type":"message_start","message":{...}}
//!
//! event: content_block_start
//! data: {"type":"content_block_start","index":0,"content_block":{...}}
//!
//! event: content_block_delta
//! data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"..."}}
//!
//! event: content_block_stop
//! data: {"type":"content_block_stop","index":0}
//!
//! event: message_delta
//! data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":N}}
//!
//! event: message_stop
//! data: {"type":"message_stop"}
//!
//! event: ping
//! data: {"type":"ping"}
//! ```
//!
//! Tool use streams `input_json_delta` events. Their partial JSON fragments are
//! accumulated into the tool call's `arguments` field.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use std::pin::Pin;
use tt_shared::{
    filter_extra_headers,
    messages::{ChunkChoice, ChunkDelta, ToolCall, ToolCallFunction},
    ChatCompletionChunk, ChatCompletionRequest, ProviderError, RequestContext, Usage,
};

use crate::errors::{map_reqwest_error, map_response_error};
use crate::translate;

// ---------------------------------------------------------------------------
// Anthropic SSE event shapes
// ---------------------------------------------------------------------------

/// The discriminator `type` field shared by all Anthropic SSE events.
#[derive(Debug, Deserialize)]
struct EventEnvelope {
    #[serde(rename = "type")]
    event_type: String,
}

/// `message_start` — carries the initial `id` and `model`.
#[derive(Debug, Deserialize)]
struct MessageStartEvent {
    message: MessageStartMessage,
}

/// Inner `message` object within `message_start`.
#[derive(Debug, Deserialize)]
struct MessageStartMessage {
    id: String,
    model: String,
    #[serde(default)]
    usage: Option<translate::AnthropicUsage>,
}

/// `content_block_start` — signals a new content block beginning.
#[derive(Debug, Deserialize)]
struct ContentBlockStartEvent {
    #[allow(dead_code)]
    index: u32,
    content_block: ContentBlockType,
}

/// The `content_block` field in `content_block_start`.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockType {
    Text,
    ToolUse { id: String, name: String },
}

/// `content_block_delta` — a streaming chunk of content.
#[derive(Debug, Deserialize)]
struct ContentBlockDeltaEvent {
    #[allow(dead_code)]
    index: u32,
    delta: ContentDelta,
}

/// Delta variants for content blocks.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentDelta {
    /// Plain text increment.
    TextDelta { text: String },
    /// Tool input JSON fragment.
    InputJsonDelta { partial_json: String },
}

/// `message_delta` — carries `stop_reason` and output token count.
#[derive(Debug, Deserialize)]
struct MessageDeltaEvent {
    delta: MessageDeltaInner,
    #[serde(default)]
    usage: Option<MessageDeltaUsage>,
}

/// The `delta` field inside `message_delta`.
#[derive(Debug, Deserialize)]
struct MessageDeltaInner {
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Partial usage in `message_delta`.
#[derive(Debug, Deserialize)]
struct MessageDeltaUsage {
    #[serde(default)]
    output_tokens: u64,
}

// ---------------------------------------------------------------------------
// State accumulated across events
// ---------------------------------------------------------------------------

/// Mutable state that persists across Anthropic SSE events within one stream.
struct StreamState {
    id: String,
    model: String,
    created: i64,
    /// Input tokens from `message_start`.
    input_tokens: u64,
    /// Prompt-cache read tokens from `message_start` (billed at the cached
    /// rate). `None` when the provider reported no cache-read figure at all —
    /// kept raw so telemetry can distinguish "reported zero" from "didn't
    /// report"; arithmetic folds with `.unwrap_or(0)`.
    cache_read_input_tokens: Option<u64>,
    /// Prompt-cache creation tokens from `message_start`.
    cache_creation_input_tokens: Option<u64>,
    /// Output tokens accumulated from `message_delta`.
    output_tokens: u64,
    /// The tool call currently being assembled (index → partial).
    current_tool: Option<PartialToolCall>,
    /// Final stop_reason from `message_delta`.
    stop_reason: Option<String>,
}

/// A partially assembled tool call, built from `content_block_start` + multiple
/// `input_json_delta` events.
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// A `BoxStream` alias used by this module.
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, ProviderError>> + Send>>;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Build and execute a streaming Anthropic chat completion request.
///
/// Returns `Err` before yielding any chunk if the HTTP response status ≥ 400.
/// On success returns a `BoxStream` of [`ChatCompletionChunk`] values translated
/// from Anthropic's typed SSE events.
pub async fn stream_chat_completion(
    client: Client,
    base_url: &str,
    req: ChatCompletionRequest,
    ctx: &RequestContext,
) -> Result<ChunkStream, ProviderError> {
    let url = format!("{base_url}/v1/messages");
    let api_key = ctx.credentials.api_key.expose().to_string();
    let extra_headers: Vec<(String, String)> = filter_extra_headers(&ctx.credentials.extra_headers);

    // Translate to the Anthropic wire shape with stream = true.
    let mut translated = translate::translate_request(req)?;
    translated.stream = true;

    let body_bytes = serde_json::to_vec(&translated)
        .map_err(|e| ProviderError::Internal(format!("failed to serialize stream body: {e}")))?;

    let mut request_builder = client
        .post(&url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .body(body_bytes);

    for (name, value) in &extra_headers {
        request_builder = request_builder.header(name, value);
    }

    let response = request_builder.send().await.map_err(map_reqwest_error)?;

    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if status >= 400 {
        let body_text = response.text().await.map_err(map_reqwest_error)?;
        return Err(map_response_error(
            status,
            &body_text,
            retry_after.as_deref(),
        ));
    }

    let bytes_stream = response.bytes_stream();
    let stream = build_sse_stream(bytes_stream);
    Ok(Box::pin(stream))
}

// ---------------------------------------------------------------------------
// SSE stream parser
// ---------------------------------------------------------------------------

/// Parse an Anthropic SSE byte stream into a stream of [`ChatCompletionChunk`].
///
/// Handles:
/// - Buffering across fragmented `bytes_stream` chunks.
/// - Typed events (`message_start`, `content_block_delta`, `message_delta`, etc.)
/// - `ping` heartbeats silently skipped.
/// - `message_stop` terminates the stream.
/// - Mid-stream JSON parse failures → `Err(ProviderError::Deserialize)`.
fn build_sse_stream<S>(
    bytes_stream: S,
) -> impl Stream<Item = Result<ChatCompletionChunk, ProviderError>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut buffer: Vec<u8> = Vec::new();
        let mut state: Option<StreamState> = None;
        futures::pin_mut!(bytes_stream);

        loop {
            let next_item: Option<Result<Bytes, reqwest::Error>> = bytes_stream.next().await;
            match next_item {
                Some(Ok(chunk)) => {
                    buffer.extend_from_slice(&chunk);
                    // Process all complete SSE events (delimited by \n\n or \r\n\r\n).
                    while let Some((event_end, sep_len)) = find_event_boundary(&buffer) {
                        let event_bytes = buffer.drain(..event_end + sep_len).collect::<Vec<_>>();
                        match process_sse_event(&event_bytes, &mut state) {
                            SseOutcome::Chunk(c) => yield Ok(c),
                            SseOutcome::Err(e) => yield Err(e),
                            SseOutcome::Done => return,
                            SseOutcome::Skip => {}
                        }
                    }
                }
                Some(Err(e)) => {
                    yield Err(map_reqwest_error(e));
                    return;
                }
                None => {
                    // Upstream closed — flush remaining buffer.
                    if !buffer.is_empty() {
                        match process_sse_event(&buffer, &mut state) {
                            SseOutcome::Chunk(c) => yield Ok(c),
                            SseOutcome::Err(e) => yield Err(e),
                            SseOutcome::Done | SseOutcome::Skip => {}
                        }
                    }
                    return;
                }
            }
        }
    }
}

/// Outcome of processing one SSE event block.
enum SseOutcome {
    Chunk(ChatCompletionChunk),
    Done,
    Err(ProviderError),
    Skip,
}

/// Parse and handle a single SSE event (bytes up to but not including `\n\n`).
fn process_sse_event(event_bytes: &[u8], state: &mut Option<StreamState>) -> SseOutcome {
    let text = match std::str::from_utf8(event_bytes) {
        Ok(t) => t,
        Err(_) => {
            return SseOutcome::Err(ProviderError::Deserialize(
                "SSE event contained invalid UTF-8".to_string(),
            ))
        }
    };

    // Extract the `data:` line and optionally the `event:` line.
    let mut event_type: Option<&str> = None;
    let mut data_line: Option<&str> = None;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(ev) = line
            .strip_prefix("event:")
            .map(|s| s.strip_prefix(' ').unwrap_or(s))
        {
            event_type = Some(ev.trim());
        } else if let Some(data) = line
            .strip_prefix("data:")
            .map(|s| s.strip_prefix(' ').unwrap_or(s))
        {
            data_line = Some(data.trim());
        }
    }

    let data = match data_line {
        Some(d) => d,
        None => return SseOutcome::Skip,
    };

    // Determine event type from either the `event:` line or the JSON `type` field.
    let ev_type = match event_type {
        Some(t) => t.to_string(),
        None => {
            // Fall back to parsing the `type` field from data JSON.
            match serde_json::from_str::<EventEnvelope>(data) {
                Ok(env) => env.event_type,
                Err(e) => {
                    return SseOutcome::Err(ProviderError::Deserialize(format!(
                        "could not determine event type: {e}"
                    )))
                }
            }
        }
    };

    match ev_type.as_str() {
        "ping" => SseOutcome::Skip,
        "message_start" => handle_message_start(data, state),
        "content_block_start" => handle_content_block_start(data, state),
        "content_block_delta" => handle_content_block_delta(data, state),
        "content_block_stop" => handle_content_block_stop(state),
        "message_delta" => handle_message_delta(data, state),
        "message_stop" => SseOutcome::Done,
        "error" => SseOutcome::Err(ProviderError::Deserialize(format!(
            "mid-stream error event: {data}"
        ))),
        _ => SseOutcome::Skip,
    }
}

// ---------------------------------------------------------------------------
// Individual event handlers
// ---------------------------------------------------------------------------

fn handle_message_start(data: &str, state: &mut Option<StreamState>) -> SseOutcome {
    let ev: MessageStartEvent = match serde_json::from_str(data) {
        Ok(e) => e,
        Err(e) => {
            return SseOutcome::Err(ProviderError::Deserialize(format!(
                "message_start parse error: {e}"
            )))
        }
    };

    let usage = ev.message.usage.as_ref();
    let input_tokens = usage.map(|u| u.input_tokens).unwrap_or(0);
    let cache_read_input_tokens = usage.and_then(|u| u.cache_read_input_tokens);
    let cache_creation_input_tokens = usage.and_then(|u| u.cache_creation_input_tokens);

    let new_state = StreamState {
        id: ev.message.id.clone(),
        model: ev.message.model.clone(),
        created: chrono::Utc::now().timestamp(),
        input_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        output_tokens: 0,
        current_tool: None,
        stop_reason: None,
    };

    let chunk = ChatCompletionChunk {
        id: new_state.id.clone(),
        object: "chat.completion.chunk".to_string(),
        created: new_state.created,
        model: new_state.model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: Some("assistant".to_string()),
                content: None,
                tool_calls: vec![],
                extra: Default::default(),
            },
            finish_reason: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
    };

    *state = Some(new_state);
    SseOutcome::Chunk(chunk)
}

fn handle_content_block_start(data: &str, state: &mut Option<StreamState>) -> SseOutcome {
    let ev: ContentBlockStartEvent = match serde_json::from_str(data) {
        Ok(e) => e,
        Err(e) => {
            return SseOutcome::Err(ProviderError::Deserialize(format!(
                "content_block_start parse error: {e}"
            )))
        }
    };

    if let Some(st) = state.as_mut() {
        match ev.content_block {
            ContentBlockType::ToolUse { id, name } => {
                st.current_tool = Some(PartialToolCall {
                    id,
                    name,
                    arguments: String::new(),
                });
            }
            ContentBlockType::Text => {
                st.current_tool = None;
            }
        }
    }

    SseOutcome::Skip
}

fn handle_content_block_delta(data: &str, state: &mut Option<StreamState>) -> SseOutcome {
    let ev: ContentBlockDeltaEvent = match serde_json::from_str(data) {
        Ok(e) => e,
        Err(e) => {
            return SseOutcome::Err(ProviderError::Deserialize(format!(
                "content_block_delta parse error: {e}"
            )))
        }
    };

    let st = match state.as_mut() {
        Some(s) => s,
        None => {
            return SseOutcome::Err(ProviderError::Deserialize(
                "content_block_delta received before message_start".to_string(),
            ))
        }
    };

    match ev.delta {
        ContentDelta::TextDelta { text } => {
            let chunk = ChatCompletionChunk {
                id: st.id.clone(),
                object: "chat.completion.chunk".to_string(),
                created: st.created,
                model: st.model.clone(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: None,
                        content: Some(text),
                        tool_calls: vec![],
                        extra: Default::default(),
                    },
                    finish_reason: None,
                    extra: Default::default(),
                }],
                usage: None,
                extra: Default::default(),
            };
            SseOutcome::Chunk(chunk)
        }
        ContentDelta::InputJsonDelta { partial_json } => {
            if let Some(tc) = st.current_tool.as_mut() {
                tc.arguments.push_str(&partial_json);
                let tool_call = ToolCall {
                    id: tc.id.clone(),
                    r#type: "function".to_string(),
                    function: ToolCallFunction {
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    },
                };
                let chunk = ChatCompletionChunk {
                    id: st.id.clone(),
                    object: "chat.completion.chunk".to_string(),
                    created: st.created,
                    model: st.model.clone(),
                    choices: vec![ChunkChoice {
                        index: 0,
                        delta: ChunkDelta {
                            role: None,
                            content: None,
                            tool_calls: vec![tool_call],
                            extra: Default::default(),
                        },
                        finish_reason: None,
                        extra: Default::default(),
                    }],
                    usage: None,
                    extra: Default::default(),
                };
                SseOutcome::Chunk(chunk)
            } else {
                SseOutcome::Skip
            }
        }
    }
}

fn handle_content_block_stop(state: &mut Option<StreamState>) -> SseOutcome {
    if let Some(st) = state.as_mut() {
        st.current_tool = None;
    }
    SseOutcome::Skip
}

fn handle_message_delta(data: &str, state: &mut Option<StreamState>) -> SseOutcome {
    let ev: MessageDeltaEvent = match serde_json::from_str(data) {
        Ok(e) => e,
        Err(e) => {
            return SseOutcome::Err(ProviderError::Deserialize(format!(
                "message_delta parse error: {e}"
            )))
        }
    };

    let st = match state.as_mut() {
        Some(s) => s,
        None => {
            return SseOutcome::Err(ProviderError::Deserialize(
                "message_delta received before message_start".to_string(),
            ))
        }
    };

    if let Some(u) = &ev.usage {
        st.output_tokens += u.output_tokens;
    }
    st.stop_reason = ev.delta.stop_reason.clone();

    let finish_reason = ev
        .delta
        .stop_reason
        .as_deref()
        .map(translate::map_stop_reason)
        .map(str::to_string);

    // Mirror the non-streaming `translate::translate_usage` mapping so streamed
    // and non-streamed Claude calls report identical usage: prompt_tokens
    // INCLUDES cache reads (OpenAI subset convention) and cached_tokens is the
    // cache-read subset — instead of input_tokens-only with cache reads zeroed.
    let prompt_tokens = st.input_tokens
        + st.cache_read_input_tokens.unwrap_or(0)
        + st.cache_creation_input_tokens.unwrap_or(0);
    let usage = Usage {
        prompt_tokens,
        completion_tokens: st.output_tokens,
        total_tokens: prompt_tokens + st.output_tokens,
        cached_tokens: st.cache_read_input_tokens.unwrap_or(0),
        cache_creation_input_tokens: st.cache_creation_input_tokens,
        // Raw Option preserved for telemetry NULL-vs-0 semantics.
        cache_read_input_tokens: st.cache_read_input_tokens,
    };

    let chunk = ChatCompletionChunk {
        id: st.id.clone(),
        object: "chat.completion.chunk".to_string(),
        created: st.created,
        model: st.model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason,
            extra: Default::default(),
        }],
        usage: Some(usage),
        extra: Default::default(),
    };

    SseOutcome::Chunk(chunk)
}

// ---------------------------------------------------------------------------
// Buffer utilities
// ---------------------------------------------------------------------------

/// Find the first SSE event boundary in `buf`.
///
/// Returns `(offset, sep_len)` where `offset` is the index of the first byte
/// of the boundary sequence and `sep_len` is the number of bytes in the
/// separator (`2` for `\n\n`, `4` for `\r\n\r\n`).  Callers should drain
/// `..offset + sep_len` to consume the full event including its terminator.
///
/// Both `\n\n` and `\r\n\r\n` are valid SSE event terminators per RFC 8898.
fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((pos, 4));
    }
    if let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
        return Some((pos, 2));
    }
    None
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_event_boundary_lf() {
        let buf = b"event: ping\ndata: {}\n\nevent: message_stop\ndata: {}\n\n";
        assert_eq!(find_event_boundary(buf), Some((20, 2)));
    }

    #[test]
    fn find_event_boundary_crlf() {
        let buf = b"event: ping\r\ndata: {}\r\n\r\nevent: message_stop\r\ndata: {}\r\n\r\n";
        assert_eq!(find_event_boundary(buf), Some((21, 4)));
    }

    #[test]
    fn find_event_boundary_none() {
        let buf = b"event: ping\ndata: {}\n";
        assert_eq!(find_event_boundary(buf), None);
    }

    #[test]
    fn ping_event_skipped() {
        let event = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
        let mut state = None;
        let outcome = process_sse_event(event, &mut state);
        assert!(matches!(outcome, SseOutcome::Skip));
    }

    #[test]
    fn message_start_initializes_state() {
        let event = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"role\":\"assistant\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n";
        let mut state = None;
        let outcome = process_sse_event(event, &mut state);
        assert!(matches!(outcome, SseOutcome::Chunk(_)));
        let st = state.expect("state initialized");
        assert_eq!(st.id, "msg_01");
        assert_eq!(st.model, "claude-sonnet-4-6");
        assert_eq!(st.input_tokens, 10);
    }

    #[test]
    fn malformed_data_yields_deserialize_error() {
        let event = b"event: content_block_delta\ndata: {not valid json}\n\n";
        let mut state = Some(StreamState {
            id: "msg_01".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            created: 0,
            input_tokens: 10,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            output_tokens: 0,
            current_tool: None,
            stop_reason: None,
        });
        let outcome = process_sse_event(event, &mut state);
        assert!(matches!(outcome, SseOutcome::Err(_)));
    }

    #[test]
    fn message_stop_returns_done() {
        let event = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut state = None;
        let outcome = process_sse_event(event, &mut state);
        assert!(matches!(outcome, SseOutcome::Done));
    }

    // --- CRLF + no-space data: regression tests (rv-sse-crlf-parsing) ---

    #[test]
    fn ping_event_skipped_crlf() {
        // Same ping event, but with CRLF line endings and \r\n\r\n terminator.
        let event = b"event: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\n";
        let mut state = None;
        let outcome = process_sse_event(event, &mut state);
        assert!(
            matches!(outcome, SseOutcome::Skip),
            "CRLF-delimited ping event should still be skipped"
        );
    }

    #[test]
    fn message_stop_crlf() {
        // message_stop with CRLF terminators must still return Done.
        let event = b"event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n";
        let mut state = None;
        let outcome = process_sse_event(event, &mut state);
        assert!(
            matches!(outcome, SseOutcome::Done),
            "CRLF message_stop must return Done"
        );
    }

    #[test]
    fn no_space_data_prefix_anthropic() {
        // data:{...} without a space after the colon must not be silently dropped.
        let event = b"event: message_stop\ndata:{\"type\":\"message_stop\"}\n\n";
        let mut state = None;
        let outcome = process_sse_event(event, &mut state);
        assert!(
            matches!(outcome, SseOutcome::Done),
            "data:{{...}} (no space) must parse the same as `data: {{...}}`"
        );
    }

    #[test]
    fn message_delta_reports_cache_tokens_from_message_start() {
        // Anthropic reports prompt-cache reads/creations in the `message_start`
        // usage block. The terminal usage must surface them — previously
        // `cached_tokens` was hardcoded to 0, zeroing cache savings on every
        // streamed Claude call.
        let start = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"role\":\"assistant\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0,\"cache_read_input_tokens\":80,\"cache_creation_input_tokens\":20}}}\n\n";
        let mut state = None;
        let _ = process_sse_event(start, &mut state);

        let delta = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n";
        let outcome = process_sse_event(delta, &mut state);
        let chunk = if let SseOutcome::Chunk(c) = outcome {
            c
        } else {
            panic!("expected terminal chunk from message_delta");
        };
        let usage = chunk.usage.expect("terminal chunk carries usage");
        assert_eq!(
            usage.cached_tokens, 80,
            "cache reads must be reported, not zeroed"
        );
        assert_eq!(usage.cache_creation_input_tokens, Some(20));
        // Raw cache-read count preserved unfolded for telemetry NULL-vs-0.
        assert_eq!(usage.cache_read_input_tokens, Some(80));
        // prompt_tokens is the FULL input: 10 fresh + 80 cache-read + 20 create = 110.
        assert_eq!(usage.prompt_tokens, 110);
    }

    #[test]
    fn message_delta_without_cache_report_keeps_raw_none() {
        // message_start usage with NO cache fields: the terminal usage must
        // carry raw `cache_read_input_tokens: None` (provider didn't report) —
        // not Some(0) — while the folded `cached_tokens` stays 0.
        let start = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_01\",\"model\":\"claude-sonnet-4-6\",\"content\":[],\"role\":\"assistant\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n";
        let mut state = None;
        let _ = process_sse_event(start, &mut state);

        let delta = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n";
        let outcome = process_sse_event(delta, &mut state);
        let chunk = if let SseOutcome::Chunk(c) = outcome {
            c
        } else {
            panic!("expected terminal chunk from message_delta");
        };
        let usage = chunk.usage.expect("terminal chunk carries usage");
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, None);
        assert_eq!(usage.cache_creation_input_tokens, None);
    }
}
