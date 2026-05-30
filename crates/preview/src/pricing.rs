//! Wrapper over per-provider pricing tables.
//!
//! Native providers (anthropic/openai/gemini) expose a free
//! `pricing_for(&str) -> Option<ModelPricing>`. OpenAI-compatible providers
//! (groq/mistral/together/openrouter) carry pricing in their adapter, so we
//! probe them via the `Provider::pricing` trait method (a cheap struct init,
//! no network). First hit wins; the provider name is returned so the response
//! can populate `current.provider`. `local` is intentionally NOT probed — its
//! pricing is a $0 catch-all for arbitrary self-hosted models, which would
//! mask genuinely-unknown models behind a free price.

use crate::error::PreviewError;
use tt_provider_openai::ClientConfig;
use tt_shared::{ModelPricing, Provider};

#[derive(Debug, Clone)]
pub struct LookupHit {
    pub provider: &'static str,
    /// Input cost per million tokens (USD).
    pub input_per_m: f64,
    /// Output cost per million tokens (USD).
    pub output_per_m: f64,
}

fn hit(provider: &'static str, p: &ModelPricing) -> LookupHit {
    LookupHit {
        provider,
        input_per_m: p.input_per_million,
        output_per_m: p.output_per_million,
    }
}

pub fn lookup(model: &str) -> Result<LookupHit, PreviewError> {
    // Native providers — free pricing_for() lookup.
    if let Some(p) = tt_provider_anthropic::pricing::pricing_for(model) {
        return Ok(hit("anthropic", &p));
    }
    if let Some(p) = tt_provider_openai::pricing::pricing_for(model) {
        return Ok(hit("openai", &p));
    }
    if let Some(p) = tt_provider_gemini::pricing::pricing_for(model) {
        return Ok(hit("gemini", &p));
    }
    // OpenAI-compatible providers — probe via the Provider trait.
    let cfg = ClientConfig::default;
    if let Some(p) = tt_provider_groq::GroqProvider::new(cfg()).pricing(model) {
        return Ok(hit("groq", &p));
    }
    if let Some(p) = tt_provider_mistral::MistralProvider::new(cfg()).pricing(model) {
        return Ok(hit("mistral", &p));
    }
    if let Some(p) = tt_provider_together::TogetherProvider::new(cfg()).pricing(model) {
        return Ok(hit("together", &p));
    }
    if let Some(p) = tt_provider_openrouter::OpenRouterProvider::new(cfg()).pricing(model) {
        return Ok(hit("openrouter", &p));
    }
    Err(PreviewError::UnknownModel(model.to_string()))
}

/// Look up pricing for `model` from a SPECIFIC provider's catalog.
///
/// Use this when the intended provider is known (e.g. a route suggestion's
/// candidate is tied to a provider) so a model that is cross-listed in
/// multiple catalogs is attributed + priced from the RIGHT provider rather
/// than whichever the [`lookup`] probe order hits first (§4.12 / preview
/// disambiguation). Returns [`PreviewError::UnknownModel`] when that provider
/// does not carry the model. `local` is intentionally not resolvable here
/// (its $0 catch-all would mask unknown models).
pub fn lookup_with_provider(model: &str, provider: &str) -> Result<LookupHit, PreviewError> {
    let cfg = ClientConfig::default;
    let found = match provider {
        "anthropic" => tt_provider_anthropic::pricing::pricing_for(model).map(|p| hit("anthropic", &p)),
        "openai" => tt_provider_openai::pricing::pricing_for(model).map(|p| hit("openai", &p)),
        "gemini" => tt_provider_gemini::pricing::pricing_for(model).map(|p| hit("gemini", &p)),
        "groq" => tt_provider_groq::GroqProvider::new(cfg()).pricing(model).map(|p| hit("groq", &p)),
        "mistral" => tt_provider_mistral::MistralProvider::new(cfg()).pricing(model).map(|p| hit("mistral", &p)),
        "together" => tt_provider_together::TogetherProvider::new(cfg()).pricing(model).map(|p| hit("together", &p)),
        "openrouter" => {
            tt_provider_openrouter::OpenRouterProvider::new(cfg()).pricing(model).map(|p| hit("openrouter", &p))
        }
        _ => None,
    };
    found.ok_or_else(|| PreviewError::UnknownModel(model.to_string()))
}

/// Cost of a single call given token counts.
pub fn cost_usd(input_tokens: u32, output_tokens: u32, hit: &LookupHit) -> f64 {
    let i = (input_tokens as f64) * hit.input_per_m / 1_000_000.0;
    let o = (output_tokens as f64) * hit.output_per_m / 1_000_000.0;
    i + o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_math_basics() {
        let hit = LookupHit {
            provider: "x",
            input_per_m: 3.0,
            output_per_m: 15.0,
        };
        // 1000 in @ $3/M = $0.003; 100 out @ $15/M = $0.0015 → total $0.0045
        let c = cost_usd(1000, 100, &hit);
        assert!((c - 0.0045).abs() < 1e-9, "cost = {c}");
    }

    #[test]
    fn lookup_unknown_model_errors() {
        let err = lookup("does-not-exist-model").unwrap_err();
        assert!(matches!(err, PreviewError::UnknownModel(_)));
    }

    #[test]
    fn lookup_resolves_compat_provider_models() {
        // Groq (the dogfood routing target) must resolve via the compat probe,
        // not just the three native providers.
        let hit = lookup("llama-3.1-8b-instant").expect("groq model should resolve");
        assert_eq!(hit.provider, "groq");
        assert!(hit.input_per_m > 0.0, "groq pricing should be > 0");
    }

    #[test]
    fn lookup_with_provider_attributes_to_named_provider() {
        // A known model resolves from its own provider's catalog.
        let hit = lookup_with_provider("gpt-4o-mini", "openai").expect("openai carries gpt-4o-mini");
        assert_eq!(hit.provider, "openai");
        let hit = lookup_with_provider("claude-haiku-4-5", "anthropic")
            .expect("anthropic carries claude-haiku-4-5");
        assert_eq!(hit.provider, "anthropic");
    }

    #[test]
    fn lookup_with_provider_errors_when_provider_lacks_model() {
        // Targeting the wrong provider for a model returns UnknownModel rather
        // than silently falling back to whatever the probe order would hit.
        let err = lookup_with_provider("gpt-4o-mini", "groq").unwrap_err();
        assert!(matches!(err, PreviewError::UnknownModel(_)));
        // An unrecognized provider name also errors.
        let err = lookup_with_provider("gpt-4o-mini", "nope").unwrap_err();
        assert!(matches!(err, PreviewError::UnknownModel(_)));
    }
}
