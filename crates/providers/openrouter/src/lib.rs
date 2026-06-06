//! OpenRouter provider adapter (OpenAI-compatible).
//!
//! Thin wrapper over [`tt_provider_compat::OpenAICompatibleProvider`] with
//! OpenRouter's models, pricing, and default endpoint baked in.
//!
//! OpenRouter is a pass-through gateway aggregating 300+ models. This v1
//! implementation hardcodes a small representative set of common models.
//!
//! # BYOK fee
//!
//! OpenRouter charges a 5% BYOK (Bring Your Own Key) fee on top of the
//! underlying model cost. This is stored in [`CompatConfig::fee_multiplier`]
//! as `1.05`. The fee is **not** applied at request time; the billing layer in
//! `tt-core` applies it when computing the final USD charge for the dashboard.
//!
//! # Dynamic model list
//!
//! TODO(w5-openrouter-models-endpoint): OpenRouter exposes a
//! `GET /models` endpoint that returns the full catalogue dynamically.
//! This v1 implementation uses a static list; a follow-up task will fetch
//! and cache the dynamic list at startup and refresh it periodically.
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_openrouter::OpenRouterProvider;
//! use tt_provider_compat::ClientConfig;
//!
//! let provider = OpenRouterProvider::new(ClientConfig::default());
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

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// 5% BYOK fee applied on top of the underlying model cost.
///
/// Stored in [`CompatConfig::fee_multiplier`]; applied by the billing layer,
/// not at request time. See the module-level documentation for details.
const BYOK_FEE_MULTIPLIER: f64 = 1.05;

/// OpenRouter adapter — delegates to the shared OpenAI-compatible provider base.
///
/// Create once with [`OpenRouterProvider::new`] and share across requests.
pub struct OpenRouterProvider {
    inner: OpenAICompatibleProvider,
}

impl OpenRouterProvider {
    /// Construct a new OpenRouter adapter from the given HTTP client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed.
    pub fn new(client_cfg: ClientConfig) -> Self {
        let cfg = CompatConfig {
            id: "openrouter",
            default_base_url: DEFAULT_BASE_URL.to_string(),
            models: models(),
            pricing_table: pricing_table(),
            fee_multiplier: BYOK_FEE_MULTIPLIER,
            allow_local: false,
        };
        Self {
            inner: OpenAICompatibleProvider::new(client_cfg, cfg),
        }
    }

    /// The BYOK fee multiplier for this adapter (always `1.05`).
    ///
    /// Exposed so the billing layer can retrieve it without accessing private
    /// fields. See [`CompatConfig::fee_multiplier`] for semantics.
    pub fn fee_multiplier(&self) -> f64 {
        self.inner.fee_multiplier()
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
            id: "openrouter",
            default_base_url: DEFAULT_BASE_URL.to_string(),
            models: models(),
            pricing_table: pricing_table(),
            fee_multiplier: BYOK_FEE_MULTIPLIER,
            allow_local: true,
        };
        Self {
            inner: OpenAICompatibleProvider::new(client_cfg, cfg),
        }
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
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

    fn fee_multiplier(&self) -> f64 {
        self.inner.fee_multiplier()
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
            "openrouter embeddings — not in v1 scope".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Model catalogue (May 2026 baselines — static subset)
// ---------------------------------------------------------------------------

fn models() -> Vec<ModelInfo> {
    tt_shared::model_catalog::model_catalog().for_provider("openrouter")
}

/// Build the OpenRouter model→rate map from the shared versioned pricing
/// catalog. These are upstream list prices; OpenRouter's 5% BYOK fee is applied
/// separately via `fee_multiplier`, not baked into the rate. Fed into the
/// OpenAI-compatible inner client at construction.
fn pricing_table() -> HashMap<String, ModelPricing> {
    tt_shared::pricing::catalog()
        .latest_for_provider("openrouter")
        .into_iter()
        .collect()
}
