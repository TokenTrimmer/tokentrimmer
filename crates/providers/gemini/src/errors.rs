//! Maps HTTP status codes and Gemini error bodies to [`tt_shared::ProviderError`].
//!
//! Gemini error body shape:
//! ```json
//! {
//!   "error": {
//!     "code": 400,
//!     "message": "...",
//!     "status": "INVALID_ARGUMENT" | "UNAUTHENTICATED" | "PERMISSION_DENIED" | ...,
//!     "details": [...]
//!   }
//! }
//! ```

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tt_shared::ProviderError;

/// Top-level Gemini error envelope.
#[derive(Debug, Deserialize)]
struct GeminiErrorBody {
    error: GeminiError,
}

/// Inner Gemini error object.
#[derive(Debug, Deserialize)]
struct GeminiError {
    /// HTTP status code echoed in the body.
    #[serde(default)]
    #[allow(dead_code)]
    code: u16,
    #[serde(default)]
    message: String,
    #[serde(default)]
    status: String,
}

/// Map a Gemini HTTP status + raw body text into a [`ProviderError`].
///
/// The `retry_after_header` is the raw value of the `Retry-After` response
/// header if present — it may be an integer number of seconds or an HTTP-date.
pub fn map_response_error(
    status: u16,
    body: &str,
    retry_after_header: Option<&str>,
    model: &str,
) -> ProviderError {
    let parsed: Option<GeminiErrorBody> = serde_json::from_str(body).ok();
    let message = parsed
        .as_ref()
        .map(|p| p.error.message.clone())
        .unwrap_or_else(|| body.to_string());
    let gemini_status = parsed
        .as_ref()
        .map(|p| p.error.status.as_str())
        .unwrap_or("");

    match (status, gemini_status) {
        (401, _) | (_, "UNAUTHENTICATED") => ProviderError::Unauthorized(message),
        (403, _) | (_, "PERMISSION_DENIED") => ProviderError::Unauthorized(message),
        (429, _) | (_, "RESOURCE_EXHAUSTED") => {
            let retry_after_ms = parse_retry_after(retry_after_header);
            ProviderError::RateLimited { retry_after_ms }
        }
        (400, _) | (_, "INVALID_ARGUMENT") => ProviderError::InvalidRequest(message),
        (404, _) => {
            let extracted_model = extract_model_name(&message).unwrap_or_else(|| model.to_string());
            ProviderError::ModelNotFound {
                model: extracted_model,
            }
        }
        (500..=599, _) | (_, "INTERNAL") | (_, "UNAVAILABLE") => {
            ProviderError::ProviderUpstream { status, message }
        }
        _ => ProviderError::ProviderUpstream { status, message },
    }
}

/// Map a [`reqwest::Error`] (network-level failure) to [`ProviderError`].
pub fn map_reqwest_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout { ms: 0 }
    } else {
        ProviderError::Network(err)
    }
}

/// Parse the `Retry-After` header value into milliseconds.
///
/// The header may be:
/// - An integer number of seconds: `"3"` → 3000 ms
/// - An HTTP-date string: `"Mon, 25 May 2026 13:00:00 GMT"` → computed delta
/// - Absent or unparseable → default 1000 ms
fn parse_retry_after(header: Option<&str>) -> u64 {
    let Some(value) = header else {
        return 1000;
    };

    // Try integer seconds first.
    if let Ok(secs) = value.trim().parse::<u64>() {
        return secs * 1000;
    }

    // Try HTTP-date (RFC 2822 / RFC 7231).
    if let Ok(date) = DateTime::parse_from_rfc2822(value.trim()) {
        let delta = date.with_timezone(&Utc) - Utc::now();
        let ms = delta.num_milliseconds().max(0) as u64;
        return ms;
    }

    1000
}

/// Attempt to extract a model name from a Gemini 404 error message.
fn extract_model_name(message: &str) -> Option<String> {
    // Messages like "models/gemini-3.1-pro is not found"
    if let Some(pos) = message.find("models/") {
        let rest = &message[pos + "models/".len()..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '\'' || c == '"')
            .unwrap_or(rest.len());
        return Some(rest[..end].to_string());
    }
    // Handle single-quoted model names.
    if let Some(start) = message.find('\'') {
        let after = &message[start + 1..];
        if let Some(end) = after.find('\'') {
            return Some(after[..end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_retry_after_integer() {
        assert_eq!(parse_retry_after(Some("3")), 3000);
    }

    #[test]
    fn parse_retry_after_missing() {
        assert_eq!(parse_retry_after(None), 1000);
    }

    #[test]
    fn parse_retry_after_garbage() {
        assert_eq!(parse_retry_after(Some("not-a-number")), 1000);
    }

    #[test]
    fn map_401_unauthorized() {
        let body =
            r#"{"error":{"code":401,"message":"Invalid API key","status":"UNAUTHENTICATED"}}"#;
        let err = map_response_error(401, body, None, "gemini-3.1-pro");
        assert!(matches!(err, ProviderError::Unauthorized(_)));
    }

    #[test]
    fn map_403_permission_denied() {
        let body =
            r#"{"error":{"code":403,"message":"Permission denied","status":"PERMISSION_DENIED"}}"#;
        let err = map_response_error(403, body, None, "gemini-3.1-pro");
        assert!(matches!(err, ProviderError::Unauthorized(_)));
    }

    #[test]
    fn map_429_with_retry_after() {
        let body = r#"{"error":{"code":429,"message":"Rate limit exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        let err = map_response_error(429, body, Some("3"), "gemini-3.1-pro");
        assert!(matches!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: 3000
            }
        ));
    }

    #[test]
    fn map_429_without_retry_after() {
        let body = r#"{"error":{"code":429,"message":"Rate limit exceeded","status":"RESOURCE_EXHAUSTED"}}"#;
        let err = map_response_error(429, body, None, "gemini-3.1-pro");
        assert!(matches!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: 1000
            }
        ));
    }

    #[test]
    fn map_400_invalid_argument() {
        let body =
            r#"{"error":{"code":400,"message":"Invalid argument","status":"INVALID_ARGUMENT"}}"#;
        let err = map_response_error(400, body, None, "gemini-3.1-pro");
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn map_404_model_not_found() {
        let body = r#"{"error":{"code":404,"message":"models/gemini-unknown is not found","status":"NOT_FOUND"}}"#;
        let err = map_response_error(404, body, None, "gemini-unknown");
        assert!(matches!(err, ProviderError::ModelNotFound { .. }));
    }

    #[test]
    fn map_500_internal() {
        let body = r#"{"error":{"code":500,"message":"Internal error","status":"INTERNAL"}}"#;
        let err = map_response_error(500, body, None, "gemini-3.1-pro");
        assert!(matches!(
            err,
            ProviderError::ProviderUpstream { status: 500, .. }
        ));
    }

    #[test]
    fn map_503_unavailable() {
        let body =
            r#"{"error":{"code":503,"message":"Service unavailable","status":"UNAVAILABLE"}}"#;
        let err = map_response_error(503, body, None, "gemini-3.1-pro");
        assert!(matches!(
            err,
            ProviderError::ProviderUpstream { status: 503, .. }
        ));
    }
}
