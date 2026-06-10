//! `POST /v1/messages` — Anthropic-native Messages API ingress.
//!
//! Lets Anthropic-wire clients (Claude Code, the Anthropic SDKs) use the hosted
//! gateway directly. The request is translated from the Anthropic Messages shape
//! into the canonical [`ChatCompletionRequest`] and dispatched through the SAME
//! [`chat::handler`](crate::routes::chat::handler) — so cost accounting, routing,
//! caching, credential resolution, and the #119 BYO-only guard all apply
//! identically; nothing forks the chat pipeline. The chat response is then
//! translated back into the Anthropic Messages shape:
//! - non-streaming: a `{type:"message", ...}` JSON body,
//! - streaming: Anthropic typed SSE event frames (`message_start`, `content_block_*`,
//!   `message_delta`, `message_stop`).
//!
//! The `x-tokentrimmer-*` cost headers and the HTTP status from the chat handler
//! are preserved verbatim — only the body shape changes.

use axum::{
    body::{Body, Bytes},
    extract::{Extension, State},
    http::HeaderMap,
    response::Response,
    Json,
};
use futures::stream::StreamExt;
use tt_auth::ApiKeyContext;
use tt_provider_anthropic::messages::{
    chat_response_to_messages, AnthropicSseEncoder, MessagesRequest,
};
use tt_shared::ChatCompletionChunk;

use crate::{middleware::trace::TraceId, routes::chat, ApiError, ApiResult, AppState};

/// Handler for `POST /v1/messages`.
///
/// Translates the inbound Anthropic Messages request to the canonical shape,
/// runs it through [`chat::handler`], and translates the response (JSON or SSE)
/// back to the Anthropic Messages shape, preserving cost headers and status.
pub async fn handler(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    // 1. Parse + translate the inbound Anthropic request → canonical request.
    let inbound =
        MessagesRequest::from_json(&body).map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    let chat_req = inbound
        .into_chat_request()
        .map_err(|e| ApiError::InvalidRequest(e.to_string()))?;

    // 2. Run the SAME chat pipeline (routing/cache/cost/credential/BYO guard).
    let chat_resp = chat::handler(
        State(state),
        Extension(trace),
        auth_ctx,
        headers,
        Json(chat_req),
    )
    .await?;

    // 3. Translate the chat response body back to the Anthropic Messages shape,
    //    keeping the status + every x-tokentrimmer-* header. Branch on the actual
    //    response content-type rather than the request's `stream` flag: the chat
    //    handler may answer a streaming request with a JSON body (e.g. the
    //    `tt_test_*` sandbox short-circuit), and that must transcode as JSON.
    if is_event_stream(&chat_resp) {
        Ok(transcode_sse_response(chat_resp))
    } else {
        transcode_json_response(chat_resp).await
    }
}

/// Whether a response is an SSE stream (`content-type: text/event-stream`).
fn is_event_stream(resp: &Response) -> bool {
    resp.headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/event-stream"))
}

/// Buffer a non-streaming chat response, translate its `ChatCompletionResponse`
/// body into an Anthropic Messages JSON body, and re-attach the original status
/// and headers.
async fn transcode_json_response(resp: Response) -> ApiResult<Response> {
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| ApiError::Internal(format!("failed to read chat response body: {e}")))?;

    let chat: tt_shared::ChatCompletionResponse = serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::Internal(format!("failed to parse chat response body: {e}")))?;

    let anthropic = chat_response_to_messages(&chat);
    let new_body = serde_json::to_vec(&anthropic)
        .map_err(|e| ApiError::Internal(format!("failed to serialize Anthropic response: {e}")))?;

    let mut out = Response::from_parts(parts, Body::from(new_body));
    // Body length changed; drop any stale content-length so axum recomputes it.
    out.headers_mut().remove(axum::http::header::CONTENT_LENGTH);
    if let Ok(ct) = "application/json".parse() {
        out.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, ct);
    }
    Ok(out)
}

/// Wrap a streaming chat (OpenAI SSE) response in a transform that re-emits
/// Anthropic typed SSE event frames, preserving status and headers.
fn transcode_sse_response(resp: Response) -> Response {
    let (mut parts, body) = resp.into_parts();
    // Content length is meaningless for a re-encoded stream.
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);

    let upstream = body.into_data_stream();
    let anthropic_stream = async_stream::stream! {
        let mut encoder = AnthropicSseEncoder::new();
        let mut buffer: Vec<u8> = Vec::new();
        futures::pin_mut!(upstream);

        while let Some(item) = upstream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(_) => break,
            };
            buffer.extend_from_slice(&chunk);

            // OpenAI SSE frames are delimited by a blank line (\n\n).
            while let Some(pos) = find_double_newline(&buffer) {
                let frame: Vec<u8> = buffer.drain(..pos + 2).collect();
                if let Some(out) = process_openai_frame(&frame, &mut encoder) {
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(out));
                }
            }
        }

        // Flush any trailing partial frame (no terminating blank line).
        if !buffer.is_empty() {
            if let Some(out) = process_openai_frame(&buffer, &mut encoder) {
                yield Ok(Bytes::from(out));
            }
        }

        // Terminal Anthropic frames (content_block_stop / message_delta / message_stop).
        let tail = encoder.finish().concat();
        if !tail.is_empty() {
            yield Ok(Bytes::from(tail));
        }
    };

    let mut out = Response::from_parts(parts, Body::from_stream(anthropic_stream));
    if let Ok(ct) = "text/event-stream".parse() {
        out.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, ct);
    }
    out
}

/// Parse one OpenAI SSE frame and feed any embedded `ChatCompletionChunk` to the
/// encoder, returning the concatenated Anthropic frames it produced (if any).
/// `data: [DONE]` and non-`data:` lines are ignored — the Anthropic terminator
/// is emitted by [`AnthropicSseEncoder::finish`].
fn process_openai_frame(frame: &[u8], encoder: &mut AnthropicSseEncoder) -> Option<String> {
    let text = std::str::from_utf8(frame).ok()?;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some(data) = line
            .strip_prefix("data:")
            .map(|s| s.strip_prefix(' ').unwrap_or(s))
        else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(chunk) = serde_json::from_str::<ChatCompletionChunk>(data) {
            let frames = encoder.push_chunk(&chunk);
            if !frames.is_empty() {
                return Some(frames.concat());
            }
        }
    }
    None
}

/// Find the first `\n\n` boundary in `buf`, returning the index of the first `\n`.
fn find_double_newline(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_double_newline_basic() {
        assert_eq!(find_double_newline(b"data: {}\n\nrest"), Some(8));
        assert_eq!(find_double_newline(b"data: {}\n"), None);
    }

    #[test]
    fn done_and_blank_lines_ignored() {
        let mut enc = AnthropicSseEncoder::new();
        assert!(process_openai_frame(b"data: [DONE]\n\n", &mut enc).is_none());
        assert!(process_openai_frame(b": keep-alive\n\n", &mut enc).is_none());
    }

    #[test]
    fn openai_chunk_frame_produces_anthropic_frames() {
        let mut enc = AnthropicSseEncoder::new();
        let frame = b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"claude-sonnet-4-6\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n";
        let out = process_openai_frame(frame, &mut enc).expect("frames produced");
        assert!(out.contains("event: message_start"));
        assert!(out.contains("event: content_block_delta"));
        assert!(out.contains("\"text\":\"Hi\""));
    }
}
