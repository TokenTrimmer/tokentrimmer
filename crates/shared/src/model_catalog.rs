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
    /// Parse a catalog from TOML text (exposed for tests).
    pub fn parse(toml_text: &str) -> Result<Self, toml::de::Error> {
        let raw: RawCatalog = toml::from_str(toml_text)?;
        let models = raw
            .model
            .into_iter()
            .map(|m| ModelInfo {
                id: m.model,
                provider: m.provider,
                capabilities: m.capabilities,
                max_input_tokens: m.max_input_tokens,
                max_output_tokens: m.max_output_tokens,
            })
            .collect();
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
    fn embedded_catalog_parses_native_providers() {
        let c = model_catalog();
        assert_eq!(c.len(), 14, "native model count");
        assert_eq!(c.for_provider("openai").len(), 8);
        assert_eq!(c.for_provider("anthropic").len(), 3);
        assert_eq!(c.for_provider("gemini").len(), 3);
        assert!(c.for_provider("nonesuch").is_empty());
        assert!(!c.is_empty());
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
