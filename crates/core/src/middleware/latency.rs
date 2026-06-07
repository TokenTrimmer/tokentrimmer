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
/// onto the response and emits HTTP RED metrics (request count + duration
/// histogram) labeled by method, matched-path, and status.
pub async fn middleware(req: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let method = req.method().as_str().to_owned();
    let endpoint = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());

    let mut response = next.run(req).await;

    let elapsed = started.elapsed();
    let status = response.status().as_u16().to_string();
    metrics::counter!(
        "http_requests_total",
        "method" => method.clone(),
        "endpoint" => endpoint.clone(),
        "status" => status,
    )
    .increment(1);
    metrics::histogram!(
        "http_request_duration_seconds",
        "method" => method,
        "endpoint" => endpoint,
    )
    .record(elapsed.as_secs_f64());

    let ms = elapsed.as_millis();
    if let Ok(value) = HeaderValue::from_str(&ms.to_string()) {
        response.headers_mut().insert(LATENCY_HEADER, value);
    }
    response
}
