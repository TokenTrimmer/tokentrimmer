//! Cache-key derivation for [`ChatCompletionRequest`].
//!
//! The key is a lowercase SHA-256 hex digest of a canonical JSON object that
//! includes only the fields that affect model outputs.  Fields that are
//! transparent to the model (e.g. `stream`, `n`, `seed`, `user`,
//! `tt_extras`) are deliberately excluded so that requests differing only in
//! those fields share the same cache entry.
//!
//! ## Content normalization (key derivation ONLY)
//!
//! When building the canonical JSON object used for hashing, **text segments
//! in message content are normalized** so that requests that are semantically
//! identical but differ only in trivial ways (trailing whitespace, Unicode
//! encoding form) hash to the same key.  The normalization applied is:
//!
//! 1. **Unicode NFC** — converts characters to Canonical Decomposition followed
//!    by Canonical Composition (e.g. `é` as two code-points → one code-point).
//! 2. **Trailing whitespace trim** — removes `\t`, `\n`, `\r`, space from the
//!    end of each text segment.
//!
//! IMPORTANT: This normalization affects **only the bytes fed to the SHA-256
//! hash**.  The request forwarded upstream to the provider is **never
//! modified** — callers see their original content reflected back verbatim.
//!
//! ## Intentionally deferred: model-alias canonicalization
//!
//! Mapping dated model snapshots to floating aliases (e.g. `gpt-4o-2024-08-06`
//! → `gpt-4o`) is NOT implemented here.  A dated snapshot and its floating
//! alias can have different capabilities or system-prompt defaults; serving
//! one model's cached response for another is a correctness risk.  This
//! optimization is deferred until a registry-backed KNOWN-equivalent map can
//! guarantee strict semantic equivalence between alias pairs.
//!
//! [`ChatCompletionRequest`]: tt_shared::messages::ChatCompletionRequest

use unicode_normalization::UnicodeNormalization as _;

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
    let bytes = serde_json::to_vec(&canonical).expect("canonical Value is always serializable");
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)
}

/// Round an `f32` to 6 decimal places, returning it as an `f64` for JSON
/// serialization precision.
fn round6(v: f32) -> f64 {
    let factor = 1_000_000.0_f64;
    ((v as f64) * factor).round() / factor
}

/// Normalize a [`MessageContent`] value for **cache-key derivation only**.
///
/// Delegates text segments to [`normalize_text_for_key`]; non-text content
/// parts (images, audio, etc.) are cloned as-is.
fn normalize_message_content(
    content: &tt_shared::messages::MessageContent,
) -> tt_shared::messages::MessageContent {
    use tt_shared::messages::{ContentPart, MessageContent};
    match content {
        MessageContent::Text(s) => MessageContent::Text(normalize_text_for_key(s)),
        MessageContent::Parts(parts) => MessageContent::Parts(
            parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => ContentPart::Text {
                        text: normalize_text_for_key(text),
                    },
                    other => other.clone(),
                })
                .collect(),
        ),
    }
}

/// Normalize a text segment for **cache-key derivation only**.
///
/// Applies Unicode NFC normalization followed by trailing-whitespace trimming.
/// The returned `String` is used only when building the canonical JSON object
/// that is hashed to produce the cache key; the original string is forwarded
/// to the upstream provider unchanged.
///
/// This merges requests that are identical except for:
/// - Different Unicode encoding forms of the same character (NFC vs NFD).
/// - Trailing whitespace characters (`\t`, `\n`, `\r`, space).
///
/// Leading whitespace and mid-content whitespace are intentionally preserved —
/// a space at the start or inside a message can be semantically meaningful.
fn normalize_text_for_key(s: &str) -> String {
    // 1. Apply Unicode NFC: iterator yields NFC code points, collect to String.
    let nfc: String = s.nfc().collect();
    // 2. Trim trailing ASCII whitespace characters.
    nfc.trim_end_matches([' ', '\t', '\n', '\r']).to_owned()
}

/// Build the canonical [`serde_json::Value`] used for key derivation.
///
/// Keys are emitted in a fixed, alphabetically-sorted order so that the JSON
/// bytes are deterministic regardless of field insertion order.
///
/// Text segments in messages are normalized via [`normalize_text_for_key`]
/// (NFC + trailing-whitespace trim) before hashing.  The upstream request is
/// never modified — normalization applies only to the bytes used for hashing.
fn build_canonical(req: &ChatCompletionRequest) -> serde_json::Value {
    use serde_json::{json, Value};
    use tt_shared::messages::Message;

    // Normalize text segments in messages for key derivation, then serialize.
    // We clone+patch the messages vec rather than the full request so that only
    // the hashed representation is affected; the original `req` is untouched.
    let normalized_messages: Vec<Message> = req
        .messages
        .iter()
        .map(|msg| match msg {
            Message::User { content, name } => Message::User {
                name: name.clone(),
                content: normalize_message_content(content),
            },
            Message::System { content } => Message::System {
                content: normalize_message_content(content),
            },
            Message::Assistant {
                content,
                tool_calls,
                name,
            } => Message::Assistant {
                name: name.clone(),
                tool_calls: tool_calls.clone(),
                content: content.as_ref().map(normalize_message_content),
            },
            Message::Tool {
                content,
                tool_call_id,
            } => Message::Tool {
                tool_call_id: tool_call_id.clone(),
                content: normalize_message_content(content),
            },
        })
        .collect();

    // Serialize normalized messages via serde so complex enum variants are
    // handled correctly.
    let messages: Value =
        serde_json::to_value(&normalized_messages).expect("messages always serializable");

    // Tools — empty vec becomes JSON null so it is omitted consistently.
    let tools: Value = if req.tools.is_empty() {
        Value::Null
    } else {
        serde_json::to_value(&req.tools).expect("tools always serializable")
    };

    let tool_choice: Value =
        serde_json::to_value(&req.tool_choice).expect("tool_choice always serializable");

    let response_format: Value =
        serde_json::to_value(&req.response_format).expect("response_format always serializable");

    // Sort stop sequences for stability.
    let mut stop = req.stop.clone();
    stop.sort_unstable();
    let stop: Value = serde_json::to_value(&stop).expect("stop always serializable");

    // Build the object with keys in sorted (alphabetical) order so the JSON
    // output is deterministic.
    // Genuinely-unknown / newer passthrough fields (`extra`) may steer model
    // output, so they contribute to the key. Serialize the whole map (BTreeMap
    // ordering inside serde_json gives a stable representation) — empty becomes
    // null so a request without extras is unaffected.
    let extra: Value = if req.extra.is_empty() {
        Value::Null
    } else {
        serde_json::to_value(&req.extra).expect("extra always serializable")
    };

    json!({
        "extra": extra,
        "frequency_penalty": req.frequency_penalty.map(round6),
        "max_completion_tokens": req.max_completion_tokens,
        "max_tokens": req.max_tokens,
        "messages": messages,
        "model": req.model,
        "parallel_tool_calls": req.parallel_tool_calls,
        "presence_penalty": req.presence_penalty.map(round6),
        "reasoning_effort": req.reasoning_effort,
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
            ..Default::default()
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
    fn max_completion_tokens_affects_key() {
        // The reasoning-model spend cap changes the output ceiling, so it must
        // not collide with a request that omits it.
        let a = base_request();
        let mut b = base_request();
        b.max_completion_tokens = Some(128);
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn reasoning_effort_affects_key() {
        let mut a = base_request();
        let mut b = base_request();
        a.reasoning_effort = Some("low".into());
        b.reasoning_effort = Some("high".into());
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn parallel_tool_calls_affects_key() {
        let mut a = base_request();
        let mut b = base_request();
        a.parallel_tool_calls = Some(true);
        b.parallel_tool_calls = Some(false);
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn passthrough_extra_affects_key() {
        // Genuinely-unknown fields may steer the model, so a request that sets
        // one must not collide with one that does not.
        let a = base_request();
        let mut b = base_request();
        b.extra
            .insert("service_tier".into(), serde_json::json!("flex"));
        assert_ne!(cache_key(&a), cache_key(&b));
    }

    #[test]
    fn stream_options_ignored() {
        // stream_options only controls SSE framing, not the model output.
        let mut a = base_request();
        let mut b = base_request();
        a.stream_options = None;
        b.stream_options = Some(serde_json::json!({ "include_usage": true }));
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
