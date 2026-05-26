//! Request-span middleware: every request gets a trace_id, a tracing span
//! carrying it, and a response header `X-TokenTrimmer-Trace-Id`.
//!
//! Wire into Axum via [`axum::middleware::from_fn`]:
//!
//! ```rust,ignore
//! Router::new()
//!     /* …routes… */
//!     .layer(axum::middleware::from_fn(crate::middleware::trace::middleware))
//! ```

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use tracing::Instrument;
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
/// 4. Inserts `X-TokenTrimmer-Trace-Id` into the response headers.
pub async fn middleware(mut req: Request, next: Next) -> Response {
    let trace_id = Uuid::now_v7().to_string();
    req.extensions_mut().insert(TraceId(trace_id.clone()));

    let span = tracing::info_span!(
        "http_request",
        trace_id = %trace_id,
        method = %req.method(),
        path = %req.uri().path(),
    );

    let mut response = next.run(req).instrument(span).await;

    if let Ok(value) = HeaderValue::from_str(&trace_id) {
        response.headers_mut().insert(TRACE_ID_HEADER, value);
    }

    response
}
