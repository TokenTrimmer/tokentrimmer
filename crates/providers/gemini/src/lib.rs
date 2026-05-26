//! Google Gemini provider adapter.
//!
//! Implements [`tt_shared::Provider`] for Google Gemini's
//! `generateContent` and `streamGenerateContent` endpoints.
//! Non-streaming and streaming (SSE via `?alt=sse`) are both fully supported.
//! Embeddings use separate Gemini embedding models and are not wired here;
//! they return [`ProviderError::Unsupported`].
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_gemini::{GeminiProvider, ClientConfig};
//!
//! let provider = GeminiProvider::new(ClientConfig::default());
//! ```
//!
//! # API differences from OpenAI
//!
//! - Model is in the URL path, not the request body.
//! - Auth is a query-string `?key=...` parameter, not a `Bearer` header.
//! - System messages map to `systemInstruction`.
//! - Tools use `functionDeclarations` inside a single `tools` object.
//! - Streaming uses SSE format with `?alt=sse`.

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

/// Default Gemini API base URL.
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Stateless Gemini adapter. Holds an HTTP client and the static pricing table.
///
/// Create once with [`GeminiProvider::new`] and share across requests.
pub struct GeminiProvider {
    client: Client,
}

impl GeminiProvider {
    /// Create a new [`GeminiProvider`] from the given client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed (very
    /// rare — only happens with invalid TLS configuration).
    pub fn new(cfg: ClientConfig) -> Self {
        let client = client::build_client(&cfg)
            .expect("failed to build reqwest::Client for Gemini adapter");
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
impl Provider for GeminiProvider {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn models(&self) -> Vec<ModelInfo> {
        pricing::all_models()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        pricing::pricing_for(model)
    }

    /// Non-streaming chat completion via
    /// `POST /v1beta/models/{model}:generateContent?key={api_key}`.
    ///
    /// Translates the canonical request to Gemini's wire format, sends it,
    /// and maps errors to [`ProviderError`].
    #[instrument(skip(self, ctx), fields(provider = "gemini", model = %req.model))]
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let base_url = self.base_url(ctx);
        let api_key = ctx.credentials.api_key.expose().to_string();
        let model = req.model.clone();

        let url = format!(
            "{}/v1beta/models/{}:generateContent?key={}",
            base_url, model, api_key
        );

        let body = translate::translate_request(req)?;

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(errors::map_reqwest_error)?;

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let response_text = response
            .text()
            .await
            .map_err(errors::map_reqwest_error)?;

        if status >= 400 {
            return Err(errors::map_response_error(
                status,
                &response_text,
                retry_after.as_deref(),
                &model,
            ));
        }

        translate::deserialize_response(&response_text, &model)
    }

    /// Streaming chat completion via
    /// `POST /v1beta/models/{model}:streamGenerateContent?key={api_key}&alt=sse`.
    ///
    /// Returns [`ProviderError`] before yielding any chunk if the server
    /// responds with HTTP ≥ 400. Otherwise returns a `BoxStream` that parses
    /// Gemini SSE events and yields [`ChatCompletionChunk`] values.
    #[instrument(skip(self, ctx), fields(provider = "gemini", model = %req.model))]
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let base_url = self.base_url(ctx).to_string();
        let client = self.client.clone();
        stream::stream_chat_completion(client, &base_url, req, ctx).await
    }

    /// Embeddings are not supported by this adapter.
    ///
    /// Gemini uses separate embedding models (e.g. `text-embedding-004`) via a
    /// different endpoint. Those are wired as a separate task.
    ///
    /// Always returns [`ProviderError::Unsupported`].
    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported(
            "Gemini embedding models use a separate endpoint; use a dedicated embedding adapter"
                .to_string(),
        ))
    }
}
