//! Shared HTTP guard middleware + loopback helpers for the network transports.
//!
//! Both the legacy SSE transport and the current Streamable HTTP transport
//! enforce the same defense-in-depth on every request:
//!   - **Bearer auth** — `Authorization: Bearer <token>` (constant-time compare).
//!   - **Loopback Host** — `Host` host-part must be `127.0.0.1`/`localhost`/`::1`
//!     (DNS-rebind defense).
//!   - **Origin** — a browser-style `Origin`, if present, must also be loopback.

use axum::response::IntoResponse;

/// Reject any request lacking a valid bearer token or arriving with a non-loopback
/// Host/Origin (DNS-rebind defense). Runs before body read + dispatch.
pub async fn guard(
    axum::extract::State(token): axum::extract::State<std::sync::Arc<str>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    let headers = req.headers();

    if !host_is_local(headers.get(header::HOST)) {
        return (StatusCode::FORBIDDEN, "non-local Host").into_response();
    }
    if !origin_is_local_or_absent(headers.get(header::ORIGIN)) {
        return (StatusCode::FORBIDDEN, "cross-origin").into_response();
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if !presented.is_some_and(|p| ct_eq(p.as_bytes(), token.as_bytes())) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"tt-mcp\"")],
            "missing or invalid bearer token",
        )
            .into_response();
    }
    next.run(req).await
}

/// Constant-time byte compare (length mismatch returns early — token length is
/// not sensitive). Mirrors the cloud `constant_time_eq`.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Host header host-part is loopback (ignoring port). Missing Host → false.
pub fn host_is_local(h: Option<&axum::http::HeaderValue>) -> bool {
    h.and_then(|v| v.to_str().ok())
        .is_some_and(is_local_authority)
}

/// Origin absent (non-browser MCP clients) or its host is loopback.
pub fn origin_is_local_or_absent(h: Option<&axum::http::HeaderValue>) -> bool {
    match h.and_then(|v| v.to_str().ok()) {
        None => true,
        // file:// pages and sandboxed iframes send Origin: null. Accepted because
        // the bearer token is still required, so this is not a bypass.
        Some("null") => true,
        Some(s) => s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("https://"))
            .is_some_and(is_local_authority),
    }
}

/// Host-part (strip a trailing `:port`) is 127.0.0.1 / localhost / ::1.
/// Note: a bare IPv6 `::1` (no brackets) is rejected per RFC 7230 — use `[::1]`.
pub fn is_local_authority(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("") // IPv6 "[::1]:port" → "::1"
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Graceful shutdown future: resolves on SIGINT or (Unix) SIGTERM.
pub async fn shutdown_signal(label: &'static str) {
    use tokio::signal;

    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("{label} shutdown: SIGINT"),
        _ = terminate => tracing::info!("{label} shutdown: SIGTERM"),
    }
}

#[cfg(test)]
mod tests {
    use super::{ct_eq, host_is_local, is_local_authority, origin_is_local_or_absent};

    #[test]
    fn ct_eq_matches_and_mismatches() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab")); // length mismatch
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn local_authority_accepts_loopback_rejects_spoofs() {
        assert!(is_local_authority("127.0.0.1"));
        assert!(is_local_authority("127.0.0.1:8080"));
        assert!(is_local_authority("localhost"));
        assert!(is_local_authority("localhost:9000"));
        assert!(is_local_authority("[::1]"));
        assert!(is_local_authority("[::1]:7000"));
        // Spoofs that must be rejected:
        assert!(!is_local_authority("127.0.0.1.evil.com"));
        assert!(!is_local_authority("localhost.evil.com"));
        assert!(!is_local_authority("evil.com"));
        assert!(!is_local_authority("LOCALHOST")); // case-sensitive on purpose
        assert!(!is_local_authority("::1")); // bare ipv6 (unbracketed) rejected
    }

    #[test]
    fn host_and_origin_helpers() {
        use axum::http::HeaderValue;
        assert!(host_is_local(Some(&HeaderValue::from_static(
            "127.0.0.1:8080"
        ))));
        assert!(!host_is_local(None)); // missing Host rejected
        assert!(!host_is_local(Some(&HeaderValue::from_static("evil.com"))));
        assert!(origin_is_local_or_absent(None));
        assert!(origin_is_local_or_absent(Some(&HeaderValue::from_static(
            "null"
        ))));
        assert!(origin_is_local_or_absent(Some(&HeaderValue::from_static(
            "http://localhost:3000"
        ))));
        assert!(!origin_is_local_or_absent(Some(&HeaderValue::from_static(
            "https://evil.com"
        ))));
    }
}
