//! Pricing tables per model. Values come from provider pricing pages and are
//! refreshed daily. `effective_at` lets us replay historical telemetry against
//! the correct rate. See `docs/02-provider-adapter-guide.md`.
//!
//! Rates live in a versioned data file (`data/pricing.toml`), embedded at build
//! time and parsed once into a [`PricingCatalog`]. Provider adapters delegate
//! to [`catalog`] instead of hardcoding rate tables, so a price refresh is a
//! data edit — decoupled from a Rust release. The catalog keeps a per-model
//! price *history*, enabling [`PricingCatalog::at`] to price historical
//! telemetry against the rate that was in effect at request time.

use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    /// USD per 1M input tokens.
    pub input_per_million: f64,
    /// USD per 1M output tokens.
    pub output_per_million: f64,
    /// USD per 1M cached input tokens (Anthropic 10%, OpenAI 10%, Gemini 10%).
    pub cached_input_per_million: Option<f64>,
    /// When this pricing took effect (for historical replay).
    pub effective_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub capabilities: Vec<Capability>,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Text,
    Vision,
    Audio,
    Tools,
    JsonMode,
    Streaming,
    Reasoning,
    PromptCaching,
}

/// Embedded versioned rate catalog. The source of truth for token rates;
/// edited as data (`data/pricing.toml`), not Rust source.
const PRICING_TOML: &str = include_str!("../data/pricing.toml");

/// One row of the catalog as it appears in `pricing.toml`.
#[derive(Debug, Deserialize)]
struct RawEntry {
    provider: String,
    model: String,
    input_per_million: f64,
    output_per_million: f64,
    #[serde(default)]
    cached_input_per_million: Option<f64>,
    effective_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RawCatalog {
    #[serde(default)]
    entry: Vec<RawEntry>,
}

/// In-memory pricing catalog: per `(provider, model)`, a price history sorted
/// ascending by `effective_at`. Built once from the embedded TOML.
#[derive(Debug)]
pub struct PricingCatalog {
    by_model: HashMap<(String, String), Vec<ModelPricing>>,
}

impl PricingCatalog {
    /// Parse a catalog from TOML text. Used by [`catalog`] over the embedded
    /// file; exposed for tests that want to parse a synthetic catalog.
    pub fn parse(toml_text: &str) -> Result<Self, toml::de::Error> {
        let raw: RawCatalog = toml::from_str(toml_text)?;
        let mut by_model: HashMap<(String, String), Vec<ModelPricing>> = HashMap::new();
        for e in raw.entry {
            by_model
                .entry((e.provider, e.model))
                .or_default()
                .push(ModelPricing {
                    input_per_million: e.input_per_million,
                    output_per_million: e.output_per_million,
                    cached_input_per_million: e.cached_input_per_million,
                    effective_at: e.effective_at,
                });
        }
        // Sort each model's history ascending by effective_at so `latest` is
        // the last element and `at` can scan from newest backward.
        for history in by_model.values_mut() {
            history.sort_by_key(|p| p.effective_at);
        }
        Ok(Self { by_model })
    }

    /// The current (most recently effective) rate for `(provider, model)`,
    /// or `None` if the model is not in the catalog.
    pub fn latest(&self, provider: &str, model: &str) -> Option<ModelPricing> {
        self.by_model
            .get(&(provider.to_string(), model.to_string()))?
            .last()
            .cloned()
    }

    /// The rate that was in effect at `at` for `(provider, model)` — the most
    /// recent entry whose `effective_at <= at`. If `at` predates every known
    /// entry, falls back to the earliest entry (best-effort historical replay
    /// rather than reporting no price). `None` only when the model is unknown.
    pub fn at(&self, provider: &str, model: &str, at: DateTime<Utc>) -> Option<ModelPricing> {
        let history = self
            .by_model
            .get(&(provider.to_string(), model.to_string()))?;
        history
            .iter()
            .rev()
            .find(|p| p.effective_at <= at)
            .or_else(|| history.first())
            .cloned()
    }

    /// Every model's current rate for `provider`, as `(model, pricing)` pairs.
    /// Order is unspecified. Used by adapters that build a model→rate map at
    /// construction time (the OpenAI-compatible providers).
    pub fn latest_for_provider(&self, provider: &str) -> Vec<(String, ModelPricing)> {
        self.by_model
            .iter()
            .filter(|((p, _), _)| p == provider)
            .filter_map(|((_, model), history)| history.last().map(|p| (model.clone(), p.clone())))
            .collect()
    }

    /// Every `(provider, model)` pair in the catalog. Order is unspecified.
    /// Pair with [`latest`](Self::latest) / [`at`](Self::at) to materialize a
    /// full rate table (e.g. for the Plan replay engine).
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.by_model.keys().cloned().collect()
    }

    /// Number of distinct `(provider, model)` pairs in the catalog.
    pub fn len(&self) -> usize {
        self.by_model.len()
    }

    /// Whether the catalog has no entries.
    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty()
    }
}

/// The process-wide pricing catalog, parsed once from the embedded
/// `data/pricing.toml`. Panics at first use only if that bundled file is
/// malformed — which a unit test guards against, so it cannot reach a release.
pub fn catalog() -> &'static PricingCatalog {
    static CATALOG: OnceLock<PricingCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        PricingCatalog::parse(PRICING_TOML).expect("embedded data/pricing.toml must be valid")
    })
}

#[cfg(test)]
mod catalog_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn embedded_catalog_parses_and_is_populated() {
        let c = catalog();
        assert!(!c.is_empty(), "embedded catalog should not be empty");
        // 32 models across 7 paid providers at import time.
        assert_eq!(
            c.len(),
            32,
            "unexpected catalog size — update if intentional"
        );
    }

    #[test]
    fn latest_returns_known_rates() {
        let c = catalog();
        let p = c.latest("openai", "gpt-4o").expect("gpt-4o present");
        assert_eq!(p.input_per_million, 2.50);
        assert_eq!(p.output_per_million, 10.00);
        assert_eq!(p.cached_input_per_million, Some(1.25));

        // A model whose cached rate is omitted in TOML → None, not 0.0.
        let g = c.latest("groq", "llama-3.1-8b-instant").expect("present");
        assert_eq!(g.cached_input_per_million, None);
    }

    #[test]
    fn unknown_provider_or_model_is_none() {
        let c = catalog();
        assert!(c.latest("openai", "no-such-model").is_none());
        assert!(c.latest("no-such-provider", "gpt-4o").is_none());
    }

    #[test]
    fn at_selects_rate_effective_at_timestamp() {
        // Two-entry history: $1/$2 from 2026-01-01, $3/$4 from 2026-06-01.
        let toml = r#"
            [[entry]]
            provider = "p"
            model = "m"
            input_per_million = 1.0
            output_per_million = 2.0
            effective_at = "2026-01-01T00:00:00Z"

            [[entry]]
            provider = "p"
            model = "m"
            input_per_million = 3.0
            output_per_million = 4.0
            effective_at = "2026-06-01T00:00:00Z"
        "#;
        let c = PricingCatalog::parse(toml).expect("valid");

        // Before either entry → earliest (best-effort).
        let before = c
            .at("p", "m", Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap())
            .unwrap();
        assert_eq!(before.input_per_million, 1.0);

        // Between the two → first (older) rate.
        let mid = c
            .at("p", "m", Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap())
            .unwrap();
        assert_eq!(mid.input_per_million, 1.0);

        // After the second → newest rate.
        let after = c
            .at("p", "m", Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap())
            .unwrap();
        assert_eq!(after.input_per_million, 3.0);

        // `latest` is always the newest regardless of time.
        assert_eq!(c.latest("p", "m").unwrap().input_per_million, 3.0);
    }
}
