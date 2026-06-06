//! Together AI provider adapter (OpenAI-compatible).
//!
//! Thin wrapper over [`tt_provider_compat::OpenAICompatibleProvider`] with
//! Together AI's models, pricing, and default endpoint baked in.
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_together::TogetherProvider;
//! use tt_provider_compat::ClientConfig;
//!
//! let provider = TogetherProvider::new(ClientConfig::default());
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tt_provider_compat::{ClientConfig, CompatConfig, OpenAICompatibleProvider};
use tt_shared::{
    pricing::{ModelInfo, ModelPricing},
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
            allow_local: false,
        };
        Self {
            inner: OpenAICompatibleProvider::new(client_cfg, cfg),
        }
    }

    /// Create an adapter that skips SSRF URL validation for tests targeting a
    /// local mock server.
    ///
    /// # Warning
    ///
    /// Do not use in production code. This bypasses the SSRF guard.
    #[doc(hidden)]
    pub fn new_allow_local(client_cfg: ClientConfig) -> Self {
        let cfg = CompatConfig {
            id: "together",
            default_base_url: DEFAULT_BASE_URL.to_string(),
            models: models(),
            pricing_table: pricing_table(),
            fee_multiplier: 1.0,
            allow_local: true,
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

    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        self.inner.dropped_params(req)
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
    tt_shared::model_catalog::model_catalog().for_provider("together")
}

/// Build the Together model→rate map from the shared versioned pricing
/// catalog. Fed into the OpenAI-compatible inner client at construction.
fn pricing_table() -> HashMap<String, ModelPricing> {
    tt_shared::pricing::catalog()
        .latest_for_provider("together")
        .into_iter()
        .collect()
}
