//! Azure OpenAI provider adapter for TokenTrimmer Gateway.
//!
//! Azure OpenAI speaks the OpenAI `chat/completions` + `embeddings` wire format,
//! so this adapter reuses the shared OpenAI-compatible machinery in
//! [`tt_provider_compat`] (request/response translation, the SSE parser, error
//! mapping, and the HTTP client). It differs from the public OpenAI adapter in
//! exactly the two ways Azure requires:
//!
//! 1. **URL** — Azure is *deployment-scoped*. Each call targets
//!    `https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={ver}`
//!    rather than a single `/v1/chat/completions`. The resource comes from
//!    `credentials.base_url`; the deployment is the request `model` id. See
//!    [`config::AzureEndpoint`].
//! 2. **Auth** — Azure authenticates with an `api-key: {key}` header, **not**
//!    `Authorization: Bearer {key}`.
//!
//! The pinned data-plane `api-version` is [`config::AZURE_API_VERSION`].
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_azure::{AzureProvider, ClientConfig};
//!
//! let provider = AzureProvider::new(ClientConfig::default());
//! ```

pub mod config;
pub mod pricing;

pub use config::{AzureEndpoint, AZURE_API_VERSION};
// Re-export the shared client config so dependents construct the adapter with
// one import, mirroring the other OpenAI-compatible adapters.
pub use tt_provider_compat::ClientConfig;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use tracing::instrument;
use tt_provider_compat::{client, errors, stream, translate};
use tt_shared::{
    filter_extra_headers, validate_provider_url, ChatCompletionChunk, ChatCompletionRequest,
    ChatCompletionResponse, EmbeddingsRequest, EmbeddingsResponse, ModelInfo, ModelPricing,
    Provider, ProviderError, RequestContext,
};

/// Azure's authentication header name (case-insensitive on the wire).
const AZURE_AUTH_HEADER: &str = "api-key";

/// Azure OpenAI adapter. Holds an HTTP client; all endpoint coordinates are
/// resolved per-request from the caller's credentials + requested deployment.
///
/// Create once with [`AzureProvider::new`] and share across requests.
pub struct AzureProvider {
    client: Client,
    /// When `true`, skip SSRF URL validation for private/loopback addresses.
    /// Always `false` in production; set to `true` only in tests that target a
    /// local mock server.
    allow_local: bool,
}

impl AzureProvider {
    /// Create a new [`AzureProvider`] from the given client configuration.
    ///
    /// # Panics
    ///
    /// Panics if the underlying [`reqwest::Client`] cannot be constructed (very
    /// rare — only happens with invalid TLS configuration).
    pub fn new(cfg: ClientConfig) -> Self {
        let client =
            client::build_client(&cfg).expect("failed to build reqwest::Client for Azure adapter");
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
    /// Do not use in production code. This bypasses the SSRF guard. The Azure
    /// host check in [`AzureEndpoint::resolve`] would reject a `localhost`
    /// endpoint, so test mock servers must be reached differently — this
    /// constructor exists for symmetry with the other adapters and to skip the
    /// SSRF check once an endpoint is otherwise resolved.
    #[doc(hidden)]
    pub fn new_allow_local(cfg: ClientConfig) -> Self {
        let client =
            client::build_client(&cfg).expect("failed to build reqwest::Client for Azure adapter");
        Self {
            client,
            allow_local: true,
        }
    }

    /// Resolve the Azure endpoint for `model` from the request credentials, then
    /// run the SSRF guard against the resolved resource origin.
    ///
    /// An `azure/<deployment>` routing prefix is stripped to the bare deployment
    /// name the resource expects on the wire; a bare model id (no prefix) is used
    /// as the deployment name directly.
    fn endpoint(&self, model: &str, ctx: &RequestContext) -> Result<AzureEndpoint, ProviderError> {
        let deployment = tt_shared::providers::azure_deployment(model).unwrap_or(model);
        let base = ctx.credentials.base_url.as_deref();
        // In production the Azure host check enforces `*.openai.azure.com`; in
        // local-test mode it is relaxed so a mock server URL resolves, while the
        // SSRF guard below (with allow_local) handles loopback safety.
        let ep = if self.allow_local {
            AzureEndpoint::resolve_allow_any_host(base, deployment)?
        } else {
            AzureEndpoint::resolve(base, deployment)?
        };
        validate_provider_url(ep.origin(), self.allow_local)
            .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        Ok(ep)
    }

    /// `("api-key", <key>)` — Azure's auth header (NOT `Authorization: Bearer`).
    fn auth_header(ctx: &RequestContext) -> (&'static str, String) {
        (
            AZURE_AUTH_HEADER,
            ctx.credentials.api_key.expose().to_string(),
        )
    }
}

#[async_trait]
impl Provider for AzureProvider {
    fn id(&self) -> &'static str {
        "azure"
    }

    fn models(&self) -> Vec<ModelInfo> {
        // Azure deployment names are customer-chosen, so there is no fixed
        // server-side catalogue to advertise. Models resolve dynamically at
        // dispatch via `infer_provider("azure/…")`; pricing is best-effort by
        // deployment name (see `pricing`).
        Vec::new()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        pricing::pricing_for(model)
    }

    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        translate::dropped_params(req)
    }

    /// Non-streaming chat completion via the deployment's `chat/completions`
    /// endpoint, authenticated with `api-key`.
    #[instrument(skip(self, ctx), fields(provider = "azure", model = %req.model))]
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let ep = self.endpoint(&req.model, ctx)?;
        let url = ep.chat_completions_url();
        let body = translate::translate_request(req)?;

        let (auth_name, auth_value) = Self::auth_header(ctx);
        let mut rb = self
            .client
            .post(&url)
            .header(auth_name, auth_value)
            .header("Content-Type", "application/json")
            .json(&body);

        for (name, value) in &filter_extra_headers(&ctx.credentials.extra_headers) {
            rb = rb.header(name, value);
        }

        let response = rb.send().await.map_err(errors::map_reqwest_error)?;

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

    /// Streaming chat completion via the deployment's `chat/completions`
    /// endpoint. Reuses the shared SSE parser through
    /// [`stream::stream_chat_completion_at`], passing the Azure URL + `api-key`.
    #[instrument(skip(self, ctx), fields(provider = "azure", model = %req.model))]
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let ep = self.endpoint(&req.model, ctx)?;
        let url = ep.chat_completions_url();
        let client = self.client.clone();
        let (auth_name, auth_value) = Self::auth_header(ctx);
        stream::stream_chat_completion_at(client, &url, (auth_name, auth_value), req, ctx).await
    }

    /// Embeddings via the deployment's `embeddings` endpoint.
    #[instrument(skip(self, ctx), fields(provider = "azure", model = %req.model))]
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        let ep = self.endpoint(&req.model, ctx)?;
        let url = ep.embeddings_url();
        let body = translate::translate_embeddings_request(req)?;

        let (auth_name, auth_value) = Self::auth_header(ctx);
        let mut rb = self
            .client
            .post(&url)
            .header(auth_name, auth_value)
            .header("Content-Type", "application/json")
            .json(&body);

        for (name, value) in &filter_extra_headers(&ctx.credentials.extra_headers) {
            rb = rb.header(name, value);
        }

        let response = rb.send().await.map_err(errors::map_reqwest_error)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::context::{ProviderCredentials, SecretString};
    use uuid::Uuid;

    fn ctx_with(base_url: Option<&str>, key: &str) -> RequestContext {
        RequestContext {
            trace_id: Uuid::now_v7(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new(key),
                base_url: base_url.map(String::from),
                extra_headers: Vec::new(),
            },
            tag: None,
            deadline: None,
        }
    }

    #[test]
    fn id_is_azure() {
        let p = AzureProvider::new(ClientConfig::default());
        assert_eq!(p.id(), "azure");
    }

    #[test]
    fn endpoint_resolves_and_passes_ssrf_for_valid_azure_host() {
        let p = AzureProvider::new(ClientConfig::default());
        let ctx = ctx_with(Some("https://acme.openai.azure.com"), "secret");
        let ep = p.endpoint("gpt-4o-prod", &ctx).expect("resolves");
        assert_eq!(
            ep.chat_completions_url(),
            format!(
                "https://acme.openai.azure.com/openai/deployments/gpt-4o-prod/chat/completions?api-version={AZURE_API_VERSION}"
            )
        );
    }

    #[test]
    fn endpoint_errors_on_missing_base_url() {
        let p = AzureProvider::new(ClientConfig::default());
        let ctx = ctx_with(None, "secret");
        let err = p.endpoint("gpt-4o", &ctx).unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn auth_header_is_api_key_not_bearer() {
        let ctx = ctx_with(Some("https://acme.openai.azure.com"), "sk-secret");
        let (name, value) = AzureProvider::auth_header(&ctx);
        assert_eq!(name, "api-key");
        assert_eq!(value, "sk-secret");
        assert!(
            !value.starts_with("Bearer"),
            "Azure must not use Bearer auth"
        );
    }
}
