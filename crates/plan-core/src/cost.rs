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
/// `pricing` directly.
///
/// Three-bucket cache model (C02):
/// - **Non-cached input**: `input_per_million`
/// - **Cache-read tokens**: `cached_input_per_million` (falls back to input rate)
/// - **Cache-write tokens** (C02): `cache_write_per_million` (falls back to input rate)
///
/// When the new `cache_creation_input_tokens` / `cache_read_input_tokens` fields
/// are present on the request, they take precedence over the legacy collapsed
/// `cached_tokens` field. The total `input_tokens` is split across three buckets:
/// cache-write + cache-read + non-cached = `input_tokens`.
///
/// When the new fields are `None` (legacy rows), the prior two-bucket model
/// applies: `cached_tokens` are charged at the cached rate and the remainder at
/// the input rate, with cache-write tokens priced at the ordinary input rate
/// (the documented pre-C02 conservative fallback).
#[must_use]
pub fn project_cost(
    req: &RequestLog,
    _target_model: &str,
    pricing: &ModelPricing,
) -> ProjectedCost {
    let cached_rate = pricing
        .cached_input_per_million
        .unwrap_or(pricing.input_per_million);
    let cache_write_rate = pricing
        .cache_write_per_million
        .unwrap_or(pricing.input_per_million);

    let cost = match (req.cache_creation_input_tokens, req.cache_read_input_tokens) {
        // New three-bucket model: explicit cache-write and cache-read counts
        // from the gateway's provider-native telemetry.
        (Some(write), Some(read)) => {
            let write = write.min(req.input_tokens);
            let read = read.min(req.input_tokens.saturating_sub(write));
            let non_cached = req.input_tokens.saturating_sub(write).saturating_sub(read);
            f64::from(non_cached) * pricing.input_per_million / 1_000_000.0
                + f64::from(read) * cached_rate / 1_000_000.0
                + f64::from(write) * cache_write_rate / 1_000_000.0
                + f64::from(req.output_tokens) * pricing.output_per_million / 1_000_000.0
        }
        // Legacy two-bucket model: `cached_tokens` collapses cache-read and
        // cache-write into one number priced at the cached rate. Cache-write
        // tokens priced at the ordinary input rate (pre-C02 behavior).
        _ => {
            let cached = req.cached_tokens.min(req.input_tokens);
            let non_cached_input = req.input_tokens.saturating_sub(cached);
            f64::from(non_cached_input) * pricing.input_per_million / 1_000_000.0
                + f64::from(cached) * cached_rate / 1_000_000.0
                + f64::from(req.output_tokens) * pricing.output_per_million / 1_000_000.0
        }
    };
    ProjectedCost { cost_usd: cost }
}

/// Re-derive the baseline cost from a pricing entry. Used by tests and
/// any caller that wants to validate the historical `cost_usd` field
/// against today's pricing snapshot.
#[must_use]
pub fn compute_baseline_cost(req: &RequestLog, pricing: &ModelPricing) -> f64 {
    project_cost(req, &req.model, pricing).cost_usd
}

/// Price one request at the target's **Batch API** rates (full input at the
/// batch-input rate — no cache-discount stacking, deliberately conservative,
/// the same basis as the gateway's forgone-discount math — plus output at the
/// batch-output rate). `None` when the entry has no batch tier: nothing real
/// to project, never a fabricated 0.5×. Feeds the `RouteAction::batch`
/// projection in [`crate::replay`].
#[must_use]
pub fn project_batch_cost(req: &RequestLog, pricing: &ModelPricing) -> Option<ProjectedCost> {
    let (batch_in, batch_out) = match (
        pricing.batch_input_per_million,
        pricing.batch_output_per_million,
    ) {
        (Some(i), Some(o)) => (i, o),
        _ => return None,
    };
    Some(ProjectedCost {
        cost_usd: f64::from(req.input_tokens) * batch_in / 1_000_000.0
            + f64::from(req.output_tokens) * batch_out / 1_000_000.0,
    })
}

/// Price one request at the served model's **Flex** service-tier rates (full
/// input at the Flex-input rate + output at the Flex-output rate — the exact
/// same shape/precision as [`project_batch_cost`], no cache-discount stacking).
/// `None` when the entry has no Flex tier: nothing real to project, never a
/// fabricated 0.5×. Feeds the `RouteAction::flex` projection in
/// [`crate::replay`]. UNLIKE batch, the gateway applies Flex synchronously and
/// in-band, so this is a REALIZED discount rather than an advisory one.
#[must_use]
pub fn project_flex_cost(req: &RequestLog, pricing: &ModelPricing) -> Option<ProjectedCost> {
    let (flex_in, flex_out) = match (
        pricing.flex_input_per_million,
        pricing.flex_output_per_million,
    ) {
        (Some(i), Some(o)) => (i, o),
        _ => return None,
    };
    Some(ProjectedCost {
        cost_usd: f64::from(req.input_tokens) * flex_in / 1_000_000.0
            + f64::from(req.output_tokens) * flex_out / 1_000_000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    #[test]
    fn cache_write_tokens_price_at_cache_write_rate() {
        // A request with explicit cache-creation (write) and cache-read counts
        // bills cache-write tokens at the (higher) cache-write rate.
        let mut req = sample_request(1000, 100, 0);
        req.cache_creation_input_tokens = Some(400);
        req.cache_read_input_tokens = Some(300);
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: Some(0.1),
            cache_write_per_million: Some(1.25),
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        };
        let result = project_cost(&req, "target", &pricing);
        // 300 non-cached + 300 cache-read + 400 cache-write + 100 output
        let expected = 300.0 * 1.0 / 1e6 // non-cached at input rate
            + 300.0 * 0.1 / 1e6 // cache-read at cached rate
            + 400.0 * 1.25 / 1e6 // cache-write at WRITE rate (not input rate)
            + 100.0 * 2.0 / 1e6; // output
        assert!(
            (result.cost_usd - expected).abs() < 1e-12,
            "three-bucket: got {} want {}",
            result.cost_usd,
            expected
        );
    }

    #[test]
    fn cache_write_falls_back_to_input_rate_without_write_tier() {
        // No cache_write_per_million → cache-write tokens price at the standard
        // input rate (backwards-compatible pre-C02 behavior).
        let mut req = sample_request(1000, 100, 0);
        req.cache_creation_input_tokens = Some(400);
        req.cache_read_input_tokens = Some(300);
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: Some(0.1),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        };
        let result = project_cost(&req, "target", &pricing);
        let expected = 300.0 * 1.0 / 1e6
            + 300.0 * 0.1 / 1e6
            + 400.0 * 1.0 / 1e6 // cache-write falls back to input rate
            + 100.0 * 2.0 / 1e6;
        assert!((result.cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn legacy_rows_without_cache_fields_keep_two_bucket_pricing() {
        // Legacy rows (cache_creation/read are None) use the prior
        // two-bucket model exactly.
        let req = sample_request(1000, 100, 400); // cached_tokens = 400
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: Some(0.1),
            cache_write_per_million: Some(1.25), // present but unused (legacy row)
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        };
        let result = project_cost(&req, "target", &pricing);
        let expected = 600.0 * 1.0 / 1e6 // non-cached at input rate
            + 400.0 * 0.1 / 1e6 // cached_tokens at cached rate
            + 100.0 * 2.0 / 1e6; // output
        assert!(
            (result.cost_usd - expected).abs() < 1e-12,
            "legacy: got {} want {}",
            result.cost_usd,
            expected
        );
    }

    #[test]
    fn cache_write_plus_read_never_exceeds_input_tokens() {
        // A malformed row where write+read > input_tokens is clamped to the
        // total, never a negative non-cached count.
        let mut req = sample_request(100, 0, 0);
        req.cache_creation_input_tokens = Some(80);
        req.cache_read_input_tokens = Some(50); // 80+50=130 > 100
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: Some(0.1),
            cache_write_per_million: Some(1.25),
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        };
        let result = project_cost(&req, "target", &pricing);
        // write clamped to 80, read clamped to min(50, 100-80=20) = 20.
        // non-cached = 100 - 80 - 20 = 0.
        let expected = 0.0 * 1.0 / 1e6 + 20.0 * 0.1 / 1e6 + 80.0 * 1.25 / 1e6 + 0.0 * 2.0 / 1e6;
        assert!(
            (result.cost_usd - expected).abs() < 1e-12,
            "clamped: got {} want {}",
            result.cost_usd,
            expected
        );
    }

    fn sample_request(input: u32, output: u32, cached: u32) -> RequestLog {
        RequestLog {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            ts: chrono::Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap(),
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet".into(),
            requested_model: None,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
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
            task_class: Default::default(),
            diff_saved_usd: None,
            minify_saved_est_usd: None,
        }
    }

    #[test]
    fn project_cost_with_full_pricing() {
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
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
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
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
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        };
        let req = sample_request(1_000_000, 0, 500_000);
        let p = project_cost(&req, "x", &pricing);
        // All input charged at the full rate -> $3.00.
        assert!((p.cost_usd - 3.0).abs() < 1e-9, "got {}", p.cost_usd);
    }

    /// `project_batch_cost` prices at the entry's REAL catalog batch rates
    /// (full input, no cache stacking) and returns `None` — never a fabricated
    /// 0.5× — when the entry carries no batch tier.
    #[test]
    fn project_batch_cost_uses_catalog_rates_or_none() {
        let with_batch = ModelPricing {
            input_per_million: 5.0,
            output_per_million: 30.0,
            cached_input_per_million: Some(0.5),
            cache_write_per_million: None,
            batch_input_per_million: Some(2.50),
            batch_output_per_million: Some(15.00),
            flex_input_per_million: None,
            flex_output_per_million: None,
        };
        let req = sample_request(1_000_000, 1_000_000, 0);
        let p = project_batch_cost(&req, &with_batch).expect("batch tier present");
        // 1M input @ $2.50 + 1M output @ $15.00 = $17.50.
        assert!((p.cost_usd - 17.50).abs() < 1e-9, "got {}", p.cost_usd);

        let without_batch = ModelPricing {
            batch_input_per_million: None,
            batch_output_per_million: None,
            ..with_batch
        };
        assert!(
            project_batch_cost(&req, &without_batch).is_none(),
            "no batch tier → None, never a fabricated discount"
        );
    }

    /// `project_flex_cost` prices at the entry's REAL catalog Flex rates (full
    /// input, no cache stacking) and returns `None` — never a fabricated 0.5×
    /// — when the entry carries no Flex tier. Mirrors
    /// `project_batch_cost_uses_catalog_rates_or_none`.
    #[test]
    fn project_flex_cost_uses_catalog_rates_or_none() {
        let with_flex = ModelPricing {
            input_per_million: 5.0,
            output_per_million: 30.0,
            cached_input_per_million: Some(0.5),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: Some(2.50),
            flex_output_per_million: Some(15.00),
        };
        let req = sample_request(1_000_000, 1_000_000, 0);
        let p = project_flex_cost(&req, &with_flex).expect("flex tier present");
        // 1M input @ $2.50 + 1M output @ $15.00 = $17.50.
        assert!((p.cost_usd - 17.50).abs() < 1e-9, "got {}", p.cost_usd);

        let without_flex = ModelPricing {
            flex_input_per_million: None,
            flex_output_per_million: None,
            ..with_flex
        };
        assert!(
            project_flex_cost(&req, &without_flex).is_none(),
            "no flex tier → None, never a fabricated discount"
        );
    }

    #[test]
    fn project_cost_clamps_cached_to_input() {
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.3),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
        };
        // cached_tokens > input_tokens — should clamp.
        let req = sample_request(1_000, 0, 5_000);
        let p = project_cost(&req, "x", &pricing);
        // All 1000 charged at cached rate ($0.30/1M).
        let want = 1_000.0 * 0.3 / 1_000_000.0;
        assert!((p.cost_usd - want).abs() < 1e-12, "got {}", p.cost_usd);
    }
}
