//! Azure OpenAI rate card (public, Global Standard deployments).
//!
//! Azure runs the same underlying OpenAI models, and its **public** per-token
//! rates for Global Standard deployments track OpenAI's list prices. We carry a
//! small in-crate table here (keyed by the OpenAI model id, which is what a
//! customer names their deployment after) rather than the shared
//! `data/pricing.toml` catalog, because:
//!
//! 1. The Azure model id seen on the wire is the *deployment name*, which is
//!    customer-chosen and need not equal the OpenAI model id — so a global
//!    catalog keyed by `(provider, model)` can't reliably price it.
//! 2. **BYOK / Enterprise Agreement rates differ.** Azure customers frequently
//!    negotiate committed-use discounts, PTU (Provisioned Throughput Unit)
//!    pricing, or regional rates that diverge from this public card. This table
//!    is therefore a *best-effort public-rate-card estimate*; the authoritative
//!    cost for a given org may be lower. Cost attribution downstream tolerates a
//!    `None`/approximate rate.
//!
//! Source: azure.microsoft.com/pricing/details/cognitive-services/openai-service
//! (Global Standard, verified 2026-06). Update alongside OpenAI list moves.

use chrono::{TimeZone, Utc};
use tt_shared::pricing::ModelPricing;

/// Effective date stamped on every row in this table (for historical replay).
fn effective_at() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()
}

fn rate(
    input_per_million: f64,
    output_per_million: f64,
    cached_input_per_million: Option<f64>,
) -> ModelPricing {
    ModelPricing {
        input_per_million,
        output_per_million,
        cached_input_per_million,
        cache_write_per_million: None,
        batch_input_per_million: None,
        batch_output_per_million: None,
        flex_input_per_million: None,
        flex_output_per_million: None,
        prompt_cache_min_tokens: None,
        effective_at: effective_at(),
    }
}

/// Best-effort public-rate-card pricing for an Azure OpenAI deployment.
///
/// Matches on the OpenAI model id the deployment is most likely named after. A
/// deployment whose name does not match a known model returns `None` (cost
/// attribution then proceeds without a per-token rate, unchanged) — Azure
/// deployment names are arbitrary, so we never guess.
///
/// **Public rates only.** Negotiated / PTU / committed-use rates differ; see the
/// module docs.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let p = match model {
        // gpt-5.x family (Global Standard, mirrors OpenAI list).
        "gpt-5.5" => rate(5.00, 30.00, Some(0.50)),
        "gpt-5.4" => rate(2.50, 15.00, Some(0.25)),
        "gpt-5.4-mini" => rate(0.75, 4.50, Some(0.075)),
        // gpt-4o family.
        "gpt-4o" => rate(2.50, 10.00, Some(1.25)),
        "gpt-4o-mini" => rate(0.15, 0.60, Some(0.075)),
        // Reasoning models.
        "o3" => rate(2.00, 8.00, Some(0.50)),
        "o4-mini" => rate(1.10, 4.40, Some(0.275)),
        // Embeddings (output rate 0).
        "text-embedding-3-small" => rate(0.02, 0.00, None),
        "text-embedding-3-large" => rate(0.13, 0.00, None),
        _ => return None,
    };
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_known_models() {
        let p = pricing_for("gpt-4o").expect("priced");
        assert!((p.input_per_million - 2.50).abs() < 1e-9);
        assert!((p.output_per_million - 10.00).abs() < 1e-9);
        assert_eq!(p.cached_input_per_million, Some(1.25));
    }

    #[test]
    fn embeddings_have_zero_output_rate() {
        let p = pricing_for("text-embedding-3-small").expect("priced");
        assert!((p.output_per_million - 0.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_deployment_name_is_unpriced() {
        // An arbitrary deployment name has no public rate — we never guess.
        assert!(pricing_for("my-custom-deployment").is_none());
        assert!(pricing_for("").is_none());
    }
}
