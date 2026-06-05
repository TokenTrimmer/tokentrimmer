//! Model METADATA catalog — per-(provider, model) context windows + capabilities.
//! Rates live in `pricing.rs`/`pricing.toml`; this is metadata only. Embedded at
//! build time and parsed once (mirroring `PricingCatalog`) — the single source of
//! truth for `ModelInfo` across provider adapters and `GET /v1/models`.

use std::sync::OnceLock;

use serde::Deserialize;

use crate::pricing::{Capability, ModelInfo};

const MODELS_TOML: &str = include_str!("../data/models.toml");

#[derive(Debug, Deserialize)]
struct RawModel {
    provider: String,
    model: String,
    max_input_tokens: u64,
    max_output_tokens: u64,
    #[serde(default)]
    capabilities: Vec<Capability>,
}

#[derive(Debug, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    model: Vec<RawModel>,
}

/// In-memory model-metadata catalog, built once from the embedded TOML.
#[derive(Debug)]
pub struct ModelCatalog {
    models: Vec<ModelInfo>,
}

impl ModelCatalog {
    /// Parse a catalog from TOML text (exposed for tests). Rejects a duplicate
    /// `(provider, model)` so a bad edit fails loudly (the embedded catalog is
    /// validated by `model_catalog()`'s `expect` + a unit test), mirroring the
    /// uniqueness `PricingCatalog` gets for free from its keyed map.
    pub fn parse(toml_text: &str) -> Result<Self, toml::de::Error> {
        use serde::de::Error as _;
        let raw: RawCatalog = toml::from_str(toml_text)?;
        let mut seen = std::collections::HashSet::new();
        let mut models = Vec::with_capacity(raw.model.len());
        for m in raw.model {
            if !seen.insert((m.provider.clone(), m.model.clone())) {
                return Err(toml::de::Error::custom(format!(
                    "duplicate model in models.toml: {}/{}",
                    m.provider, m.model
                )));
            }
            models.push(ModelInfo {
                id: m.model,
                provider: m.provider,
                capabilities: m.capabilities,
                max_input_tokens: m.max_input_tokens,
                max_output_tokens: m.max_output_tokens,
            });
        }
        Ok(Self { models })
    }

    /// All models for `provider`, in file order.
    #[must_use]
    pub fn for_provider(&self, provider: &str) -> Vec<ModelInfo> {
        self.models
            .iter()
            .filter(|m| m.provider == provider)
            .cloned()
            .collect()
    }

    /// Metadata for an exact `(provider, model)`.
    #[must_use]
    pub fn model_info(&self, provider: &str, model: &str) -> Option<ModelInfo> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == model)
            .cloned()
    }

    #[must_use]
    pub fn all(&self) -> &[ModelInfo] {
        &self.models
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.models.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// The process-wide model-metadata catalog, parsed once from the embedded
/// `data/models.toml`. A unit test guards the bundled file's validity.
pub fn model_catalog() -> &'static ModelCatalog {
    static CATALOG: OnceLock<ModelCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        ModelCatalog::parse(MODELS_TOML).expect("embedded data/models.toml must be valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_parses_all_providers() {
        let c = model_catalog();
        assert_eq!(c.len(), 32, "native (14) + compat (18)");
        assert_eq!(c.for_provider("openai").len(), 8);
        assert_eq!(c.for_provider("anthropic").len(), 3);
        assert_eq!(c.for_provider("gemini").len(), 3);
        assert_eq!(c.for_provider("mistral").len(), 5);
        assert_eq!(c.for_provider("groq").len(), 4);
        assert_eq!(c.for_provider("together").len(), 4);
        assert_eq!(c.for_provider("openrouter").len(), 5);
        assert!(c.for_provider("nonesuch").is_empty());
        assert!(!c.is_empty());
    }

    #[test]
    fn spot_check_compat_models() {
        let c = model_catalog();
        let codestral = c.model_info("mistral", "codestral-latest").unwrap();
        assert_eq!(codestral.max_input_tokens, 256_000);
        let pixtral = c.model_info("mistral", "pixtral-large-latest").unwrap();
        assert!(pixtral.capabilities.contains(&Capability::Vision));
        let deepseek = c
            .model_info("groq", "deepseek-r1-distill-llama-70b")
            .unwrap();
        assert!(deepseek.capabilities.contains(&Capability::Reasoning));
        // namespaced ids are distinct (provider, model) keys
        let or_gemini = c.model_info("openrouter", "google/gemini-3.1-pro").unwrap();
        assert_eq!(or_gemini.max_input_tokens, 1_000_000);
        let together_v3 = c.model_info("together", "deepseek-ai/DeepSeek-V3").unwrap();
        assert_eq!(together_v3.max_input_tokens, 64_000);
    }

    #[test]
    fn parse_rejects_duplicate_models() {
        let toml = r#"
            [[model]]
            provider = "openai"
            model = "gpt-4o"
            max_input_tokens = 128000
            max_output_tokens = 16000
            capabilities = ["text"]

            [[model]]
            provider = "openai"
            model = "gpt-4o"
            max_input_tokens = 99999
            max_output_tokens = 1
            capabilities = ["text"]
        "#;
        let err = ModelCatalog::parse(toml).unwrap_err();
        assert!(err.to_string().contains("duplicate model"), "{err}");
    }

    #[test]
    fn spot_check_known_models() {
        let c = model_catalog();
        let haiku = c.model_info("anthropic", "claude-haiku-4-5").unwrap();
        assert_eq!(haiku.max_input_tokens, 200_000);
        assert_eq!(haiku.max_output_tokens, 8192);
        assert_eq!(
            haiku.capabilities,
            vec![
                Capability::Text,
                Capability::Vision,
                Capability::Tools,
                Capability::JsonMode,
                Capability::Streaming,
                Capability::PromptCaching,
            ]
        );
        let o3 = c.model_info("openai", "o3").unwrap();
        assert_eq!(o3.max_input_tokens, 200_000);
        assert_eq!(o3.max_output_tokens, 100_000);
        assert!(o3.capabilities.contains(&Capability::Reasoning));
        let pro = c.model_info("gemini", "gemini-3.1-pro").unwrap();
        assert_eq!(pro.max_input_tokens, 2_000_000);
        assert!(c.model_info("openai", "nope").is_none());
    }
}
