//! Process-global Prometheus recorder + render helpers.
//!
//! The public crate is a library; the serving binary lives in the cloud repo.
//! So the recorder self-installs from `build_router` (idempotent via `OnceLock`)
//! and `/metrics` renders the shared handle. Emission elsewhere uses the
//! `metrics` facade macros, which are no-ops until `install()` has run.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use metrics_exporter_prometheus::PrometheusHandle;

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Install the global Prometheus recorder exactly once per process. Idempotent:
/// safe to call from every `build_router` invocation (including many per test
/// binary). The `OnceLock` guard guarantees the body — and therefore
/// `install_recorder()` / `set_global_recorder` — runs only once.
pub fn install() {
    HANDLE.get_or_init(|| {
        START.get_or_init(Instant::now);
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .set_buckets(&[
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ])
            .expect("static bucket list is valid")
            .install_recorder()
            .expect(
                "failed to install Prometheus recorder: a global metrics recorder was already set",
            );
        metrics::gauge!("tt_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);
        handle
    });
}

/// Render the current Prometheus text exposition, or `None` if not installed.
pub fn render() -> Option<String> {
    HANDLE.get().map(|h| h.render())
}

/// Seconds since the recorder was installed (for `process_uptime_seconds`).
pub fn uptime_seconds() -> f64 {
    START
        .get()
        .map(|s| s.elapsed().as_secs_f64())
        .unwrap_or(0.0)
}

/// Count one SERVED, billable response — incremented IN-BAND (synchronously, on
/// the request thread) on every response the gateway returns to the caller:
/// TokenTrimmer cache hits (L1/L2) AND dispatched completions, across the chat /
/// SSE / embeddings paths.
///
/// This is the cheap synchronous truth to diff against the ASYNC-written
/// `request_logs` count (the billing rows are spawned fire-and-forget). A
/// divergence — `tt_requests_served_total` climbing faster than the row count —
/// is the alertable signal that billing writes are being lost (transient DB
/// failure, an undrained SIGTERM, etc.). It is NOT itself a billing record.
///
/// `path` ∈ `chat|sse|embeddings|agent_run` (the served endpoint family); `result` ∈
/// `cache_hit|dispatch` (whether TokenTrimmer served from cache or dispatched
/// upstream). Both label sets are bounded, matching the discipline of the other
/// counters in this module.
pub fn record_request_served(path: &'static str, result: &'static str) {
    metrics::counter!("tt_requests_served_total", "path" => path, "result" => result).increment(1);
}

/// Count one PERMANENT `request_logs` write failure — a billing row that was
/// committed-as-served (`tt_requests_served_total`) but could NOT be persisted
/// even after the bounded retry in
/// [`tt_telemetry::request_logs::write_with_retry`]. This is silent
/// under-billing on the revenue substrate, so the counter is loud and meant to
/// be alerted on (it should sit at zero). A `tracing` error is emitted
/// alongside at the call site.
///
/// `path` ∈ `chat|sse` (the paths that persist a row; embeddings writes none
/// today). Bounded label.
pub fn record_request_log_write_failed(path: &'static str) {
    metrics::counter!("tt_request_log_write_failed_total", "path" => path).increment(1);
}

/// Record a provider-dispatch latency sample. DRY helper for the dispatch sites.
pub fn record_provider_latency(provider: &'static str, operation: &'static str, dur: Duration) {
    metrics::histogram!(
        "provider_request_duration_seconds",
        "provider" => provider,
        "operation" => operation,
    )
    .record(dur.as_secs_f64());
}

/// Count a provider dispatch that hit the request deadline (timed out). Kept
/// separate from `provider_request_duration_seconds` so right-censored-at-
/// deadline values don't skew the latency percentiles.
pub fn record_provider_timeout(provider: &'static str, operation: &'static str) {
    metrics::counter!(
        "provider_timeouts_total",
        "provider" => provider,
        "operation" => operation,
    )
    .increment(1);
}

/// Record provider prompt-cache usage counters once authoritative provider
/// usage lands (research Phase 0.2). `route` is the matched route NAME
/// (bounded cardinality), `"none"` when no route matched.
///
/// Emits:
/// - `provider_cache_read_tokens_total{provider,route}` += read when reported,
/// - `provider_cache_write_tokens_total{provider,route}` += write when reported,
/// - `provider_cache_requests_total{provider,route,result}` += 1, where
///   `result` is `"hit"` (reported read > 0), `"miss"` (reported read == 0, or
///   read absent but a NONZERO write was reported — the provider clearly
///   supports caching and read nothing), or `"unreported"` (neither field
///   reported, or only a zero write — a reported write of 0 with no read
///   field is too weak to assert a miss, so it must not deflate the route's
///   hit rate).
///
/// Owned-String labels follow the `catalog_zero_price_total` precedent.
pub fn record_provider_cache_usage(
    provider: &str,
    route: Option<&str>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
) {
    let provider = provider.to_string();
    let route = route.unwrap_or("none").to_string();
    if let Some(read) = cache_read {
        metrics::counter!(
            "provider_cache_read_tokens_total",
            "provider" => provider.clone(),
            "route" => route.clone(),
        )
        .increment(read);
    }
    if let Some(write) = cache_creation {
        metrics::counter!(
            "provider_cache_write_tokens_total",
            "provider" => provider.clone(),
            "route" => route.clone(),
        )
        .increment(write);
    }
    metrics::counter!(
        "provider_cache_requests_total",
        "provider" => provider,
        "route" => route,
        "result" => cache_result(cache_read, cache_creation),
    )
    .increment(1);
}

/// Classify a request's provider prompt-cache outcome for the
/// `provider_cache_requests_total{result}` label. See
/// [`record_provider_cache_usage`] for the semantics.
fn cache_result(cache_read: Option<u64>, cache_creation: Option<u64>) -> &'static str {
    match (cache_read, cache_creation) {
        (Some(r), _) if r > 0 => "hit",
        (Some(_), _) => "miss",
        (None, Some(w)) if w > 0 => "miss",
        _ => "unreported",
    }
}

/// Count a request pass discarded by the token-true gate (its transform ADDED
/// tokens and was rolled back, booking zero savings). Labelled by pass name so
/// a misbehaving pass is attributable.
pub fn record_request_pass_rejected(pass: &'static str) {
    metrics::counter!("request_pass_rejected_total", "pass" => pass).increment(1);
}

/// Count one request that matched a PAUSED route and was passed through on
/// its originally-requested model (rewrite suppressed — the fail-safe
/// expensive direction). `route` is the matched route NAME (bounded
/// cardinality); owned-String label per the `record_provider_cache_usage`
/// precedent.
pub fn record_route_paused_passthrough(route: &str) {
    metrics::counter!("route_paused_passthrough_total", "route" => route.to_string()).increment(1);
}

/// Count one auto-pause TRIGGER: the quality evaluator
/// (`crate::route_autopause::AutoPauseJudgeSink`) sticky-paused a route whose
/// windowed paired pass-rate fell below its floor. One increment per newly
/// created pause (a raced `Ok(false)` does not count). `route` is the route
/// NAME (bounded cardinality), owned-String label as above.
pub fn record_route_auto_paused(route: &str) {
    metrics::counter!("route_auto_paused_total", "route" => route.to_string()).increment(1);
}

/// Convert a non-negative USD amount to integer micro-USD for a true
/// Prometheus counter (the `metrics` counter API is integer-only; a gauge
/// "used as a counter" breaks `rate()`/`increase()` across process restarts).
fn usd_to_microusd(usd: f64) -> u64 {
    if usd.is_finite() && usd > 0.0 {
        (usd * 1_000_000.0).round() as u64
    } else {
        0
    }
}

/// Record a deliberate stable-prefix mutation (a booked
/// [`CacheBustEstimate`](crate::passes::CacheBustEstimate)): one count per
/// bust by source, plus the estimated penalty accumulated as integer
/// micro-USD into a true counter (divide by 1e6 in dashboards).
///
/// Basis notes:
/// - the penalty here is **pre-fee** (provider-invoice basis); the
///   `x-tokentrimmer-cache-bust-usd` header / `CostBreakdown` figure is
///   fee-applied — the two differ by the provider fee multiplier.
/// - this meters in the PASS stage, BEFORE the L1/L2 cache lookups: a request
///   later served from TokenTrimmer's own cache (no upstream dispatch) still
///   counts here while its response header reads 0. Series consumers should
///   treat these as "busts computed at ingress", not "busts dispatched".
pub fn record_cache_bust(source: &'static str, penalty_usd: f64) {
    metrics::counter!("cache_bust_total", "source" => source).increment(1);
    metrics::counter!("cache_bust_penalty_microusd_total").increment(usd_to_microusd(penalty_usd));
}

/// Record a cache-classifier finding: a volatile marker (timestamp / uuid /
/// hex-token) inside a would-be-stable cached prefix. One count per kind, plus
/// the estimated per-request waste accumulated as integer micro-USD into a
/// true counter (the caller books the waste exactly once per request — pass
/// 0.0 for additional kinds on the same request). Monthly cost = a 30d
/// aggregation over the waste series in the dashboard (÷ 1e6 for USD); the
/// handler cannot know per-route volume. Same basis caveats as
/// [`record_cache_bust`]: pre-fee, metered before the L1/L2 lookups.
pub fn record_cache_dynamic_prefix(kind: &'static str, est_wasted_usd: f64) {
    metrics::counter!("cache_dynamic_prefix_total", "kind" => kind).increment(1);
    metrics::counter!("cache_dynamic_prefix_waste_microusd_total")
        .increment(usd_to_microusd(est_wasted_usd));
}

/// Record one minified-JSON steering event (`RouteAction::minify_json`): the
/// instruction was injected into the dispatched request. `route` is the
/// matched route NAME (bounded cardinality; `"none"` never occurs in practice
/// — the action is route-gated — but the label is total for safety).
///
/// Emits:
/// - `output_minified_total{route}` += 1 (every injection, streaming included),
/// - `minify_saved_tokens_total{route}` += the per-response tokenizer-grounded
///   ESTIMATE (0 for streaming v1 / non-JSON responses),
/// - `minify_saved_est_microusd_total{route}` += the estimate priced at the
///   billed output rate, FEE-APPLIED (the same basis as the
///   `x-tokentrimmer-minify-saved-est-usd` header / request_logs column —
///   unlike `record_cache_bust`, which meters pre-fee), as integer micro-USD
///   (divide by 1e6 in dashboards; the `record_cache_bust` counter pattern).
///
/// These series are ESTIMATES — dashboards must label them as such and never
/// fold them into the invoice-reconciled saved-usd headline.
pub fn record_minify_estimate(route: &str, est_tokens: u32, est_usd: f64) {
    let route = route.to_string();
    metrics::counter!("output_minified_total", "route" => route.clone()).increment(1);
    metrics::counter!("minify_saved_tokens_total", "route" => route.clone())
        .increment(u64::from(est_tokens));
    metrics::counter!("minify_saved_est_microusd_total", "route" => route)
        .increment(usd_to_microusd(est_usd));
}

/// Record one applied reasoning cap (`RouteAction::reasoning_max_effort` /
/// `reasoning_budget_tokens`): `reasoning_capped_total{route,lever,cap}` += 1.
/// `lever` ∈ {`reasoning_effort`, `thinking_budget`}; `cap` is "low"/"medium"
/// or the configured budget as a string — bounded cardinality (one value per
/// route config). NO savings are booked anywhere for this event — the counter
/// plus the #163 netted route savings are the truth channel.
pub fn record_reasoning_capped(route: &str, lever: &'static str, cap: &str) {
    metrics::counter!(
        "reasoning_capped_total",
        "route" => route.to_string(),
        "lever" => lever,
        "cap" => cap.to_string(),
    )
    .increment(1);
}

/// Record one reasoning-cap refusal: `reasoning_cap_skipped_total{reason}` += 1.
/// `reason` is the refusal KIND only (`class` / `unknown_effort` /
/// `not_reasoning` / `unsupported`) — the warnings-header token carries the
/// unbounded detail (class name / model / provider), the metric label stays
/// bounded.
pub fn record_reasoning_cap_skipped(reason: &'static str) {
    metrics::counter!("reasoning_cap_skipped_total", "reason" => reason).increment(1);
}

/// Record a format-switch outcome (research Phase 3.3). `format` ∈
/// `csv|bare`; `outcome` ∈ `applied|failed` (a failed strip-validation fails
/// OPEN — the untouched body is served). Bounded label sets.
pub fn record_format_switch(format: &'static str, outcome: &'static str) {
    metrics::counter!("tt_format_switch_total", "format" => format, "outcome" => outcome)
        .increment(1);
}

/// Count a format-switch request that NO-OP'd at the eligibility gate.
/// `reason` is the bounded skip set (`unknown_format|streaming|tools|n|
/// no_schema|strict_schema|nested_schema|not_single_value|conflict`).
pub fn record_format_switch_skip(reason: &'static str) {
    metrics::counter!("tt_format_switch_skipped_total", "reason" => reason).increment(1);
}

/// Count a VALIDATED format switch whose JSON-equivalent reconstruction was
/// not computable — the saving is booked $0 (never invented) and this counter
/// is the only trace of the unestimated win.
pub fn record_format_switch_unestimated() {
    metrics::counter!("tt_format_switch_unestimated_total").increment(1);
}

/// Record a diff-lane outcome (research Phase 3.4). `outcome` ∈
/// `applied|failed|degraded|skipped`; `reason` carries the bounded skip
/// reason / `DiffError::reason()` / re-emit error class (empty for
/// `applied`). Bounded label sets.
pub fn record_diff(outcome: &'static str, reason: &'static str) {
    metrics::counter!("tt_diff_total", "outcome" => outcome, "reason" => reason).increment(1);
}

/// Count one Fusion panel request. `strategy` is the panel strategy
/// name (bounded cardinality — e.g. `"consensus"`, `"best_of"`); `outcome` ∈
/// `success|quorum_unmet|disabled|strategy_unsupported|error` (bounded).
pub fn record_panel_request(strategy: &str, outcome: &str) {
    metrics::counter!(
        "tt_panel_requests_total",
        "strategy" => strategy.to_string(),
        "outcome" => outcome.to_string(),
    )
    .increment(1);
}

/// Count one Fusion panel leg dispatch. `role` ∈ `primary|secondary`
/// or an ordinal index string (bounded by the panel size, small); `status` ∈
/// `success|error|timeout` (bounded).
pub fn record_panel_leg(role: &str, status: &str) {
    metrics::counter!(
        "tt_panel_legs_total",
        "role" => role.to_string(),
        "status" => status.to_string(),
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::cache_result;

    /// Pins the `result` label semantics the cloud dashboard's hit-rate
    /// denominator depends on. In particular: a reported write of ZERO with
    /// no read field is "unreported", not "miss" — weak evidence must not
    /// deflate a route's hit rate.
    #[test]
    fn cache_result_classification() {
        assert_eq!(cache_result(Some(80), Some(20)), "hit");
        assert_eq!(cache_result(Some(80), None), "hit");
        assert_eq!(cache_result(Some(0), Some(20)), "miss");
        assert_eq!(cache_result(Some(0), None), "miss");
        assert_eq!(cache_result(None, Some(20)), "miss");
        assert_eq!(cache_result(None, Some(0)), "unreported");
        assert_eq!(cache_result(None, None), "unreported");
    }

    /// Smoke: `record_panel_request` and `record_panel_leg` do not panic under
    /// the no-op `metrics` facade (no recorder installed in unit tests).
    #[test]
    fn panel_metrics_helpers_do_not_panic() {
        super::record_panel_request("consensus", "success");
        super::record_panel_request("best_of", "quorum_unmet");
        super::record_panel_leg("primary", "success");
        super::record_panel_leg("secondary", "error");
    }
}
