//! Mistral provider adapter (OpenAI-compatible).
//!
//! Thin wrapper over [`tt_provider_compat::OpenAICompatibleProvider`] with
//! Mistral's models, pricing, and default endpoint baked in.
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_mistral::MistralProvider;
//! use tt_provider_compat::ClientConfig;
//!
//! let provider = MistralProvider::new(ClientConfig::default());
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

const DEFAULT_BASE_URL: &str = "https://api.mistral.ai/v1";

/// Mistral adapter — delegates to the shared OpenAI-compatible provider base.
///
/// Create once with [`MistralProvider::new`] and share across requests.
pub struct MistralProvider {
    inner: OpenAICompatibleProvider,
}

impl MistralProvider {
    /// Construct a new Mistral adapter from the given HTTP client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed.
    pub fn new(client_cfg: ClientConfig) -> Self {
        let cfg = CompatConfig {
            id: "mistral",
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
impl Provider for MistralProvider {
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
            "mistral embeddings — not in v1 scope".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Model catalogue (May 2026 baselines)
// ---------------------------------------------------------------------------

fn models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "mistral-large-latest".to_string(),
            provider: "mistral".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 4_096,
        },
        ModelInfo {
            id: "mistral-medium-latest".to_string(),
            provider: "mistral".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 4_096,
        },
        ModelInfo {
            id: "mistral-small-latest".to_string(),
            provider: "mistral".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 4_096,
        },
        ModelInfo {
            id: "codestral-latest".to_string(),
            provider: "mistral".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 256_000,
            max_output_tokens: 8_192,
        },
        ModelInfo {
            id: "pixtral-large-latest".to_string(),
            provider: "mistral".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Vision,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 128_000,
            max_output_tokens: 4_096,
        },
    ]
}

/// Build the Mistral model→rate map from the shared versioned pricing catalog.
/// Fed into the OpenAI-compatible inner client at construction.
fn pricing_table() -> HashMap<String, ModelPricing> {
    tt_shared::pricing::catalog()
        .latest_for_provider("mistral")
        .into_iter()
        .collect()
}
