//! Pricing table for OpenAI models. Values sourced from openai.com/api/pricing (May 2026).
//! `effective_at` is a placeholder; the pricing-fetcher service will backfill this field daily.

use chrono::Utc;
use tt_shared::pricing::{Capability, ModelInfo, ModelPricing};

/// Return the pricing entry for a known OpenAI model, or `None` if unrecognized.
///
/// Includes both chat-completion models and embedding models
/// (`text-embedding-3-small` at $0.02/1M tokens,
/// `text-embedding-3-large` at $0.13/1M tokens).
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let now = Utc::now();
    match model {
        "gpt-5.5" => Some(ModelPricing {
            input_per_million: 5.00,
            output_per_million: 30.00,
            cached_input_per_million: Some(0.50),
            effective_at: now,
        }),
        "gpt-5.4" => Some(ModelPricing {
            input_per_million: 2.50,
            output_per_million: 15.00,
            cached_input_per_million: Some(0.25),
            effective_at: now,
        }),
        "gpt-4o" => Some(ModelPricing {
            input_per_million: 2.50,
            output_per_million: 10.00,
            cached_input_per_million: Some(1.25),
            effective_at: now,
        }),
        "gpt-4o-mini" => Some(ModelPricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
            cached_input_per_million: Some(0.075),
            effective_at: now,
        }),
        "o3" => Some(ModelPricing {
            input_per_million: 60.00,
            output_per_million: 240.00,
            cached_input_per_million: Some(15.00),
            effective_at: now,
        }),
        "o4-mini" => Some(ModelPricing {
            input_per_million: 1.10,
            output_per_million: 4.40,
            cached_input_per_million: Some(0.275),
            effective_at: now,
        }),
        // Embedding models — output_per_million is 0 (no completion tokens).
        "text-embedding-3-small" => Some(ModelPricing {
            input_per_million: 0.02,
            output_per_million: 0.00,
            cached_input_per_million: None,
            effective_at: now,
        }),
        "text-embedding-3-large" => Some(ModelPricing {
            input_per_million: 0.13,
            output_per_million: 0.00,
            cached_input_per_million: None,
            effective_at: now,
        }),
        _ => None,
    }
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

/// True if `model` is an OpenAI reasoning model (o3, o4-mini, etc.) with
/// different parameter constraints (no temperature, `max_completion_tokens`).
pub fn is_reasoning_model(model: &str) -> bool {
    matches!(model, "o3" | "o4-mini")
}
