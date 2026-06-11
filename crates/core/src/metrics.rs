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
///   read absent but a write was reported — the provider clearly supports
///   caching and read nothing), or `"unreported"` (neither field reported).
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
    let result = match (cache_read, cache_creation) {
        (Some(r), _) if r > 0 => "hit",
        (Some(_), _) => "miss",
        (None, Some(_)) => "miss",
        (None, None) => "unreported",
    };
    metrics::counter!(
        "provider_cache_requests_total",
        "provider" => provider,
        "route" => route,
        "result" => result,
    )
    .increment(1);
}
