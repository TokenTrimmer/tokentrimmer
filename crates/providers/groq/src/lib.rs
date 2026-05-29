//! Groq provider adapter (OpenAI-compatible).
//!
//! Thin wrapper over [`tt_provider_compat::OpenAICompatibleProvider`] with
//! Groq's models, pricing, and default endpoint baked in.
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_groq::GroqProvider;
//! use tt_provider_compat::ClientConfig;
//!
//! let provider = GroqProvider::new(ClientConfig::default());
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tt_provider_compat::{ClientConfig, CompatConfig, OpenAICompatibleProvider};
use tt_shared::{
    pricing::{Capability, ModelInfo, ModelPricing},
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, Provider, ProviderError, RequestContext,
};

const DEFAULT_BASE_URL: &str = "https://api.groq.com/openai/v1";

/// Groq adapter — delegates to the shared OpenAI-compatible provider base.
///
/// Create once with [`GroqProvider::new`] and share across requests.
pub struct GroqProvider {
    inner: OpenAICompatibleProvider,
}

impl GroqProvider {
    /// Construct a new Groq adapter from the given HTTP client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed.
    pub fn new(client_cfg: ClientConfig) -> Self {
        let cfg = CompatConfig {
            id: "groq",
            default_base_url: DEFAULT_BASE_URL.to_string(),
            models: models(),
            pricing_table: pricing_table(),
            fee_multiplier: 1.0,
        };
        Self {
            inner: OpenAICompatibleProvider::new(client_cfg, cfg),
        }
    }
}

#[async_trait]
impl Provider for GroqProvider {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.inner.models()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.inner.pricing(model)
    }

    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        self.inner.chat_completion(req, ctx).await
    }

    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        self.inner.chat_completion_stream(req, ctx).await
    }

    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported(
            "groq embeddings — not in v1 scope".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Model catalogue (May 2026 baselines)
// ---------------------------------------------------------------------------

fn models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "llama-3.3-70b-versatile".to_string(),
            provider: "groq".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 8_192,
        },
        ModelInfo {
            id: "llama-3.1-8b-instant".to_string(),
            provider: "groq".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 8_192,
        },
        ModelInfo {
            id: "deepseek-r1-distill-llama-70b".to_string(),
            provider: "groq".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
                Capability::Reasoning,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 8_192,
        },
        ModelInfo {
            id: "mixtral-8x7b-32768".to_string(),
            provider: "groq".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 32_768,
            max_output_tokens: 4_096,
        },
    ]
}

/// Build the Groq model→rate map from the shared versioned pricing catalog.
/// Fed into the OpenAI-compatible inner client at construction.
fn pricing_table() -> HashMap<String, ModelPricing> {
    tt_shared::pricing::catalog()
        .latest_for_provider("groq")
        .into_iter()
        .collect()
}
