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

/// A `BoxStream` alias used by this module.
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, ProviderError>> + Send>>;

/// Return a `stream_options` value with `include_usage: true` set, preserving
/// any other keys a caller already supplied. A non-object caller value is
/// replaced (the streaming usage option is mandatory for accounting).
fn merge_include_usage(caller: Option<serde_json::Value>) -> serde_json::Value {
    match caller {
        Some(serde_json::Value::Object(mut map)) => {
            map.insert("include_usage".to_string(), serde_json::Value::Bool(true));
            serde_json::Value::Object(map)
        }
        _ => serde_json::json!({ "include_usage": true }),
    }
}

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
    stream_chat_completion_at(
        client,
        &url,
        ("Authorization", format!("Bearer {api_key}")),
        req,
        ctx,
    )
    .await
}

/// Streaming chat completion against a fully-formed `url` with a caller-supplied
/// auth header.
///
/// This is the endpoint-agnostic core of [`stream_chat_completion`]. The OpenAI
/// path passes `{base_url}/chat/completions` + `("Authorization", "Bearer …")`;
/// the Azure adapter passes its deployment URL (with the `api-version` query
/// param baked in) + `("api-key", …)`. Translation, the `stream:true` /
/// `include_usage` override, extra-header forwarding, error mapping, and the SSE
/// parser are all shared.
pub async fn stream_chat_completion_at(
    client: Client,
    url: &str,
    auth_header: (&str, String),
    req: ChatCompletionRequest,
    ctx: &RequestContext,
) -> Result<ChunkStream, ProviderError> {
    let extra_headers: Vec<(String, String)> = filter_extra_headers(&ctx.credentials.extra_headers);

    // Translate to the OpenAI wire shape, then override the stream flag and
    // force `stream_options.include_usage` so the final chunk carries usage —
    // merging into any caller-supplied `stream_options` rather than dropping it.
    let mut translated = translate::translate_request(req)?;
    translated.stream = true;
    translated.stream_options = Some(merge_include_usage(translated.stream_options.take()));

    let body_bytes = serde_json::to_vec(&translated)
        .map_err(|e| ProviderError::Internal(format!("failed to serialize stream body: {e}")))?;

    let mut request_builder = client
        .post(url)
        .header(auth_header.0, auth_header.1)
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
                                    for c in handle_raw_chunk(&mut acc, raw) {
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
                                    for c in handle_raw_chunk(&mut acc, raw) {
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
    usage: Option<RawUsage>,
    /// Unknown / newer top-level chunk fields preserved for round-trip passthrough.
    #[serde(flatten, default)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Lenient streaming usage block. OpenAI-wire providers report cache reads as
/// `prompt_tokens_details.cached_tokens` — deserializing chunk usage directly
/// into the canonical [`Usage`] (which has no such field) silently dropped
/// them, so streamed cached prompts were priced at the full input rate and
/// `provider_cache_saved_usd` was zeroed. This shape parses the wire detail
/// AND passes through already-canonical fields (fake-streams / TT hops).
#[derive(Debug, Deserialize)]
struct RawUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<crate::translate::PromptTokensDetails>,
    // Passthrough for already-canonical shapes (preserves previously accepted
    // inputs such as our own synthesized usage chunks).
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

impl RawUsage {
    fn into_canonical(self) -> Usage {
        // Wire detail wins when present; raw Option-ness is preserved so
        // telemetry can tell "reported zero" from "didn't report".
        let detail = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens);
        Usage {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            cached_tokens: detail.unwrap_or(self.cached_tokens),
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_read_input_tokens: detail.or(self.cache_read_input_tokens),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawChoice {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    delta: RawDelta,
    #[serde(default)]
    finish_reason: Option<String>,
    /// Unknown per-choice fields (e.g. `logprobs`) preserved for passthrough.
    #[serde(flatten, default)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDelta {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCallDelta>,
    /// Unknown per-delta fields (e.g. `refusal`) preserved for passthrough.
    #[serde(flatten, default)]
    extra: std::collections::HashMap<String, serde_json::Value>,
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
                        extra: c.delta.extra,
                    },
                    finish_reason: c.finish_reason,
                    extra: c.extra,
                })
                .collect(),
            usage: self.usage.map(RawUsage::into_canonical),
            extra: self.extra,
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

/// Accumulates streaming tool-call fragments (keyed by `(choice index, tool-call
/// index)`) until the call is complete, then drains them into one canonical
/// chunk. Keying on the choice index too keeps `n>1` choices from colliding (each
/// streams its own tool calls starting at tool index 0).
#[derive(Default)]
struct ToolAccum {
    calls: BTreeMap<(u32, u32), PartialToolCall>,
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
                let e = self.calls.entry((choice.index, tc.index)).or_default();
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

    /// Drain the accumulated calls into one chunk, one `ChunkChoice` per choice
    /// index (BTreeMap key order keeps choices and tool calls index-ordered).
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
        // Group tool calls by their choice index, preserving tool-index order.
        let mut by_choice: BTreeMap<u32, Vec<ToolCall>> = BTreeMap::new();
        for ((choice_index, _tool_index), partial) in std::mem::take(&mut self.calls) {
            by_choice
                .entry(choice_index)
                .or_default()
                .push(partial.into_tool_call());
        }
        let choices = by_choice
            .into_iter()
            .map(|(index, tool_calls)| ChunkChoice {
                index,
                delta: ChunkDelta {
                    role: None,
                    content: None,
                    tool_calls,
                    extra: Default::default(),
                },
                finish_reason: finish_reason.clone(),
                extra: Default::default(),
            })
            .collect();
        Some(ChatCompletionChunk {
            id: meta.id,
            object: meta.object,
            created: meta.created,
            model: meta.model,
            choices,
            usage,
            extra: Default::default(),
        })
    }
}

/// Build a content/role-only canonical chunk from a raw chunk that also carried a
/// tool-call fragment, so role (`{"role":"assistant", tool_calls:[…]}` — OpenAI's
/// standard first tool delta) and any text are forwarded rather than swallowed.
/// `tool_calls` is stripped (those flow through [`ToolAccum`]) and `finish_reason`
/// is dropped (it rides with the drained tool-call chunk). Returns `None` when no
/// choice carries role or content.
fn content_chunk(raw: &RawChunk) -> Option<ChatCompletionChunk> {
    if !raw
        .choices
        .iter()
        .any(|c| c.delta.role.is_some() || c.delta.content.is_some())
    {
        return None;
    }
    Some(ChatCompletionChunk {
        id: raw.id.clone(),
        object: raw.object.clone(),
        created: raw.created,
        model: raw.model.clone(),
        choices: raw
            .choices
            .iter()
            .map(|c| ChunkChoice {
                index: c.index,
                delta: ChunkDelta {
                    role: c.delta.role.clone(),
                    content: c.delta.content.clone(),
                    tool_calls: Vec::new(),
                    extra: c.delta.extra.clone(),
                },
                finish_reason: None,
                extra: c.extra.clone(),
            })
            .collect(),
        usage: None,
        extra: raw.extra.clone(),
    })
}

/// Process one raw chunk against the accumulator, returning the canonical chunks
/// to forward. Usually 0 or 1, but a chunk that carries role/content *and* a
/// tool-call fragment yields the content chunk plus (on finish) the drained
/// tool-call chunk. A mid-accumulation tool fragment with no role/content yields
/// nothing (swallowed until the call completes).
fn handle_raw_chunk(acc: &mut ToolAccum, raw: RawChunk) -> Vec<ChatCompletionChunk> {
    let has_tool_frag = raw.choices.iter().any(|c| !c.delta.tool_calls.is_empty());
    let finish_reason = raw.choices.iter().find_map(|c| c.finish_reason.clone());

    if has_tool_frag {
        let mut out = Vec::new();
        // Preserve any role/content riding alongside the tool fragment.
        if let Some(c) = content_chunk(&raw) {
            out.push(c);
        }
        acc.merge(&raw);
        if finish_reason.is_some() {
            out.extend(acc.drain(finish_reason, raw.usage.map(RawUsage::into_canonical)));
        }
        return out;
    }
    // A finish_reason may arrive on a separate chunk after the fragments.
    if finish_reason.is_some() && !acc.is_empty() {
        return acc
            .drain(finish_reason, raw.usage.map(RawUsage::into_canonical))
            .into_iter()
            .collect();
    }
    vec![raw.into_canonical()]
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
    fn merge_include_usage_sets_flag_when_absent() {
        let v = merge_include_usage(None);
        assert_eq!(v, serde_json::json!({ "include_usage": true }));
    }

    #[test]
    fn merge_include_usage_preserves_caller_keys() {
        let caller = serde_json::json!({ "continuous_usage_stats": true });
        let v = merge_include_usage(Some(caller));
        assert_eq!(v["include_usage"], true);
        assert_eq!(v["continuous_usage_stats"], true);
    }

    #[test]
    fn merge_include_usage_overrides_caller_false() {
        // Streaming usage accounting is mandatory: a caller's `false` is upgraded.
        let caller = serde_json::json!({ "include_usage": false });
        let v = merge_include_usage(Some(caller));
        assert_eq!(v["include_usage"], true);
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
    fn unknown_chunk_fields_round_trip_through_pipeline() {
        // An upstream SSE chunk carrying newer/unknown fields (top-level, per-
        // choice, per-delta) must survive parse → handle_raw_chunk → serialize.
        let chunk_json = r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"gpt-4o","system_fingerprint":"fp_x","choices":[{"index":0,"delta":{"content":"Hi","refusal":null},"finish_reason":null,"logprobs":{"content":[]}}]}"#;
        let event = format!("data: {chunk_json}\n\n");
        let mut acc = ToolAccum::default();
        let mut canonical = Vec::new();
        for ev in parse_sse_event(event.as_bytes()) {
            if let SseEvent::Chunk(raw) = ev {
                canonical.extend(handle_raw_chunk(&mut acc, raw));
            }
        }
        assert_eq!(canonical.len(), 1);
        let out = serde_json::to_value(&canonical[0]).unwrap();
        assert_eq!(out["system_fingerprint"], "fp_x");
        assert_eq!(
            out["choices"][0]["logprobs"],
            serde_json::json!({ "content": [] })
        );
        assert_eq!(
            out["choices"][0]["delta"]["refusal"],
            serde_json::Value::Null
        );
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
                    extra: Default::default(),
                },
                finish_reason: finish_reason.map(String::from),
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
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
        .is_empty());
        // frag 2: args fragment, no id → swallowed
        assert!(handle_raw_chunk(
            &mut acc,
            raw_chunk(vec![frag(0, None, None, "{\"a\":")], None)
        )
        .is_empty());
        // frag 3: closing args + finish → drained
        let out = handle_raw_chunk(
            &mut acc,
            raw_chunk(vec![frag(0, None, None, "1}")], Some("tool_calls")),
        );
        assert_eq!(out.len(), 1);
        let tc = &out[0].choices[0].delta.tool_calls;
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_1");
        assert_eq!(tc[0].r#type, "function");
        assert_eq!(tc[0].function.name, "f");
        assert_eq!(tc[0].function.arguments, "{\"a\":1}");
        assert_eq!(
            out[0].choices[0].finish_reason.as_deref(),
            Some("tool_calls")
        );
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
        let out = handle_raw_chunk(&mut acc, raw_chunk(vec![], Some("tool_calls")));
        assert_eq!(out.len(), 1);
        let tc = &out[0].choices[0].delta.tool_calls;
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
                    extra: Default::default(),
                },
                finish_reason: None,
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        };
        let out = handle_raw_chunk(&mut acc, raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].choices[0].delta.content.as_deref(), Some("Hi"));
        assert!(out[0].choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn handle_preserves_role_riding_with_tool_fragment() {
        // OpenAI's first tool delta is `{role:"assistant", tool_calls:[…]}`. The
        // role must be forwarded, not swallowed with the fragment.
        let mut acc = ToolAccum::default();
        let first = RawChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            created: 1,
            model: "gpt-4o".into(),
            choices: vec![RawChoice {
                index: 0,
                delta: RawDelta {
                    role: Some("assistant".into()),
                    content: None,
                    tool_calls: vec![frag(0, Some("call_1"), Some("f"), "{}")],
                    extra: Default::default(),
                },
                finish_reason: None,
                extra: Default::default(),
            }],
            usage: None,
            extra: Default::default(),
        };
        let out = handle_raw_chunk(&mut acc, first);
        // One chunk: the role/content carrier (the tool fragment is swallowed).
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].choices[0].delta.role.as_deref(), Some("assistant"));
        assert!(out[0].choices[0].delta.tool_calls.is_empty());

        // The fragment then drains on finish.
        let done = handle_raw_chunk(&mut acc, raw_chunk(vec![], Some("tool_calls")));
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].choices[0].delta.tool_calls.len(), 1);
        assert_eq!(done[0].choices[0].delta.tool_calls[0].id, "call_1");
    }

    #[test]
    fn handle_keys_tool_calls_by_choice_index() {
        // n>1: two choices each stream a tool call at tool-index 0 — they must NOT
        // collide; drain emits one ChunkChoice per choice index.
        let mut acc = ToolAccum::default();
        let raw = RawChunk {
            id: "c".into(),
            object: "chat.completion.chunk".into(),
            created: 1,
            model: "gpt-4o".into(),
            choices: vec![
                RawChoice {
                    index: 0,
                    delta: RawDelta {
                        role: None,
                        content: None,
                        tool_calls: vec![frag(0, Some("call_0"), Some("f0"), "{}")],
                        extra: Default::default(),
                    },
                    finish_reason: Some("tool_calls".into()),
                    extra: Default::default(),
                },
                RawChoice {
                    index: 1,
                    delta: RawDelta {
                        role: None,
                        content: None,
                        tool_calls: vec![frag(0, Some("call_1"), Some("f1"), "{}")],
                        extra: Default::default(),
                    },
                    finish_reason: Some("tool_calls".into()),
                    extra: Default::default(),
                },
            ],
            usage: None,
            extra: Default::default(),
        };
        let out = handle_raw_chunk(&mut acc, raw);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].choices.len(), 2, "one ChunkChoice per choice index");
        assert_eq!(out[0].choices[0].index, 0);
        assert_eq!(out[0].choices[0].delta.tool_calls[0].id, "call_0");
        assert_eq!(out[0].choices[1].index, 1);
        assert_eq!(out[0].choices[1].delta.tool_calls[0].id, "call_1");
    }

    // --- KEYSTONE: streamed provider cache reads must not be silently lost ---

    /// OpenAI-wire STREAMING usage chunks report cache reads as
    /// `prompt_tokens_details.cached_tokens` — the canonical `Usage` has no
    /// such field, so deserializing chunk usage directly into `Usage` silently
    /// dropped them (cached_tokens=0, raw None): streamed cached prompts were
    /// priced at the full input rate and provider_cache_saved_usd was zeroed.
    #[test]
    fn streaming_usage_chunk_preserves_prompt_tokens_details_cached_tokens() {
        let chunk_json = r#"{"id":"c9","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":80}}}"#;
        let event = format!("data: {chunk_json}\n\n");
        let mut acc = ToolAccum::default();
        let mut canonical = Vec::new();
        for ev in parse_sse_event(event.as_bytes()) {
            if let SseEvent::Chunk(raw) = ev {
                canonical.extend(handle_raw_chunk(&mut acc, raw));
            } else {
                panic!("usage chunk must parse, got {ev:?}");
            }
        }
        assert_eq!(canonical.len(), 1);
        let usage = canonical[0].usage.as_ref().expect("usage present");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(
            usage.cached_tokens, 80,
            "streamed prompt_tokens_details.cached_tokens must fold into cached_tokens"
        );
        assert_eq!(
            usage.cache_read_input_tokens,
            Some(80),
            "raw cache-read Option must be preserved for telemetry"
        );
    }

    /// Already-canonical usage shapes (e.g. our own fake-stream or another
    /// TokenTrimmer hop) keep working: top-level cached_tokens / cache fields
    /// pass through unchanged.
    #[test]
    fn streaming_usage_chunk_passes_through_canonical_cache_fields() {
        let chunk_json = r#"{"id":"c9","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"cached_tokens":40,"cache_read_input_tokens":40,"cache_creation_input_tokens":20}}"#;
        let event = format!("data: {chunk_json}\n\n");
        let mut acc = ToolAccum::default();
        let mut canonical = Vec::new();
        for ev in parse_sse_event(event.as_bytes()) {
            if let SseEvent::Chunk(raw) = ev {
                canonical.extend(handle_raw_chunk(&mut acc, raw));
            }
        }
        let usage = canonical[0].usage.as_ref().expect("usage present");
        assert_eq!(usage.cached_tokens, 40);
        assert_eq!(usage.cache_read_input_tokens, Some(40));
        assert_eq!(usage.cache_creation_input_tokens, Some(20));
    }

    /// A streamed usage block with NO cache report keeps raw None (NULL in
    /// telemetry), distinct from a reported zero.
    #[test]
    fn streaming_usage_chunk_without_cache_report_keeps_raw_none() {
        let chunk_json = r#"{"id":"c9","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105}}"#;
        let event = format!("data: {chunk_json}\n\n");
        let mut acc = ToolAccum::default();
        let mut canonical = Vec::new();
        for ev in parse_sse_event(event.as_bytes()) {
            if let SseEvent::Chunk(raw) = ev {
                canonical.extend(handle_raw_chunk(&mut acc, raw));
            }
        }
        let usage = canonical[0].usage.as_ref().expect("usage present");
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, None);
    }

    /// A details object with an explicit `"cached_tokens": null` (or carrying
    /// only other keys like audio_tokens) must NOT error the chunk parse —
    /// the terminal usage frame still reaches the client, and the raw
    /// cache-read stays None ("unreported"), not a fabricated Some(0).
    #[test]
    fn streaming_usage_chunk_with_null_or_missing_cached_tokens_is_lenient_none() {
        for details in [r#"{"cached_tokens":null}"#, r#"{"audio_tokens":5}"#] {
            let chunk_json = format!(
                r#"{{"id":"c9","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[],"usage":{{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{details}}}}}"#
            );
            let event = format!("data: {chunk_json}\n\n");
            let mut acc = ToolAccum::default();
            let mut canonical = Vec::new();
            for ev in parse_sse_event(event.as_bytes()) {
                if let SseEvent::Chunk(raw) = ev {
                    canonical.extend(handle_raw_chunk(&mut acc, raw));
                } else {
                    panic!("usage chunk with details {details} must parse, got {ev:?}");
                }
            }
            let usage = canonical[0].usage.as_ref().expect("usage present");
            assert_eq!(usage.cached_tokens, 0);
            assert_eq!(usage.cache_read_input_tokens, None);
        }
    }

    /// Usage riding on the tool-call drain path (finish chunk carries both the
    /// final tool fragment/finish_reason AND the usage block) also preserves
    /// the cache-read details.
    #[test]
    fn tool_call_drain_preserves_cached_tokens_from_usage() {
        let mut acc = ToolAccum::default();
        handle_raw_chunk(
            &mut acc,
            raw_chunk(vec![frag(0, Some("call_1"), Some("f"), "{}")], None),
        );
        let data = r#"{"id":"c","object":"chat.completion.chunk","created":1,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"prompt_tokens_details":{"cached_tokens":80}}}"#;
        let event = format!("data: {data}\n\n");
        let mut out = Vec::new();
        for ev in parse_sse_event(event.as_bytes()) {
            if let SseEvent::Chunk(raw) = ev {
                out.extend(handle_raw_chunk(&mut acc, raw));
            }
        }
        assert_eq!(out.len(), 1);
        let usage = out[0].usage.as_ref().expect("usage rides the drain chunk");
        assert_eq!(usage.cached_tokens, 80);
        assert_eq!(usage.cache_read_input_tokens, Some(80));
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
