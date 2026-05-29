//! Anthropic model catalog. Rates come from the versioned, embedded pricing
//! catalog (`tt_shared::pricing`); `cached_input_per_million` there is the
//! cache **read** rate (~10% of standard input). Cache-write surcharges are
//! not modeled (recovered on second use). Model descriptors stay typed below.

use tt_shared::pricing::{catalog, Capability, ModelInfo, ModelPricing};

/// Return the pricing entry for a known Anthropic model, or `None` if
/// unrecognized. Delegates to the shared catalog's current rate.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    catalog().latest("anthropic", model)
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
