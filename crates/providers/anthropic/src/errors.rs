//! Maps HTTP status codes and Anthropic error bodies to [`tt_shared::ProviderError`].
//!
//! Anthropic error body shape:
//! ```json
//! {
//!   "type": "error",
//!   "error": {
//!     "type": "invalid_request_error" | "authentication_error" | ...,
//!     "message": "..."
//!   }
//! }
//! ```

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tt_shared::ProviderError;

/// Top-level Anthropic error envelope.
#[derive(Debug, Deserialize)]
struct AnthropicErrorBody {
    error: AnthropicError,
}

/// Inner Anthropic error object.
#[derive(Debug, Deserialize)]
struct AnthropicError {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
}

/// Map an Anthropic HTTP status + raw body text into a [`ProviderError`].
///
/// The `retry_after_header` is the raw value of the `Retry-After` response
/// header if present — it may be an integer number of seconds or an HTTP-date.
pub fn map_response_error(
    status: u16,
    body: &str,
    retry_after_header: Option<&str>,
) -> ProviderError {
    let parsed: Option<AnthropicErrorBody> = serde_json::from_str(body).ok();
    let message = parsed
        .as_ref()
        .map(|p| p.error.message.clone())
        .unwrap_or_else(|| body.to_string());
    let error_type = parsed
        .as_ref()
        .map(|p| p.error.error_type.as_str())
        .unwrap_or("");

    match status {
        401 => ProviderError::Unauthorized(message),
        429 => {
            let retry_after_ms = parse_retry_after(retry_after_header);
            ProviderError::RateLimited { retry_after_ms }
        }
        400 => ProviderError::InvalidRequest(message),
        404 => {
            let model = extract_model_name(&message);
            ProviderError::ModelNotFound { model }
        }
        // Anthropic uses 529 for overloaded (not standard HTTP).
        529 => ProviderError::ProviderUpstream {
            status: 503,
            message,
        },
        _ if error_type == "overloaded_error" => ProviderError::ProviderUpstream {
            status: 503,
            message,
        },
        500..=599 => ProviderError::ProviderUpstream { status, message },
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

/// Attempt to extract a model name from an Anthropic 404 error message.
fn extract_model_name(message: &str) -> String {
    // Handle messages like "model 'claude-xxx' not found".
    if let Some(start) = message.find('\'') {
        let after = &message[start + 1..];
        if let Some(end) = after.find('\'') {
            return after[..end].to_string();
        }
    }
    if let Some(start) = message.find('"') {
        let after = &message[start + 1..];
        if let Some(end) = after.find('"') {
            return after[..end].to_string();
        }
    }
    message.to_string()
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
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"Invalid API key"}}"#;
        let err = map_response_error(401, body, None);
        assert!(matches!(err, ProviderError::Unauthorized(_)));
    }

    #[test]
    fn map_429_with_retry_after() {
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limit exceeded"}}"#;
        let err = map_response_error(429, body, Some("3"));
        assert!(matches!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: 3000
            }
        ));
    }

    #[test]
    fn map_429_without_retry_after() {
        let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limit exceeded"}}"#;
        let err = map_response_error(429, body, None);
        assert!(matches!(
            err,
            ProviderError::RateLimited {
                retry_after_ms: 1000
            }
        ));
    }

    #[test]
    fn map_400_invalid_request() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens required"}}"#;
        let err = map_response_error(400, body, None);
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn map_529_overloaded() {
        let body = r#"{"type":"error","error":{"type":"overloaded_error","message":"Service overloaded"}}"#;
        let err = map_response_error(529, body, None);
        assert!(
            matches!(err, ProviderError::ProviderUpstream { status: 503, .. }),
            "expected ProviderUpstream{{503}}, got {err:?}"
        );
    }

    #[test]
    fn map_500_server_error() {
        let body =
            r#"{"type":"error","error":{"type":"api_error","message":"Internal server error"}}"#;
        let err = map_response_error(500, body, None);
        assert!(matches!(
            err,
            ProviderError::ProviderUpstream { status: 500, .. }
        ));
    }
}
