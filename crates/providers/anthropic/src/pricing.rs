//! Anthropic model catalog. Rates come from the versioned, embedded pricing
//! catalog (`tt_shared::pricing`); `cached_input_per_million` there is the
//! cache **read** rate (~10% of standard input). Cache-write surcharges are
//! not modeled (recovered on second use). Model descriptors stay typed below.

use tt_shared::pricing::{catalog, ModelInfo, ModelPricing};

/// Return the pricing entry for a known Anthropic model, or `None` if
/// unrecognized. Delegates to the shared catalog's current rate.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    catalog().latest("anthropic", model)
}

/// Return all supported Anthropic model descriptors.
pub fn all_models() -> Vec<ModelInfo> {
    tt_shared::model_catalog::model_catalog().for_provider("anthropic")
}
