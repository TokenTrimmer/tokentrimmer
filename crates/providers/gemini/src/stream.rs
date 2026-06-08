//! SSE parser and streaming chat completion for the Gemini adapter.
//!
//! Gemini streaming uses SSE format with `?alt=sse` query parameter. Each SSE
//! event contains a partial `generateContent` response JSON.
//!
//! # Event format
//!
//! ```text
//! data: {"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"index":0}]}
//!
//! data: {"candidates":[{"content":{"role":"model","parts":[{"text":" world"}]},"index":0}]}
//!
//! data: {"candidates":[{"content":{"role":"model","parts":[{"text":"!"}]},"finishReason":"STOP","index":0}],"usageMetadata":{...}}
//!
//! ```
//!
//! **Note**: there is no `[DONE]` sentinel — the stream ends when the server
//! closes the connection or a chunk with `finishReason` is received.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::Client;
use std::pin::Pin;
use tt_shared::{
    filter_extra_headers,
    messages::{ChunkChoice, ChunkDelta, ToolCall, ToolCallFunction},
    ChatCompletionChunk, ChatCompletionRequest, ProviderError, RequestContext, Usage,
};
use uuid::Uuid;

use crate::errors::{map_reqwest_error, map_response_error};
use crate::translate::{self, GeminiCandidate, GeminiPart, GeminiUsageMetadata};

// ---------------------------------------------------------------------------
// Gemini SSE event shape (streaming-specific subset)
// ---------------------------------------------------------------------------

/// A partial Gemini streaming event (one SSE data payload).
#[derive(Debug, serde::Deserialize)]
struct GeminiStreamEvent {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

/// A `BoxStream` alias used by this module.
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, ProviderError>> + Send>>;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Build and execute a streaming Gemini chat completion request.
///
/// Returns `Err` before yielding any chunk if the HTTP response status ≥ 400.
/// On success returns a `BoxStream` of [`ChatCompletionChunk`] values translated
/// from Gemini SSE events.
pub async fn stream_chat_completion(
    client: Client,
    base_url: &str,
    req: ChatCompletionRequest,
    ctx: &RequestContext,
) -> Result<ChunkStream, ProviderError> {
    let model = req.model.clone();
    let api_key = ctx.credentials.api_key.expose().to_string();

    // Translate to the Gemini wire shape.
    let body = translate::translate_request(req)?;

    translate::validate_model_id(&model)?;
    let url = format!("{base_url}/v1beta/models/{model}:streamGenerateContent?alt=sse");

    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| ProviderError::Internal(format!("failed to serialize stream body: {e}")))?;

    let mut request_builder = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("x-goog-api-key", &api_key)
        .body(body_bytes);
    for (name, value) in &filter_extra_headers(&ctx.credentials.extra_headers) {
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
            &model,
        ));
    }

    let bytes_stream = response.bytes_stream();
    let stream = build_sse_stream(bytes_stream, model);
    Ok(Box::pin(stream))
}

// ---------------------------------------------------------------------------
// SSE stream parser
// ---------------------------------------------------------------------------

/// Parse a Gemini SSE byte stream into a stream of [`ChatCompletionChunk`].
///
/// Handles:
/// - Buffering across fragmented body chunks.
/// - First chunk emitting `role: "assistant"`.
/// - Function-call parts → `tool_calls` in delta.
/// - Final chunk (with `finishReason`) includes `finish_reason` and `usage`.
/// - Mid-stream JSON parse failures → `Err(ProviderError::Deserialize)`.
/// - Stream ending without `finishReason` → ends cleanly.
fn build_sse_stream<S>(
    bytes_stream: S,
    model: String,
) -> impl Stream<Item = Result<ChatCompletionChunk, ProviderError>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut buffer: Vec<u8> = Vec::new();
        let mut first_chunk = true;
        let stream_id = format!("chatcmpl-gem-{}", Uuid::new_v4());
        let created = chrono::Utc::now().timestamp();

        futures::pin_mut!(bytes_stream);

        loop {
            let next_item: Option<Result<Bytes, reqwest::Error>> = bytes_stream.next().await;
            match next_item {
                Some(Ok(chunk)) => {
                    buffer.extend_from_slice(&chunk);
                    // Process all complete SSE events (delimited by \n\n or \r\n\r\n).
                    while let Some((event_end, sep_len)) = find_event_boundary(&buffer) {
                        let event_bytes = buffer.drain(..event_end + sep_len).collect::<Vec<_>>();
                        let outcomes = process_sse_event(
                            &event_bytes,
                            &stream_id,
                            created,
                            &model,
                            &mut first_chunk,
                        );
                        for outcome in outcomes {
                            match outcome {
                                SseOutcome::Chunk(c) => yield Ok(c),
                                SseOutcome::Err(e) => yield Err(e),
                                SseOutcome::Skip => {}
                            }
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
                        let outcomes = process_sse_event(
                            &buffer,
                            &stream_id,
                            created,
                            &model,
                            &mut first_chunk,
                        );
                        for outcome in outcomes {
                            match outcome {
                                SseOutcome::Chunk(c) => yield Ok(c),
                                SseOutcome::Err(e) => yield Err(e),
                                SseOutcome::Skip => {}
                            }
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
    Err(ProviderError),
    Skip,
}

/// Parse and handle a single SSE event (bytes up to but not including `\n\n`).
///
/// Returns a Vec because a single event may yield multiple chunks (e.g. first
/// chunk with role + content chunk).
fn process_sse_event(
    event_bytes: &[u8],
    stream_id: &str,
    created: i64,
    model: &str,
    first_chunk: &mut bool,
) -> Vec<SseOutcome> {
    let text = match std::str::from_utf8(event_bytes) {
        Ok(t) => t,
        Err(_) => {
            return vec![SseOutcome::Err(ProviderError::Deserialize(
                "SSE event contained invalid UTF-8".to_string(),
            ))]
        }
    };

    // Extract the `data:` line(s).
    let mut data_line: Option<&str> = None;

    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(data) = line
            .strip_prefix("data:")
            .map(|s| s.strip_prefix(' ').unwrap_or(s))
        {
            data_line = Some(data.trim());
        }
    }

    let data = match data_line {
        Some(d) => d,
        None => return vec![SseOutcome::Skip],
    };

    // Parse the Gemini streaming event.
    let event: GeminiStreamEvent = match serde_json::from_str(data) {
        Ok(e) => e,
        Err(e) => {
            return vec![SseOutcome::Err(ProviderError::Deserialize(format!(
                "Gemini stream event parse error: {e}"
            )))]
        }
    };

    let mut outcomes = Vec::new();

    for (idx, candidate) in event.candidates.into_iter().enumerate() {
        if idx > 0 {
            tracing::debug!(
                "gemini stream: ignoring extra candidate #{idx} — this gateway is \
                 single-candidate (n>1 is dropped for Gemini)"
            );
            continue;
        }
        let has_finish_reason = candidate.finish_reason.is_some();
        let finish_reason = candidate
            .finish_reason
            .as_deref()
            .map(translate::map_finish_reason)
            .map(str::to_string);

        let content_parts = candidate.content.map(|c| c.parts).unwrap_or_default();

        // Separate text parts and function call parts.
        let mut text_content: Option<String> = None;
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        for part in content_parts {
            match part {
                GeminiPart::Text(t) => {
                    text_content = Some(match text_content {
                        Some(existing) => existing + &t,
                        None => t,
                    });
                }
                GeminiPart::FunctionCall(fc) => {
                    tool_calls.push(ToolCall {
                        id: format!("call_{}", Uuid::new_v4()),
                        r#type: "function".to_string(),
                        function: ToolCallFunction {
                            name: fc.name,
                            arguments: fc.args.to_string(),
                        },
                    });
                }
                _ => {}
            }
        }

        // Override finish_reason to "tool_calls" if we have function calls.
        let effective_finish_reason = if !tool_calls.is_empty() {
            Some("tool_calls".to_string())
        } else {
            finish_reason
        };

        // First chunk: emit a role-only chunk before content.
        if *first_chunk {
            *first_chunk = false;
            outcomes.push(SseOutcome::Chunk(ChatCompletionChunk {
                id: stream_id.to_string(),
                object: "chat.completion.chunk".to_string(),
                created,
                model: model.to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: Some("assistant".to_string()),
                        content: None,
                        tool_calls: vec![],
                    },
                    finish_reason: None,
                }],
                usage: None,
            }));
        }

        // Emit content/tool_call chunk if there is content.
        if text_content.is_some() || !tool_calls.is_empty() {
            let usage = if has_finish_reason {
                event.usage_metadata.as_ref().map(|u| Usage {
                    prompt_tokens: u.prompt_token_count,
                    completion_tokens: u.candidates_token_count,
                    total_tokens: u.total_token_count,
                    cached_tokens: u.cached_content_token_count,
                    cache_creation_input_tokens: None,
                })
            } else {
                None
            };

            outcomes.push(SseOutcome::Chunk(ChatCompletionChunk {
                id: stream_id.to_string(),
                object: "chat.completion.chunk".to_string(),
                created,
                model: model.to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta {
                        role: None,
                        content: text_content,
                        tool_calls,
                    },
                    finish_reason: effective_finish_reason.clone(),
                }],
                usage,
            }));
        } else if has_finish_reason {
            // Final chunk with finish_reason but no content.
            let usage = event.usage_metadata.as_ref().map(|u| Usage {
                prompt_tokens: u.prompt_token_count,
                completion_tokens: u.candidates_token_count,
                total_tokens: u.total_token_count,
                cached_tokens: u.cached_content_token_count,
                cache_creation_input_tokens: None,
            });

            outcomes.push(SseOutcome::Chunk(ChatCompletionChunk {
                id: stream_id.to_string(),
                object: "chat.completion.chunk".to_string(),
                created,
                model: model.to_string(),
                choices: vec![ChunkChoice {
                    index: 0,
                    delta: ChunkDelta::default(),
                    finish_reason: effective_finish_reason,
                }],
                usage,
            }));
        }
    }

    if outcomes.is_empty() {
        vec![SseOutcome::Skip]
    } else {
        outcomes
    }
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
        let buf = b"data: {}\n\ndata: {}\n\n";
        assert_eq!(find_event_boundary(buf), Some((8, 2)));
    }

    #[test]
    fn find_event_boundary_crlf() {
        let buf = b"data: {}\r\n\r\ndata: {}\r\n\r\n";
        assert_eq!(find_event_boundary(buf), Some((8, 4)));
    }

    #[test]
    fn find_event_boundary_none() {
        let buf = b"data: {}\n";
        assert_eq!(find_event_boundary(buf), None);
    }

    #[test]
    fn process_sse_event_text_chunk() {
        let event = b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]},\"index\":0}]}\n\n";
        let mut first = true;
        let outcomes = process_sse_event(event, "test-id", 0, "gemini-3.1-pro", &mut first);
        // Should have role chunk + content chunk
        assert_eq!(outcomes.len(), 2);
        assert!(!first); // first_chunk should now be false
    }

    #[test]
    fn process_sse_event_malformed_json() {
        let event = b"data: {not valid json}\n\n";
        let mut first = false;
        let outcomes = process_sse_event(event, "test-id", 0, "gemini-3.1-pro", &mut first);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], SseOutcome::Err(_)));
    }

    #[test]
    fn process_sse_event_no_data_line() {
        let event = b"comment: ignore me\n\n";
        let mut first = false;
        let outcomes = process_sse_event(event, "test-id", 0, "gemini-3.1-pro", &mut first);
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], SseOutcome::Skip));
    }

    #[test]
    fn process_sse_event_with_finish_reason() {
        let event = b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Done\"}]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5,\"totalTokenCount\":15}}\n\n";
        let mut first = false;
        let outcomes = process_sse_event(event, "test-id", 0, "gemini-3.1-pro", &mut first);
        // Should have one content+finish chunk
        assert!(!outcomes.is_empty());
        if let SseOutcome::Chunk(chunk) = &outcomes[0] {
            assert_eq!(chunk.choices[0].finish_reason.as_deref(), Some("stop"));
            assert!(chunk.usage.is_some());
        }
    }

    // --- CRLF + no-space data: regression tests (rv-sse-crlf-parsing) ---

    #[test]
    fn process_sse_event_crlf_text_chunk() {
        // CRLF line endings within the event bytes (after boundary stripping).
        let event = b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hello\"}]},\"index\":0}]}\r\n\r\n";
        let mut first = true;
        let outcomes = process_sse_event(event, "test-id", 0, "gemini-3.1-pro", &mut first);
        // Should yield the same role chunk + content chunk as the LF form.
        assert_eq!(
            outcomes.len(),
            2,
            "CRLF-delimited Gemini event should yield role + content chunks"
        );
        assert!(!first);
    }

    #[test]
    fn process_sse_event_no_space_data_prefix() {
        // data:{...} (no space) must not be dropped — parse identically to data: {...}.
        let event = b"data:{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hi\"}]},\"index\":0}]}\n\n";
        let mut first = true;
        let outcomes = process_sse_event(event, "test-id", 0, "gemini-3.1-pro", &mut first);
        assert_eq!(
            outcomes.len(),
            2,
            "no-space data: prefix should parse the same as data: with space"
        );
    }

    #[test]
    fn process_sse_event_ignores_extra_candidates() {
        let event = b"data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"first\"}]},\"index\":0},{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"second\"}]},\"index\":1}]}\n\n";
        let mut first = false;
        let outcomes = process_sse_event(event, "id", 0, "gemini-3.1-pro", &mut first);
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            SseOutcome::Chunk(c) => {
                assert_eq!(c.choices[0].delta.content.as_deref(), Some("first"))
            }
            _ => panic!("expected one content chunk"),
        }
    }
}
