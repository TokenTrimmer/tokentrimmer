//! Wrapper over per-provider pricing tables.
//!
//! Each provider crate exposes `pricing_for(&str) -> Option<ModelPricing>`.
//! We probe all three; first hit wins. Returns the pricing plus the
//! provider name so the response can populate `current.provider`.

use crate::error::PreviewError;

#[derive(Debug, Clone)]
pub struct LookupHit {
    pub provider: &'static str,
    /// Input cost per million tokens (USD).
    pub input_per_m: f64,
    /// Output cost per million tokens (USD).
    pub output_per_m: f64,
}

pub fn lookup(model: &str) -> Result<LookupHit, PreviewError> {
    if let Some(p) = tt_provider_anthropic::pricing::pricing_for(model) {
        return Ok(LookupHit {
            provider: "anthropic",
            input_per_m: p.input_per_million,
            output_per_m: p.output_per_million,
        });
    }
    if let Some(p) = tt_provider_openai::pricing::pricing_for(model) {
        return Ok(LookupHit {
            provider: "openai",
            input_per_m: p.input_per_million,
            output_per_m: p.output_per_million,
        });
    }
    if let Some(p) = tt_provider_gemini::pricing::pricing_for(model) {
        return Ok(LookupHit {
            provider: "gemini",
            input_per_m: p.input_per_million,
            output_per_m: p.output_per_million,
        });
    }
    Err(PreviewError::UnknownModel(model.to_string()))
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
        let hit = LookupHit { provider: "x", input_per_m: 3.0, output_per_m: 15.0 };
        // 1000 in @ $3/M = $0.003; 100 out @ $15/M = $0.0015 → total $0.0045
        let c = cost_usd(1000, 100, &hit);
        assert!((c - 0.0045).abs() < 1e-9, "cost = {c}");
    }

    #[test]
    fn lookup_unknown_model_errors() {
        let err = lookup("does-not-exist-model").unwrap_err();
        assert!(matches!(err, PreviewError::UnknownModel(_)));
    }
}
