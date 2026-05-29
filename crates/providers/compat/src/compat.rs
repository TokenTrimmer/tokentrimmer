//! Shared base for OpenAI-compatible provider adapters.
//!
//! Many inference providers (Mistral, Groq, Together AI, OpenRouter) expose an
//! endpoint that is wire-compatible with OpenAI's `POST /chat/completions` API.
//! Rather than duplicating HTTP plumbing in each adapter crate, this module
//! provides [`OpenAICompatibleProvider`] — a single generic implementation that
//! each adapter instantiates with its own [`CompatConfig`].
//!
//! # Billing note
//!
//! [`CompatConfig::fee_multiplier`] is stored but **not applied at request time**.
//! Token counts and raw per-token costs flow through the response unchanged;
//! the billing layer (in `tt-core`) multiplies by `fee_multiplier` when it
//! computes the final USD charge displayed on the dashboard. This is intentional:
//! the adapter should not alter usage numbers, only report them faithfully.
//! (Tracked as a follow-up in the cost-accounting work item.)
//!
//! # Usage
//!
//! ```rust,no_run
//! use std::collections::HashMap;
//! use tt_provider_compat::{CompatConfig, OpenAICompatibleProvider, ClientConfig};
//! use tt_shared::pricing::{Capability, ModelInfo, ModelPricing};
//! use chrono::Utc;
//!
//! let cfg = CompatConfig {
//!     id: "my-provider",
//!     default_base_url: "https://api.example.com/v1".to_string(),
//!     models: vec![],
//!     pricing_table: HashMap::new(),
//!     fee_multiplier: 1.0,
//! };
//! let provider = OpenAICompatibleProvider::new(ClientConfig::default(), cfg);
//! ```

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use tracing::instrument;
use tt_shared::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
};

use crate::client::{build_client, ClientConfig};
use crate::errors::{map_reqwest_error, map_response_error};
use crate::{stream, translate};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Per-provider configuration for [`OpenAICompatibleProvider`].
///
/// Construct this once at startup (or in a lazy static) and pass it to
/// [`OpenAICompatibleProvider::new`].
pub struct CompatConfig {
    /// Stable, lower-case identifier used by the routing and telemetry layers.
    ///
    /// Examples: `"mistral"`, `"groq"`, `"together"`, `"openrouter"`.
    pub id: &'static str,

    /// Default base URL, used when the caller's [`RequestContext`] does not
    /// supply a `base_url` override in its credentials.
    pub default_base_url: String,

    /// All models exposed by this provider configuration.
    pub models: Vec<ModelInfo>,

    /// Pricing keyed by model ID string, mirroring the per-provider tables in
    /// the OpenAI adapter's `pricing.rs`.
    pub pricing_table: HashMap<String, ModelPricing>,

    /// Optional fee multiplier stored for the billing layer (e.g. `1.05` for a
    /// 5% BYOK fee on OpenRouter).
    ///
    /// **This value is NOT applied to usage at request time.** The adapter
    /// faithfully reports raw token counts; the dashboard billing pass applies
    /// the multiplier when computing the final USD charge. Default: `1.0`.
    pub fee_multiplier: f64,
}

// ---------------------------------------------------------------------------
// Provider struct
// ---------------------------------------------------------------------------

/// Generic OpenAI-compatible chat-completion adapter.
///
/// Holds an HTTP client and a [`CompatConfig`] that varies per provider.
/// All four thin adapter crates (Mistral, Groq, Together, OpenRouter) wrap
/// this struct and forward every [`Provider`] method to it.
pub struct OpenAICompatibleProvider {
    client: Client,
    cfg: CompatConfig,
}

impl OpenAICompatibleProvider {
    /// Construct a new adapter from the given HTTP client configuration and
    /// provider-specific configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed (very
    /// rare — only happens with invalid TLS configuration).
    pub fn new(client_cfg: ClientConfig, cfg: CompatConfig) -> Self {
        let client = build_client(&client_cfg)
            .unwrap_or_else(|e| panic!("failed to build HTTP client for {}: {e}", cfg.id));
        Self { client, cfg }
    }

    /// The fee multiplier stored in this provider's config.
    ///
    /// Exposed so that the billing layer can retrieve it without accessing
    /// private fields. See [`CompatConfig::fee_multiplier`] for semantics.
    pub fn fee_multiplier(&self) -> f64 {
        self.cfg.fee_multiplier
    }

    /// Resolve the base URL: prefer the credential override, fall back to the
    /// compiled-in default.
    fn base_url<'a>(&'a self, ctx: &'a RequestContext) -> &'a str {
        ctx.credentials
            .base_url
            .as_deref()
            .unwrap_or(self.cfg.default_base_url.as_str())
    }
}

// ---------------------------------------------------------------------------
// Provider trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Provider for OpenAICompatibleProvider {
    fn id(&self) -> &'static str {
        self.cfg.id
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.cfg.models.clone()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        self.cfg.pricing_table.get(model).cloned()
    }

    /// Non-streaming chat completion via `POST /chat/completions`.
    ///
    /// Translates the canonical request, sends it to the provider's endpoint
    /// (resolved from credentials or the default base URL), and maps any HTTP
    /// error to the appropriate [`ProviderError`] variant.
    #[instrument(skip(self, ctx), fields(provider = %self.cfg.id, model = %req.model))]
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url(ctx));
        let body = translate::translate_request(req)?;

        let mut rb = self
            .client
            .post(&url)
            .header(
                "Authorization",
                format!("Bearer {}", ctx.credentials.api_key.expose()),
            )
            .header("Content-Type", "application/json")
            .json(&body);

        for (name, value) in &ctx.credentials.extra_headers {
            rb = rb.header(name, value);
        }

        let response = rb.send().await.map_err(map_reqwest_error)?;

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let response_text = response.text().await.map_err(map_reqwest_error)?;

        if status >= 400 {
            return Err(map_response_error(
                status,
                &response_text,
                retry_after.as_deref(),
            ));
        }

        translate::deserialize_response(&response_text)
    }

    /// Streaming chat completion via `POST /chat/completions` with `stream: true`.
    ///
    /// Returns a [`BoxStream`] that yields [`ChatCompletionChunk`] values parsed
    /// from OpenAI-compatible SSE events. HTTP errors before the first byte are
    /// surfaced as `Err` before any chunk is produced.
    #[instrument(skip(self, ctx), fields(provider = %self.cfg.id, model = %req.model))]
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let base_url = self.base_url(ctx).to_string();
        let client = self.client.clone();
        stream::stream_chat_completion(client, &base_url, req, ctx).await
    }

    /// Embeddings via `POST /embeddings`.
    ///
    /// Sends the canonical [`EmbeddingsRequest`] to the provider's `/embeddings`
    /// endpoint (resolved from credentials or the default base URL). All
    /// OpenAI-compatible providers (Mistral, Together, etc.) expose the same
    /// `/embeddings` path and wire format, so no translation is needed beyond
    /// what [`translate::translate_embeddings_request`] provides.
    #[instrument(skip(self, ctx), fields(provider = %self.cfg.id, model = %req.model))]
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        let url = format!("{}/embeddings", self.base_url(ctx));
        let body = translate::translate_embeddings_request(req)?;

        let mut rb = self
            .client
            .post(&url)
            .header(
                "Authorization",
                format!("Bearer {}", ctx.credentials.api_key.expose()),
            )
            .header("Content-Type", "application/json")
            .json(&body);

        for (name, value) in &ctx.credentials.extra_headers {
            rb = rb.header(name, value);
        }

        let response = rb.send().await.map_err(map_reqwest_error)?;

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let response_text = response.text().await.map_err(map_reqwest_error)?;

        if status >= 400 {
            return Err(map_response_error(
                status,
                &response_text,
                retry_after.as_deref(),
            ));
        }

        translate::deserialize_embeddings_response(&response_text)
    }
}
