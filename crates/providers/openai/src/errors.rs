//! Maps HTTP status codes and OpenAI error bodies to [`tt_shared::ProviderError`].
//!
//! OpenAI error body shape:
//! ```json
//! { "error": { "message": "...", "type": "...", "code": "...", "param": null } }
//! ```

use chrono::{DateTime, Utc};
use serde::Deserialize;
use tt_shared::ProviderError;

/// Top-level OpenAI error envelope.
#[derive(Debug, Deserialize)]
struct OpenAiErrorBody {
    error: OpenAiError,
}

/// Inner OpenAI error object.
#[derive(Debug, Deserialize)]
struct OpenAiError {
    message: String,
    #[serde(rename = "type")]
    error_type: Option<String>,
    #[allow(dead_code)]
    code: Option<String>,
    #[allow(dead_code)]
    param: Option<serde_json::Value>,
}

/// Map an OpenAI HTTP status + raw body text into a [`ProviderError`].
///
/// The `retry_after_header` is the raw value of the `Retry-After` response header
/// if present — it may be an integer number of seconds or an HTTP-date string.
pub fn map_response_error(
    status: u16,
    body: &str,
    retry_after_header: Option<&str>,
) -> ProviderError {
    let parsed: Option<OpenAiErrorBody> = serde_json::from_str(body).ok();
    let message = parsed
        .as_ref()
        .map(|p| p.error.message.clone())
        .unwrap_or_else(|| body.to_string());
    let error_type = parsed
        .as_ref()
        .and_then(|p| p.error.error_type.clone())
        .unwrap_or_default();

    match status {
        401 => ProviderError::Unauthorized(message),
        429 => {
            let retry_after_ms = parse_retry_after(retry_after_header);
            ProviderError::RateLimited { retry_after_ms }
        }
        400 if error_type == "invalid_request_error" => ProviderError::InvalidRequest(message),
        400 => ProviderError::InvalidRequest(message),
        404 if message.to_lowercase().contains("model") => {
            let model = extract_model_name(&message);
            ProviderError::ModelNotFound { model }
        }
        404 => ProviderError::InvalidRequest(message),
        408 => ProviderError::Timeout { ms: 0 },
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
/// - An integer number of seconds: `"5"` → 5000 ms
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

/// Attempt to extract a model name from an OpenAI 404 error message.
///
/// Handles messages like `"The model 'gpt-99' does not exist"`.
fn extract_model_name(message: &str) -> String {
    // Look for a quoted token after "model".
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
        assert_eq!(parse_retry_after(Some("5")), 5000);
    }

    #[test]
    fn parse_retry_after_missing() {
        assert_eq!(parse_retry_after(None), 1000);
    }

    #[test]
    fn parse_retry_after_garbage() {
        assert_eq!(parse_retry_after(Some("garbage")), 1000);
    }

    #[test]
    fn extract_model_single_quoted() {
        let msg = "The model 'gpt-99' does not exist";
        assert_eq!(extract_model_name(msg), "gpt-99");
    }

    #[test]
    fn map_401_to_unauthorized() {
        let body = r#"{"error":{"message":"Invalid API key","type":"invalid_api_key","code":"invalid_api_key","param":null}}"#;
        let err = map_response_error(401, body, None);
        assert!(matches!(err, ProviderError::Unauthorized(_)));
    }

    #[test]
    fn map_429_with_retry_after() {
        let body = r#"{"error":{"message":"Rate limit exceeded","type":"requests","code":null,"param":null}}"#;
        let err = map_response_error(429, body, Some("5"));
        assert!(matches!(err, ProviderError::RateLimited { retry_after_ms: 5000 }));
    }

    #[test]
    fn map_429_without_retry_after() {
        let body = r#"{"error":{"message":"Rate limit exceeded","type":"requests","code":null,"param":null}}"#;
        let err = map_response_error(429, body, None);
        assert!(matches!(err, ProviderError::RateLimited { retry_after_ms: 1000 }));
    }
}
