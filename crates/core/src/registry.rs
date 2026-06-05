//! Provider registry — central lookup from model ID to Provider impl.
//! Adapters register themselves at server startup.

use std::collections::HashMap;
use std::sync::Arc;

use tt_provider_anthropic::{AnthropicProvider, ClientConfig as AnthropicClientConfig};
use tt_provider_gemini::{ClientConfig as GeminiClientConfig, GeminiProvider};
use tt_provider_groq::GroqProvider;
use tt_provider_local::{LocalBackend, LocalProvider};
use tt_provider_mistral::MistralProvider;
use tt_provider_openai::{ClientConfig as OpenAiClientConfig, OpenAiProvider};
use tt_provider_openrouter::OpenRouterProvider;
use tt_provider_together::TogetherProvider;
use tt_shared::{ModelInfo, Provider};

#[derive(Default)]
pub struct ProviderRegistry {
    by_id: HashMap<&'static str, Arc<dyn Provider>>,
    by_model: HashMap<String, Arc<dyn Provider>>,
    model_info: HashMap<String, ModelInfo>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        let id = provider.id();
        for model in provider.models() {
            self.model_info.insert(model.id.clone(), model.clone());
            self.by_model
                .insert(model.id.clone(), Arc::clone(&provider));
        }
        self.by_id.insert(id, provider);
    }

    /// Look up the static [`ModelInfo`] for `model_id`.
    ///
    /// Returns `None` when the model is unknown to the catalog (dispatch may
    /// still succeed via [`Self::resolve`]'s fallback path, but capability
    /// checking treats unknown models as *permissive* — not blocked).
    pub fn model_info(&self, model_id: &str) -> Option<&ModelInfo> {
        self.model_info.get(model_id)
    }

    pub fn by_id(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.by_id.get(id).cloned()
    }

    pub fn by_model(&self, model: &str) -> Option<Arc<dyn Provider>> {
        self.by_model.get(model).cloned()
    }

    /// Resolve a model to a provider for DISPATCH. Tries the exact static
    /// `by_model` table first; on miss, falls back to
    /// [`tt_shared::providers::infer_provider`] + [`Self::by_id`] so a
    /// valid-but-unlisted model (a newly-released id, or an aggregator
    /// passthrough) still dispatches instead of 404ing. Pricing then falls
    /// back to `None`, which the cost path already tolerates. The static table
    /// stays the source of truth for pricing/capabilities — this only widens
    /// dispatch.
    pub fn resolve(&self, model: &str) -> Option<Arc<dyn Provider>> {
        self.by_model(model)
            .or_else(|| tt_shared::providers::local_backend(model).and_then(|id| self.by_id(id)))
            .or_else(|| tt_shared::providers::infer_provider(model).and_then(|id| self.by_id(id)))
    }

    /// Iterate all registered providers as `(id, provider)` pairs.
    /// Used by `/v1/models` to enumerate the model catalog.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &Arc<dyn Provider>)> {
        self.by_id.iter().map(|(id, p)| (*id, p))
    }
}

/// Which built-in providers to register. Defaults to all-on, preserving the
/// historical "register everything" behavior. A deployment that only brokers a
/// subset of providers can narrow this so `/v1/models` advertises only models
/// the gateway is actually configured to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvidersConfig {
    pub openai: bool,
    pub anthropic: bool,
    pub gemini: bool,
    pub mistral: bool,
    pub groq: bool,
    pub together: bool,
    pub openrouter: bool,
}

impl ProvidersConfig {
    /// All built-in providers enabled (the default).
    pub const fn all() -> Self {
        Self {
            openai: true,
            anthropic: true,
            gemini: true,
            mistral: true,
            groq: true,
            together: true,
            openrouter: true,
        }
    }

    /// None enabled — start from here and turn providers on explicitly.
    pub const fn none() -> Self {
        Self {
            openai: false,
            anthropic: false,
            gemini: false,
            mistral: false,
            groq: false,
            together: false,
            openrouter: false,
        }
    }

    /// Resolve from the environment. With `TT_PROVIDERS` unset or empty, every
    /// provider is enabled (historical behavior). When set, it's a
    /// comma-separated allowlist of provider ids (e.g. `openai,anthropic,groq`)
    /// — only those are registered. Unknown ids are ignored.
    pub fn from_env() -> Self {
        match std::env::var("TT_PROVIDERS") {
            Ok(v) if !v.trim().is_empty() => Self::from_allowlist(&v),
            _ => Self::all(),
        }
    }

    /// Parse a comma-separated allowlist of provider ids into a config.
    pub fn from_allowlist(list: &str) -> Self {
        let mut cfg = Self::none();
        for id in list.split(',').map(str::trim) {
            match id.to_ascii_lowercase().as_str() {
                "openai" => cfg.openai = true,
                "anthropic" => cfg.anthropic = true,
                "gemini" => cfg.gemini = true,
                "mistral" => cfg.mistral = true,
                "groq" => cfg.groq = true,
                "together" => cfg.together = true,
                "openrouter" => cfg.openrouter = true,
                _ => {} // ignore unknown / empty entries
            }
        }
        cfg
    }
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self::all()
    }
}

/// Register every in-tree provider into the given registry. Call at startup
/// from [`crate::AppState::with_default_providers`]. Honors `TT_PROVIDERS`
/// (see [`ProvidersConfig::from_env`]); unset = register everything.
///
/// Local-LLM providers (Ollama, vLLM, LM Studio) are not registered by default
/// because they require a per-customer `base_url`. Register them ad-hoc via
/// [`ProviderRegistry::register`] when the customer configures one.
pub fn register_default_providers(registry: &mut ProviderRegistry) {
    register_providers(registry, &ProvidersConfig::from_env());
}

/// Register the built-in providers selected by `cfg`. The config-aware core of
/// [`register_default_providers`]; call directly to register a fixed subset
/// independent of the environment (tests, embedded uses).
pub fn register_providers(registry: &mut ProviderRegistry, cfg: &ProvidersConfig) {
    // Native APIs
    if cfg.openai {
        registry.register(Arc::new(OpenAiProvider::new(OpenAiClientConfig::default())));
    }
    if cfg.anthropic {
        registry.register(Arc::new(AnthropicProvider::new(
            AnthropicClientConfig::default(),
        )));
    }
    if cfg.gemini {
        registry.register(Arc::new(GeminiProvider::new(GeminiClientConfig::default())));
    }

    // OpenAI-compatible (use OpenAI's ClientConfig)
    let oai_cfg = OpenAiClientConfig::default;
    if cfg.mistral {
        registry.register(Arc::new(MistralProvider::new(oai_cfg())));
    }
    if cfg.groq {
        registry.register(Arc::new(GroqProvider::new(oai_cfg())));
    }
    if cfg.together {
        registry.register(Arc::new(TogetherProvider::new(oai_cfg())));
    }
    if cfg.openrouter {
        registry.register(Arc::new(OpenRouterProvider::new(oai_cfg())));
    }

    // Local backends register only when their base-URL env var is set.
    register_local_providers(registry, &LocalProviders::from_env());
}

/// Self-hosted local backends, each registered only when its base URL is set
/// (`TT_LOCAL_OLLAMA_URL`, `TT_LOCAL_VLLM_URL`, `TT_LOCAL_LMSTUDIO_URL`).
#[derive(Debug, Clone, Default)]
pub struct LocalProviders {
    pub ollama: Option<String>,
    pub vllm: Option<String>,
    pub lmstudio: Option<String>,
}

impl LocalProviders {
    /// Read the three base-URL env vars; an unset/empty var leaves that backend off.
    pub fn from_env() -> Self {
        let v = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        Self {
            ollama: v("TT_LOCAL_OLLAMA_URL"),
            vllm: v("TT_LOCAL_VLLM_URL"),
            lmstudio: v("TT_LOCAL_LMSTUDIO_URL"),
        }
    }
}

/// Register a `LocalProvider` for each configured backend (longer client
/// timeout for cold-start latency).
pub fn register_local_providers(registry: &mut ProviderRegistry, cfg: &LocalProviders) {
    let cc = LocalProvider::suggested_client_config();
    if let Some(url) = &cfg.ollama {
        registry.register(Arc::new(LocalProvider::with_base_url(
            LocalBackend::Ollama,
            url.clone(),
            cc.clone(),
        )));
    }
    if let Some(url) = &cfg.vllm {
        registry.register(Arc::new(LocalProvider::with_base_url(
            LocalBackend::Vllm,
            url.clone(),
            cc.clone(),
        )));
    }
    if let Some(url) = &cfg.lmstudio {
        registry.register(Arc::new(LocalProvider::with_base_url(
            LocalBackend::LmStudio,
            url.clone(),
            cc.clone(),
        )));
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn allowlist_enables_only_listed() {
        let cfg = ProvidersConfig::from_allowlist("openai, groq");
        assert!(cfg.openai && cfg.groq);
        assert!(!cfg.anthropic && !cfg.gemini && !cfg.mistral && !cfg.together && !cfg.openrouter);
    }

    #[test]
    fn allowlist_ignores_unknown_and_is_case_insensitive() {
        let cfg = ProvidersConfig::from_allowlist("OpenAI,not-a-provider,");
        assert!(cfg.openai);
        assert_eq!(cfg, {
            let mut c = ProvidersConfig::none();
            c.openai = true;
            c
        });
    }

    #[test]
    fn register_subset_only_registers_selected() {
        let mut reg = ProviderRegistry::new();
        let mut cfg = ProvidersConfig::none();
        cfg.groq = true;
        register_providers(&mut reg, &cfg);
        assert!(reg.by_id("groq").is_some());
        assert!(reg.by_id("openai").is_none());
        assert!(reg.by_id("anthropic").is_none());
    }

    #[test]
    fn resolves_local_prefixed_model_to_registered_backend() {
        let mut reg = ProviderRegistry::new();
        reg.register(std::sync::Arc::new(tt_provider_local::LocalProvider::new(
            tt_provider_local::LocalBackend::Ollama,
            tt_provider_openai::ClientConfig::default(),
        )));
        assert!(reg.resolve("ollama/llama3.1:8b").is_some());
        // Unregistered backend → None (gateway not configured for it).
        assert!(reg.resolve("vllm/qwen").is_none());
    }

    #[test]
    fn register_local_providers_honors_configured_urls() {
        let mut reg = ProviderRegistry::new();
        register_local_providers(
            &mut reg,
            &LocalProviders {
                ollama: Some("http://localhost:11434/v1".into()),
                vllm: None,
                lmstudio: None,
            },
        );
        assert!(reg.by_id("ollama").is_some());
        assert!(reg.by_id("vllm").is_none());
    }
}
