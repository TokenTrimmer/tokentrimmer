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

use tt_shared::{ChatCompletionChunk, Provider, ProviderError};

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
