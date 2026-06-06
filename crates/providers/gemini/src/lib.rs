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
//! - Auth is the `x-goog-api-key` request header (NOT a URL `?key=` query
//!   param — keys in URLs leak via logs/proxies; see review §5.2).
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
    filter_extra_headers, validate_provider_url, ChatCompletionChunk, ChatCompletionRequest,
    ChatCompletionResponse, EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext,
};

pub use client::ClientConfig;

/// Default Gemini API base URL.
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";

/// Stateless Gemini adapter. Holds an HTTP client and the static pricing table.
///
/// Create once with [`GeminiProvider::new`] and share across requests.
pub struct GeminiProvider {
    client: Client,
    /// When `true`, skip SSRF URL validation for private/loopback addresses.
    /// Always `false` in production; set to `true` only in tests that target
    /// a local mock server.
    allow_local: bool,
}

impl GeminiProvider {
    /// Create a new [`GeminiProvider`] from the given client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed (very
    /// rare — only happens with invalid TLS configuration).
    pub fn new(cfg: ClientConfig) -> Self {
        let client =
            client::build_client(&cfg).expect("failed to build reqwest::Client for Gemini adapter");
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
        let client =
            client::build_client(&cfg).expect("failed to build reqwest::Client for Gemini adapter");
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

    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        // Mirror translate.rs: Gemini drops these; response_format is translated.
        let mut out = Vec::new();
        if req.n.is_some() {
            out.push("n".to_string());
        }
        if req.seed.is_some() {
            out.push("seed".to_string());
        }
        if req.presence_penalty.is_some() {
            out.push("presence_penalty".to_string());
        }
        if req.frequency_penalty.is_some() {
            out.push("frequency_penalty".to_string());
        }
        if req.user.is_some() {
            out.push("user".to_string());
        }
        out
    }

    /// Non-streaming chat completion via
    /// `POST /v1beta/models/{model}:generateContent` (key in `x-goog-api-key` header).
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
        // Validate customer-supplied base_url overrides; skip when using the
        // compiled-in default (always safe) or when allow_local is set (tests).
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }

        let api_key = ctx.credentials.api_key.expose().to_string();
        let model = req.model.clone();

        translate::validate_model_id(&model)?;
        let url = format!("{base_url}/v1beta/models/{model}:generateContent");

        let body = translate::translate_request(req)?;

        let mut request_builder = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("x-goog-api-key", &api_key)
            .json(&body);
        // Forward customer-supplied extra headers (denylist-filtered), matching
        // the OpenAI/Anthropic/compat adapters.
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
                &model,
            ));
        }

        translate::deserialize_response(&response_text, &model)
    }

    /// Streaming chat completion via
    /// `POST /v1beta/models/{model}:streamGenerateContent?alt=sse` (key in `x-goog-api-key` header).
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
        let base_url = self.base_url(ctx);
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }

        let base_url = base_url.to_string();
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
