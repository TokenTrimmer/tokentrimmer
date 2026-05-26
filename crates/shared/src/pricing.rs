//! Pricing tables per model. Values come from provider pricing pages and are
//! refreshed daily. `effective_at` lets us replay historical telemetry against
//! the correct rate. See `docs/02-provider-adapter-guide.md`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    /// USD per 1M input tokens.
    pub input_per_million: f64,
    /// USD per 1M output tokens.
    pub output_per_million: f64,
    /// USD per 1M cached input tokens (Anthropic 10%, OpenAI 10%, Gemini 10%).
    pub cached_input_per_million: Option<f64>,
    /// When this pricing took effect (for historical replay).
    pub effective_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub capabilities: Vec<Capability>,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Text,
    Vision,
    Audio,
    Tools,
    JsonMode,
    Streaming,
    Reasoning,
    PromptCaching,
}
