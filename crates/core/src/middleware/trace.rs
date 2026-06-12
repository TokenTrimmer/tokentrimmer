//! Request-span middleware: every request gets a trace_id, a tracing span
//! carrying it, and a response header `X-TokenTrimmer-Trace-Id`.
//!
//! ## W3C TraceContext ingest
//!
//! If the incoming request carries a valid W3C [`traceparent`] header, the
//! request span is made a **child** of that inbound trace: it inherits the
//! caller's `trace_id` and records the inbound span as its parent, so a
//! customer's existing distributed trace flows through the gateway ("cost on
//! your existing traces"). Any `tracestate` header is preserved. When no
//! `traceparent` is present the span starts a fresh root, exactly as before.
//!
//! The OTel-specific extraction lives in [`tt_telemetry::propagation`]; here we
//! wrap the request `HeaderMap` in an [`opentelemetry_http::HeaderExtractor`]
//! and, on a hit, attach the extracted context as the span's parent via
//! [`tracing_opentelemetry::OpenTelemetrySpanExt::set_parent`]. The parentage
//! is recorded on OpenTelemetry exports; the response-header `trace_id` below
//! remains the gateway's own request-correlation id (a UUID v7).
//!
//! Wire into Axum via [`axum::middleware::from_fn`]:
//!
//! ```rust,ignore
//! Router::new()
//!     /* …routes… */
//!     .layer(axum::middleware::from_fn(crate::middleware::trace::middleware))
//! ```
//!
//! [`traceparent`]: https://www.w3.org/TR/trace-context/

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use opentelemetry_http::HeaderExtractor;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

/// Response header injected into every HTTP response.
pub const TRACE_ID_HEADER: HeaderName = HeaderName::from_static("x-tokentrimmer-trace-id");

/// Request extension that makes the trace-id available to inner handlers
/// without requiring them to parse the response header.
///
/// # Example
///
/// ```rust,ignore
/// async fn my_handler(Extension(trace): Extension<TraceId>) -> impl IntoResponse {
///     println!("handling request {}", trace.0);
/// }
/// ```
#[derive(Clone, Debug)]
pub struct TraceId(pub String);

/// Axum `from_fn`-compatible middleware function.
///
/// For every incoming request:
/// 1. Generates a UUID v7 trace-id.
/// 2. Attaches it as a [`TraceId`] request extension.
/// 3. Wraps the downstream handler in a `tracing` span carrying the id.
/// 4. If the request carries a valid W3C `traceparent`, makes that span a child
///    of the inbound trace (continue-the-trace); otherwise starts a fresh root.
/// 5. Inserts `X-TokenTrimmer-Trace-Id` into the response headers.
pub async fn middleware(mut req: Request, next: Next) -> Response {
    let trace_id = Uuid::now_v7().to_string();
    req.extensions_mut().insert(TraceId(trace_id.clone()));

    let span = tracing::info_span!(
        "http_request",
        trace_id = %trace_id,
        method = %req.method(),
        path = %req.uri().path(),
    );

    // Honour an inbound W3C `traceparent`: continue the caller's distributed
    // trace by parenting this span on the extracted remote context. No-op when
    // absent or malformed (fresh root). Reads headers only — does not consume
    // or strip them, so downstream handlers/providers are unaffected.
    //
    // `set_parent` returns `Err` when no `tracing-opentelemetry` layer is
    // attached (e.g. OTLP export disabled) — discard it to keep the pre-0.33
    // silent-no-op semantics; parenting correctness is covered by the
    // traceparent_ingest test, which runs with a layer attached.
    if let Some(parent_cx) =
        tt_telemetry::propagation::extract_remote_parent(&HeaderExtractor(req.headers()))
    {
        let _ = span.set_parent(parent_cx);
    }

    let mut response = next.run(req).instrument(span).await;

    if let Ok(value) = HeaderValue::from_str(&trace_id) {
        response.headers_mut().insert(TRACE_ID_HEADER, value);
    }

    response
}
