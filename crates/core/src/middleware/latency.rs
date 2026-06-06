//! Response-latency middleware: times every request and sets
//! `X-TokenTrimmer-Latency-Ms` on the response (present on every response —
//! success, cache hit, sandbox, or error). Mirrors [`super::trace`].

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};

/// Response header set on every HTTP response.
pub const LATENCY_HEADER: HeaderName = HeaderName::from_static("x-tokentrimmer-latency-ms");

/// Axum `from_fn`-compatible middleware: stamps wall-clock request latency (ms)
/// onto the response.
pub async fn middleware(req: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let mut response = next.run(req).await;
    let ms = started.elapsed().as_millis();
    if let Ok(value) = HeaderValue::from_str(&ms.to_string()) {
        response.headers_mut().insert(LATENCY_HEADER, value);
    }
    response
}
