//! The `Provider` trait every adapter implements. See
//! `docs/02-provider-adapter-guide.md` for the contract and the worked Anthropic example.

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, ProviderError, RequestContext,
};

/// Adapters are stateless beyond their HTTP client and pricing table.
/// All authentication, telemetry, and routing concerns live in the core layer.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Unique provider ID (e.g. "openai", "anthropic", "gemini").
    fn id(&self) -> &'static str;

    /// All models supported by this adapter, with capabilities.
    fn models(&self) -> Vec<ModelInfo>;

    /// Pricing for a model. Drawn from the manually-curated `data/pricing.toml`
    /// snapshot embedded at build time; rates are updated by hand, not
    /// automatically. Returns `None` only when the model is absent from the
    /// catalog — local providers should return `Some` with zero rates instead.
    fn pricing(&self, model: &str) -> Option<ModelPricing>;

    /// Multiplier applied to computed cost/baseline to account for a provider
    /// surcharge on top of the underlying model cost (e.g. OpenRouter's 5% BYOK
    /// fee). Default `1.0` (no surcharge).
    fn fee_multiplier(&self) -> f64 {
        1.0
    }

    /// Names of request params this adapter **silently drops** for `req`
    /// during translation because the upstream provider rejects them. The
    /// gateway emits each as `X-TokenTrimmer-Warnings: param_dropped:<name>`.
    /// Default: nothing dropped.
    fn dropped_params(&self, _req: &ChatCompletionRequest) -> Vec<String> {
        Vec::new()
    }

    /// Whether this provider honors `response_format: json_schema` (structured
    /// outputs). Default `true`: most adapters forward `response_format`
    /// verbatim, so the gateway must NOT strip a schema it isn't sure is
    /// unsupported (doing so would silently lose structured-output capability).
    /// Override `false` only for a provider known to be `json_object`-only —
    /// the gateway then downgrades to `json_object` with a
    /// `response_format_downgrade` warning.
    fn supports_response_schema(&self) -> bool {
        true
    }

    /// Non-streaming chat completion.
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError>;

    /// Streaming chat completion.
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>;

    /// Embeddings. Returns Unsupported if the provider doesn't offer them.
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported(format!(
            "{} does not support embeddings",
            self.id()
        )))
    }

    /// Liveness check. Should not call the provider's pricey endpoints.
    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }
}
