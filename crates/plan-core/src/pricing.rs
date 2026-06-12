//! Build a replay [`PricingTable`] from the shared versioned catalog
//! (`tt_shared::pricing`).
//!
//! Before this, every `tt plan` / cloud replay had to hand-supply a full
//! `pricing` map in its `PlanInput` JSON — easy to drift from what the gateway
//! actually bills. These helpers materialize the table straight from the same
//! embedded catalog the gateway prices with, keyed `"provider:model"` to match
//! [`crate::types::pricing_key`]. Use [`catalog_pricing_table_at`] to price a
//! historical replay against the rate that was in effect at the telemetry's
//! timestamp.

use chrono::{DateTime, Utc};

use crate::types::{pricing_key, ModelPricing, PricingTable};

fn to_plan(p: &tt_shared::pricing::ModelPricing) -> ModelPricing {
    ModelPricing {
        input_per_million: p.input_per_million,
        output_per_million: p.output_per_million,
        cached_input_per_million: p.cached_input_per_million,
        // Real catalog Batch-API rates (None = no batch tier) — feed the
        // batch-eligibility route projection; never a fabricated 0.5×.
        batch_input_per_million: p.batch_input_per_million,
        batch_output_per_million: p.batch_output_per_million,
    }
}

/// A `PricingTable` of every catalog model at its **current** rate.
#[must_use]
pub fn catalog_pricing_table() -> PricingTable {
    let catalog = tt_shared::pricing::catalog();
    let mut table = PricingTable::new();
    for (provider, model) in catalog.pairs() {
        if let Some(p) = catalog.latest(&provider, &model) {
            table.insert(pricing_key(&provider, &model), to_plan(&p));
        }
    }
    table
}

/// A `PricingTable` of every catalog model at the rate **effective at `at`** —
/// for historical replay against the rate in force when the telemetry was
/// recorded (best-effort: predating the earliest known rate falls back to it).
#[must_use]
pub fn catalog_pricing_table_at(at: DateTime<Utc>) -> PricingTable {
    let catalog = tt_shared::pricing::catalog();
    let mut table = PricingTable::new();
    for (provider, model) in catalog.pairs() {
        if let Some(p) = catalog.at(&provider, &model, at) {
            table.insert(pricing_key(&provider, &model), to_plan(&p));
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn catalog_table_is_populated_and_keyed_provider_model() {
        let table = catalog_pricing_table();
        assert!(!table.is_empty(), "shared catalog should yield rows");
        // A known gateway model is present under the provider:model key.
        let p = table
            .get(&pricing_key("openai", "gpt-4o"))
            .expect("openai:gpt-4o should be in the catalog table");
        assert_eq!(p.input_per_million, 2.50);
        assert_eq!(p.output_per_million, 10.00);
        assert_eq!(p.cached_input_per_million, Some(1.25));
    }

    /// `to_plan` copies the catalog Batch-API rates so the batch-eligibility
    /// route projection prices at the REAL tier — and maps a model without a
    /// batch tier to `None`/`None` (no fabricated discount).
    #[test]
    fn to_plan_copies_batch_rates() {
        let table = catalog_pricing_table();
        let gpt = table
            .get(&pricing_key("openai", "gpt-5.5"))
            .expect("openai:gpt-5.5 in catalog table");
        assert_eq!(gpt.batch_input_per_million, Some(2.50));
        assert_eq!(gpt.batch_output_per_million, Some(15.00));

        let groq = table
            .get(&pricing_key("groq", "llama-3.1-8b-instant"))
            .expect("groq:llama-3.1-8b-instant in catalog table");
        assert_eq!(groq.batch_input_per_million, None);
        assert_eq!(groq.batch_output_per_million, None);
    }

    #[test]
    fn at_time_table_matches_latest_for_single_history() {
        // The seed catalog has one rate per model, so `at` (after that date)
        // equals `latest`.
        let after = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let live = catalog_pricing_table();
        let hist = catalog_pricing_table_at(after);
        let key = pricing_key("anthropic", "claude-sonnet-4-6");
        assert_eq!(
            live.get(&key).map(|p| p.input_per_million),
            hist.get(&key).map(|p| p.input_per_million),
        );
    }
}
