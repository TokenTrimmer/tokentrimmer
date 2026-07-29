//! Model METADATA catalog — per-(provider, model) context windows + capabilities.
//! Rates live in `pricing.rs`/`pricing.toml`; this is metadata only. Embedded at
//! build time and parsed once (mirroring `PricingCatalog`) — the single source of
//! truth for `ModelInfo` across provider adapters and `GET /v1/models`.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::pricing::{Capability, ModelInfo, ModelPricing};

const MODELS_TOML: &str = include_str!("../data/models.toml");

/// Wire version for TokenTrimmer's additive `GET /v1/models` extension.
///
/// OpenAI-compatible `object`/`data` fields remain at the response root. A
/// breaking change to the TokenTrimmer metadata must use a new version.
pub const MODELS_SCHEMA_VERSION: u32 = 1;
pub const MODELS_SNAPSHOT_SCOPE: &str = "responding_process";
pub const MODELS_SOURCE: &str = "registered_provider_catalog";
pub const MODELS_PROVIDER_CREDENTIALS: &str = "not_inspected";
pub const MODELS_PROVIDER_HEALTH: &str = "not_probed";
pub const MODELS_REQUEST_ACCEPTANCE: &str = "not_negotiated";
pub const MODELS_FLEET_CONSISTENCY: &str = "not_attested";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelEntry>,
    pub tokentrimmer: ModelsDocumentMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct ModelEntry {
    pub id: String,
    pub object: String,
    pub owned_by: String,
    pub tokentrimmer: ModelTokenTrimmerMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct ModelTokenTrimmerMeta {
    pub provider: String,
    pub pricing: Option<ModelPricing>,
    pub capabilities: Vec<Capability>,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

/// Provenance and explicit limitations for one responding process's catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ModelsDocumentMeta {
    pub schema_version: u32,
    pub snapshot_scope: String,
    pub source: String,
    /// SHA-256 over the deterministic JSON serialization of the sorted `data`
    /// array. It identifies this exact responder snapshot; it is not a signed
    /// release revision or fleet-consistency claim.
    pub snapshot_sha256: String,
    pub limitations: ModelCatalogLimitations,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub struct ModelCatalogLimitations {
    pub provider_credentials: String,
    pub provider_health: String,
    pub request_acceptance: String,
    pub fleet_consistency: String,
}

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
        assert_eq!(c.len(), 34, "native (16) + compat (18)");
        assert_eq!(c.for_provider("openai").len(), 9); // + gpt-5.4-mini
        assert_eq!(c.for_provider("anthropic").len(), 4); // + claude-opus-4-8
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
        assert_eq!(haiku.max_output_tokens, 65_536); // 64K, not the stale 8192
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

    /// The corrected Anthropic flagships: Sonnet 4.6 is 1M context / 64K output
    /// (was stale at 200K/8192) and Opus 4.8 is present at 1M / 128K.
    #[test]
    fn anthropic_flagships_have_current_windows() {
        let c = model_catalog();

        let sonnet = c.model_info("anthropic", "claude-sonnet-4-6").unwrap();
        assert_eq!(
            sonnet.max_input_tokens, 1_000_000,
            "sonnet-4-6 is 1M context"
        );
        assert_eq!(
            sonnet.max_output_tokens, 65_536,
            "sonnet-4-6 is 64K max output"
        );

        let opus47 = c.model_info("anthropic", "claude-opus-4-7").unwrap();
        assert_eq!(opus47.max_input_tokens, 1_000_000);
        assert_eq!(opus47.max_output_tokens, 131_072);

        // Opus 4.8 must exist (was missing from the catalog).
        let opus48 = c.model_info("anthropic", "claude-opus-4-8").unwrap();
        assert_eq!(opus48.max_input_tokens, 1_000_000);
        assert_eq!(opus48.max_output_tokens, 131_072);
        assert!(opus48.capabilities.contains(&Capability::PromptCaching));

        // The OpenRouter mirror of Sonnet 4.6 tracks the same window.
        let or_sonnet = c
            .model_info("openrouter", "anthropic/claude-sonnet-4-6")
            .unwrap();
        assert_eq!(or_sonnet.max_input_tokens, 1_000_000);
        assert_eq!(or_sonnet.max_output_tokens, 65_536);
    }
}
