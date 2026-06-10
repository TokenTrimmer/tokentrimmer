//! W3C TraceContext (`traceparent` / `tracestate`) ingest.
//!
//! This module extracts an inbound [W3C TraceContext] from an HTTP request's
//! headers so the gateway's request span can *continue* a caller's existing
//! distributed trace rather than always minting a fresh root. It uses the
//! standard OpenTelemetry [`TraceContextPropagator`], so any caller that emits
//! a spec-compliant `traceparent` header (and optional `tracestate`) flows
//! through the gateway as a single trace — "cost on your existing traces".
//!
//! The extraction is intentionally a pure function over an
//! [`Extractor`]: the gateway middleware
//! wraps the request's `HeaderMap` in an extractor and calls
//! [`extract_remote_parent`]. Keeping the OTel-specific logic here (the crate
//! that already owns the propagator + exporter) lets it be unit-tested
//! hermetically with an in-memory carrier, with no HTTP or network involved.
//!
//! When no `traceparent` is present (or it is malformed), [`extract_remote_parent`]
//! returns `None` and the caller starts a new root span exactly as before.
//!
//! [W3C TraceContext]: https://www.w3.org/TR/trace-context/

use opentelemetry::propagation::{Extractor, TextMapPropagator};
use opentelemetry::trace::TraceContextExt;
use opentelemetry::Context;
use opentelemetry_sdk::propagation::TraceContextPropagator;

/// Extract an inbound trace context from a header carrier and, if it carries a
/// valid remote parent, return an OpenTelemetry [`Context`] whose active span is
/// that remote parent.
///
/// Returns `Some(cx)` only when the carrier contained a **valid** W3C
/// `traceparent` (i.e. a non-zero trace-id and span-id). The returned context
/// is suitable to pass to
/// [`tracing_opentelemetry::OpenTelemetrySpanExt::set_parent`], which makes the
/// gateway's request span a child of the caller's trace — inheriting its
/// `trace_id` and recording the inbound span as the parent. Any `tracestate`
/// header carried alongside `traceparent` is preserved on the returned context.
///
/// Returns `None` when:
/// * no `traceparent` header is present, or
/// * the `traceparent` is malformed / unsupported (the propagator yields an
///   invalid span context).
///
/// In the `None` case the caller should start a fresh root span as it does
/// today — this function never fabricates a parent.
pub fn extract_remote_parent(carrier: &dyn Extractor) -> Option<Context> {
    let propagator = TraceContextPropagator::new();
    let cx = propagator.extract(carrier);

    // `extract` returns the *input* (empty) context unchanged when the carrier
    // has no usable `traceparent`; only a successful extraction installs a
    // remote span context. Gate on validity so a missing/garbage header cleanly
    // yields a fresh root rather than a zero-valued (invalid) parent.
    if cx.span().span_context().is_valid() {
        Some(cx)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // A spec example traceparent (W3C TraceContext §3.2.1):
    //   version=00, trace-id, parent-id (span-id), trace-flags=01 (sampled).
    const TRACE_ID_HEX: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
    const PARENT_ID_HEX: &str = "00f067aa0ba902b7";
    const VALID_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    fn carrier_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A valid inbound `traceparent` is extracted: the returned context's span
    /// carries the inbound trace-id and the inbound span-id (which becomes the
    /// gateway request span's parent), and is marked remote.
    #[test]
    fn extracts_valid_traceparent_as_remote_parent() {
        let carrier = carrier_with(&[("traceparent", VALID_TRACEPARENT)]);

        let cx =
            extract_remote_parent(&carrier).expect("valid traceparent should extract a parent");
        let span = cx.span();
        let sc = span.span_context();

        assert!(sc.is_valid(), "extracted span context must be valid");
        assert!(
            sc.is_remote(),
            "extracted parent must be flagged remote (came from inbound headers)"
        );
        assert_eq!(
            sc.trace_id().to_string(),
            TRACE_ID_HEX,
            "trace_id must match the inbound traceparent"
        );
        assert_eq!(
            sc.span_id().to_string(),
            PARENT_ID_HEX,
            "span_id must be the inbound parent-id (the future request span's parent)"
        );
    }

    /// `tracestate` carried alongside a valid `traceparent` is preserved on the
    /// extracted context (so vendor trace state continues through the gateway).
    #[test]
    fn preserves_tracestate_alongside_traceparent() {
        let carrier = carrier_with(&[
            ("traceparent", VALID_TRACEPARENT),
            ("tracestate", "vendorname=opaque-value,other=42"),
        ]);

        let cx =
            extract_remote_parent(&carrier).expect("valid traceparent should extract a parent");
        let span = cx.span();
        let header = span.span_context().trace_state().header();
        assert!(
            header.contains("vendorname=opaque-value"),
            "tracestate should be preserved, got {header:?}"
        );
    }

    /// No `traceparent` header → no remote parent → caller starts a fresh root.
    #[test]
    fn no_traceparent_yields_no_parent() {
        let carrier = carrier_with(&[("x-unrelated", "value")]);
        assert!(
            extract_remote_parent(&carrier).is_none(),
            "absence of traceparent must not fabricate a parent"
        );
    }

    /// Empty carrier → no remote parent.
    #[test]
    fn empty_carrier_yields_no_parent() {
        let carrier = carrier_with(&[]);
        assert!(extract_remote_parent(&carrier).is_none());
    }

    /// A malformed `traceparent` (here: an all-zero / invalid trace-id) must not
    /// be honoured as a parent — the propagator yields an invalid span context.
    #[test]
    fn malformed_traceparent_yields_no_parent() {
        // All-zero trace-id is invalid per the spec.
        let carrier = carrier_with(&[(
            "traceparent",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
        )]);
        assert!(
            extract_remote_parent(&carrier).is_none(),
            "all-zero trace-id is invalid and must not be honoured"
        );

        // Structurally broken header.
        let carrier = carrier_with(&[("traceparent", "not-a-traceparent")]);
        assert!(extract_remote_parent(&carrier).is_none());
    }
}
