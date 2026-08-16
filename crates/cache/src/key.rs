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
//! ## Model-alias canonicalization (opt-in)
//!
//! Mapping dated model snapshots to floating aliases (e.g. `gpt-4o-2024-08-06`
//! → `gpt-4o`) is exposed here as an **opt-in hook** ([`ModelCanonicalizer`]),
//! not a default.  A dated snapshot and its floating alias can differ in
//! capabilities or system-prompt defaults, so collapsing them is correct ONLY
//! for pairs a registry asserts are identical.  [`cache_key`] therefore uses the
//! no-op [`IdentityCanonicalizer`] (key derivation is unchanged); a caller that
//! holds an asserted-identical alias map opts in via [`cache_key_with`] with an
//! [`AliasMapCanonicalizer`], so requests pinning an identical snapshot share
//! one cache entry instead of fragmenting (higher hit rate, no correctness
//! risk).
//!
//! [`ChatCompletionRequest`]: tt_shared::messages::ChatCompletionRequest

use std::borrow::Cow;
use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization as _;

use sha2::{Digest, Sha256};
use tt_shared::messages::ChatCompletionRequest;

// ---------------------------------------------------------------------------
// Model-alias canonicalization hook (COST-5)
// ---------------------------------------------------------------------------

/// Maps a (possibly dated-snapshot) model id to the canonical id used for cache
/// identity, so requests pinning a dated snapshot of a model a registry asserts
/// is identical to its floating alias (e.g. `gpt-4o-2024-08-06` → `gpt-4o`)
/// share one cache entry instead of fragmenting across snapshots.
///
/// # Correctness contract
///
/// An implementation MUST collapse two ids ONLY when a registry asserts the two
/// produce identical outputs for identical inputs. A dated snapshot and its
/// floating alias can otherwise differ in capabilities or system-prompt
/// defaults, and serving one's cached response for the other is a correctness
/// bug, not a hit-rate win. When in doubt, leave the id unchanged
/// ([`IdentityCanonicalizer`]).
pub trait ModelCanonicalizer {
    /// The canonical cache identity for `model`. Return `model` unchanged when
    /// no asserted-identical alias is known.
    fn canonicalize<'a>(&self, model: &'a str) -> Cow<'a, str>;
}

/// The no-op canonicalizer: every id is its own cache identity. This is the
/// default used by [`cache_key`], so the default key derivation is unchanged —
/// no snapshot is ever collapsed unless a caller opts in via [`cache_key_with`]
/// with a populated [`AliasMapCanonicalizer`].
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityCanonicalizer;

impl ModelCanonicalizer for IdentityCanonicalizer {
    fn canonicalize<'a>(&self, model: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(model)
    }
}

/// A canonicalizer backed by an explicit `dated-snapshot → canonical-alias` map.
///
/// The map is supplied by the caller from a source that ASSERTS the pairs are
/// identical (the correctness contract on [`ModelCanonicalizer`]). An id absent
/// from the map is left unchanged.
#[derive(Debug, Clone, Default)]
pub struct AliasMapCanonicalizer {
    aliases: HashMap<String, String>,
}

impl AliasMapCanonicalizer {
    /// Build from a map of `dated-snapshot → canonical-alias` pairs the caller
    /// has asserted are identical.
    #[must_use]
    pub fn new(aliases: HashMap<String, String>) -> Self {
        Self { aliases }
    }
}

impl ModelCanonicalizer for AliasMapCanonicalizer {
    fn canonicalize<'a>(&self, model: &'a str) -> Cow<'a, str> {
        match self.aliases.get(model) {
            Some(canonical) => Cow::Owned(canonical.clone()),
            None => Cow::Borrowed(model),
        }
    }
}

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
    cache_key_with(req, &IdentityCanonicalizer)
}

/// Derive a cache key like [`cache_key`], but canonicalize the request's model
/// id through `canonicalizer` first.
///
/// With [`IdentityCanonicalizer`] this is byte-for-byte identical to
/// [`cache_key`]. With a populated [`AliasMapCanonicalizer`], a request pinning
/// a dated snapshot the caller asserts is identical to its floating alias hashes
/// to the SAME key as the alias — so the L1/L2 cache does not fragment across
/// snapshots. Every other field contributes to the key exactly as in
/// [`cache_key`].
pub fn cache_key_with(
    req: &ChatCompletionRequest,
    canonicalizer: &dyn ModelCanonicalizer,
) -> String {
    cache_key_with_policy(req, canonicalizer, tt_shared::messages::CachePrunePolicy::None)
}

/// Derive a cache key with explicit [`ModelCanonicalizer`] and [`tt_shared::messages::CachePrunePolicy`].
///
/// When a prune policy like [`CachePrunePolicy::Whitespace`] or [`CachePrunePolicy::Tools`] is active,
/// extra non-semantic noise (multiline empty pads, volatile tool whitespace) is pruned before hashing,
/// increasing cache hit rates across equivalent conversations.
pub fn cache_key_with_policy(
    req: &ChatCompletionRequest,
    canonicalizer: &dyn ModelCanonicalizer,
    policy: tt_shared::messages::CachePrunePolicy,
) -> String {
    let canonical_model = canonicalizer.canonicalize(&req.model);
    let canonical = build_canonical_with_policy(req, &canonical_model, policy);
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
    normalize_message_content_with_policy(content, tt_shared::messages::CachePrunePolicy::None)
}

fn normalize_message_content_with_policy(
    content: &tt_shared::messages::MessageContent,
    policy: tt_shared::messages::CachePrunePolicy,
) -> tt_shared::messages::MessageContent {
    use tt_shared::messages::{ContentPart, MessageContent};
    match content {
        MessageContent::Text(s) => MessageContent::Text(normalize_text_for_key_with_policy(s, policy)),
        MessageContent::Parts(parts) => MessageContent::Parts(
            parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => ContentPart::Text {
                        text: normalize_text_for_key_with_policy(text, policy),
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
    normalize_text_for_key_with_policy(s, tt_shared::messages::CachePrunePolicy::None)
}

fn normalize_text_for_key_with_policy(s: &str, policy: tt_shared::messages::CachePrunePolicy) -> String {
    use tt_shared::messages::CachePrunePolicy;
    let nfc: String = s.nfc().collect();
    let trimmed = nfc.trim_end_matches([' ', '\t', '\n', '\r']);
    match policy {
        CachePrunePolicy::None => trimmed.to_owned(),
        CachePrunePolicy::Auto | CachePrunePolicy::Whitespace => {
            // Standardize CRLF to LF then collapse 3+ consecutive newlines into 2
            let standardized = trimmed.replace("\r\n", "\n").replace('\r', "\n");
            let mut out = String::with_capacity(standardized.len());
            let mut newline_count = 0;
            for c in standardized.chars() {
                if c == '\n' {
                    newline_count += 1;
                    if newline_count <= 2 {
                        out.push(c);
                    }
                } else {
                    newline_count = 0;
                    out.push(c);
                }
            }
            out
        }
        CachePrunePolicy::Tools => {
            // Minify JSON if valid JSON payload in tool output/arguments
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                serde_json::to_string(&v).unwrap_or_else(|_| trimmed.to_owned())
            } else {
                trimmed.to_owned()
            }
        }
        _ => trimmed.to_owned(),
    }
}

fn build_canonical_with_policy(
    req: &ChatCompletionRequest,
    model: &str,
    policy: tt_shared::messages::CachePrunePolicy,
) -> serde_json::Value {
    use serde_json::{json, Value};
    use tt_shared::messages::Message;

    let normalized_messages: Vec<Message> = req
        .messages
        .iter()
        .map(|msg| match msg {
            Message::User { content, name } => Message::User {
                name: name.clone(),
                content: normalize_message_content_with_policy(content, policy),
            },
            Message::System { content } => Message::System {
                content: normalize_message_content_with_policy(content, policy),
            },
            Message::Assistant {
                content,
                tool_calls,
                name,
            } => Message::Assistant {
                name: name.clone(),
                tool_calls: tool_calls.clone(),
                content: content.as_ref().map(|c| normalize_message_content_with_policy(c, policy)),
            },
            Message::Tool {
                content,
                tool_call_id,
            } => Message::Tool {
                tool_call_id: tool_call_id.clone(),
                content: normalize_message_content_with_policy(content, policy),
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
        "model": model,
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

    // ── Model-alias canonicalization (COST-5) ───────────────────────────────

    #[test]
    fn identity_canonicalizer_matches_default_key() {
        // The default key derivation IS `cache_key_with(.., IdentityCanonicalizer)`
        // — so the public default is unchanged (no key churn).
        let req = base_request();
        assert_eq!(
            cache_key(&req),
            cache_key_with(&req, &IdentityCanonicalizer)
        );
    }

    #[test]
    fn alias_map_collapses_asserted_identical_snapshot() {
        let mut dated = base_request();
        dated.model = "gpt-4o-2024-08-06".into();
        let mut bare = base_request();
        bare.model = "gpt-4o".into();

        // Default (no canonicalization): a dated snapshot fragments the cache.
        assert_ne!(
            cache_key(&dated),
            cache_key(&bare),
            "without canonicalization a dated snapshot must NOT collide with its alias"
        );

        // With an asserted-identical alias map, the snapshot shares the alias's key.
        let mut aliases = std::collections::HashMap::new();
        aliases.insert("gpt-4o-2024-08-06".to_string(), "gpt-4o".to_string());
        let canon = AliasMapCanonicalizer::new(aliases);
        assert_eq!(
            cache_key_with(&dated, &canon),
            cache_key_with(&bare, &canon),
            "a canonicalized snapshot must share its alias's cache entry"
        );
        // The canonicalized snapshot key equals the bare model's DEFAULT key.
        assert_eq!(cache_key_with(&dated, &canon), cache_key(&bare));
    }

    #[test]
    fn alias_map_leaves_unmapped_model_unchanged() {
        let req = base_request(); // model = "gpt-4o", not in the map
        let mut aliases = std::collections::HashMap::new();
        aliases.insert("some-other-snapshot".to_string(), "some-other".to_string());
        let canon = AliasMapCanonicalizer::new(aliases);
        assert_eq!(
            cache_key_with(&req, &canon),
            cache_key(&req),
            "an unmapped model must hash identically to the default key"
        );
    }
}
