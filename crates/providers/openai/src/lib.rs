//! OpenAI provider adapter for TokenTrimmer Gateway.
//!
//! Implements the [`tt_shared::Provider`] trait for OpenAI's chat-completions
//! and embeddings endpoints. Non-streaming and streaming chat completions are
//! fully implemented. Embeddings use `POST /embeddings` with
//! `text-embedding-3-small` (ADR-008) or any model the caller specifies.
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_openai::{OpenAiProvider, ClientConfig};
//!
//! let provider = OpenAiProvider::new(ClientConfig::default());
//! ```

pub mod batch;
pub mod pricing;

pub use batch::{Batch, BatchRequestCounts, BatchStatus, DeletedFile};

// The OpenAI-wire machinery now lives in `tt-provider-compat`. The native
// adapter builds on it; these re-exports preserve the historical
// `tt_provider_openai::{ClientConfig, CompatConfig, OpenAICompatibleProvider}`
// and `tt_provider_openai::{translate, errors, stream, client}` paths so
// dependents need not change import sites.
pub use tt_provider_compat::{
    client, errors, stream, translate, ClientConfig, CompatConfig, OpenAICompatibleProvider,
};

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use tracing::instrument;
use tt_shared::{
    filter_extra_headers, validate_provider_url, ChatCompletionChunk, ChatCompletionRequest,
    ChatCompletionResponse, EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Stateless OpenAI adapter. Holds an HTTP client and the static pricing table.
///
/// Create once with [`OpenAiProvider::new`] and share across requests.
pub struct OpenAiProvider {
    client: Client,
    /// When `true`, skip SSRF URL validation for private/loopback addresses.
    /// Always `false` in production; set to `true` only in tests that target
    /// a local mock server.
    allow_local: bool,
}

impl OpenAiProvider {
    /// Create a new [`OpenAiProvider`] from the given client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed (very
    /// rare — only happens with invalid TLS configuration).
    pub fn new(cfg: ClientConfig) -> Self {
        let client =
            client::build_client(&cfg).expect("failed to build reqwest::Client for OpenAI adapter");
        Self {
            client,
            allow_local: false,
        }
    }

    /// Create an adapter that skips SSRF URL validation for tests targeting a
    /// local mock server.
    ///
    /// # Warning
    ///
    /// Do not use in production code. This bypasses the SSRF guard.
    #[doc(hidden)]
    pub fn new_allow_local(cfg: ClientConfig) -> Self {
        // Unguarded client: `allow_local` targets localhost/private mock
        // servers that the connect-time SSRF guard (installed by the guarded
        // `build_client`) would block.
        let client = client::build_unguarded_client(&cfg)
            .expect("failed to build reqwest::Client for OpenAI adapter");
        Self {
            client,
            allow_local: true,
        }
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
impl Provider for OpenAiProvider {
    fn id(&self) -> &'static str {
        "openai"
    }

    fn models(&self) -> Vec<ModelInfo> {
        pricing::all_models()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        pricing::pricing_for(model)
    }

    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        // Same reasoning-model temperature drop as the compat layer.
        translate::dropped_params(req)
    }

    /// Non-streaming chat completion via `POST /chat/completions`.
    ///
    /// Strips `tt_extras`, applies reasoning-model parameter fixups, sends the
    /// request, and maps any error to [`ProviderError`].
    #[instrument(skip(self, ctx), fields(provider = "openai", model = %req.model))]
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let base_url = self.base_url(ctx);
        // Validate customer-supplied base_url overrides; skip when using the
        // compiled-in default (always safe) or when allow_local is set (tests).
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }

        let url = format!("{base_url}/chat/completions");

        let body = translate::translate_request(req)?;

        let mut request_builder = self
            .client
            .post(&url)
            .header(
                "Authorization",
                format!("Bearer {}", ctx.credentials.api_key.expose()),
            )
            .header("Content-Type", "application/json")
            .json(&body);

        // Apply any provider-specific extra headers from credentials (denylist-filtered).
        for (name, value) in &filter_extra_headers(&ctx.credentials.extra_headers) {
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

    /// Streaming chat completion via `POST /chat/completions` with `stream: true`.
    ///
    /// Returns [`ProviderError::Unsupported`] immediately (no HTTP call) for
    /// reasoning models (`o3`, `o4-mini`), which do not support streaming per
    /// OpenAI's documentation.
    ///
    /// Returns [`ProviderError`] before yielding any chunk if the server
    /// responds with HTTP ≥ 400. Otherwise returns a `BoxStream` that parses
    /// OpenAI SSE events and yields [`ChatCompletionChunk`] values.
    #[instrument(skip(self, ctx), fields(provider = "openai", model = %req.model))]
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let base_url = self.base_url(ctx);
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }

        let base_url = base_url.to_string();
        let client = self.client.clone();
        stream::stream_chat_completion(client, &base_url, req, ctx).await
    }

    /// Embeddings via `POST /embeddings`.
    ///
    /// Sends the canonical [`EmbeddingsRequest`] (which is already OpenAI-shaped)
    /// to the provider and returns the deserialized [`EmbeddingsResponse`].
    ///
    /// Uses `text-embedding-3-small` by default (ADR-008) but respects whatever
    /// model the caller specifies in the request.
    #[instrument(skip(self, ctx), fields(provider = "openai", model = %req.model))]
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        let base_url = self.base_url(ctx);
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }

        let url = format!("{base_url}/embeddings");

        let body = translate::translate_embeddings_request(req)?;

        let mut request_builder = self
            .client
            .post(&url)
            .header(
                "Authorization",
                format!("Bearer {}", ctx.credentials.api_key.expose()),
            )
            .header("Content-Type", "application/json")
            .json(&body);

        for (name, value) in &filter_extra_headers(&ctx.credentials.extra_headers) {
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

        translate::deserialize_embeddings_response(&response_text)
    }
}
