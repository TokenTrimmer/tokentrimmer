//! SSE streaming helper for `/v1/chat/completions` with `stream: true`.
//!
//! Wraps a `BoxStream<ChatCompletionChunk>` as an Axum [`Sse`] response,
//! following the OpenAI SSE convention (JSON chunks terminated with `data: [DONE]`).

use std::convert::Infallible;
use std::sync::Arc;

use axum::response::{
    sse::{Event, KeepAlive},
    IntoResponse, Response, Sse,
};
use futures::stream::{BoxStream, StreamExt};
use uuid::Uuid;

use tt_shared::{
    messages::{ChunkChoice, ChunkDelta, Message, MessageContent},
    ChatCompletionChunk, ChatCompletionResponse, Provider, ProviderError,
};

/// Convert a streaming provider response into an Axum SSE [`Response`].
///
/// Each chunk is serialized to JSON and emitted as `data: <json>`.
/// On stream error, emits `data: {"error":{"message":"…","type":"upstream_error"}}` then ends.
/// Terminates with `data: [DONE]` per OpenAI convention.
///
/// Sets `X-TokenTrimmer-Trace-Id` and `X-TokenTrimmer-Provider` on the response.
pub fn stream_response(
    stream: BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>,
    provider: &Arc<dyn Provider>,
    trace_id: Uuid,
) -> Response {
    let provider_id = provider.id().to_string();
    let trace_id_str = trace_id.to_string();

    // Translate each chunk into an SSE Event with a JSON payload.
    let event_stream = stream
        .map(|result| {
            let json = match result {
                Ok(chunk) => serde_json::to_string(&chunk)
                    .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}")),
                Err(e) => format!(
                    "{{\"error\":{{\"message\":{:?},\"type\":\"upstream_error\"}}}}",
                    e.to_string()
                ),
            };
            Ok::<_, Infallible>(Event::default().data(json))
        })
        // OpenAI streaming convention: end stream with `data: [DONE]`.
        .chain(futures::stream::once(async {
            Ok::<_, Infallible>(Event::default().data("[DONE]"))
        }));

    let mut response = Sse::new(event_stream)
        .keep_alive(KeepAlive::default())
        .into_response();

    let headers = response.headers_mut();
    if let Ok(v) = trace_id_str.parse() {
        headers.insert("x-tokentrimmer-trace-id", v);
    }
    if let Ok(v) = provider_id.parse() {
        headers.insert("x-tokentrimmer-provider", v);
    }

    response
}

/// Build a synthetic streaming response from a cached
/// [`ChatCompletionResponse`]. Used by `w7-fake-stream-cache` — when an
/// L1/L2 hit lands on a request with `stream: true`, we don't have a
/// real upstream stream to forward, so we synthesize one matching the
/// OpenAI SSE format the client expects.
///
/// Three chunks before `[DONE]`:
///
/// 1. `delta.role = "assistant"` — primes clients that switch on role.
/// 2. `delta.content = <full assistant text>` — single content chunk
///    carrying the whole response. Splitting into N small "typing"
///    chunks would add complexity for no behavioural win — clients
///    re-assemble by appending deltas regardless.
/// 3. `finish_reason` + the cached usage — matches OpenAI's
///    stream-with-usage shape so client SDKs can read counts off the
///    terminator.
pub fn fake_stream_from_response(
    response: ChatCompletionResponse,
) -> BoxStream<'static, Result<ChatCompletionChunk, ProviderError>> {
    let id = response.id.clone();
    let model = response.model.clone();
    let created = response.created;
    let usage = response.usage.clone();

    let assistant_text = response
        .choices
        .first()
        .and_then(|c| match &c.message {
            Message::Assistant { content, .. } => match content {
                Some(MessageContent::Text(s)) => Some(s.clone()),
                Some(MessageContent::Parts(parts)) => parts.iter().find_map(|p| match p {
                    tt_shared::ContentPart::Text { text } => Some(text.clone()),
                    _ => None,
                }),
                None => None,
            },
            _ => None,
        })
        .unwrap_or_default();
    let finish_reason = response
        .choices
        .first()
        .and_then(|c| c.finish_reason.clone())
        .unwrap_or_else(|| "stop".into());

    let role_chunk = ChatCompletionChunk {
        id: id.clone(),
        object: "chat.completion.chunk".into(),
        created,
        model: model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: Some("assistant".into()),
                content: None,
                tool_calls: Vec::new(),
            },
            finish_reason: None,
        }],
        usage: None,
    };

    let content_chunk = ChatCompletionChunk {
        id: id.clone(),
        object: "chat.completion.chunk".into(),
        created,
        model: model.clone(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: None,
                content: Some(assistant_text),
                tool_calls: Vec::new(),
            },
            finish_reason: None,
        }],
        usage: None,
    };

    let finish_chunk = ChatCompletionChunk {
        id,
        object: "chat.completion.chunk".into(),
        created,
        model,
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some(finish_reason),
        }],
        usage: Some(usage),
    };

    futures::stream::iter(vec![Ok(role_chunk), Ok(content_chunk), Ok(finish_chunk)]).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::{
        messages::{Choice, Message, MessageContent},
        Usage,
    };

    fn cached_response(text: &str) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl-cached".into(),
            object: "chat.completion".into(),
            created: 1000,
            model: "gpt-4o-mini".into(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text(text.into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 4,
                total_tokens: 9,
                cached_tokens: 5,
                cache_creation_input_tokens: None,
            },
        }
    }

    #[tokio::test]
    async fn fake_stream_emits_role_content_finish() {
        let stream = fake_stream_from_response(cached_response("Hello!"));
        let chunks: Vec<ChatCompletionChunk> =
            stream.filter_map(|r| async { r.ok() }).collect().await;

        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0].choices[0].delta.role.as_deref(),
            Some("assistant")
        );
        assert_eq!(chunks[0].choices[0].delta.content, None);
        assert_eq!(chunks[1].choices[0].delta.role, None);
        assert_eq!(
            chunks[1].choices[0].delta.content.as_deref(),
            Some("Hello!")
        );
        assert_eq!(chunks[2].choices[0].finish_reason.as_deref(), Some("stop"));
        assert!(chunks[2].usage.is_some());
        assert_eq!(chunks[2].usage.as_ref().unwrap().total_tokens, 9);
    }

    #[tokio::test]
    async fn fake_stream_handles_empty_content() {
        let mut resp = cached_response("");
        resp.choices[0].message = Message::Assistant {
            content: None,
            tool_calls: vec![],
            name: None,
        };
        let stream = fake_stream_from_response(resp);
        let chunks: Vec<_> = stream.filter_map(|r| async { r.ok() }).collect().await;
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[1].choices[0].delta.content.as_deref(), Some(""));
    }
}
