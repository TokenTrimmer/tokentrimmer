# Gateway `/metrics` Prometheus endpoint — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** Audit-remediation Wave 3 (public repo, `crates/core`). Closes `gw-metrics-endpoint` — the platform's biggest observability gap. The API ref and architecture spec advertise `GET /metrics` (Prometheus) but no such route exists and there is zero metrics instrumentation; an operator scraping `/metrics` gets a 404.

## Goal

Expose a real `GET /metrics` Prometheus exposition endpoint and instrument the gateway's hot paths so operators get request RED metrics (Rate/Errors/Duration) plus the domain signals the docs promise: cache hit/miss, provider failover, per-provider latency, and pricing-catalog misses.

## Key facts (verified)

- **Public repo is a library**: it exports `build_router` / `build_router_with_retrieval` (`crates/core/src/lib.rs:28`, defined in `server.rs`). The serving binary (`tt-api`) lives in the cloud repo. So metrics must self-install from `build_router` — no cloud change required.
- **No metrics infra today**: no `metrics`/`prometheus`/`metrics-exporter-prometheus` dep anywhere; zero counters/histograms. The doc's "telemetry crate already maintains the underlying counters" (API ref §17) is false.
- **RED seam is free**: `crates/core/src/middleware/latency.rs::middleware` already times every request (`Instant::now()` → `elapsed()`), is wired at `server.rs:88`, and sees the response status. It currently consumes `req` into `next.run(req)`, so method + matched path must be read *before* that call.
- **Auth is path-allowlist-free**: `crates/core/src/middleware/auth.rs::middleware` passes through any request with no bearer token (and not dogfood) straight to `next.run` — that is exactly how `/health` works without auth. A Prometheus scraper sends no bearer, so **`/metrics` needs no auth change**.
- Instrumentation sites (all in `crates/core`): cache `chat.rs:1035` (L1) / `chat.rs:1072` (L2); failover continue-branch `failover.rs:216-223`; provider dispatch `chat.rs:1199` (non-stream), `chat.rs:847` (stream), `failover.rs:207`, `embeddings.rs:227`; pricing miss `chat.rs:1300` (`if pricing.is_none()`).
- `AppState` (`state.rs`) is `Clone` with a `new(registry)` + `with_*` builder. The metrics handle is a **process-global** (`OnceLock`), so **no `AppState` field is added** — avoids churn to every test constructor and matches the `metrics` facade's global-recorder model.

## Architecture (`crates/core`)

### Dependencies
Add to root `[workspace.dependencies]` and to `crates/core/Cargo.toml` (`workspace = true`):
- `metrics` (facade — `counter!`/`histogram!`/`gauge!` macros at event sites)
- `metrics-exporter-prometheus` (global recorder + `PrometheusHandle::render()` text exposition)

Only `tt-core` depends on these (all event sites are in core). The implementer adds a known-compatible version pair and confirms with `cargo build -p tt-core`, bumping the pair together if resolution complains.

### New module `crates/core/src/metrics.rs`
```rust
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use metrics_exporter_prometheus::PrometheusHandle;

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Install the global Prometheus recorder exactly once per process. Idempotent:
/// safe to call from every `build_router` invocation (incl. many per test binary).
pub fn install() {
    HANDLE.get_or_init(|| {
        START.get_or_init(Instant::now);
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .set_buckets(&[
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ])
            .expect("static bucket list is valid")
            .install_recorder()
            .expect("global recorder not yet set");
        metrics::gauge!("tt_build_info", "version" => env!("CARGO_PKG_VERSION"))
            .set(1.0);
        handle
    });
}

/// Render the current Prometheus text exposition, or None if not installed.
pub fn render() -> Option<String> {
    HANDLE.get().map(|h| h.render())
}

/// Seconds since the recorder was installed (for `process_uptime_seconds`).
pub fn uptime_seconds() -> f64 {
    START.get().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0)
}

/// Record a provider-dispatch latency sample (DRY helper for the 4 sites).
pub fn record_provider_latency(provider: &'static str, operation: &'static str, dur: Duration) {
    metrics::histogram!(
        "provider_request_duration_seconds",
        "provider" => provider,
        "operation" => operation,
    )
    .record(dur.as_secs_f64());
}
```
Note: `install_recorder()` calls `set_global_recorder` once; the `OnceLock` guard guarantees the body runs once per process so the `.expect` cannot fire on a second call. If a non-default recorder were somehow pre-set, the panic is at startup (loud, correct for a misconfig).

### Route + handler
- Register `.route("/metrics", get(routes::metrics::handler))` beside `/health` in `build_router_with_retrieval`.
- `crates/core/src/routes/metrics.rs`:
```rust
use axum::http::{header::CONTENT_TYPE, StatusCode};
use axum::response::IntoResponse;

pub async fn handler() -> impl IntoResponse {
    metrics::gauge!("process_uptime_seconds").set(crate::metrics::uptime_seconds());
    match crate::metrics::render() {
        Some(body) => (
            [(CONTENT_TYPE, "text/plain; version=0.0.4")],
            body,
        )
            .into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
```
- Call `crate::metrics::install()` as the first statement of `build_router_with_retrieval` (and `build_router` delegates to it, so both install).

### Instrumentation (event sites)
1. **RED — `latency.rs`**: before `next.run`, capture `method` (`req.method().clone()` → `&str`) and `endpoint` from `req.extensions().get::<axum::extract::MatchedPath>().map(|m| m.as_str().to_owned()).unwrap_or_else(|| "unmatched".into())`. After `next.run`, read `response.status().as_u16()`. Emit:
   - `counter!("http_requests_total", "method"=>method, "endpoint"=>endpoint, "status"=>status_str).increment(1)`
   - `histogram!("http_request_duration_seconds", "method"=>method, "endpoint"=>endpoint).record(elapsed_secs)`
2. **Cache — `chat.rs`**: at the L1 branch (`:1035`) emit `counter!("cache_lookups_total","tier"=>"l1","result"=>"hit"|"miss").increment(1)` on the hit/miss arms; same at L2 (`:1072`) with `"tier"=>"l2"`.
3. **Failover — `failover.rs`** continue-branch (`:216-223`): `counter!("provider_failover_total","from"=>provider.id()).increment(1)` when a fallback-eligible error moves to the next candidate.
4. **Provider latency**: wrap the 4 dispatch sites — measure `Instant::now()` around the `with_retry(|| provider.<op>(...))` call (or the direct `await` for embeddings) and call `crate::metrics::record_provider_latency(provider.id(), "chat"|"chat_stream"|"embeddings", elapsed)`.
5. **Catalog miss — `chat.rs:1300`** (`if pricing.is_none()`): `counter!("catalog_zero_price_total","provider"=>provider.id(),"model"=>response.model.clone()).increment(1)` alongside the existing `tracing::warn!`.

Cardinality is bounded: `endpoint` uses the route template (not raw path); `model` is only reached for models that already resolved to a provider (registry-validated), so it is bounded by the catalog.

## Error handling
- No new fallible paths in the request flow — metric emission is infallible (facade no-ops if no recorder). `/metrics` returns `503` only if `install()` never ran (shouldn't happen via `build_router`).
- `set_buckets`/`install_recorder` errors surface at startup via `.expect` (a misconfiguration, not a runtime condition).

## Testing (`crates/core/tests/metrics_endpoint.rs`, integration)
The global recorder is shared across the whole test binary, so assert **presence**, not exact counts:
- `GET /metrics` → `200`, `content-type: text/plain; version=0.0.4`, body non-empty.
- After a `GET /health` through the router, `/metrics` body contains `http_requests_total` and an `endpoint="/health"` label.
- Body contains `tt_build_info` and `process_uptime_seconds`.
- A chat request through a test router (existing echo-provider harness pattern, e.g. as in `retrieval_isolation.rs`) causes `/metrics` to contain `cache_lookups_total` (cache configured) — assert the family name appears. (If wiring a cache into the test harness is heavy, assert this via the L1/L2 unit path instead; keep the endpoint test to RED + build_info presence.)
- `crate::metrics::render()` returns `Some(_)` after `build_router` was called.

Gates: `cargo test -p tt-core`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all -- --check`. (Per `ci-verify-all-targets`: a new dep + route touches workspace build — run the `--all-targets` clippy and `--workspace --no-run` before pushing.)

## Documentation
- API ref `docs/04-gateway-api-reference.md` §17: replace the "Planned … telemetry crate already maintains the underlying counters" note with an "implemented" section listing the exposed metric families and a one-line **operator note**: `/metrics` is unauthenticated (like `/health`); restrict it at the network/proxy layer.
- Architecture spec `docs/tokentrimmer-architecture-spec-v1.md` §17.2: mark the metric list live; keep the Grafana scraping guidance (now accurate).

## Decisions
1. **`/metrics` is auth-exempt** (matches `/health`; scrapers send no key) + a doc note to restrict at the network layer. Not token-gating it keeps Prometheus scrape config standard.
2. **`metrics` facade + `metrics-exporter-prometheus`** over the raw `prometheus` crate (one-line macros at event sites, swappable recorder, global model fits a library).
3. **Process-global handle (`OnceLock`)**, not an `AppState` field — no test-constructor churn; matches the facade's global recorder.

## Out of scope
- Per-org / per-tenant metric labels (cardinality + privacy — deferred).
- OpenTelemetry metrics export / push gateway (tracing-otel already exists separately).
- A Grafana dashboard definition / SLO alerting rules (the `post-scale-slo-proof` backlog item).
- Instrumenting crates outside `crates/core`.
- Token-protecting `/metrics` (network-layer restriction is the chosen control).
