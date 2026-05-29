//! Provider registry — central lookup from model ID to Provider impl.
//! Adapters register themselves at server startup.

use std::collections::HashMap;
use std::sync::Arc;

use tt_provider_anthropic::{AnthropicProvider, ClientConfig as AnthropicClientConfig};
use tt_provider_gemini::{ClientConfig as GeminiClientConfig, GeminiProvider};
use tt_provider_groq::GroqProvider;
use tt_provider_mistral::MistralProvider;
use tt_provider_openai::{ClientConfig as OpenAiClientConfig, OpenAiProvider};
use tt_provider_openrouter::OpenRouterProvider;
use tt_provider_together::TogetherProvider;
use tt_shared::Provider;

#[derive(Default)]
pub struct ProviderRegistry {
    by_id: HashMap<&'static str, Arc<dyn Provider>>,
    by_model: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        let id = provider.id();
        for model in provider.models() {
            self.by_model
                .insert(model.id.clone(), Arc::clone(&provider));
        }
        self.by_id.insert(id, provider);
    }

    pub fn by_id(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.by_id.get(id).cloned()
    }

    pub fn by_model(&self, model: &str) -> Option<Arc<dyn Provider>> {
        self.by_model.get(model).cloned()
    }

    /// Iterate all registered providers as `(id, provider)` pairs.
    /// Used by `/v1/models` to enumerate the model catalog.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &Arc<dyn Provider>)> {
        self.by_id.iter().map(|(id, p)| (*id, p))
    }
}

/// Register every in-tree provider into the given registry. Call at startup
/// from [`crate::AppState::with_default_providers`].
///
/// Local-LLM providers (Ollama, vLLM, LM Studio) are not registered by default
/// because they require a per-customer `base_url`. Register them ad-hoc via
/// [`ProviderRegistry::register`] when the customer configures one.
pub fn register_default_providers(registry: &mut ProviderRegistry) {
    // Native APIs
    registry.register(Arc::new(OpenAiProvider::new(OpenAiClientConfig::default())));
    registry.register(Arc::new(AnthropicProvider::new(
        AnthropicClientConfig::default(),
    )));
    registry.register(Arc::new(GeminiProvider::new(GeminiClientConfig::default())));

    // OpenAI-compatible (use OpenAI's ClientConfig)
    let oai_cfg = OpenAiClientConfig::default;
    registry.register(Arc::new(MistralProvider::new(oai_cfg())));
    registry.register(Arc::new(GroqProvider::new(oai_cfg())));
    registry.register(Arc::new(TogetherProvider::new(oai_cfg())));
    registry.register(Arc::new(OpenRouterProvider::new(oai_cfg())));

    // Local (deferred):
    // registry.register(Arc::new(LocalProvider::new(...)));
}
