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
}
