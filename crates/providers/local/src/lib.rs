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

mod runtime;

pub use runtime::{
    AdmissionController, AdmissionError, AdmissionPermit, CapabilityOverrides, CapabilitySource,
    CapabilityValue, CapacityConfig, CapacitySnapshot, EndpointCapabilities,
    EndpointCapabilitySnapshot, EndpointDiscoveryConfig, EndpointHealth, EndpointNetworkScope,
    EndpointPrivacyConfig, EndpointStateStore, HardwareProvenance, LocalModelProvenance,
    LocalTcoEvidence, LocalTcoProfile, RuntimeError, ENDPOINT_STATE_SCHEMA_VERSION,
};

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
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
    /// llama.cpp OpenAI-compatible server.
    LlamaCpp,
    /// MLX-LM OpenAI-compatible server.
    Mlx,
    /// Hugging Face Text Generation Inference.
    Tgi,
    /// SGLang OpenAI-compatible server.
    Sglang,
    /// Generic OpenAI-compatible endpoint.
    Generic,
}

impl LocalBackend {
    /// Stable provider id surfaced via `Provider::id()`.
    pub fn id(self) -> &'static str {
        match self {
            LocalBackend::Ollama => "ollama",
            LocalBackend::Vllm => "vllm",
            LocalBackend::LmStudio => "lmstudio",
            LocalBackend::LlamaCpp => "llamacpp",
            LocalBackend::Mlx => "mlx",
            LocalBackend::Tgi => "tgi",
            LocalBackend::Sglang => "sglang",
            LocalBackend::Generic => "local",
        }
    }

    /// Default `base_url` for this backend on localhost.
    pub fn default_base_url(self) -> &'static str {
        match self {
            LocalBackend::Ollama => "http://localhost:11434/v1",
            LocalBackend::Vllm => "http://localhost:8000/v1",
            LocalBackend::LmStudio => "http://localhost:1234/v1",
            LocalBackend::LlamaCpp => "http://localhost:8080/v1",
            LocalBackend::Mlx => "http://localhost:8080/v1",
            LocalBackend::Tgi => "http://localhost:8080/v1",
            LocalBackend::Sglang => "http://localhost:30000/v1",
            LocalBackend::Generic => "http://localhost:8080/v1",
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

/// Known OpenAI-compatible serving-engine shape for one endpoint profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointPreset {
    OpenAiCompatible,
    Ollama,
    Vllm,
    LmStudio,
    LlamaCpp,
    Mlx,
    Tgi,
    Sglang,
}

impl EndpointPreset {
    fn backend(self) -> LocalBackend {
        match self {
            Self::OpenAiCompatible => LocalBackend::Generic,
            Self::Ollama => LocalBackend::Ollama,
            Self::Vllm => LocalBackend::Vllm,
            Self::LmStudio => LocalBackend::LmStudio,
            Self::LlamaCpp => LocalBackend::LlamaCpp,
            Self::Mlx => LocalBackend::Mlx,
            Self::Tgi => LocalBackend::Tgi,
            Self::Sglang => LocalBackend::Sglang,
        }
    }

    fn default_base_url(self) -> Option<&'static str> {
        match self {
            Self::OpenAiCompatible => None,
            other => Some(other.backend().default_base_url()),
        }
    }
}

/// Authentication source for an endpoint profile. The config names an
/// environment variable but never contains its secret value.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EndpointAuth {
    #[default]
    None,
    BearerEnv {
        env: String,
    },
}

/// Source-controlled, non-secret configuration for one local endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointProfileConfig {
    pub name: String,
    pub preset: EndpointPreset,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth: EndpointAuth,
    #[serde(default)]
    pub discovery: EndpointDiscoveryConfig,
    #[serde(default)]
    pub capacity: CapacityConfig,
    #[serde(default)]
    pub privacy: EndpointPrivacyConfig,
    #[serde(default)]
    pub tco: Option<LocalTcoProfile>,
    #[serde(default)]
    pub require_provenance: bool,
    #[serde(default)]
    pub models: BTreeMap<String, LocalModelProvenance>,
}

#[derive(Debug, thiserror::Error)]
pub enum EndpointProfileError {
    #[error("endpoint profile JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("at least one endpoint profile is required")]
    Empty,
    #[error("invalid endpoint profile name {0:?}")]
    InvalidName(String),
    #[error("duplicate endpoint profile name {0:?}")]
    DuplicateName(String),
    #[error("generic endpoint profile {0:?} requires base_url")]
    MissingBaseUrl(String),
    #[error("endpoint profile {profile:?} has invalid base_url: {detail}")]
    InvalidBaseUrl { profile: String, detail: String },
    #[error("endpoint profile {profile:?} has invalid credential environment name {env:?}")]
    InvalidCredentialEnv { profile: String, env: String },
    #[error("endpoint profile {profile:?} credential environment {env:?} is unset or empty")]
    MissingCredential { profile: String, env: String },
    #[error("endpoint profile {profile:?} runtime policy is invalid: {source}")]
    Runtime {
        profile: String,
        #[source]
        source: RuntimeError,
    },
    #[error(
        "endpoint profile {profile:?} base_url does not match declared {scope:?} network scope"
    )]
    NetworkScope {
        profile: String,
        scope: EndpointNetworkScope,
    },
}

/// Parse and validate a strict JSON array of endpoint profiles.
pub fn parse_endpoint_profiles_json(
    raw: &str,
) -> Result<Vec<EndpointProfileConfig>, EndpointProfileError> {
    let profiles: Vec<EndpointProfileConfig> = serde_json::from_str(raw)?;
    if profiles.is_empty() {
        return Err(EndpointProfileError::Empty);
    }
    let mut names = std::collections::BTreeSet::new();
    for profile in &profiles {
        validate_profile_name(&profile.name)?;
        if !names.insert(profile.name.clone()) {
            return Err(EndpointProfileError::DuplicateName(profile.name.clone()));
        }
        validate_profile_config(profile)?;
        if let EndpointAuth::BearerEnv { env } = &profile.auth {
            if !valid_environment_name(env) {
                return Err(EndpointProfileError::InvalidCredentialEnv {
                    profile: profile.name.clone(),
                    env: env.clone(),
                });
            }
        }
    }
    Ok(profiles)
}

fn validate_profile_name(name: &str) -> Result<(), EndpointProfileError> {
    if name.is_empty()
        || name.len() > 48
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
    {
        return Err(EndpointProfileError::InvalidName(name.to_owned()));
    }
    Ok(())
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn profile_base_url(profile: &EndpointProfileConfig) -> Result<String, EndpointProfileError> {
    let raw = profile
        .base_url
        .as_deref()
        .or_else(|| profile.preset.default_base_url())
        .ok_or_else(|| EndpointProfileError::MissingBaseUrl(profile.name.clone()))?;
    let url = reqwest::Url::parse(raw).map_err(|error| EndpointProfileError::InvalidBaseUrl {
        profile: profile.name.clone(),
        detail: error.to_string(),
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(EndpointProfileError::InvalidBaseUrl {
            profile: profile.name.clone(),
            detail: "must be an uncredentialed HTTP(S) origin/path without query or fragment"
                .into(),
        });
    }
    Ok(raw.trim_end_matches('/').to_owned())
}

fn validate_profile_config(profile: &EndpointProfileConfig) -> Result<(), EndpointProfileError> {
    let base_url = profile_base_url(profile)?;
    profile
        .discovery
        .validate()
        .and_then(|()| profile.capacity.validate())
        .and_then(|()| profile.privacy.validate())
        .and_then(|()| {
            profile
                .tco
                .as_ref()
                .map_or(Ok(()), LocalTcoProfile::validate)
        })
        .map_err(|source| EndpointProfileError::Runtime {
            profile: profile.name.clone(),
            source,
        })?;
    if profile.require_provenance && profile.models.is_empty() {
        return Err(EndpointProfileError::Runtime {
            profile: profile.name.clone(),
            source: RuntimeError::InvalidProvenance,
        });
    }
    for (model, provenance) in &profile.models {
        if model.trim().is_empty() || provenance.validate().is_err() {
            return Err(EndpointProfileError::Runtime {
                profile: profile.name.clone(),
                source: RuntimeError::InvalidProvenance,
            });
        }
    }
    let url = reqwest::Url::parse(&base_url).expect("base URL was validated");
    let host = url.host_str().expect("validated URL has a host");
    let ip = host.parse::<std::net::IpAddr>().ok();
    let matches_scope = match profile.privacy.network_scope {
        EndpointNetworkScope::Loopback => {
            host.eq_ignore_ascii_case("localhost")
                || host.ends_with(".localhost")
                || ip.is_some_and(|address| address.is_loopback())
        }
        EndpointNetworkScope::Private => ip.is_some_and(is_private_address),
        EndpointNetworkScope::External => true,
    };
    if !matches_scope {
        return Err(EndpointProfileError::NetworkScope {
            profile: profile.name.clone(),
            scope: profile.privacy.network_scope,
        });
    }
    Ok(())
}

fn is_private_address(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => {
            address.is_private() || address.is_link_local() || address.is_loopback()
        }
        std::net::IpAddr::V6(address) => {
            address.is_loopback()
                || (address.segments()[0] & 0xfe00) == 0xfc00
                || (address.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

#[derive(Clone)]
struct ResolvedEndpointProfile {
    preset: EndpointPreset,
    base_url: String,
    provider: Arc<LocalProvider>,
    credentials: tt_shared::context::ProviderCredentials,
    authentication_configured: bool,
}

/// Non-secret profile metadata suitable for health/provenance evidence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EndpointProfileMetadata {
    pub name: String,
    pub preset: EndpointPreset,
    pub base_url: String,
    pub authentication_configured: bool,
}

/// One generic provider that multiplexes `local/<profile>/<model>` requests
/// across strict, explicitly configured OpenAI-compatible endpoint profiles.
pub struct ProfiledLocalProvider {
    profiles: BTreeMap<String, ResolvedEndpointProfile>,
}

impl ProfiledLocalProvider {
    pub fn from_configs<F>(
        configs: Vec<EndpointProfileConfig>,
        client_cfg: ClientConfig,
        mut credential_resolver: F,
    ) -> Result<Self, EndpointProfileError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        if configs.is_empty() {
            return Err(EndpointProfileError::Empty);
        }
        let mut profiles = BTreeMap::new();
        for config in configs {
            validate_profile_name(&config.name)?;
            if profiles.contains_key(&config.name) {
                return Err(EndpointProfileError::DuplicateName(config.name));
            }
            let base_url = profile_base_url(&config)?;
            let (api_key, authentication_configured) = match &config.auth {
                EndpointAuth::None => ("local-no-auth".to_owned(), false),
                EndpointAuth::BearerEnv { env } => {
                    if !valid_environment_name(env) {
                        return Err(EndpointProfileError::InvalidCredentialEnv {
                            profile: config.name,
                            env: env.clone(),
                        });
                    }
                    let value = credential_resolver(env)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| EndpointProfileError::MissingCredential {
                            profile: config.name.clone(),
                            env: env.clone(),
                        })?;
                    (value, true)
                }
            };
            let provider = Arc::new(LocalProvider::with_base_url(
                config.preset.backend(),
                base_url.clone(),
                client_cfg.clone(),
            ));
            let credentials = tt_shared::context::ProviderCredentials {
                api_key: tt_shared::context::SecretString::new(api_key),
                base_url: Some(base_url.clone()),
                extra_headers: Vec::new(),
            };
            profiles.insert(
                config.name,
                ResolvedEndpointProfile {
                    preset: config.preset,
                    base_url,
                    provider,
                    credentials,
                    authentication_configured,
                },
            );
        }
        Ok(Self { profiles })
    }

    pub fn profile_metadata(&self) -> Vec<EndpointProfileMetadata> {
        self.profiles
            .iter()
            .map(|(name, profile)| EndpointProfileMetadata {
                name: name.clone(),
                preset: profile.preset,
                base_url: profile.base_url.clone(),
                authentication_configured: profile.authentication_configured,
            })
            .collect()
    }

    fn select<'profile, 'model>(
        &'profile self,
        model: &'model str,
    ) -> Result<(&'profile ResolvedEndpointProfile, &'model str), ProviderError> {
        let rest = model
            .strip_prefix("local/")
            .ok_or_else(|| ProviderError::ModelNotFound {
                model: model.to_owned(),
            })?;
        let (profile, upstream_model) =
            rest.split_once('/')
                .ok_or_else(|| ProviderError::ModelNotFound {
                    model: model.to_owned(),
                })?;
        if profile.is_empty() || upstream_model.is_empty() {
            return Err(ProviderError::ModelNotFound {
                model: model.to_owned(),
            });
        }
        self.profiles
            .get(profile)
            .map(|resolved| (resolved, upstream_model))
            .ok_or_else(|| ProviderError::ModelNotFound {
                model: model.to_owned(),
            })
    }

    fn routed_context(profile: &ResolvedEndpointProfile, ctx: &RequestContext) -> RequestContext {
        let mut routed = ctx.clone();
        routed.credentials = profile.credentials.clone();
        routed
    }
}

#[async_trait]
impl Provider for ProfiledLocalProvider {
    fn id(&self) -> &'static str {
        "local"
    }

    fn models(&self) -> Vec<ModelInfo> {
        Vec::new()
    }

    fn pricing(&self, model: &str) -> Option<ModelPricing> {
        let (profile, upstream_model) = self.select(model).ok()?;
        profile.provider.pricing(upstream_model)
    }

    fn dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String> {
        let Ok((profile, upstream_model)) = self.select(&req.model) else {
            return Vec::new();
        };
        let mut routed = req.clone();
        routed.model = upstream_model.to_owned();
        profile.provider.dropped_params(&routed)
    }

    async fn chat_completion(
        &self,
        mut req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError> {
        let (profile, upstream_model) = self.select(&req.model)?;
        let upstream_model = upstream_model.to_owned();
        req.model = upstream_model;
        let routed = Self::routed_context(profile, ctx);
        profile.provider.chat_completion(req, &routed).await
    }

    async fn chat_completion_stream(
        &self,
        mut req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError> {
        let (profile, upstream_model) = self.select(&req.model)?;
        let upstream_model = upstream_model.to_owned();
        req.model = upstream_model;
        let routed = Self::routed_context(profile, ctx);
        profile.provider.chat_completion_stream(req, &routed).await
    }

    async fn embeddings(
        &self,
        mut req: EmbeddingsRequest,
        ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        let (profile, upstream_model) = self.select(&req.model)?;
        let upstream_model = upstream_model.to_owned();
        req.model = upstream_model;
        let routed = Self::routed_context(profile, ctx);
        profile.provider.embeddings(req, &routed).await
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
        mut req: EmbeddingsRequest,
        ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError> {
        req.model = strip_backend_prefix(self.backend, &req.model);
        self.inner.embeddings(req, ctx).await
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

    #[test]
    fn profiled_local_provider_selects_and_routes_profiles() {
        let configs = vec![
            EndpointProfileConfig {
                name: "gpu-box".into(),
                preset: EndpointPreset::Ollama,
                base_url: Some("http://gpu-box:11434/v1".into()),
                auth: EndpointAuth::None,
                discovery: EndpointDiscoveryConfig::default(),
                capacity: CapacityConfig::default(),
                privacy: EndpointPrivacyConfig::default(),
                tco: None,
                require_provenance: false,
                models: BTreeMap::new(),
            },
            EndpointProfileConfig {
                name: "vllm-cluster".into(),
                preset: EndpointPreset::Vllm,
                base_url: Some("http://vllm-box:8000/v1".into()),
                auth: EndpointAuth::None,
                discovery: EndpointDiscoveryConfig::default(),
                capacity: CapacityConfig::default(),
                privacy: EndpointPrivacyConfig::default(),
                tco: None,
                require_provenance: false,
                models: BTreeMap::new(),
            },
        ];

        let provider =
            ProfiledLocalProvider::from_configs(configs, ClientConfig::default(), |_| None)
                .expect("profile provider should build");

        assert_eq!(provider.id(), "local");
        let metadata = provider.profile_metadata();
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata[0].name, "gpu-box");
        assert_eq!(metadata[0].preset, EndpointPreset::Ollama);
        assert_eq!(metadata[1].name, "vllm-cluster");
        assert_eq!(metadata[1].preset, EndpointPreset::Vllm);

        let pricing = provider.pricing("local/gpu-box/llama3.1:8b");
        assert!(pricing.is_some());
        assert_eq!(pricing.unwrap().input_per_million, 0.0);

        let invalid_prefix = provider.pricing("ollama/llama3.1:8b");
        assert!(invalid_prefix.is_none());

        let missing_profile = provider.pricing("local/nonexistent/llama3.1:8b");
        assert!(missing_profile.is_none());
    }
}
