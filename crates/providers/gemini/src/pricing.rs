//! Pricing table for Google Gemini models (May 2026 baseline).
//!
//! Values sourced from the Google AI pricing page.
//!
//! # Pricing brackets
//!
//! Gemini uses a two-bracket pricing model based on prompt size:
//! - ≤200K input tokens: standard rate
//! - >200K input tokens: higher rate (approximately 2× standard)
//!
//! `ModelPricing.input_per_million` uses the ≤200K rate as the headline
//! rate, since most requests fit. When a request exceeds 200K input tokens,
//! a `tracing::debug!` is emitted. Full bracket support is a v2 enhancement
//! to the pricing struct.
//!
//! # Cache pricing
//!
//! `cached_input_per_million` is the cached-read rate (approximately 10% of
//! the standard input rate). Cache-write surcharges are not exposed here.
//!
//! Rates come from the versioned, embedded pricing catalog
//! (`tt_shared::pricing`); the headline rate is the ≤200K-token bracket.

use tt_shared::pricing::{catalog, Capability, ModelInfo, ModelPricing};

/// The 200K token threshold above which the higher pricing bracket applies.
/// Adapters log a debug message when exceeded; full per-request bracket pricing
/// is a v2 enhancement.
pub const BRACKET_THRESHOLD_TOKENS: u64 = 200_000;

/// Return the pricing entry for a known Gemini model, or `None` if
/// unrecognized. Delegates to the shared catalog's current rate (≤200K bracket).
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    catalog().latest("gemini", model)
}

/// Return all supported Gemini model descriptors.
pub fn all_models() -> Vec<ModelInfo> {
    let capabilities = vec![
        Capability::Text,
        Capability::Vision,
        Capability::Tools,
        Capability::JsonMode,
        Capability::Streaming,
        Capability::PromptCaching,
    ];

    vec![
        ModelInfo {
            id: "gemini-3.1-flash-lite".to_string(),
            provider: "gemini".to_string(),
            capabilities: capabilities.clone(),
            max_input_tokens: 1_000_000,
            max_output_tokens: 8192,
        },
        ModelInfo {
            id: "gemini-3.5-flash".to_string(),
            provider: "gemini".to_string(),
            capabilities: capabilities.clone(),
            max_input_tokens: 1_000_000,
            max_output_tokens: 8192,
        },
        ModelInfo {
            id: "gemini-3.1-pro".to_string(),
            provider: "gemini".to_string(),
            capabilities,
            max_input_tokens: 2_000_000,
            // Conservative: pro has 65536 in reasoning mode; use 8192 for non-reasoning
            max_output_tokens: 8192,
        },
    ]
}
