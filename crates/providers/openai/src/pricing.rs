//! OpenAI model catalog. Rates are sourced from the versioned, embedded
//! pricing catalog (`tt_shared::pricing`) rather than hardcoded here — a rate
//! refresh is a `data/pricing.toml` edit, not a code change. Model descriptors
//! (capabilities / token limits) stay typed in Rust below.

use tt_shared::pricing::{catalog, Capability, ModelInfo, ModelPricing};

/// Return the pricing entry for a known OpenAI model, or `None` if unrecognized.
///
/// Delegates to the shared catalog's current rate. The catalog also covers
/// embedding models (`text-embedding-3-small`, `text-embedding-3-large`),
/// which price input tokens only (zero output rate).
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    catalog().latest("openai", model)
}

/// Return all supported OpenAI model descriptors, including embedding models.
pub fn all_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "gpt-5.5".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Vision,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
                Capability::PromptCaching,
            ],
            max_input_tokens: 200_000,
            max_output_tokens: 16_000,
        },
        ModelInfo {
            id: "gpt-5.4".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Vision,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
                Capability::PromptCaching,
            ],
            max_input_tokens: 200_000,
            max_output_tokens: 16_000,
        },
        ModelInfo {
            id: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Vision,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
                Capability::PromptCaching,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 16_000,
        },
        ModelInfo {
            id: "gpt-4o-mini".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Vision,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
                Capability::PromptCaching,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 16_000,
        },
        ModelInfo {
            id: "o3".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Reasoning,
                Capability::Streaming,
            ],
            max_input_tokens: 200_000,
            max_output_tokens: 100_000,
        },
        ModelInfo {
            id: "o4-mini".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Reasoning,
                Capability::Streaming,
            ],
            max_input_tokens: 200_000,
            max_output_tokens: 100_000,
        },
        ModelInfo {
            id: "text-embedding-3-small".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8_191,
            max_output_tokens: 0,
        },
        ModelInfo {
            id: "text-embedding-3-large".to_string(),
            provider: "openai".to_string(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 8_191,
            max_output_tokens: 0,
        },
    ]
}
