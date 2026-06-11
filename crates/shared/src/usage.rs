//! Token usage and cost accounting. Token counts come from provider responses —
//! we never estimate locally for billing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,

    /// Cached input tokens (Anthropic cache_read_input_tokens, OpenAI cached_tokens).
    /// Always populated by the adapter; 0 when no cache hit.
    #[serde(default)]
    pub cached_tokens: u64,

    /// Anthropic-specific: tokens written to cache on this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,

    /// Raw provider-reported cache-read token count (Anthropic
    /// `cache_read_input_tokens`, OpenAI `prompt_tokens_details.cached_tokens`,
    /// Gemini `cachedContentTokenCount`). `None` = the provider reported no
    /// cache-read figure at all — distinct from `Some(0)` ("reported zero").
    /// `cached_tokens` above remains the folded convenience (absent => 0) that
    /// the cost math consumes; when this is `Some(n)`, `cached_tokens == n`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}
