//! Anthropic provider adapter — the worked reference for non-OpenAI providers.
//!
//! Implements [`tt_shared::Provider`] for Anthropic's `/v1/messages` endpoint.
//! Non-streaming and streaming are both fully supported. Embeddings are not
//! offered by Anthropic and always return [`ProviderError::Unsupported`].
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_anthropic::{AnthropicProvider, ClientConfig};
//!
//! let provider = AnthropicProvider::new(ClientConfig::default());
//! ```

pub mod client;
pub mod errors;
pub mod pricing;
pub mod stream;
pub mod translate;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use tracing::instrument;
use tt_shared::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
};

pub use client::ClientConfig;

/// Default Anthropic API base URL.
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic API version header value.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Stateless Anthropic adapter. Holds an HTTP client and the static pricing
/// table.
///
/// Create once with [`AnthropicProvider::new`] and share across requests.
pub struct AnthropicProvider {
    client: Client,
}

impl AnthropicProvider {
    /// Create a new [`AnthropicProvider`] from the given client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed (very
    /// rare — only happens with invalid TLS configuration).
    pub fn new(cfg: ClientConfig) -> Self {
        let client = client::build_client(&cfg)
            .expect("failed to build reqwest::Client for Anthropic adapter");
        Self { client }
    }

    /// Resolve the base URL from credentials or fall back to the default.
    fn base_url<'a>(&self, ctx: &'a RequestContext) -> &'a str {
        ctx.credentials
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn models(&self) -> Vec<ModelInfo> {
        pricing::all_models()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        pricing::pricing_for(model)
    }

    /// Non-streaming chat completion via `POST /v1/messages`.
    ///
    /// Translates the canonical request to Anthropic's wire format, sends it,
    /// and maps errors to [`ProviderError`].
    #[instrument(skip(self, ctx), fields(provider = "anthropic", model = %req.model))]
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let base_url = self.base_url(ctx);
        let url = format!("{base_url}/v1/messages");

        let body = translate::translate_request(req)?;

        let mut request_builder = self
            .client
            .post(&url)
            .header("x-api-key", ctx.credentials.api_key.expose())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json")
            .json(&body);

        for (name, value) in &ctx.credentials.extra_headers {
            request_builder = request_builder.header(name, value);
        }

        let response = request_builder
            .send()
            .await
            .map_err(errors::map_reqwest_error)?;

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let response_text = response.text().await.map_err(errors::map_reqwest_error)?;

        if status >= 400 {
            return Err(errors::map_response_error(
                status,
                &response_text,
                retry_after.as_deref(),
            ));
        }

        translate::deserialize_response(&response_text)
    }

    /// Streaming chat completion via `POST /v1/messages` with `stream: true`.
    ///
    /// Returns [`ProviderError`] before yielding any chunk if the server
    /// responds with HTTP ≥ 400. Otherwise returns a `BoxStream` that parses
    /// Anthropic typed SSE events and yields [`ChatCompletionChunk`] values.
    #[instrument(skip(self, ctx), fields(provider = "anthropic", model = %req.model))]
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let base_url = self.base_url(ctx).to_string();
        let client = self.client.clone();
        stream::stream_chat_completion(client, &base_url, req, ctx).await
    }

    /// Embeddings are not supported by Anthropic.
    ///
    /// Always returns [`ProviderError::Unsupported`].
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported(
            "Anthropic does not offer an embeddings endpoint".to_string(),
        ))
    }
}
