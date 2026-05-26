//! Pricing table for Anthropic models (May 2026 baseline).
//!
//! Values sourced from Anthropic pricing page. `cached_input_per_million` uses
//! the cache **read** rate (10% of standard input). Cache-write surcharges are
//! not exposed here because they are recovered on second use.

use chrono::Utc;
use tt_shared::pricing::{Capability, ModelInfo, ModelPricing};

/// Return the pricing entry for a known Anthropic model, or `None` if
/// unrecognized.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let now = Utc::now();
    match model {
        "claude-haiku-4-5" => Some(ModelPricing {
            input_per_million: 1.00,
            output_per_million: 5.00,
            cached_input_per_million: Some(0.10),
            effective_at: now,
        }),
        "claude-sonnet-4-6" => Some(ModelPricing {
            input_per_million: 3.00,
            output_per_million: 15.00,
            cached_input_per_million: Some(0.30),
            effective_at: now,
        }),
        "claude-opus-4-7" => Some(ModelPricing {
            input_per_million: 5.00,
            output_per_million: 25.00,
            cached_input_per_million: Some(0.50),
            effective_at: now,
        }),
        _ => None,
    }
}

/// Return all supported Anthropic model descriptors.
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
            id: "claude-haiku-4-5".to_string(),
            provider: "anthropic".to_string(),
            capabilities: capabilities.clone(),
            max_input_tokens: 200_000,
            max_output_tokens: 8192,
        },
        ModelInfo {
            id: "claude-sonnet-4-6".to_string(),
            provider: "anthropic".to_string(),
            capabilities: capabilities.clone(),
            max_input_tokens: 200_000,
            max_output_tokens: 8192,
        },
        ModelInfo {
            id: "claude-opus-4-7".to_string(),
            provider: "anthropic".to_string(),
            capabilities,
            max_input_tokens: 200_000,
            max_output_tokens: 8192,
        },
    ]
}
