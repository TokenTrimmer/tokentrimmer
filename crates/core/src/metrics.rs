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
            .expect("global metrics recorder not already set");
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
