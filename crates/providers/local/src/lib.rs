//! Local-LLM provider adapter — Ollama, vLLM, LM Studio.
//!
//! All three speak the OpenAI Chat Completions wire format on their default
//! ports, so we wrap [`tt_provider_compat::OpenAICompatibleProvider`] with a
//! per-backend `base_url` and a zero-cost pricing fallback.
//!
//! Unlike hosted providers, local backends serve whatever model the user has
//! loaded. We do NOT enumerate a static model catalogue — the registry exposes
//! whichever model id the caller specifies, and pricing always returns `0`.
//!
//! # Usage
//!
//! ```rust,no_run
//! use tt_provider_local::{ClientConfig, LocalBackend, LocalProvider};
//!
//! let ollama = LocalProvider::new(LocalBackend::Ollama, ClientConfig::default());
//! let vllm   = LocalProvider::new(LocalBackend::Vllm,   ClientConfig::default());
//! let lmstudio = LocalProvider::new(LocalBackend::LmStudio, ClientConfig::default());
//! ```

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::stream::BoxStream;
pub use tt_provider_compat::ClientConfig;
use tt_provider_compat::{CompatConfig, OpenAICompatibleProvider};
use tt_shared::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, EmbeddingsRequest,
    EmbeddingsResponse, ModelInfo, ModelPricing, Provider, ProviderError, RequestContext,
};

/// Which local backend a `LocalProvider` targets. Determines the default
/// `base_url`; the caller can still override via `RequestContext.credentials.base_url`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalBackend {
    /// Ollama (default `http://localhost:11434/v1`).
    Ollama,
    /// vLLM OpenAI-compatible server (default `http://localhost:8000/v1`).
    Vllm,
    /// LM Studio (default `http://localhost:1234/v1`).
    LmStudio,
}

impl LocalBackend {
    /// Stable provider id surfaced via `Provider::id()`.
    pub fn id(self) -> &'static str {
        match self {
            LocalBackend::Ollama => "ollama",
            LocalBackend::Vllm => "vllm",
            LocalBackend::LmStudio => "lmstudio",
        }
    }

    /// Default `base_url` for this backend on localhost.
    pub fn default_base_url(self) -> &'static str {
        match self {
            LocalBackend::Ollama => "http://localhost:11434/v1",
            LocalBackend::Vllm => "http://localhost:8000/v1",
            LocalBackend::LmStudio => "http://localhost:1234/v1",
        }
    }
}

/// Provider adapter for self-hosted OpenAI-compatible LLM servers.
///
/// Wraps [`OpenAICompatibleProvider`] with backend-specific defaults and
/// overrides `pricing()` to always return a zero-cost entry.
pub struct LocalProvider {
    backend: LocalBackend,
    inner: OpenAICompatibleProvider,
}

impl LocalProvider {
    /// Construct a `LocalProvider` for the given backend. The HTTP client uses
    /// the supplied [`ClientConfig`] — note that local backends commonly need
    /// higher timeouts than hosted providers (model load, slow GPUs), so the
    /// caller should pass `ClientConfig { request_timeout: Duration::from_secs(300), .. }`
    /// when appropriate. We do NOT silently raise the default here so that
    /// behavior stays predictable across providers.
    pub fn new(backend: LocalBackend, client_cfg: ClientConfig) -> Self {
        Self::with_base_url(backend, backend.default_base_url(), client_cfg)
    }

    /// Like [`LocalProvider::new`] but with an explicit `base_url` (e.g. from
    /// `TT_LOCAL_OLLAMA_URL`). Self-hosted gateways point this at their backend.
    pub fn with_base_url(
        backend: LocalBackend,
        base_url: impl Into<String>,
        client_cfg: ClientConfig,
    ) -> Self {
        let cfg = CompatConfig {
            id: backend.id(),
            default_base_url: base_url.into(),
            // Local backends serve whatever model the user has loaded — no
            // static catalogue; the registry resolves models by name at request
            // time. Pricing table empty (zero-cost fallback). allow_local lets
            // localhost / private IPs through.
            models: Vec::new(),
            pricing_table: HashMap::new(),
            fee_multiplier: 1.0,
            allow_local: true,
        };
        Self {
            backend,
            inner: OpenAICompatibleProvider::new(client_cfg, cfg),
        }
    }

    /// Convenience: which backend this provider targets.
    pub fn backend(&self) -> LocalBackend {
        self.backend
    }

    /// Suggested default `ClientConfig` for local backends — same as
    /// `ClientConfig::default()` but with a longer total timeout to absorb
    /// model-loading latency on cold start.
    pub fn suggested_client_config() -> ClientConfig {
        ClientConfig {
            timeout: Duration::from_secs(300),
            ..ClientConfig::default()
        }
    }
}

/// Remove a leading `"<backend.id()>/"` from `model`; otherwise return it
/// unchanged. Local backends serve bare model names — the gateway routes to
/// `ollama/llama3` but Ollama expects `llama3`.
pub(crate) fn strip_backend_prefix(backend: LocalBackend, model: &str) -> String {
    match model
        .strip_prefix(backend.id())
        .and_then(|r| r.strip_prefix('/'))
    {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => model.to_string(),
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn id(&self) -> &'static str {
        self.inner.id()
    }

    /// Empty — local backends advertise their loaded models out-of-band.
    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    /// Always returns a zero-cost pricing entry. Local inference is free
    /// (modulo hardware), and reporting that as zero is the most honest
    /// number the dashboard can show.
    fn pricing(&self, _model: &str) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_million: 0.0,
            output_per_million: 0.0,
            cached_input_per_million: Some(0.0),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        })
    }

    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        // Strip the backend prefix first so the inner sees the same model id it
        // dispatches (chat_completion strips it too) — otherwise a prefixed
        // reasoning model wouldn't report its dropped params.
        let mut req = req.clone();
        req.model = strip_backend_prefix(self.backend, &req.model);
        self.inner.dropped_params(&req)
    }

    async fn chat_completion(
        &self,
        mut req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        req.model = strip_backend_prefix(self.backend, &req.model);
        self.inner.chat_completion(req, ctx).await
    }

    async fn chat_completion_stream(
        &self,
        mut req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        req.model = strip_backend_prefix(self.backend, &req.model);
        self.inner.chat_completion_stream(req, ctx).await
    }

    async fn embeddings(
        &self,
        _req: EmbeddingsRequest,
        _ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        Err(ProviderError::Unsupported(
            "local embeddings — wire a dedicated embedding model adapter".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_ids_are_stable() {
        assert_eq!(LocalBackend::Ollama.id(), "ollama");
        assert_eq!(LocalBackend::Vllm.id(), "vllm");
        assert_eq!(LocalBackend::LmStudio.id(), "lmstudio");
    }

    #[test]
    fn backend_default_urls() {
        assert_eq!(
            LocalBackend::Ollama.default_base_url(),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            LocalBackend::Vllm.default_base_url(),
            "http://localhost:8000/v1"
        );
        assert_eq!(
            LocalBackend::LmStudio.default_base_url(),
            "http://localhost:1234/v1"
        );
    }

    #[test]
    fn provider_id_propagates_from_backend() {
        let p = LocalProvider::new(LocalBackend::Ollama, ClientConfig::default());
        assert_eq!(p.id(), "ollama");
        assert_eq!(p.backend(), LocalBackend::Ollama);
    }

    #[test]
    fn pricing_is_always_zero() {
        let p = LocalProvider::new(LocalBackend::Vllm, ClientConfig::default());
        let pr = p.pricing("llama-3.3-70b").expect("pricing returns Some");
        assert_eq!(pr.input_per_million, 0.0);
        assert_eq!(pr.output_per_million, 0.0);
        assert_eq!(pr.cached_input_per_million, Some(0.0));
    }

    #[test]
    fn models_is_empty() {
        let p = LocalProvider::new(LocalBackend::LmStudio, ClientConfig::default());
        assert!(p.models().is_empty());
    }

    #[test]
    fn suggested_client_config_has_long_timeout() {
        let cfg = LocalProvider::suggested_client_config();
        assert!(cfg.timeout.as_secs() >= 60);
    }

    #[test]
    fn strips_backend_prefix() {
        assert_eq!(
            strip_backend_prefix(LocalBackend::Ollama, "ollama/llama3.1:8b"),
            "llama3.1:8b"
        );
        // Bare model name (no prefix) is forwarded unchanged.
        assert_eq!(
            strip_backend_prefix(LocalBackend::Ollama, "llama3.1:8b"),
            "llama3.1:8b"
        );
        // A different backend's prefix is NOT stripped by this backend.
        assert_eq!(
            strip_backend_prefix(LocalBackend::Vllm, "ollama/llama3"),
            "ollama/llama3"
        );
    }

    #[test]
    fn with_base_url_overrides_default() {
        let p = LocalProvider::with_base_url(
            LocalBackend::Ollama,
            "http://gpu-box:11434/v1",
            ClientConfig::default(),
        );
        assert_eq!(p.id(), "ollama");
    }
}
