//! L1 cache value envelope.
//!
//! The gateway stores a JSON-serialized [`L1Entry`] in the L1 cache rather
//! than the bare provider response. Carrying the cost figures computed at
//! insert time lets a cache hit report accurate `saved_usd` without trying
//! to recompute pricing from a cached [`Usage`] block (whose token counts
//! are an approximation of input size, not of price).
//!
//! # At-rest posture (SEC-2)
//!
//! [`to_bytes`](L1Entry::to_bytes) emits the envelope in **plaintext** — the
//! verbatim provider response included. That plaintext is what lands in Redis
//! unless the L1 cache is wired with a
//! [`ResponseCodec`](crate::ResponseCodec): the codec seals these envelope bytes
//! at the cache-store boundary
//! ([`RedisL1Cache::with_response_codec`](crate::redis_impl::RedisL1Cache::with_response_codec)),
//! so the JSON here never has to change shape. See the crate-root docs for the
//! full at-rest contract.

use serde::{Deserialize, Serialize};
use tt_shared::ChatCompletionResponse;

/// The format the gateway writes into [`L1Cache`].
///
/// [`L1Cache`]: crate::L1Cache
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L1Entry {
    /// Cached provider response body — returned to the client verbatim on a hit.
    pub response: ChatCompletionResponse,

    /// What the original request would have cost without TokenTrimmer
    /// optimizations. `saved_usd` on a hit equals this value (since
    /// `cost_usd` on a hit is 0).
    pub baseline_cost_usd: f64,

    /// What the provider actually charged for the original miss. Useful for
    /// cost-of-cache analytics; not used to build the hit response.
    #[serde(default)]
    pub cost_usd: f64,

    /// Provider that originally served this request, e.g. `"openai"`.
    /// Cache analytics. Not surfaced in hit response headers — those report
    /// `provider=cache` to signal the cache short-circuit.
    #[serde(default)]
    pub provider_id: String,

    /// Schema version. Bump for breaking changes to the envelope shape.
    /// `0` is reserved as a sentinel for entries that pre-date the envelope
    /// format and were decoded via the [`L1Entry::from_bytes`] fallback path.
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    1
}

impl L1Entry {
    /// Build a fresh envelope around a provider response.
    pub fn new(
        response: ChatCompletionResponse,
        baseline_cost_usd: f64,
        cost_usd: f64,
        provider_id: impl Into<String>,
    ) -> Self {
        Self {
            response,
            baseline_cost_usd,
            cost_usd,
            provider_id: provider_id.into(),
            version: 1,
        }
    }

    /// JSON-encode for storage.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Decode storage bytes.
    ///
    /// Tries the envelope format first. Falls back to parsing the bytes as a
    /// raw [`ChatCompletionResponse`] so entries written by the pre-envelope
    /// gateway are still usable; in that case `baseline_cost_usd` and
    /// `cost_usd` are 0 and `version` is 0, telling the caller it must
    /// synthesize savings figures from the cached [`Usage`] block.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        if let Ok(entry) = serde_json::from_slice::<Self>(bytes) {
            return Ok(entry);
        }
        let response: ChatCompletionResponse = serde_json::from_slice(bytes)?;
        Ok(L1Entry {
            response,
            baseline_cost_usd: 0.0,
            cost_usd: 0.0,
            provider_id: String::new(),
            version: 0,
        })
    }

    /// `true` when this envelope was synthesized from a pre-envelope cache
    /// entry — callers should fall back to computing baseline from `Usage`.
    pub fn is_legacy_format(&self) -> bool {
        self.version == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::{
        messages::{Choice, Message, MessageContent},
        Usage,
    };

    fn sample_response() -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl-test".into(),
            object: "chat.completion".into(),
            created: 1_700_000_000,
            model: "gpt-4o".into(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("hi".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        }
    }

    #[test]
    fn envelope_round_trips_through_bytes() {
        let entry = L1Entry::new(sample_response(), 0.0045, 0.0030, "openai");
        let bytes = entry.to_bytes().unwrap();
        let parsed = L1Entry::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.response.id, "chatcmpl-test");
        assert_eq!(parsed.response.model, "gpt-4o");
        assert!((parsed.baseline_cost_usd - 0.0045).abs() < 1e-9);
        assert!((parsed.cost_usd - 0.0030).abs() < 1e-9);
        assert_eq!(parsed.provider_id, "openai");
        assert_eq!(parsed.version, 1);
        assert!(!parsed.is_legacy_format());
    }

    #[test]
    fn from_bytes_falls_back_to_raw_chat_response() {
        // Simulate a pre-envelope entry written by older gateway code: just the
        // ChatCompletionResponse JSON, no wrapping envelope.
        let raw = sample_response();
        let raw_bytes = serde_json::to_vec(&raw).unwrap();

        let parsed = L1Entry::from_bytes(&raw_bytes).unwrap();
        assert_eq!(parsed.response.id, "chatcmpl-test");
        assert_eq!(parsed.baseline_cost_usd, 0.0);
        assert_eq!(parsed.cost_usd, 0.0);
        assert_eq!(parsed.provider_id, "");
        assert_eq!(parsed.version, 0);
        assert!(parsed.is_legacy_format());
    }

    #[test]
    fn from_bytes_rejects_junk() {
        let err = L1Entry::from_bytes(b"not json at all").unwrap_err();
        // Just confirm we got an error rather than a silent default.
        assert!(err.is_syntax() || err.is_data() || err.is_eof());
    }
}
