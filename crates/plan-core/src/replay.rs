//! The deterministic replay loop. No async, no I/O, no clock reads — pure
//! function of the input. Determinism is the contract enforced by
//! `tests/replay.rs`.

use std::collections::HashMap;

use uuid::Uuid;

use crate::{
    bootstrap, cost,
    error::PlanError,
    routing,
    types::{
        Aggregates, ConfidenceIntervals, PerRouteBreakdown, PlanInput, PlanResult, ProposedRoute,
        RequestLog,
    },
};

/// The Plan replay entry point.
///
/// Pure function: same `(historical_rows, proposed_config, seed)` →
/// bit-identical [`PlanResult`]. Determinism is the contract; the CI
/// snapshot test in `tests/replay.rs` verifies it.
///
/// # Errors
///
/// Returns [`PlanError::InvalidWindow`] when `window_end <= window_start`
/// and [`PlanError::ZeroBootstrapIterations`] when the caller passes
/// `bootstrap_iterations = 0` (every CI would be `(0, 0)`, almost
/// certainly a mistake).
pub fn replay(input: PlanInput) -> Result<PlanResult, PlanError> {
    validate(&input)?;

    // Sort routes by priority descending — first match wins. Tie-break on
    // the route's `id` (ascending) so equal-priority routes have a stable,
    // config-intrinsic order independent of the caller's input array order.
    // Without this, two logically-identical configs that differ only in the
    // ordering of two equal-priority matching routes could resolve to
    // different winners and thus different projected savings — violating the
    // replay's "same config → bit-identical result" determinism contract.
    let mut routes = input.proposed_routes.clone();
    routes.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.id.cmp(&b.id)));

    // Walk requests in deterministic order (by id).
    let mut requests = input.requests.clone();
    requests.sort_by_key(|r| r.id);

    // Project L1 cache hits (exact-match) under the proposed TTL. A projected
    // hit serves the response for free, so its projected cost is zeroed in the
    // cost loop — otherwise a cache-adding diff would show $0 savings.
    // (L2 semantic-hit cost-zeroing is a follow-up: it needs a per-request hit
    // set at the chosen threshold, and L2 isn't wired in the live gateway yet.)
    let cache_hit_ids = crate::cache_projection::project_l1_hit_ids(&requests, &input.config);
    let projection = project_requests(&requests, &routes, &input.pricing, &cache_hit_ids);

    let mut aggregates = aggregate(&projection);

    // Apply L1 cache projection from the proposed PlanConfig (if any TTL is set).
    // This overrides the historical cache_hit_rate echo with the projected rate
    // under the proposed TTL window — the actual answer to "would this cache
    // config have helped?"
    if input.config.l1_ttl_seconds.is_some() {
        let proj = crate::cache_projection::project_l1_hits(&requests, &input.config);
        aggregates.cache_hit_rate_projected = proj.projected_l1_hit_rate;
    }

    // Apply L2 (semantic) projection when any request in the window carries
    // an embedding. The projection module short-circuits on missing
    // configuration (TTL `None`, empty sweep, no embeddings present) — the
    // outer guard here keeps the snapshot test stable on embedding-less
    // fixtures by ensuring the new fields stay at their `Default::default()`.
    if !requests.is_empty() && requests.iter().any(|r| r.embedding.is_some()) {
        let l2 = crate::l2_projection::project_l2_hits(&requests, &input.config);
        aggregates.l2_projections = l2.per_threshold;
        aggregates.l2_poisoning_candidates = l2.poisoning_candidates;
    }

    let confidence_intervals = compute_cis(&projection, input.seed, input.bootstrap_iterations);
    let per_route_breakdown = build_per_route(projection.per_route);

    // Carry the proposed routes through to the result so the apply path can
    // persist them. We move the *original* (unsorted) input vec rather than
    // the priority-sorted `routes` clone above — apply re-sorts at write time
    // and we want to preserve the caller's authored ordering for round-trip
    // fidelity. This is a partial move out of `input`; the remaining fields
    // read below (`plan_id`, `org_id`, the window bounds) are all `Copy`.
    let proposed_routes = input.proposed_routes;

    let mut caveats = build_caveats(
        requests.len(),
        aggregates.requests_unprice_able,
        projection.latency_unprojected,
        projection.would_block,
    );
    caveats.extend(wide_ci_caveats(&aggregates, &confidence_intervals));

    Ok(PlanResult {
        plan_id: input.plan_id,
        org_id: input.org_id,
        window_start: input.window_start,
        window_end: input.window_end,
        sample_size: requests.len() as u32,
        aggregates,
        confidence_intervals,
        per_route_breakdown,
        caveats,
        // Tier 3 quality scoring is opt-in and dispatched via
        // `replay_with_quality`; bare `replay()` returns `None` here so the
        // existing JSON snapshot stays byte-identical.
        quality: None,
        proposed_routes,
    })
}

/// Convenience helper that runs [`replay`] then attaches a Tier 3 quality
/// score via [`crate::quality::score_quality`]. The CLI / hosted worker
/// calls this when the org has body logging enabled and supplied a judge.
///
/// `proposed_response_for` is the caller-owned hook that re-runs the
/// proposed model for a given request id; see
/// [`crate::quality::score_quality`] for the contract.
///
/// # Errors
///
/// - Any error [`replay`] would return (validation failures).
/// - Any error [`crate::quality::score_quality`] would return — these are
///   *not* converted to [`PlanError`] because they're distinct conditions
///   (opt-in gate, budget, judge failure) the caller surfaces differently.
pub async fn replay_with_quality<F>(
    input: PlanInput,
    judge: &dyn crate::quality::JudgeProvider,
    quality_config: &crate::quality::QualityConfig,
    proposed_response_for: F,
) -> Result<PlanResult, ReplayWithQualityError>
where
    F: Fn(&Uuid) -> Option<String>,
{
    // Clone the requests slice up front so we can hand it to both the
    // sync replay and the async quality scorer without juggling ownership.
    let requests = input.requests.clone();
    let mut result = replay(input).map_err(ReplayWithQualityError::Replay)?;
    let quality =
        crate::quality::score_quality(&requests, quality_config, judge, proposed_response_for)
            .await
            .map_err(ReplayWithQualityError::Quality)?;
    result.quality = Some(quality);
    Ok(result)
}

/// Combined error envelope for [`replay_with_quality`]. Variants stay
/// distinct so callers can render appropriate UX (`PlanError` is a
/// validation/config failure; `QualityError` is a runtime / opt-in issue).
#[derive(Debug, thiserror::Error)]
pub enum ReplayWithQualityError {
    /// The deterministic replay stage failed (invalid window, etc.).
    #[error("replay: {0}")]
    Replay(#[from] crate::error::PlanError),
    /// The Tier 3 quality scoring stage failed (no opt-in, over budget,
    /// judge error, …).
    #[error("quality: {0}")]
    Quality(#[from] crate::quality::QualityError),
}

fn validate(input: &PlanInput) -> Result<(), PlanError> {
    if input.window_end <= input.window_start {
        return Err(PlanError::InvalidWindow {
            start: input.window_start.to_rfc3339(),
            end: input.window_end.to_rfc3339(),
        });
    }
    if input.bootstrap_iterations == 0 {
        return Err(PlanError::ZeroBootstrapIterations);
    }
    Ok(())
}

/// Per-route accumulator built during the projection pass.
struct PerRouteBucket {
    route_id: Uuid,
    route_name: String,
    matched: u32,
    baseline_cost_usd: f64,
    projected_cost_usd: f64,
}

/// All the per-request vectors plus the per-route buckets the aggregation
/// pass needs. Kept as a struct (not a tuple) so the field meanings are
/// obvious at call sites.
struct Projection {
    per_request_baseline: Vec<f64>,
    per_request_projected: Vec<f64>,
    per_request_latency: Vec<f64>,
    per_request_cache_hit: Vec<f64>,
    per_route: HashMap<Uuid, PerRouteBucket>,
    requests_rerouted: u32,
    requests_unchanged: u32,
    requests_unprice_able: u32,
    /// Rerouted requests whose target model had no latency history in the
    /// window — their latency is shown unchanged (can't be projected).
    latency_unprojected: u32,
    /// Requests a matched route's `max_cost_usd` ceiling would reject at runtime
    /// — projected unchanged (no fabricated savings) and surfaced as a caveat.
    would_block: u32,
}

fn project_requests(
    requests: &[RequestLog],
    routes: &[ProposedRoute],
    pricing: &crate::types::PricingTable,
    cache_hit_ids: &std::collections::HashSet<Uuid>,
) -> Projection {
    let cap = requests.len();
    let mut per_request_baseline = Vec::with_capacity(cap);
    let mut per_request_projected = Vec::with_capacity(cap);
    let mut per_request_latency = Vec::with_capacity(cap);
    let mut per_request_cache_hit = Vec::with_capacity(cap);
    let mut per_route: HashMap<Uuid, PerRouteBucket> = HashMap::new();
    let mut requests_rerouted: u32 = 0;
    let mut requests_unchanged: u32 = 0;
    let mut requests_unprice_able: u32 = 0;
    let mut latency_unprojected: u32 = 0;
    let mut would_block: u32 = 0;

    // Median latency per model across the window — used to project a rerouted
    // request's latency from its TARGET model's history rather than echoing the
    // original (baseline) model's latency.
    let model_medians = model_median_latencies(requests);

    // Map each priced model to its provider, derived from the pricing-table keys
    // ("{provider}:{model}"; provider ids never contain ':'). Built from SORTED
    // keys so the first-wins choice on a duplicate model id is deterministic (the
    // replay's bit-identical contract). Used to price a cross-provider route's
    // target by the target's OWN provider rather than the request's provider.
    let model_to_provider: HashMap<&str, &str> = {
        let mut keys: Vec<&str> = pricing.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut m: HashMap<&str, &str> = HashMap::new();
        for k in keys {
            if let Some((prov, model)) = k.split_once(':') {
                m.entry(model).or_insert(prov);
            }
        }
        m
    };

    for req in requests {
        per_request_baseline.push(req.baseline_cost_usd);
        per_request_cache_hit.push(if req.cached { 1.0 } else { 0.0 });

        // A projected cache hit serves the response for free regardless of
        // routing, so its projected cost is 0.
        let is_cache_hit = cache_hit_ids.contains(&req.id);

        let matched = routing::match_route(req, routes);
        match matched {
            Some(route) => {
                // Prefer the same-provider key (keeps same-provider replays
                // byte-identical); else resolve the target's own provider so a
                // cross-provider route is priced correctly.
                let same_provider_key =
                    crate::types::pricing_key(&req.provider, &route.then.target_model);
                let target_key = if pricing.contains_key(&same_provider_key) {
                    same_provider_key
                } else {
                    let target_provider = model_to_provider
                        .get(route.then.target_model.as_str())
                        .copied()
                        .unwrap_or(req.provider.as_str());
                    crate::types::pricing_key(target_provider, &route.then.target_model)
                };
                if let Some(p) = pricing.get(&target_key) {
                    let projected = cost::project_cost(req, &route.then.target_model, p);
                    let mut projected_cost = if is_cache_hit {
                        0.0
                    } else {
                        projected.cost_usd
                    };
                    // Per-request ceiling: a projected cost over max_cost_usd would
                    // be rejected at runtime — count it unchanged (never a saving)
                    // and surface a caveat. Cache hits are served for free and are
                    // never blocked.
                    if !is_cache_hit && route.then.max_cost_usd.is_some_and(|c| projected.cost_usd > c)
                    {
                        projected_cost = req.cost_usd;
                        would_block += 1;
                    }
                    per_request_projected.push(projected_cost);
                    // Project latency from the target model's window history;
                    // fall back to the request's own latency (and flag it) when
                    // the target model has no history to project from.
                    match model_medians.get(route.then.target_model.as_str()) {
                        Some(&med) => per_request_latency.push(med),
                        None => {
                            per_request_latency.push(f64::from(req.latency_ms));
                            latency_unprojected += 1;
                        }
                    }
                    let bucket = per_route.entry(route.id).or_insert_with(|| PerRouteBucket {
                        route_id: route.id,
                        route_name: route.name.clone(),
                        matched: 0,
                        baseline_cost_usd: 0.0,
                        projected_cost_usd: 0.0,
                    });
                    bucket.matched += 1;
                    bucket.baseline_cost_usd += req.baseline_cost_usd;
                    bucket.projected_cost_usd += projected_cost;
                    requests_rerouted += 1;
                } else {
                    // No pricing for the target model — count as unchanged.
                    // Conservative invariant: never fabricate savings.
                    per_request_projected.push(if is_cache_hit { 0.0 } else { req.cost_usd });
                    per_request_latency.push(f64::from(req.latency_ms));
                    requests_unprice_able += 1;
                }
            }
            None => {
                per_request_projected.push(if is_cache_hit { 0.0 } else { req.cost_usd });
                per_request_latency.push(f64::from(req.latency_ms));
                requests_unchanged += 1;
            }
        }
    }

    Projection {
        per_request_baseline,
        per_request_projected,
        per_request_latency,
        per_request_cache_hit,
        per_route,
        requests_rerouted,
        requests_unchanged,
        requests_unprice_able,
        latency_unprojected,
        would_block,
    }
}

/// Median latency (ms) per model across the window. Deterministic: sorts the
/// per-model latencies and takes the upper-middle element. Empty input → empty
/// map.
fn model_median_latencies(requests: &[RequestLog]) -> HashMap<&str, f64> {
    let mut by_model: HashMap<&str, Vec<u32>> = HashMap::new();
    for r in requests {
        by_model
            .entry(r.model.as_str())
            .or_default()
            .push(r.latency_ms);
    }
    by_model
        .into_iter()
        .map(|(model, mut lat)| {
            lat.sort_unstable();
            (model, f64::from(lat[lat.len() / 2]))
        })
        .collect()
}

fn aggregate(p: &Projection) -> Aggregates {
    let total_baseline: f64 = p.per_request_baseline.iter().sum();
    let total_projected: f64 = p.per_request_projected.iter().sum();
    let projected_savings = (total_baseline - total_projected).max(0.0);
    let projected_savings_pct = if total_baseline > 0.0 {
        projected_savings / total_baseline * 100.0
    } else {
        0.0
    };
    let cache_hit_rate = if p.per_request_cache_hit.is_empty() {
        0.0
    } else {
        p.per_request_cache_hit.iter().sum::<f64>() / p.per_request_cache_hit.len() as f64
    };
    let p50_latency = percentile(&p.per_request_latency, 0.50);
    let p95_latency = percentile(&p.per_request_latency, 0.95);

    Aggregates {
        total_baseline_cost_usd: total_baseline,
        total_projected_cost_usd: total_projected,
        projected_savings_usd: projected_savings,
        projected_savings_pct,
        cache_hit_rate_projected: cache_hit_rate,
        p50_latency_ms_projected: p50_latency,
        p95_latency_ms_projected: p95_latency,
        requests_rerouted: p.requests_rerouted,
        requests_unchanged: p.requests_unchanged,
        requests_unprice_able: p.requests_unprice_able,
        // L2 sweep + poisoning are populated downstream by `replay` when the
        // window carries embeddings; default to empty/zero here.
        l2_projections: Vec::new(),
        l2_poisoning_candidates: 0,
    }
}

fn compute_cis(p: &Projection, seed: u64, iterations: u32) -> ConfidenceIntervals {
    // Savings (USD): bootstrap the per-request savings delta, scale the
    // resampled MEAN back to a TOTAL by multiplying by the original n.
    // (Each resample has the same n as the original, so mean × n = total.)
    let n = p.per_request_baseline.len() as f64;
    let savings_per_req: Vec<f64> = p
        .per_request_baseline
        .iter()
        .zip(p.per_request_projected.iter())
        .map(|(b, pr)| (b - pr).max(0.0))
        .collect();
    let (sv_lo_mean, sv_hi_mean) =
        bootstrap::bootstrap_ci(&savings_per_req, seed, iterations, (0.025, 0.975));
    let savings_usd_95 = (sv_lo_mean * n, sv_hi_mean * n);

    // Savings pct: must bootstrap baseline + projected jointly because
    // pct = (sum_b - sum_p) / sum_b is a ratio of two sums.
    let savings_pct_95 = bootstrap_pct_savings_ci(
        &p.per_request_baseline,
        &p.per_request_projected,
        seed.wrapping_add(1),
        iterations,
    );

    // Cache hit rate: bootstrap the 0/1 hit vector — the mean of bools is
    // exactly the hit rate.
    let cache_hit_rate_95 = bootstrap::bootstrap_ci(
        &p.per_request_cache_hit,
        seed.wrapping_add(2),
        iterations,
        (0.025, 0.975),
    );

    // Latency percentile CIs: percentile-of-percentiles bootstrap — each
    // resample computes its own p50/p95, then we take the 2.5/97.5 of those.
    let p50_latency_ms_95 = bootstrap_percentile_ci(
        &p.per_request_latency,
        0.50,
        seed.wrapping_add(3),
        iterations,
    );
    let p95_latency_ms_95 = bootstrap_percentile_ci(
        &p.per_request_latency,
        0.95,
        seed.wrapping_add(4),
        iterations,
    );

    ConfidenceIntervals {
        savings_usd_95,
        savings_pct_95,
        cache_hit_rate_95,
        p50_latency_ms_95,
        p95_latency_ms_95,
    }
}

/// Bootstrap a CI on a quantile of `values`. Each iteration: resample with
/// replacement, compute the requested percentile on the resample, collect.
/// Return the 2.5/97.5 percentiles of those resampled-percentile values.
fn bootstrap_percentile_ci(values: &[f64], q: f64, seed: u64, iterations: u32) -> (f64, f64) {
    use rand::{Rng, SeedableRng};
    use rand_chacha::ChaCha8Rng;
    if values.is_empty() || iterations == 0 {
        return (0.0, 0.0);
    }
    let n = values.len();
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut samples: Vec<f64> = Vec::with_capacity(iterations as usize);
    let mut buf: Vec<f64> = Vec::with_capacity(n);
    for _ in 0..iterations {
        buf.clear();
        for _ in 0..n {
            buf.push(values[rng.gen_range(0..n)]);
        }
        samples.push(percentile(&buf, q));
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let lo_idx = (0.025 * iterations as f64) as usize;
    let hi_idx = ((0.975 * iterations as f64) as usize).min(iterations as usize - 1);
    (samples[lo_idx], samples[hi_idx])
}

fn build_per_route(buckets: HashMap<Uuid, PerRouteBucket>) -> Vec<PerRouteBreakdown> {
    let mut rows: Vec<PerRouteBreakdown> = buckets
        .into_values()
        .map(|b| PerRouteBreakdown {
            route_id: b.route_id,
            route_name: b.route_name,
            matched: b.matched,
            baseline_cost_usd: b.baseline_cost_usd,
            projected_cost_usd: b.projected_cost_usd,
            savings_usd: (b.baseline_cost_usd - b.projected_cost_usd).max(0.0),
        })
        .collect();
    // Sort by route_id for determinism — savings-desc is unstable on ties
    // and would break the snapshot test on slight float drift.
    rows.sort_by_key(|r| r.route_id);
    rows
}

fn build_caveats(
    sample_size: usize,
    requests_unprice_able: u32,
    latency_unprojected: u32,
    would_block: u32,
) -> Vec<String> {
    let mut caveats = Vec::new();
    if sample_size < 1000 {
        caveats.push(format!(
            "Small sample size ({sample_size} requests) — confidence intervals are wide."
        ));
    }
    if requests_unprice_able > 0 {
        caveats.push(format!(
            "{requests_unprice_able} request(s) routed to a target model with no pricing entry — counted as unchanged."
        ));
    }
    if latency_unprojected > 0 {
        caveats.push(format!(
            "{latency_unprojected} rerouted request(s) had no latency history for the target model — their latency is shown unchanged, not projected."
        ));
    }
    if would_block > 0 {
        caveats.push(format!(
            "{would_block} request(s) would be rejected by a max_cost_usd ceiling — counted unchanged, not as savings."
        ));
    }
    caveats
}

/// Relative CI width > 30% means the projection is too uncertain to act on.
/// Called from `replay()` after CIs are computed so the caveat reflects the
/// actual bootstrap result rather than just the sample size.
pub(crate) fn wide_ci_caveats(aggregates: &Aggregates, cis: &ConfidenceIntervals) -> Vec<String> {
    let mut out = Vec::new();
    let rel_width = |lo: f64, hi: f64, center: f64| -> Option<f64> {
        if center.abs() < f64::EPSILON {
            return None;
        }
        Some((hi - lo).abs() / center.abs())
    };
    if let Some(w) = rel_width(
        cis.savings_usd_95.0,
        cis.savings_usd_95.1,
        aggregates.projected_savings_usd,
    ) {
        if w > 0.30 {
            out.push(format!(
                "Savings CI is wide: ±{:.0}% relative width. Treat the headline savings number as a rough estimate; consider scanning a larger window.",
                w * 100.0
            ));
        }
    }
    if let Some(w) = rel_width(
        cis.p50_latency_ms_95.0,
        cis.p50_latency_ms_95.1,
        aggregates.p50_latency_ms_projected,
    ) {
        if w > 0.30 {
            out.push(format!(
                "p50 latency CI is wide: ±{:.0}% relative width.",
                w * 100.0
            ));
        }
    }
    out
}

fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((q * (v.len() as f64 - 1.0)).round() as usize).min(v.len() - 1);
    v[idx]
}

/// Bootstrap the percentage-savings CI by jointly resampling baseline and
/// projected costs. Distinct from `bootstrap_ci` because we need the
/// ratio of two sums, not the mean of a single sample.
fn bootstrap_pct_savings_ci(
    baseline: &[f64],
    projected: &[f64],
    seed: u64,
    iterations: u32,
) -> (f64, f64) {
    use rand::Rng;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    let n = baseline.len();
    if n == 0 || iterations == 0 || n != projected.len() {
        return (0.0, 0.0);
    }
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut pct_samples: Vec<f64> = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let mut b_sum = 0.0;
        let mut p_sum = 0.0;
        for _ in 0..n {
            let idx = rng.gen_range(0..n);
            b_sum += baseline[idx];
            p_sum += projected[idx];
        }
        let pct = if b_sum > 0.0 {
            (b_sum - p_sum) / b_sum * 100.0
        } else {
            0.0
        };
        pct_samples.push(pct.max(0.0));
    }
    pct_samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let iter_f = iterations as f64;
    let lo_idx = ((0.025 * iter_f) as usize).min(pct_samples.len() - 1);
    let hi_idx = ((0.975 * iter_f) as usize).min(pct_samples.len() - 1);
    (pct_samples[lo_idx], pct_samples[hi_idx])
}
