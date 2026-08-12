//! Canonical-host enforcement for hosted deployments.
//!
//! Fly's public `*.fly.dev` hostname bypasses edge policy unless the application
//! rejects it. When `TT_CANONICAL_HOST` is configured, every non-probe request
//! must carry that exact HTTP `Host` (an optional numeric port is accepted).
//! `/health` and `/ready` remain reachable for Fly health checks.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header::HOST, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

/// Hosted canonical host environment variable.
pub const CANONICAL_HOST_ENV: &str = "TT_CANONICAL_HOST";

/// Resolve the configured host once while the router is built.
///
/// Empty and missing values disable the check for local/self-hosted use. A
/// malformed non-empty value is retained deliberately: it can match no valid
/// `Host` header, so the deployment fails closed instead of silently exposing a
/// direct origin.
#[must_use]
pub fn configured_host() -> Option<Arc<str>> {
    std::env::var(CANONICAL_HOST_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(Arc::<str>::from)
}

fn is_probe(path: &str) -> bool {
    matches!(path, "/health" | "/ready")
}

fn host_matches(raw: &str, expected: &str) -> bool {
    if raw.eq_ignore_ascii_case(expected) {
        return true;
    }

    let Some((host, port)) = raw.rsplit_once(':') else {
        return false;
    };
    host.eq_ignore_ascii_case(expected)
        && !port.is_empty()
        && port.as_bytes().iter().all(u8::is_ascii_digit)
}

/// Reject non-canonical direct-origin traffic before authentication or handlers.
pub async fn middleware(State(expected): State<Arc<str>>, req: Request, next: Next) -> Response {
    if is_probe(req.uri().path()) {
        return next.run(req).await;
    }

    let accepted = req
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| host_matches(host, &expected));
    if accepted {
        return next.run(req).await;
    }

    (
        StatusCode::MISDIRECTED_REQUEST,
        Json(serde_json::json!({
            "error": {
                "type": "invalid_request_error",
                "code": "non_canonical_origin",
                "message": "Request host is not served by this deployment."
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    #[test]
    fn canonical_host_accepts_case_and_numeric_port_only() {
        assert!(host_matches("api.tokentrimmer.com", "api.tokentrimmer.com"));
        assert!(host_matches(
            "API.TokenTrimmer.Com:443",
            "api.tokentrimmer.com"
        ));
        assert!(!host_matches(
            "api.tokentrimmer.com.evil.test",
            "api.tokentrimmer.com"
        ));
        assert!(!host_matches(
            "api.tokentrimmer.com:bad",
            "api.tokentrimmer.com"
        ));
        assert!(!host_matches(
            "tokentrimmer.fly.dev",
            "api.tokentrimmer.com"
        ));
    }

    #[test]
    fn only_platform_probes_bypass_host_enforcement() {
        assert!(is_probe("/health"));
        assert!(is_probe("/ready"));
        assert!(!is_probe("/metrics"));
        assert!(!is_probe("/v1/models"));
    }

    #[tokio::test]
    async fn middleware_blocks_direct_origin_but_keeps_health_probe() {
        let app = Router::new()
            .route("/v1/models", get(|| async { StatusCode::OK }))
            .route("/health", get(|| async { StatusCode::OK }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::<str>::from("api.tokentrimmer.com"),
                middleware,
            ));

        let direct = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(HOST, "tokentrimmer.fly.dev")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(direct.status(), StatusCode::MISDIRECTED_REQUEST);

        let canonical = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(HOST, "api.tokentrimmer.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(canonical.status(), StatusCode::OK);

        let health = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(HOST, "tokentrimmer.fly.dev")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }
}
