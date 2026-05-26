//! Cache-key derivation for [`ChatCompletionRequest`].
//!
//! The key is a lowercase SHA-256 hex digest of a canonical JSON object that
//! includes only the fields that affect model outputs.  Fields that are
//! transparent to the model (e.g. `stream`, `n`, `seed`, `user`,
//! `tt_extras`) are deliberately excluded so that requests differing only in
//! those fields share the same cache entry.
//!
//! [`ChatCompletionRequest`]: tt_shared::messages::ChatCompletionRequest

use sha2::{Digest, Sha256};
use tt_shared::messages::ChatCompletionRequest;

/// Derive a stable, hex-encoded SHA-256 cache key from `req`.
///
/// Two requests that differ *only* in `stream`, `n`, `seed`, `user`, or
/// `tt_extras` will produce the **same** key.  All other semantic fields
/// contribute to the key.
///
/// `stop` sequences are sorted lexicographically before hashing so that
/// `["a","b"]` and `["b","a"]` produce the same key.
///
/// Floating-point fields (`temperature`, `top_p`, `presence_penalty`,
/// `frequency_penalty`) are rounded to 6 decimal places before serialization
/// to avoid hash churn from tiny floating-point representation differences.
pub fn cache_key(req: &ChatCompletionRequest) -> String {
    let canonical = build_canonical(req);
    // serde_json::to_vec with sorted keys is not built-in; we serialize our
    // carefully constructed Value whose keys are already in a defined order.
    let bytes = serde_json::to_vec(&canonical)
        .expect("canonical Value is always serializable");
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

/// Round an `f32` to 6 decimal places, returning it as an `f64` for JSON
/// serialization precision.
fn round6(v: f32) -> f64 {
    let factor = 1_000_000.0_f64;
    ((v as f64) * factor).round() / factor
}

/// Build the canonical [`serde_json::Value`] used for key derivation.
///
/// Keys are emitted in a fixed, alphabetically-sorted order so that the JSON
/// bytes are deterministic regardless of field insertion order.
fn build_canonical(req: &ChatCompletionRequest) -> serde_json::Value {
    use serde_json::{json, Value};

    // Serialize messages via serde so complex enum variants are handled correctly.
    let messages: Value = serde_json::to_value(&req.messages)
        .expect("messages always serializable");

    // Tools — empty vec becomes JSON null so it is omitted consistently.
    let tools: Value = if req.tools.is_empty() {
        Value::Null
    } else {
        serde_json::to_value(&req.tools).expect("tools always serializable")
    };

    let tool_choice: Value = serde_json::to_value(&req.tool_choice)
        .expect("tool_choice always serializable");

    let response_format: Value = serde_json::to_value(&req.response_format)
        .expect("response_format always serializable");

    // Sort stop sequences for stability.
    let mut stop = req.stop.clone();
    stop.sort_unstable();
    let stop: Value = serde_json::to_value(&stop).expect("stop always serializable");

    // Build the object with keys in sorted (alphabetical) order so the JSON
    // output is deterministic.
    json!({
        "frequency_penalty": req.frequency_penalty.map(round6),
        "max_tokens": req.max_tokens,
        "messages": messages,
        "model": req.model,
        "presence_penalty": req.presence_penalty.map(round6),
        "response_format": response_format,
        "stop": stop,
        "temperature": req.temperature.map(round6),
        "tool_choice": tool_choice,
        "tools": tools,
        "top_p": req.top_p.map(round6),
    })
}

// ---------------------------------------------------------------------------
// hex encoding — tiny inline impl to avoid pulling in the `hex` crate when
// `sha2` already provides the bytes.  Actually: let's just use std formatting.
// ---------------------------------------------------------------------------
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::{ChatCompletionRequest, Message, MessageContent};

    fn base_request() -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::User {
                content: MessageContent::Text("hello".into()),
                name: None,
            }],
            temperature: Some(0.7),
            top_p: None,
            max_tokens: Some(256),
            stream: false,
            tools: vec![],
            tool_choice: None,
            response_format: None,
            stop: vec![],
            presence_penalty: None,
            frequency_penalty: None,
            n: None,
            seed: None,
            user: None,
            tt_extras: Default::default(),
        }
    }

    #[test]
    fn stream_field_ignored() {
        let mut a = base_request();
        let mut b = base_request();
        a.stream = false;
        b.stream = true;
        assert_eq!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn user_field_ignored() {
        let mut a = base_request();
        let mut b = base_request();
        a.user = None;
        b.user = Some("alice".into());
        assert_eq!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn different_message_produces_different_key() {
        let a = base_request();
        let mut b = base_request();
        b.messages = vec![Message::User {
            content: MessageContent::Text("goodbye".into()),
            name: None,
        }];
        assert_ne!(cache_key(&a), cache_key(&b));
    }
}
