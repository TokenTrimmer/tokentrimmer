//! `GET /metrics` — Prometheus text exposition for ops scraping.
//!
//! Unauthenticated (like `/health`): a Prometheus scraper sends no bearer
//! token, and the auth middleware passes tokenless requests through. Operators
//! should restrict this endpoint at the network / reverse-proxy layer.

use axum::http::{header::CONTENT_TYPE, StatusCode};
use axum::response::IntoResponse;

pub async fn handler() -> impl IntoResponse {
    metrics::gauge!("process_uptime_seconds").set(crate::metrics::uptime_seconds());
    match crate::metrics::render() {
        Some(body) => ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
