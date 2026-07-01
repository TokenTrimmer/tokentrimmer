//! Deterministic document/image-token savings projection (no LLM).
//!
//! Mirrors [`crate::cache_projection`]: a pure `project()` + a struct. Given the
//! raw image tokens a request will spend and a (directional) projection of the
//! text-equivalent tokens a distillation would produce, it prices both at the
//! model's INPUT rate and reports the projected saving — labelled an estimate.
//!
//! The **provider-direction guard** is load-bearing: Gemini prices a page-image
//! flat (~258 tok), *cheaper* than distilled text, so replacing the image with
//! text would INCREASE cost. When the resolved model is Gemini the projector
//! books ZERO saving. Negative savings (distilled costs more than raw) clamp to
//! zero for every provider.

use tt_tokenize::image_tokens::{formula_for_model, ImageFormula};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DocProjection {
    /// Estimated input tokens the raw image parts will spend.
    pub raw_image_tokens: u32,
    /// Cost of `raw_image_tokens` at the model's input rate.
    pub raw_image_cost_usd: f64,
    /// Directional projection of the text-equivalent token count after distill.
    pub projected_distilled_tokens: u32,
    /// Cost of `projected_distilled_tokens` at the model's input rate.
    pub projected_distilled_cost_usd: f64,
    /// `max(raw_cost - distilled_cost, 0)`, clamped to 0 by the Gemini guard.
    pub projected_savings_usd: f64,
    /// Always `"estimate"` — this is a directional figure, not billing.
    pub basis: &'static str,
    /// Provider-direction note when the saving is guarded to 0 (else `None`).
    pub note: Option<String>,
}

/// Project the savings of replacing raw image tokens with distilled text tokens.
///
/// Both sides are priced at the model's INPUT rate (`input_price_per_mtok`),
/// labelled an estimate. The provider-direction guard books ZERO for Gemini
/// (page-images are priced flat and cheaper than distilled text there); any
/// negative saving clamps to 0.
#[must_use]
pub fn project(
    raw_image_tokens: u32,
    projected_distilled_tokens: u32,
    input_price_per_mtok: f64,
    model: &str,
) -> DocProjection {
    let price = |tok: u32| f64::from(tok) * input_price_per_mtok / 1_000_000.0;
    let raw_cost = price(raw_image_tokens);
    let distilled_cost = price(projected_distilled_tokens);
    let gemini = matches!(formula_for_model(model), ImageFormula::GeminiTiled);
    let (savings, note) = if gemini {
        (
            0.0,
            Some(
                "Gemini prices page-images flat and cheaper than distilled text; \
                 no saving booked."
                    .to_string(),
            ),
        )
    } else {
        ((raw_cost - distilled_cost).max(0.0), None)
    };
    DocProjection {
        raw_image_tokens,
        raw_image_cost_usd: raw_cost,
        projected_distilled_tokens,
        projected_distilled_cost_usd: distilled_cost,
        projected_savings_usd: savings,
        basis: "estimate",
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savings_positive_for_openai() {
        // raw 1000 img tok vs 200 distilled text tok @ $5/Mtok input.
        let p = project(1000, 200, 5.0, "gpt-4o");
        assert!((p.raw_image_cost_usd - 0.005).abs() < 1e-9);
        assert!((p.projected_distilled_cost_usd - 0.001).abs() < 1e-9);
        assert!((p.projected_savings_usd - 0.004).abs() < 1e-9);
        assert_eq!(p.basis, "estimate");
        assert!(p.note.is_none());
    }

    #[test]
    fn gemini_direction_guard_books_zero() {
        // Gemini prices page-images flat + cheaper than text → NO saving.
        let p = project(258, 800, 1.0, "gemini-2.5-flash");
        assert_eq!(p.projected_savings_usd, 0.0);
        assert!(p.note.is_some());
    }

    #[test]
    fn negative_savings_clamped_to_zero() {
        // Distilled costs more than raw → clamp to 0 (non-Gemini).
        let p = project(100, 500, 5.0, "gpt-4o");
        assert_eq!(p.projected_savings_usd, 0.0);
        assert!(p.note.is_none());
    }

    #[test]
    fn serializes_with_estimate_basis() {
        let p = project(1000, 200, 5.0, "gpt-4o");
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"basis\":\"estimate\""));
        assert!(json.contains("\"raw_image_tokens\":1000"));
    }
}
