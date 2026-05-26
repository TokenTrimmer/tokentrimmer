//! Together AI provider adapter (OpenAI-compatible).
//!
//! Thin wrapper over [`tt_provider_openai::OpenAICompatibleProvider`] with
//! Together AI's models, pricing, and default endpoint baked in.
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_together::TogetherProvider;
//! use tt_provider_openai::ClientConfig;
//!
//! let provider = TogetherProvider::new(ClientConfig::default());
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::BoxStream;
use tt_provider_openai::{CompatConfig, ClientConfig, OpenAICompatibleProvider};
use tt_shared::{
    pricing::{Capability, ModelInfo, ModelPricing},
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, Provider, ProviderError, RequestContext,
};

const DEFAULT_BASE_URL: &str = "https://api.together.xyz/v1";

/// Together AI adapter — delegates to the shared OpenAI-compatible provider base.
///
/// Create once with [`TogetherProvider::new`] and share across requests.
pub struct TogetherProvider {
    inner: OpenAICompatibleProvider,
}

impl TogetherProvider {
    /// Construct a new Together AI adapter from the given HTTP client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed.
    pub fn new(client_cfg: ClientConfig) -> Self {
        let cfg = CompatConfig {
            id: "together",
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
impl Provider for TogetherProvider {
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
            "together embeddings — not in v1 scope".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Model catalogue (May 2026 baselines)
// ---------------------------------------------------------------------------

fn models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo".to_string(),
            provider: "together".to_string(),
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
            id: "meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo".to_string(),
            provider: "together".to_string(),
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
            id: "Qwen/Qwen2.5-72B-Instruct-Turbo".to_string(),
            provider: "together".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 32_768,
            max_output_tokens: 4_096,
        },
        ModelInfo {
            id: "deepseek-ai/DeepSeek-V3".to_string(),
            provider: "together".to_string(),
            capabilities: vec![
                Capability::Text,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
            ],
            max_input_tokens: 64_000,
            max_output_tokens: 8_192,
        },
    ]
}

fn pricing_table() -> HashMap<String, ModelPricing> {
    let now = Utc::now();
    let mut table = HashMap::new();

    table.insert(
        "meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo".to_string(),
        ModelPricing {
            input_per_million: 0.88,
            output_per_million: 0.88,
            cached_input_per_million: None,
            effective_at: now,
        },
    );
    table.insert(
        "meta-llama/Meta-Llama-3.1-405B-Instruct-Turbo".to_string(),
        ModelPricing {
            input_per_million: 3.50,
            output_per_million: 3.50,
            cached_input_per_million: None,
            effective_at: now,
        },
    );
    table.insert(
        "Qwen/Qwen2.5-72B-Instruct-Turbo".to_string(),
        ModelPricing {
            input_per_million: 1.20,
            output_per_million: 1.20,
            cached_input_per_million: None,
            effective_at: now,
        },
    );
    table.insert(
        "deepseek-ai/DeepSeek-V3".to_string(),
        ModelPricing {
            input_per_million: 1.25,
            output_per_million: 1.25,
            cached_input_per_million: None,
            effective_at: now,
        },
    );

    table
}
