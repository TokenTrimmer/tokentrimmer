//! Cost projection for a single replayed request. The math is deliberately
//! minimal so the determinism contract is easy to audit: same inputs in,
//! same `f64` out.
//!
//! `compute_baseline_cost` re-derives the historical cost from the same
//! pricing table — it's the denominator we compare against. Both helpers
//! charge cached input tokens at `cached_input_per_million` when set,
//! falling back to the full input rate when the pricing entry doesn't
//! advertise a cache discount.

use crate::types::{ModelPricing, RequestLog};

/// A projected cost for one replayed request.
#[derive(Debug, Clone)]
pub struct ProjectedCost {
    /// The recomputed cost, USD, under the proposed model + pricing.
    pub cost_usd: f64,
}

/// Project the cost of one request under a different model + pricing entry.
///
/// `target_model` is taken purely for traceability — the math uses
/// `pricing` directly. Cached-token rate falls back to the non-cached
/// input rate when the pricing entry doesn't advertise a discount.
#[must_use]
pub fn project_cost(req: &RequestLog, _target_model: &str, pricing: &ModelPricing) -> ProjectedCost {
    let cached = req.cached_tokens.min(req.input_tokens);
    let non_cached_input = req.input_tokens.saturating_sub(cached);
    let cached_rate = pricing
        .cached_input_per_million
        .unwrap_or(pricing.input_per_million);
    let cost = (f64::from(non_cached_input)) * pricing.input_per_million / 1_000_000.0
        + (f64::from(cached)) * cached_rate / 1_000_000.0
        + (f64::from(req.output_tokens)) * pricing.output_per_million / 1_000_000.0;
    ProjectedCost { cost_usd: cost }
}

/// Re-derive the baseline cost from a pricing entry. Used by tests and
/// any caller that wants to validate the historical `cost_usd` field
/// against today's pricing snapshot.
#[must_use]
pub fn compute_baseline_cost(req: &RequestLog, pricing: &ModelPricing) -> f64 {
    project_cost(req, &req.model, pricing).cost_usd
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn sample_request(input: u32, output: u32, cached: u32) -> RequestLog {
        RequestLog {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            ts: chrono::Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet".into(),
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cost_usd: 0.0,
            baseline_cost_usd: 0.0,
            cached: false,
            cache_layer: None,
            matched_route_id: None,
            latency_ms: 0,
            upstream_latency_ms: None,
            status: 200,
            tag: None,
            embedding: None,
            finish_reason: None,
            body: None,
            response_body: None,
        }
    }

    #[test]
    fn project_cost_with_full_pricing() {
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
        };
        let req = sample_request(1_000_000, 1_000_000, 0);
        let p = project_cost(&req, "x", &pricing);
        // 1M input @ $3 + 1M output @ $15 = $18.
        assert!((p.cost_usd - 18.0).abs() < 1e-9, "got {}", p.cost_usd);
    }

    #[test]
    fn project_cost_charges_cached_at_discount() {
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
        };
        let req = sample_request(1_000_000, 0, 500_000);
        let p = project_cost(&req, "x", &pricing);
        // 500K non-cached @ $3/1M + 500K cached @ $0.30/1M = $1.50 + $0.15 = $1.65
        assert!((p.cost_usd - 1.65).abs() < 1e-9, "got {}", p.cost_usd);
    }

    #[test]
    fn project_cost_falls_back_to_full_rate_when_no_cache_discount() {
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: None,
        };
        let req = sample_request(1_000_000, 0, 500_000);
        let p = project_cost(&req, "x", &pricing);
        // All input charged at the full rate -> $3.00.
        assert!((p.cost_usd - 3.0).abs() < 1e-9, "got {}", p.cost_usd);
    }

    #[test]
    fn project_cost_clamps_cached_to_input() {
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
        };
        // cached_tokens > input_tokens — should clamp.
        let req = sample_request(1_000, 0, 5_000);
        let p = project_cost(&req, "x", &pricing);
        // All 1000 charged at cached rate ($0.30/1M).
        let want = 1_000.0 * 0.3 / 1_000_000.0;
        assert!((p.cost_usd - want).abs() < 1e-12, "got {}", p.cost_usd);
    }
}
