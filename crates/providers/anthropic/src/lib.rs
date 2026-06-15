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
pub mod count_tokens;
pub mod errors;
pub mod messages;
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
    /// When `true`, skip SSRF URL validation for private/loopback addresses.
    /// Always `false` in production; set to `true` only in tests that target
    /// a local mock server.
    allow_local: bool,
    /// In-memory memo cache for [`AnthropicProvider::count_tokens`] keyed on a
    /// content hash of the request body, so repeated counts of the same prompt
    /// cost a single network round-trip. Bounded only by distinct prompts seen
    /// in-process; the provider is constructed once and shared via `Arc`.
    count_tokens_cache: std::sync::Mutex<std::collections::HashMap<u64, u64>>,
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
        Self {
            client,
            allow_local: false,
            count_tokens_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
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
        // servers that the connect-time SSRF guard would block.
        let client = client::build_unguarded_client(&cfg)
            .expect("failed to build reqwest::Client for Anthropic adapter");
        Self {
            client,
            allow_local: true,
            count_tokens_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Resolve the base URL from credentials or fall back to the default.
    fn base_url<'a>(&self, ctx: &'a RequestContext) -> &'a str {
        ctx.credentials
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
    }

    /// Authoritative input-token count via Anthropic `POST /v1/messages/count_tokens`.
    ///
    /// The `cl100k` tiktoken proxy [`tt_tokenize`] uses for Anthropic
    /// systematically UNDERCOUNTS Anthropic input by ~15–20%; this returns
    /// Anthropic's own count instead, for the `$`-bearing sites where the exact
    /// prompt size matters. Results are memoized on a content hash of the
    /// request body, so repeated counts of the same prompt cost one network
    /// call.
    ///
    /// **Never errors:** on ANY failure (unsupported content during translation,
    /// HTTP ≥ 400, network, deserialize) it falls back to the corrected
    /// `tt_tokenize` proxy estimate ([`tt_tokenize::estimate_tokens_for_model`]),
    /// which carries the `+18%` Anthropic correction — so the returned count is
    /// directional whether served by the API or the proxy, and a transient
    /// upstream failure never blocks the caller.
    #[instrument(skip(self, ctx), fields(provider = "anthropic", model = %req.model))]
    pub async fn count_tokens(&self, req: &ChatCompletionRequest, ctx: &RequestContext) -> u64 {
        // Building the body can fail only on untranslatable content (e.g. audio);
        // the fallback estimator ignores those parts, so degrade to it.
        let body = match count_tokens::build_count_tokens_body(req.clone()) {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(error = %e, "count_tokens body build failed — proxy estimate");
                return self.count_tokens_fallback(req);
            }
        };
        let key = count_tokens::content_hash(&body);
        if let Some(&cached) = self
            .count_tokens_cache
            .lock()
            .expect("count_tokens cache poisoned")
            .get(&key)
        {
            return cached;
        }

        match self.count_tokens_remote(&body, ctx).await {
            Ok(count) => {
                self.count_tokens_cache
                    .lock()
                    .expect("count_tokens cache poisoned")
                    .insert(key, count);
                count
            }
            Err(e) => {
                tracing::debug!(error = %e, "count_tokens request failed — proxy estimate");
                self.count_tokens_fallback(req)
            }
        }
    }

    /// Corrected `tt_tokenize` proxy estimate used when `count_tokens` can't
    /// reach the API. Uses the model-aware estimator (which applies the
    /// documented `+18%` Anthropic correction), not a raw `cl100k` count.
    fn count_tokens_fallback(&self, req: &ChatCompletionRequest) -> u64 {
        let text = tt_shared::message_text_for_estimation(req);
        u64::from(tt_tokenize::estimate_tokens_for_model(
            "anthropic",
            &req.model,
            &text,
        ))
    }

    /// Issue the actual `count_tokens` HTTP call (no caching, no fallback).
    /// Mirrors [`chat_completion`](Self::chat_completion)'s URL/SSRF/header
    /// handling.
    async fn count_tokens_remote(
        &self,
        body: &serde_json::Value,
        ctx: &RequestContext,
    ) -> Result<u64, ProviderError> {
        let base_url = self.base_url(ctx);
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }

        let url = format!("{base_url}/v1/messages/count_tokens");
        let mut request_builder = self
            .client
            .post(&url)
            .header("x-api-key", ctx.credentials.api_key.expose())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("Content-Type", "application/json")
            .json(body);
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
            .map(std::string::ToString::to_string);
        let response_text = response.text().await.map_err(errors::map_reqwest_error)?;
        if status >= 400 {
            return Err(errors::map_response_error(
                status,
                &response_text,
                retry_after.as_deref(),
            ));
        }
        let parsed: count_tokens::CountTokensResponse = serde_json::from_str(&response_text)
            .map_err(|e| ProviderError::Deserialize(e.to_string()))?;
        Ok(parsed.input_tokens)
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

    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        // Mirror translate.rs: Anthropic rejects these OpenAI-only fields.
        let mut out = Vec::new();
        if req.n.is_some() {
            out.push("n".to_string());
        }
        if req.seed.is_some() {
            out.push("seed".to_string());
        }
        if req.response_format.is_some() {
            out.push("response_format".to_string());
        }
        if req.presence_penalty.is_some() {
            out.push("presence_penalty".to_string());
        }
        if req.frequency_penalty.is_some() {
            out.push("frequency_penalty".to_string());
        }
        // Newer OpenAI-only fields Anthropic's wire format has no equivalent for.
        // (`max_completion_tokens` IS honored — mapped to Anthropic `max_tokens`
        // in translate.rs — so it is deliberately absent here.)
        if req.parallel_tool_calls.is_some() {
            out.push("parallel_tool_calls".to_string());
        }
        if req.reasoning_effort.is_some() {
            out.push("reasoning_effort".to_string());
        }
        if req.stream_options.is_some() {
            out.push("stream_options".to_string());
        }
        // Extended-thinking passthrough (mirror of `translate::forwardable_thinking`):
        // a `thinking` config translation will NOT forward (malformed shape,
        // sub-1024 budget, or budget >= the resolved max_tokens — Anthropic
        // 400s those) is dropped AND reported; a FORWARDED enabled config
        // makes Anthropic reject temperature/top_p modifications, so those
        // are omitted by translation and reported here instead.
        if req.extra.contains_key("thinking") && translate::forwardable_thinking(req).is_none() {
            out.push("thinking".to_string());
        }
        if translate::forwards_enabled_thinking(req) {
            if req.temperature.is_some() {
                out.push("temperature".to_string());
            }
            if req.top_p.is_some() {
                out.push("top_p".to_string());
            }
        }
        out
    }

    fn supports_response_schema(&self) -> bool {
        // Anthropic has no OpenAI-style `response_format` field at all — it is
        // dropped entirely (see `dropped_params`), so this is operationally moot
        // (the gateway's downgrade guard skips a dropped param), but `false` is
        // the honest answer.
        false
    }

    fn temperature_range(&self) -> (f32, f32) {
        // Anthropic rejects temperature > 1.0.
        (0.0, 1.0)
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
        // Validate customer-supplied base_url overrides; skip when using the
        // compiled-in default (always safe) or when allow_local is set (tests).
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }

        let url = format!("{base_url}/v1/messages");

        let body = translate::translate_request(req)?;

        let mut request_builder = self
            .client
            .post(&url)
            .header("x-api-key", ctx.credentials.api_key.expose())
            .header("anthropic-version", ANTHROPIC_VERSION)
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
        let base_url = self.base_url(ctx);
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }

        let base_url = base_url.to_string();
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
