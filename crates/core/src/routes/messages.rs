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
    response::{IntoResponse, Response},
    Json,
};
use futures::stream::StreamExt;
use tt_auth::ApiKeyContext;
use tt_provider_anthropic::messages::{
    anthropic_error_body, anthropic_error_frame, chat_response_to_messages, AnthropicSseEncoder,
    MessagesRequest,
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
) -> Response {
    // Errors must NOT escape via `?` to axum's default `IntoResponse for ApiError`,
    // which renders the gateway's OpenAI-style error envelope. This endpoint is
    // Anthropic-native, so every error body — parse/translate failures, the
    // credential-guard 400, and any upstream error propagated from `chat::handler`
    // — is re-shaped into the Anthropic error envelope before it leaves the handler.
    match handle(State(state), Extension(trace), auth_ctx, headers, body).await {
        Ok(resp) => resp,
        // Render the ApiError to its (status, OpenAI body), then transcode that body
        // to the Anthropic shape so strict Anthropic SDK clients (Claude Code) parse it.
        Err(err) => transcode_error_response(err.into_response()).await,
    }
}

/// Inner handler: parses + translates the request, runs the shared chat pipeline,
/// and transcodes the response. Returns `Err(ApiError)` for the error paths; the
/// outer [`handler`] is responsible for re-shaping those into the Anthropic error
/// envelope (so they never reach axum's OpenAI-style renderer).
async fn handle(
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
        None,
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
///
/// Not every `Ok` chat response is success-shaped: the chat handler returns some
/// client errors (e.g. a negative-cache 4xx hit, or the BYO-only credential guard)
/// as `Ok((status_4xx, Json({"error":{...}})))` rather than via `?`. Those bodies
/// are the gateway's OpenAI-style error envelope, NOT a `ChatCompletionResponse`,
/// so a non-2xx status or an error-shaped body is re-shaped into the Anthropic error
/// envelope (preserving the real status + headers) rather than mis-transcoded as a
/// success body.
async fn transcode_json_response(resp: Response) -> ApiResult<Response> {
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| ApiError::Internal(format!("failed to read chat response body: {e}")))?;

    // Non-success / error-shaped bodies carry the gateway's OpenAI error envelope.
    // Re-shape them to the Anthropic error envelope so strict Anthropic-wire clients
    // (Claude Code) parse them; the original 4xx/5xx status + headers are preserved.
    if !parts.status.is_success() || is_error_body(&bytes) {
        return Ok(rebuild_as_anthropic_error(parts, &bytes));
    }

    let chat: tt_shared::ChatCompletionResponse = serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::Internal(format!("failed to parse chat response body: {e}")))?;

    let mut anthropic = chat_response_to_messages(&chat);
    crate::routes::graft_tokentrimmer_panel(&mut anthropic, &bytes);
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

/// Re-shape an error response (an `ApiError` rendered to its OpenAI envelope) into
/// the Anthropic error envelope, preserving status + headers. Used for the
/// `Err(ApiError)` paths — parse/translate failures, the credential-guard 400, and
/// upstream errors propagated from `chat::handler`.
async fn transcode_error_response(resp: Response) -> Response {
    let (parts, body) = resp.into_parts();
    match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => rebuild_as_anthropic_error(parts, &bytes),
        // Reading the (in-memory) error body should never fail; if it somehow does,
        // surface a minimal Anthropic-shaped error rather than the OpenAI one.
        Err(_) => {
            let value =
                anthropic_error_body(&serde_json::json!({"error": {}}), parts.status.as_u16());
            (parts.status, Json(value)).into_response()
        }
    }
}

/// Build an Anthropic-error-shaped response from response `parts` (status + headers)
/// and an OpenAI-style error body. The status code is preserved verbatim and drives
/// the Anthropic error `type`; the message is taken from the OpenAI `error.message`.
fn rebuild_as_anthropic_error(parts: axum::http::response::Parts, bytes: &[u8]) -> Response {
    let status = parts.status;
    let openai: serde_json::Value =
        serde_json::from_slice(bytes).unwrap_or_else(|_| serde_json::json!({"error": {}}));
    let anthropic = anthropic_error_body(&openai, status.as_u16());
    let new_body = serde_json::to_vec(&anthropic).unwrap_or_default();

    let mut out = Response::from_parts(parts, Body::from(new_body));
    // Body length changed; drop any stale content-length so axum recomputes it.
    out.headers_mut().remove(axum::http::header::CONTENT_LENGTH);
    if let Ok(ct) = "application/json".parse() {
        out.headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, ct);
    }
    out
}

/// Whether `bytes` is a JSON object carrying a top-level `error` field (the chat
/// handler's error-response shape).
fn is_error_body(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("error").cloned())
        .is_some()
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
///
/// The OpenAI-compatible streaming path emits a mid-stream upstream failure as an
/// in-band `data: {"error":{...,"type":"upstream_error"}}` chunk (see
/// `routes::sse`). Such a frame does NOT deserialize as a `ChatCompletionChunk`;
/// rather than silently dropping it (which would present a truncated response as a
/// clean success), it is translated into an Anthropic `event: error` frame so the
/// client sees the failure.
fn process_openai_frame(frame: &[u8], encoder: &mut AnthropicSseEncoder) -> Option<String> {
    let text = std::str::from_utf8(frame).ok()?;

    // Phase 6: forward TokenTrimmer panel/usage SSE events verbatim. The Phase-5
    // streaming panel emits frames like `event: tokentrimmer.panel\ndata: {...}`;
    // the data-line loop below only understands ChatCompletionChunk/error frames and
    // would drop these. A TT-aware client on /v1/messages reads them (and on streams
    // they are the ONLY carrier of panel cost — no x-tokentrimmer-* headers on SSE).
    if text.lines().any(|l| {
        l.trim_end_matches('\r')
            .strip_prefix("event:")
            .map(|s| s.trim().starts_with("tokentrimmer."))
            .unwrap_or(false)
    }) {
        // Emit the frame unchanged, with the blank-line terminator that delimits an
        // SSE frame on the wire.
        let mut out = text.trim_end().to_string();
        out.push_str("\n\n");
        return Some(out);
    }

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
        } else if let Some(err) = parse_inband_error(data) {
            // Mid-stream provider failure — surface it as an Anthropic error event
            // instead of dropping the frame.
            return Some(anthropic_error_frame(&err));
        }
    }
    None
}

/// If `data` is an OpenAI-shaped in-band error payload (`{"error":{...}}`), return
/// the parsed JSON; otherwise `None`.
fn parse_inband_error(data: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(data).ok()?;
    value.get("error").is_some().then_some(value)
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
    fn inband_error_frame_becomes_anthropic_error_event() {
        let mut enc = AnthropicSseEncoder::new();
        // The OpenAI-compat streaming path emits mid-stream failures like this.
        let frame =
            b"data: {\"error\":{\"message\":\"upstream blew up\",\"type\":\"upstream_error\"}}\n\n";
        let out = process_openai_frame(frame, &mut enc).expect("error frame surfaced");
        assert!(out.starts_with("event: error\n"), "out: {out}");
        assert!(out.contains("\"type\":\"api_error\""));
        assert!(out.contains("upstream blew up"));
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

    // Phase 6: tokentrimmer.* SSE event forwarding tests.

    #[test]
    fn tokentrimmer_panel_frame_forwarded_verbatim() {
        let mut enc = AnthropicSseEncoder::new();
        let frame =
            b"event: tokentrimmer.panel\ndata: {\"strategy\":\"synthesize\",\"legs\":[]}\n\n";
        let out = process_openai_frame(frame, &mut enc)
            .expect("tokentrimmer.panel frame must be forwarded");
        assert!(
            out.contains("event: tokentrimmer.panel"),
            "forwarded frame missing event line: {out}"
        );
        assert!(
            out.contains("\"strategy\":\"synthesize\""),
            "forwarded frame missing data payload: {out}"
        );
        // Must be a well-formed SSE frame (ends with blank-line terminator).
        assert!(out.ends_with("\n\n"), "frame must end with \\n\\n: {out:?}");
    }

    #[test]
    fn tokentrimmer_usage_frame_forwarded_verbatim() {
        let mut enc = AnthropicSseEncoder::new();
        let frame = b"event: tokentrimmer.usage\ndata: {\"cost_usd\":0.001}\n\n";
        let out = process_openai_frame(frame, &mut enc)
            .expect("tokentrimmer.usage frame must be forwarded");
        assert!(
            out.contains("event: tokentrimmer.usage"),
            "forwarded frame missing event line: {out}"
        );
        assert!(
            out.contains("\"cost_usd\""),
            "forwarded frame missing data payload: {out}"
        );
        assert!(out.ends_with("\n\n"), "frame must end with \\n\\n: {out:?}");
    }

    #[test]
    fn unrelated_non_data_non_tt_frame_returns_none() {
        let mut enc = AnthropicSseEncoder::new();
        // A comment / keep-alive frame — should still return None.
        assert!(process_openai_frame(b": keep-alive\n\n", &mut enc).is_none());
        // An event frame with an unrelated event name — should return None.
        assert!(process_openai_frame(b"event: ping\ndata: {}\n\n", &mut enc).is_none());
    }

    #[test]
    fn normal_chunk_still_transcodes_after_tt_detection_added() {
        // Regression: adding tt-event detection must NOT break the normal path.
        let mut enc = AnthropicSseEncoder::new();
        let frame = b"data: {\"id\":\"c2\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"claude-sonnet-4-6\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"World\"},\"finish_reason\":null}]}\n\n";
        let out = process_openai_frame(frame, &mut enc).expect("normal chunk must produce frames");
        assert!(out.contains("event: content_block_delta"));
        assert!(out.contains("\"text\":\"World\""));
    }

    /// M-1 negative: `tokentrimmer.panel` that appears only *inside* a `data:` JSON
    /// payload must NOT be force-forwarded — it is NOT an `event: tokentrimmer.*`
    /// frame, so the guard does not match and it goes through the normal
    /// ChatCompletionChunk parse path (returning None here since the data isn't a
    /// valid chunk). Documents the no-false-positive invariant.
    #[test]
    fn tokentrimmer_panel_in_data_payload_not_force_forwarded() {
        let mut enc = AnthropicSseEncoder::new();
        // A `data:` line whose JSON value happens to contain the string
        // "event: tokentrimmer.panel" — the guard must NOT match this.
        let frame = b"data: {\"x\":\"event: tokentrimmer.panel\"}\n\n";
        // This is not a valid ChatCompletionChunk and not a recognized error, so the
        // normal path returns None — it does NOT get force-forwarded as a TT event.
        let out = process_openai_frame(frame, &mut enc);
        assert!(
            out.is_none(),
            "tokentrimmer.panel inside a data: payload must not be force-forwarded; got: {out:?}"
        );
    }
}
