//! `GET /metrics` — Prometheus text exposition for ops scraping.
//!
//! Hosted deployments require a dedicated bearer token. Self-hosted/local
//! deployments may leave authentication disabled unless
//! `TT_REQUIRE_METRICS_AUTH` is enabled.

use axum::http::{
    header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE},
    HeaderMap, StatusCode,
};
use axum::response::{IntoResponse, Response};

const METRICS_TOKEN_ENV: &str = "TT_METRICS_TOKEN";
const REQUIRE_METRICS_AUTH_ENV: &str = "TT_REQUIRE_METRICS_AUTH";

pub async fn handler(headers: HeaderMap) -> Response {
    match configured_metrics_token() {
        Ok(Some(expected)) if !metrics_authorized(&headers, &expected) => {
            return (
                StatusCode::UNAUTHORIZED,
                [
                    (WWW_AUTHENTICATE, "Bearer realm=\"tokentrimmer-metrics\""),
                    (CACHE_CONTROL, "no-store"),
                ],
                "metrics authentication required",
            )
                .into_response();
        }
        Err(()) => {
            tracing::error!(
                "{METRICS_TOKEN_ENV} is missing while {REQUIRE_METRICS_AUTH_ENV} is enabled"
            );
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(CACHE_CONTROL, "no-store")],
                "metrics unavailable",
            )
                .into_response();
        }
        Ok(Some(_) | None) => {}
    }

    metrics::gauge!("process_uptime_seconds").set(crate::metrics::uptime_seconds());
    match crate::metrics::render() {
        Some(body) => (
            [
                (CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8"),
                (CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn configured_metrics_token() -> Result<Option<String>, ()> {
    let token = std::env::var(METRICS_TOKEN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    if token.is_none() && env_flag_enabled(REQUIRE_METRICS_AUTH_ENV) {
        Err(())
    } else {
        Ok(token)
    }
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn metrics_authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn metrics_bearer_is_exact_and_length_strict() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer metrics-secret"),
        );
        assert!(metrics_authorized(&headers, "metrics-secret"));
        assert!(!metrics_authorized(&headers, "metrics-secreu"));
        assert!(!metrics_authorized(&headers, "metrics-secret-longer"));

        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Basic metrics-secret"),
        );
        assert!(!metrics_authorized(&headers, "metrics-secret"));
    }
}
