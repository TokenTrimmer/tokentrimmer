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

use chrono::Utc;
use tt_shared::pricing::{Capability, ModelInfo, ModelPricing};

/// The 200K token threshold above which the higher pricing bracket applies.
/// Adapters log a debug message when exceeded; full per-request bracket pricing
/// is a v2 enhancement.
pub const BRACKET_THRESHOLD_TOKENS: u64 = 200_000;

/// Return the pricing entry for a known Gemini model, or `None` if
/// unrecognized.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let now = Utc::now();
    match model {
        // gemini-3.1-flash-lite: lightweight, cost-efficient
        // Input ≤200K: $0.25/1M | Output ≤200K: $1.50/1M
        // Input >200K: $0.50/1M | Output >200K: $3.00/1M
        // Cached read: $0.025/1M
        "gemini-3.1-flash-lite" => Some(ModelPricing {
            input_per_million: 0.25,
            output_per_million: 1.50,
            cached_input_per_million: Some(0.025),
            effective_at: now,
        }),
        // gemini-3.5-flash: balanced performance and cost
        // Input ≤200K: $1.50/1M | Output ≤200K: $9.00/1M
        // Input >200K: $3.00/1M | Output >200K: $18.00/1M
        // Cached read: $0.15/1M
        "gemini-3.5-flash" => Some(ModelPricing {
            input_per_million: 1.50,
            output_per_million: 9.00,
            cached_input_per_million: Some(0.15),
            effective_at: now,
        }),
        // gemini-3.1-pro: highest capability, 2M context
        // Input ≤200K: $2.00/1M | Output ≤200K: $12.00/1M
        // Input >200K: $4.00/1M | Output >200K: $18.00/1M
        // Cached read: $0.20/1M
        "gemini-3.1-pro" => Some(ModelPricing {
            input_per_million: 2.00,
            output_per_million: 12.00,
            cached_input_per_million: Some(0.20),
            effective_at: now,
        }),
        _ => None,
    }
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
