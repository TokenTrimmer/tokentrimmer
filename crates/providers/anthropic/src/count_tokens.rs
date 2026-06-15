//! Anthropic `POST /v1/messages/count_tokens` support (COST-6).
//!
//! `cl100k` (the tiktoken proxy [`tt_tokenize`] uses for Anthropic) systematically
//! UNDERCOUNTS Anthropic input by ~15–20%, so every Anthropic-attributed dollar
//! estimate keyed on it runs low. This module adds an authoritative token count
//! via Anthropic's own `count_tokens` endpoint, used wherever the exact prompt
//! size matters more than the proxy's speed.
//!
//! The endpoint accepts the same request body as `/v1/messages` **minus**
//! `max_tokens` and `stream`, and returns `{ "input_tokens": <int> }`. We build
//! the body by reusing [`crate::translate::translate_request`] (so the count
//! reflects exactly what would be dispatched) and stripping those two fields.
//!
//! Results are memoized in an in-memory cache keyed on a content hash of the
//! request body, so repeated counts of the same prompt cost one network call.
//! On ANY error (HTTP, network, deserialize) the caller falls back to the
//! `tt_tokenize` proxy estimate — see [`crate::AnthropicProvider::count_tokens`].

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::Deserialize;
use tt_shared::ProviderError;

/// Response body of `POST /v1/messages/count_tokens`.
#[derive(Debug, Deserialize)]
pub struct CountTokensResponse {
    /// The number of input tokens the prompt would consume.
    pub input_tokens: u64,
}

/// Build the `count_tokens` request body from a canonical request.
///
/// Reuses [`crate::translate::translate_request`] so the counted prompt is
/// byte-identical to what the chat path would send, then strips the two fields
/// the count endpoint does not accept: `max_tokens` (required by `/v1/messages`,
/// rejected here) and `stream` (counting is never streamed).
pub fn build_count_tokens_body(
    req: tt_shared::ChatCompletionRequest,
) -> Result<serde_json::Value, ProviderError> {
    let translated = crate::translate::translate_request(req)?;
    let mut value = serde_json::to_value(&translated)
        .map_err(|e| ProviderError::Deserialize(format!("count_tokens body serialize: {e}")))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("max_tokens");
        obj.remove("stream");
    }
    Ok(value)
}

/// A stable (within-process) content hash of a `count_tokens` body, used as the
/// memo-cache key. `serde_json::Value` objects serialize deterministically, so
/// identical bodies hash identically; `DefaultHasher` uses fixed seeds so the
/// value is reproducible for the life of the process.
#[must_use]
pub fn content_hash(body: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    body.to_string().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tt_shared::{
        messages::{Message, MessageContent},
        ChatCompletionRequest,
    };

    fn req(model: &str, text: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![Message::User {
                content: MessageContent::Text(text.to_string()),
                name: None,
            }],
            max_tokens: Some(512),
            stream: true,
            tt_extras: HashMap::new(),
            ..Default::default()
        }
    }

    #[test]
    fn body_strips_max_tokens_and_stream() {
        let body = build_count_tokens_body(req("claude-sonnet-4-6", "hello")).expect("body");
        let obj = body.as_object().expect("object");
        assert!(
            !obj.contains_key("max_tokens"),
            "count_tokens rejects max_tokens"
        );
        assert!(
            !obj.contains_key("stream"),
            "count_tokens is never streamed"
        );
        // The fields the endpoint DOES need survive.
        assert_eq!(
            obj.get("model").and_then(|v| v.as_str()),
            Some("claude-sonnet-4-6")
        );
        assert!(obj.get("messages").and_then(|v| v.as_array()).is_some());
    }

    #[test]
    fn content_hash_is_stable_and_distinguishes_bodies() {
        let a = build_count_tokens_body(req("claude-sonnet-4-6", "hello")).unwrap();
        let a2 = build_count_tokens_body(req("claude-sonnet-4-6", "hello")).unwrap();
        let b = build_count_tokens_body(req("claude-sonnet-4-6", "different")).unwrap();
        assert_eq!(content_hash(&a), content_hash(&a2), "same body → same hash");
        assert_ne!(
            content_hash(&a),
            content_hash(&b),
            "different prompt → different hash"
        );
    }
}
