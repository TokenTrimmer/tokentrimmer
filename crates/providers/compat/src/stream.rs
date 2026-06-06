//! SSE parser and streaming chat completion for the OpenAI adapter.
//!
//! Provides [`stream_chat_completion`], which:
//! 1. Translates the canonical request, setting `stream: true` and requesting
//!    usage data on the final chunk.
//! 2. POSTs to `/chat/completions`.
//! 3. If the response status is ≥ 400, returns [`ProviderError`] before any
//!    chunk is yielded.
//! 4. Otherwise, returns a `BoxStream` that parses SSE events line by line and
//!    yields [`ChatCompletionChunk`] values.
//!
//! # SSE format
//!
//! Each event looks like:
//! ```text
//! data: {"id":"chatcmpl-1","object":"chat.completion.chunk",...}
//!
//! data: [DONE]
//!
//! ```
//!
//! Lines starting with `:` are heartbeat comments and are skipped silently.
//! The `[DONE]` sentinel closes the stream cleanly — it is never parsed as JSON.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::pin::Pin;
use tt_shared::{
    filter_extra_headers,
    messages::{ChunkChoice, ChunkDelta, ToolCall, ToolCallFunction},
    ChatCompletionChunk, ChatCompletionRequest, ProviderError, RequestContext, Usage,
};

use crate::errors::{map_reqwest_error, map_response_error};
use crate::translate;

/// Extra fields sent alongside the body for streaming requests.
///
/// The `stream_options` object asks OpenAI to include usage in the final chunk
/// so that callers can account for token consumption even when streaming.
#[derive(Debug, serde::Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Extended request body with streaming fields.
#[derive(Debug, serde::Serialize)]
struct OpenAiStreamBody {
    /// All standard fields come from the translated request body.
    #[serde(flatten)]
    inner: translate::OpenAiRequestBody,
    /// Options that control SSE stream behaviour.
    stream_options: StreamOptions,
}

/// A `BoxStream` alias used by this module.
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, ProviderError>> + Send>>;

/// Build and execute a streaming chat completion request.
///
/// Returns `Err` after the HTTP call (but before yielding any chunk) if the
/// response status is ≥ 400. Reasoning models (`o3`, `o4-mini`) stream like any
/// other model; `translate_request` handles their parameter quirks
/// (max_completion_tokens rename, temperature drop).
///
/// On success, returns a `BoxStream` that yields deserialized
/// [`ChatCompletionChunk`] values. Mid-stream errors (network drops, malformed
/// JSON) are yielded as `Err` items; the stream then closes.
pub async fn stream_chat_completion(
    client: Client,
    base_url: &str,
    req: ChatCompletionRequest,
    ctx: &RequestContext,
) -> Result<ChunkStream, ProviderError> {
    let url = format!("{base_url}/chat/completions");
    let api_key = ctx.credentials.api_key.expose().to_string();
    let extra_headers: Vec<(String, String)> = filter_extra_headers(&ctx.credentials.extra_headers);

    // Translate to the OpenAI wire shape, then override the stream flag.
    let mut translated = translate::translate_request(req)?;
    translated.stream = true;

    let body = OpenAiStreamBody {
        inner: translated,
        stream_options: StreamOptions {
            include_usage: true,
        },
    };

    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| ProviderError::Internal(format!("failed to serialize stream body: {e}")))?;

    let mut request_builder = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
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

/// Parse an SSE byte stream into a stream of [`ChatCompletionChunk`] values.
///
/// Handles:
/// - Buffering across fragmented `bytes_stream` chunks.
/// - `[DONE]` sentinel — closes the stream cleanly.
/// - Heartbeat comments (lines starting with `:`).
/// - Empty lines between events.
/// - Malformed JSON — yields `Err(ProviderError::Deserialize)` and continues.
/// - Network errors — yields `Err(ProviderError::Network)` and closes.
fn build_sse_stream<S>(
    bytes_stream: S,
) -> impl Stream<Item = Result<ChatCompletionChunk, ProviderError>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut buffer: Vec<u8> = Vec::new();
        let mut acc = ToolAccum::default();
        futures::pin_mut!(bytes_stream);

        loop {
            let next_item: Option<Result<Bytes, reqwest::Error>> = bytes_stream.next().await;
            match next_item {
                Some(Ok(chunk)) => {
                    buffer.extend_from_slice(&chunk);
                    // Process all complete SSE events (delimited by \n\n or \r\n\r\n).
                    while let Some((event_end, sep_len)) = find_event_boundary(&buffer) {
                        let event_bytes = buffer.drain(..event_end + sep_len).collect::<Vec<_>>();
                        // Each event may have multiple lines; process each.
                        let mut done = false;
                        for event in parse_sse_event(&event_bytes) {
                            match event {
                                SseEvent::Done => {
                                    // Flush any tool call that never got an explicit finish.
                                    if let Some(c) = acc.drain(Some("tool_calls".to_string()), None) {
                                        yield Ok(c);
                                    }
                                    done = true;
                                    break;
                                }
                                SseEvent::Chunk(raw) => {
                                    if let Some(c) = handle_raw_chunk(&mut acc, raw) {
                                        yield Ok(c);
                                    }
                                }
                                SseEvent::Err(e) => {
                                    yield Err(e);
                                    // Continue parsing remaining events (don't abort).
                                }
                                SseEvent::Skip => {}
                            }
                        }
                        if done {
                            return;
                        }
                    }
                }
                Some(Err(e)) => {
                    // Network-level error mid-stream.
                    yield Err(map_reqwest_error(e));
                    return;
                }
                None => {
                    // Upstream closed without [DONE] — flush remaining buffer then acc.
                    if !buffer.is_empty() {
                        for event in parse_sse_event(&buffer) {
                            match event {
                                SseEvent::Chunk(raw) => {
                                    if let Some(c) = handle_raw_chunk(&mut acc, raw) {
                                        yield Ok(c);
                                    }
                                }
                                SseEvent::Err(e) => yield Err(e),
                                SseEvent::Done | SseEvent::Skip => {}
                            }
                        }
                    }
                    if let Some(c) = acc.drain(Some("tool_calls".to_string()), None) {
                        yield Ok(c);
                    }
                    return;
                }
            }
        }
    }
}

// ── Lenient raw chunk shapes ────────────────────────────────────────────────
// OpenAI streams tool-call deltas as fragments: the first carries id/type/name,
// continuations carry only `index` + an `arguments` fragment. The canonical
// `ToolCall` requires all fields, so we deserialize into these lenient shapes
// and reassemble (see `ToolAccum`).

#[derive(Debug, Deserialize)]
struct RawChunk {
    id: String,
    object: String,
    created: i64,
    model: String,
    #[serde(default)]
    choices: Vec<RawChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct RawChoice {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: RawDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCallDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct RawToolCallDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "type")]
    r#type: Option<String>,
    #[serde(default)]
    function: Option<RawFnDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFnDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

impl RawChunk {
    /// Map a non-tool-call chunk (content/role/usage) to the canonical shape,
    /// leaving `tool_calls` empty — tool calls flow through `ToolAccum`.
    fn into_canonical(self) -> ChatCompletionChunk {
        ChatCompletionChunk {
            id: self.id,
            object: self.object,
            created: self.created,
            model: self.model,
            choices: self
                .choices
                .into_iter()
                .map(|c| ChunkChoice {
                    index: c.index,
                    delta: ChunkDelta {
                        role: c.delta.role,
                        content: c.delta.content,
                        tool_calls: Vec::new(),
                    },
                    finish_reason: c.finish_reason,
                })
                .collect(),
            usage: self.usage,
        }
    }
}

// ── Tool-call accumulator ───────────────────────────────────────────────────

#[derive(Default)]
struct PartialToolCall {
    id: String,
    r#type: String,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    fn into_tool_call(self) -> ToolCall {
        ToolCall {
            id: self.id,
            r#type: if self.r#type.is_empty() {
                "function".to_string()
            } else {
                self.r#type
            },
            function: ToolCallFunction {
                name: self.name,
                arguments: self.arguments,
            },
        }
    }
}

#[derive(Clone)]
struct ChunkMeta {
    id: String,
    object: String,
    created: i64,
    model: String,
}

/// Accumulates streaming tool-call fragments (keyed by OpenAI `index`) until the
/// call is complete, then drains them into one canonical chunk.
#[derive(Default)]
struct ToolAccum {
    calls: BTreeMap<u32, PartialToolCall>,
    meta: Option<ChunkMeta>,
}

impl ToolAccum {
    fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Merge every tool-call fragment in `raw` into the accumulator.
    fn merge(&mut self, raw: &RawChunk) {
        let mut saw_fragment = false;
        for choice in &raw.choices {
            for tc in &choice.delta.tool_calls {
                saw_fragment = true;
                let e = self.calls.entry(tc.index).or_default();
                if let Some(id) = &tc.id {
                    if !id.is_empty() {
                        e.id = id.clone();
                    }
                }
                if let Some(t) = &tc.r#type {
                    if !t.is_empty() {
                        e.r#type = t.clone();
                    }
                }
                if let Some(f) = &tc.function {
                    if let Some(n) = &f.name {
                        if !n.is_empty() {
                            e.name = n.clone();
                        }
                    }
                    if let Some(a) = &f.arguments {
                        e.arguments.push_str(a);
                    }
                }
            }
        }
        if saw_fragment {
            self.meta = Some(ChunkMeta {
                id: raw.id.clone(),
                object: raw.object.clone(),
                created: raw.created,
                model: raw.model.clone(),
            });
        }
    }

    /// Drain the accumulated calls into one chunk (index-ordered via BTreeMap).
    /// Returns `None` when nothing is accumulated.
    fn drain(
        &mut self,
        finish_reason: Option<String>,
        usage: Option<Usage>,
    ) -> Option<ChatCompletionChunk> {
        if self.calls.is_empty() {
            return None;
        }
        let meta = self.meta.take()?;
        let tool_calls: Vec<ToolCall> = std::mem::take(&mut self.calls)
            .into_values()
            .map(PartialToolCall::into_tool_call)
            .collect();
        Some(ChatCompletionChunk {
            id: meta.id,
            object: meta.object,
            created: meta.created,
            model: meta.model,
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls,
                },
                finish_reason,
            }],
            usage,
        })
    }
}

/// Process one raw chunk against the accumulator, returning a canonical chunk to
/// forward (or `None` when the chunk is a mid-accumulation tool fragment that is
/// swallowed until the call completes).
fn handle_raw_chunk(acc: &mut ToolAccum, raw: RawChunk) -> Option<ChatCompletionChunk> {
    let has_tool_frag = raw.choices.iter().any(|c| !c.delta.tool_calls.is_empty());
    let finish_reason = raw.choices.iter().find_map(|c| c.finish_reason.clone());

    if has_tool_frag {
        acc.merge(&raw);
        if finish_reason.is_some() {
            return acc.drain(finish_reason, raw.usage);
        }
        return None; // swallow until complete
    }
    // A finish_reason may arrive on a separate chunk after the fragments.
    if finish_reason.is_some() && !acc.is_empty() {
        return acc.drain(finish_reason, raw.usage);
    }
    Some(raw.into_canonical())
}

/// Outcome of processing one line in an SSE event.
#[derive(Debug)]
enum SseEvent {
    /// A successfully deserialized (lenient) chunk.
    Chunk(RawChunk),
    /// The `[DONE]` sentinel; caller should close the stream.
    Done,
    /// A deserialization error; caller should yield the error and continue.
    Err(ProviderError),
    /// Empty line, comment, or other ignorable content.
    Skip,
}

/// Parse a single SSE event (the bytes up to but not including the trailing `\n\n`).
///
/// An event may consist of multiple lines; each `data:` line is parsed
/// independently.
fn parse_sse_event(event_bytes: &[u8]) -> Vec<SseEvent> {
    let text = match std::str::from_utf8(event_bytes) {
        Ok(t) => t,
        Err(_) => {
            return vec![SseEvent::Err(ProviderError::Deserialize(
                "SSE event contained invalid UTF-8".to_string(),
            ))]
        }
    };

    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim_end_matches('\r');

        if line.is_empty() {
            // Empty line — separator within buffered data, skip.
            continue;
        }

        if line.starts_with(':') {
            // Heartbeat comment — skip silently.
            continue;
        }

        if let Some(data) = line
            .strip_prefix("data:")
            .map(|s| s.strip_prefix(' ').unwrap_or(s))
        {
            if data == "[DONE]" {
                results.push(SseEvent::Done);
                // Stop processing further lines in this event.
                break;
            }

            match serde_json::from_str::<RawChunk>(data) {
                Ok(chunk) => results.push(SseEvent::Chunk(chunk)),
                Err(e) => results.push(SseEvent::Err(ProviderError::Deserialize(format!(
                    "failed to parse SSE chunk: {e}"
                )))),
            }
        }
        // Lines without "data: " prefix in an event are ignored per SSE spec.
    }

    if results.is_empty() {
        results.push(SseEvent::Skip);
    }

    results
}

/// Find the first SSE event boundary in `buf`.
///
/// Returns `(offset, sep_len)` where `offset` is the index of the first byte
/// of the boundary sequence and `sep_len` is the number of bytes in the
/// separator (`2` for `\n\n`, `4` for `\r\n\r\n`).  Callers should drain
/// `..offset + sep_len` to consume the full event including its terminator.
///
/// Both `\n\n` and `\r\n\r\n` are valid SSE event terminators per RFC 8898.
fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    // Scan for \r\n\r\n first so a CRLF stream never accidentally matches the
    // \r byte of a CRLF pair as a lone \n\n pair.
    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((pos, 4));
    }
    if let Some(pos) = buf.windows(2).position(|w| w == b"\n\n") {
        return Some((pos, 2));
    }
    None
}

// ---------------------------------------------------------------------------
// Response body shape for error extraction (internal)
// ---------------------------------------------------------------------------

/// Minimal shape of OpenAI SSE error line (rare but observed).
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct OpenAiSseError {
    error: OpenAiSseErrorInner,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct OpenAiSseErrorInner {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_event_boundary_lf() {
        let buf = b"data: hello\n\ndata: world\n\n";
        assert_eq!(find_event_boundary(buf), Some((11, 2)));
    }

    #[test]
    fn find_event_boundary_crlf() {
        let buf = b"data: hello\r\n\r\ndata: world\r\n\r\n";
        assert_eq!(find_event_boundary(buf), Some((11, 4)));
    }

    #[test]
    fn find_event_boundary_none() {
        let buf = b"data: hello\n";
        assert_eq!(find_event_boundary(buf), None);
    }

    #[test]
    fn parse_sse_event_data_line() {
        let chunk_json = r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let event = format!("data: {chunk_json}\n\n");
        let results = parse_sse_event(event.as_bytes());
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], SseEvent::Chunk(c) if c.id == "c1"));
    }

    #[test]
    fn parse_sse_event_done() {
        let results = parse_sse_event(b"data: [DONE]\n\n");
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], SseEvent::Done));
    }

    #[test]
    fn parse_sse_event_tool_call_fragment_no_id() {
        // A continuation fragment (only index + arguments) must now deserialize
        // (previously failed because canonical ToolCall required id/type/name).
        let data = r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]},"finish_reason":null}]}"#;
        let event = format!("data: {data}\n\n");
        let results = parse_sse_event(event.as_bytes());
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], SseEvent::Chunk(c) if c.id == "c"));
    }

    fn frag(index: u32, id: Option<&str>, name: Option<&str>, args: &str) -> RawToolCallDelta {
        RawToolCallDelta {
            index,
            id: id.map(String::from),
            r#type: id.map(|_| "function".to_string()),
            function: Some(RawFnDelta {
                name: name.map(String::from),
                arguments: Some(args.to_string()),
            }),
        }
    }

    fn raw_chunk(tool_calls: Vec<RawToolCallDelta>, finish_reason: Option<&str>) -> RawChunk {
        RawChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            created: 1,
            model: "gpt-4o".into(),
            choices: vec![RawChoice {
                index: 0,
                delta: RawDelta {
                    role: None,
                    content: None,
                    tool_calls,
                },
                finish_reason: finish_reason.map(String::from),
            }],
            usage: None,
        }
    }

    #[test]
    fn handle_reassembles_single_tool_call() {
        let mut acc = ToolAccum::default();
        // frag 1: id+name, empty args → swallowed
        assert!(handle_raw_chunk(
            &mut acc,
            raw_chunk(vec![frag(0, Some("call_1"), Some("f"), "")], None)
        )
        .is_none());
        // frag 2: args fragment, no id → swallowed
        assert!(
            handle_raw_chunk(&mut acc, raw_chunk(vec![frag(0, None, None, "{\"a\":")], None))
                .is_none()
        );
        // frag 3: closing args + finish → drained
        let out =
            handle_raw_chunk(&mut acc, raw_chunk(vec![frag(0, None, None, "1}")], Some("tool_calls")))
                .expect("complete chunk");
        let tc = &out.choices[0].delta.tool_calls;
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_1");
        assert_eq!(tc[0].r#type, "function");
        assert_eq!(tc[0].function.name, "f");
        assert_eq!(tc[0].function.arguments, "{\"a\":1}");
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        assert!(acc.is_empty());
    }

    #[test]
    fn handle_reassembles_two_tool_calls_by_index() {
        let mut acc = ToolAccum::default();
        handle_raw_chunk(
            &mut acc,
            raw_chunk(vec![frag(0, Some("a"), Some("fa"), "{}")], None),
        );
        handle_raw_chunk(
            &mut acc,
            raw_chunk(vec![frag(1, Some("b"), Some("fb"), "{}")], None),
        );
        let out = handle_raw_chunk(&mut acc, raw_chunk(vec![], Some("tool_calls")))
            .expect("complete chunk");
        let tc = &out.choices[0].delta.tool_calls;
        assert_eq!(tc.len(), 2);
        assert_eq!(tc[0].id, "a"); // index 0 first
        assert_eq!(tc[1].id, "b");
    }

    #[test]
    fn handle_forwards_content_chunk() {
        let mut acc = ToolAccum::default();
        let raw = RawChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            created: 1,
            model: "gpt-4o".into(),
            choices: vec![RawChoice {
                index: 0,
                delta: RawDelta {
                    role: None,
                    content: Some("Hi".into()),
                    tool_calls: vec![],
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let out = handle_raw_chunk(&mut acc, raw).expect("forwarded");
        assert_eq!(out.choices[0].delta.content.as_deref(), Some("Hi"));
        assert!(out.choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn parse_sse_event_comment_skipped() {
        let results = parse_sse_event(b":keep-alive\n\n");
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], SseEvent::Skip));
    }

    #[test]
    fn parse_sse_event_malformed_json() {
        let results = parse_sse_event(b"data: {not valid json}\n\n");
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], SseEvent::Err(_)));
    }

    // --- CRLF + no-space data: regression tests (rv-sse-crlf-parsing) ---

    #[test]
    fn parse_sse_event_crlf_data_line() {
        // CRLF line endings: the event bytes passed to parse_sse_event still have \r\n
        // (after draining with sep_len=4 the trailing \r\n\r\n is consumed, so the
        // body itself uses \r\n line endings).
        let chunk_json = r#"{"id":"c2","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let event = format!("data: {chunk_json}\r\n\r\n");
        let results = parse_sse_event(event.as_bytes());
        assert_eq!(results.len(), 1, "CRLF event should parse to one chunk");
        assert!(
            matches!(&results[0], SseEvent::Chunk(c) if c.id == "c2"),
            "CRLF-delimited event should parse identical to LF form"
        );
    }

    #[test]
    fn parse_sse_event_no_space_data_prefix() {
        // `data:{...}` (no space after colon) is valid SSE and must not be dropped.
        let chunk_json = r#"{"id":"c3","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let event = format!("data:{chunk_json}\n\n");
        let results = parse_sse_event(event.as_bytes());
        assert_eq!(
            results.len(),
            1,
            "no-space data: event should parse to one chunk"
        );
        assert!(
            matches!(&results[0], SseEvent::Chunk(c) if c.id == "c3"),
            "data:{{...}} (no space) should parse the same as `data: {{...}}`"
        );
    }

    #[test]
    fn find_event_boundary_prefers_crlf_over_lf_in_same_buf() {
        // A buffer with \r\n\r\n earlier than \n\n should return the CRLF boundary.
        let buf = b"data: a\r\n\r\ndata: b\n\n";
        let (pos, sep) = find_event_boundary(buf).expect("boundary found");
        assert_eq!(sep, 4, "should detect CRLF boundary");
        assert_eq!(pos, 7);
    }
}
