//! Initialize tracing-subscriber. For v1 we ship a JSON formatter to stdout;
//! OTLP export is wired via the `tracing-opentelemetry` layer and activates
//! only when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (opt-in, prod-ready).
//!
//! ## Transport
//!
//! The OTLP exporter uses **HTTP/protobuf** (`application/x-protobuf`, the
//! OTLP spec default) over the existing `reqwest`/`rustls` stack, so the export
//! path reuses the gateway's existing rustls TLS stack rather than standing up
//! a second TLS stack for a gRPC/`tonic` transport. (`tonic`/`prost` are still
//! pulled in transitively by `opentelemetry-proto` for the generated wire
//! types; this is not a tonic-free build.) Spans are exported in the background
//! via a batch span processor (tokio runtime).
//!
//! ## Scope: export + inbound ingest (not outbound propagation)
//!
//! This module exports *locally generated* spans over OTLP. It does **not**
//! install a global [`TextMapPropagator`], so this module never *emits* a
//! `traceparent` on outbound requests.
//!
//! Inbound W3C `traceparent` **ingest** is wired separately, at the gateway's
//! request-entry middleware: it extracts the inbound trace context via
//! [`crate::propagation::extract_remote_parent`] (a [`TraceContextPropagator`])
//! and parents the request span on the caller's trace, so an existing
//! distributed trace flows through the gateway. When no `traceparent` is
//! present the middleware mints a fresh root with its own trace-id, as before.
//!
//! [`TextMapPropagator`]: opentelemetry::propagation::TextMapPropagator
//! [`TraceContextPropagator`]: opentelemetry_sdk::propagation::TraceContextPropagator

use std::env;
use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::{Config, TracerProvider};
use opentelemetry_sdk::{runtime, Resource};
use thiserror::Error;
use tracing_subscriber::{filter::EnvFilter, prelude::*};

/// Env var that, when set, activates the OTLP span exporter.
const OTLP_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Errors that can occur during tracing initialisation.
#[derive(Debug, Error)]
pub enum TracingError {
    /// The tracing subscriber could not be initialised.
    #[error("tracing init failed: {0}")]
    Init(String),
    /// The OTLP exporter could not be built (bad endpoint, transport error).
    #[error("OTLP exporter init failed: {0}")]
    Otlp(String),
}

/// Opaque guard — drop on shutdown to flush the exporter.
///
/// Holding this value alive for the lifetime of the process ensures that any
/// buffered spans are flushed before the program exits. When OTLP is not
/// configured the guard holds nothing and dropping it is a no-op.
pub struct TracingGuard {
    /// The OTLP `TracerProvider`, kept alive so its batch processor keeps
    /// exporting. `None` in local/dev mode (no OTLP endpoint configured).
    provider: Option<TracerProvider>,
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            shutdown_provider(provider);
        }
    }
}

/// Flush + shut down the OTLP batch processor without blocking the caller's
/// thread.
///
/// `TracerProvider::shutdown` blocks the calling thread on
/// `futures_executor::block_on` while it waits for the background batch worker
/// (running on the tokio runtime) to acknowledge. If that blocking call runs on
/// a tokio runtime **worker** thread it can deadlock — the worker is busy
/// blocking and so cannot also drive the export future. Since the guard is
/// commonly dropped from inside `#[tokio::main]` (a runtime thread), we move the
/// blocking shutdown onto a dedicated OS thread and join it with a bounded
/// timeout so `Drop` can never hang the process.
fn shutdown_provider(provider: TracerProvider) {
    let handle = std::thread::spawn(move || {
        // Best-effort: errors here are non-fatal (we're shutting down anyway).
        if let Err(err) = provider.shutdown() {
            tracing::warn!(error = %err, "OTLP tracer provider shutdown failed");
        }
    });
    // Give the flush a brief window. If the collector is unreachable the export
    // future will error out quickly; we don't want to wedge process exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handle.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() {
        let _ = handle.join();
    } else {
        // Detach: the OS thread will finish (or be reaped at process exit). We
        // log rather than block exit indefinitely.
        tracing::warn!("OTLP tracer provider shutdown timed out; detaching flush thread");
    }
}

/// Build an OTLP `TracerProvider` exporting over HTTP/protobuf to `endpoint`.
///
/// Spans are flushed in the background by a batch processor on the tokio
/// runtime, so this must be called from within a tokio runtime (the gateway's
/// `#[tokio::main]`). Returns an error if the exporter cannot be constructed
/// (e.g. a malformed endpoint URL).
///
/// This does **not** require the collector to be reachable at construction
/// time — exporting is asynchronous and failures are logged, not fatal.
fn build_otlp_provider(endpoint: &str, service_name: &str) -> Result<TracerProvider, TracingError> {
    let exporter = opentelemetry_otlp::new_exporter()
        .http()
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(3))
        .build_span_exporter()
        .map_err(|e| TracingError::Otlp(e.to_string()))?;

    let resource = Resource::new(vec![KeyValue::new("service.name", service_name.to_owned())]);

    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, runtime::Tokio)
        .with_config(Config::default().with_resource(resource))
        .build();

    Ok(provider)
}

/// Initialize the global tracing subscriber for the given service name.
///
/// Idempotent — safe to call once in `main`. Subsequent calls (e.g., in
/// tests) are silently a no-op when the global default is already set.
///
/// # Environment variables
///
/// * `RUST_LOG` — controls the [`EnvFilter`]. Defaults to
///   `info,tt_core=debug,tt_provider_openai=debug`.
/// * `OTEL_EXPORTER_OTLP_ENDPOINT` — when set, activates an OTLP span exporter
///   (HTTP/protobuf) targeting that endpoint and installs it as a
///   `tracing-opentelemetry` span layer. When unset, only the JSON stdout
///   formatter runs (local/dev default).
pub fn init(service_name: &str) -> Result<TracingGuard, TracingError> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tt_core=debug,tt_provider_openai=debug"));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_target(true)
        .with_thread_ids(false)
        .with_current_span(true)
        .with_span_list(false);

    // OTLP exporter wires in only when OTEL_EXPORTER_OTLP_ENDPOINT is set —
    // keeps dev quiet, prod-ready.
    let provider = match env::var(OTLP_ENDPOINT_ENV) {
        Ok(endpoint) if !endpoint.trim().is_empty() => {
            Some(build_otlp_provider(endpoint.trim(), service_name)?)
        }
        _ => None,
    };

    // The OTLP layer (if any) bridges `tracing` spans into OpenTelemetry. We
    // build the registry with or without it. The `Option<Layer>` is itself a
    // `Layer`, so a `None` cleanly contributes nothing.
    let otel_layer = provider.as_ref().map(|p| {
        let tracer = p.tracer(service_name.to_owned());
        tracing_opentelemetry::layer().with_tracer(tracer)
    });

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer);

    // `try_init` returns Err if a global subscriber was already installed. We
    // treat that as success because it makes tests easier (cargo test runs many
    // tests sharing a process).
    let _ = registry.try_init();

    if let Some(ref p) = provider {
        // Register the provider globally so direct `opentelemetry::global`
        // tracer lookups resolve to it. NOTE: this does not install a *global*
        // `TextMapPropagator`, so we never emit `traceparent` on outbound
        // requests. Inbound `traceparent` ingest is handled explicitly at the
        // gateway request middleware via `crate::propagation` (see the
        // module-level "Scope" note).
        opentelemetry::global::set_tracer_provider(p.clone());
        tracing::info!(service = service_name, "OTLP span exporter active");
    }

    tracing::info!(service = service_name, "tracing initialized");
    Ok(TracingGuard { provider })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The OTLP provider constructs + installs (batch processor spawns) without
    // panicking against a dummy, never-reached collector endpoint. No network
    // is performed at construction time — export is asynchronous — so this is
    // hermetic.
    //
    // Multi-thread flavour so the batch worker (spawned on `runtime::Tokio`)
    // can run on a separate worker thread while `shutdown_provider`'s blocking
    // flush waits — mirroring the gateway's production runtime and letting the
    // unreachable-collector export error out promptly instead of timing out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_otlp_provider_constructs_against_dummy_endpoint() {
        let provider = build_otlp_provider("http://127.0.0.1:4318", "tt-test")
            .expect("OTLP provider should build against a dummy endpoint");

        // The provider yields a tracer (the layer's tracer source). Producing
        // one must not panic even though the collector is unreachable.
        let _tracer = provider.tracer("tt-test");

        // Shut down via the off-thread helper so the blocking flush does not
        // deadlock this tokio worker thread (see `shutdown_provider`). Must
        // return promptly even though nothing was ever exported.
        shutdown_provider(provider);
    }

    /// A malformed endpoint surfaces as a `TracingError::Otlp`, not a panic.
    /// The exporter builder validates the endpoint URL eagerly, so a string with
    /// embedded spaces (`"ht tp://bad endpoint"`) deterministically fails to
    /// parse — we assert the error rather than swallowing it, so a regression
    /// that started silently accepting bad endpoints would be caught.
    #[tokio::test]
    async fn build_otlp_provider_rejects_malformed_endpoint() {
        let result = build_otlp_provider("ht tp://bad endpoint", "tt-test");
        assert!(
            matches!(result, Err(TracingError::Otlp(_))),
            "a malformed endpoint URL must surface as TracingError::Otlp, got {result:?}"
        );
    }

    /// `init` no-ops the exporter cleanly when the OTLP endpoint env is unset
    /// or empty: the returned guard carries no provider and dropping it is a
    /// no-op. Both env states are exercised in one test to avoid races on the
    /// process-global env var across parallel tests.
    #[tokio::test]
    async fn init_no_ops_exporter_when_endpoint_absent_or_empty() {
        // Unset -> no provider.
        std::env::remove_var(OTLP_ENDPOINT_ENV);
        let guard = init("tt-test-noop").expect("init should succeed with no OTLP endpoint");
        assert!(
            guard.provider.is_none(),
            "no OTLP provider should be installed when the endpoint env is unset"
        );
        drop(guard); // dropping the no-op guard must not panic

        // Whitespace-only -> treated as unset.
        std::env::set_var(OTLP_ENDPOINT_ENV, "   ");
        let guard = init("tt-test-empty").expect("init should succeed");
        assert!(
            guard.provider.is_none(),
            "an empty OTLP endpoint should not activate the exporter"
        );
        std::env::remove_var(OTLP_ENDPOINT_ENV);
    }
}
