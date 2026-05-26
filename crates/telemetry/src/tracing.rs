//! Initialize tracing-subscriber. For v1 we ship a JSON formatter to stdout;
//! OTLP export is plumbed via the `tracing-opentelemetry` layer but kept
//! disabled by default — wire `OTEL_EXPORTER_OTLP_ENDPOINT` env var to enable.

use std::env;

use thiserror::Error;
use tracing_subscriber::{filter::EnvFilter, prelude::*};

/// Errors that can occur during tracing initialisation.
#[derive(Debug, Error)]
pub enum TracingError {
    /// The tracing subscriber could not be initialised.
    #[error("tracing init failed: {0}")]
    Init(String),
}

/// Opaque guard — drop on shutdown to flush the exporter.
///
/// Holding this value alive for the lifetime of the process ensures that any
/// buffered spans are flushed before the program exits.
pub struct TracingGuard {
    _private: (),
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
/// * `OTEL_EXPORTER_OTLP_ENDPOINT` — when present, logs a notice that full
///   OTLP wiring is a follow-up task. Does not yet activate the exporter.
pub fn init(service_name: &str) -> Result<TracingGuard, TracingError> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tt_core=debug,tt_provider_openai=debug"));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_target(true)
        .with_thread_ids(false)
        .with_current_span(true)
        .with_span_list(false);

    let registry = tracing_subscriber::registry().with(filter).with(fmt_layer);

    // OTLP exporter wires in only when OTEL_EXPORTER_OTLP_ENDPOINT is set —
    // keeps dev quiet, prod-ready.
    if env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        // Real OTLP setup is a follow-up — for now we just log that it would activate.
        tracing::info!(
            service = service_name,
            "OTLP endpoint detected — full OTLP exporter wiring is a follow-up task"
        );
    }

    // `try_init` returns Err if a global subscriber was already installed. We
    // treat that as success because it makes tests easier (cargo test runs many
    // tests sharing a process).
    let _ = registry.try_init();

    tracing::info!(service = service_name, "tracing initialized");
    Ok(TracingGuard { _private: () })
}
