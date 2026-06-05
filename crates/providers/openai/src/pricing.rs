//! OpenAI model catalog. Rates are sourced from the versioned, embedded
//! pricing catalog (`tt_shared::pricing`) rather than hardcoded here — a rate
//! refresh is a `data/pricing.toml` edit, not a code change. Model descriptors
//! (capabilities / token limits) stay typed in Rust below.

use tt_shared::pricing::{catalog, ModelInfo, ModelPricing};

/// Return the pricing entry for a known OpenAI model, or `None` if unrecognized.
///
/// Delegates to the shared catalog's current rate. The catalog also covers
/// embedding models (`text-embedding-3-small`, `text-embedding-3-large`),
/// which price input tokens only (zero output rate).
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    catalog().latest("openai", model)
}

/// Return all supported OpenAI model descriptors, including embedding models.
pub fn all_models() -> Vec<ModelInfo> {
    tt_shared::model_catalog::model_catalog().for_provider("openai")
}
