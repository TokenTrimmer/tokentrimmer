//! Error types returned by provider adapters. The core layer maps these to
//! HTTP status codes and decides retry strategy — adapters do not retry.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("rate limited (retry after {retry_after_ms} ms)")]
    RateLimited { retry_after_ms: u64 },

    #[error("model not found: {model}")]
    ModelNotFound { model: String },

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("upstream provider error (status {status}): {message}")]
    ProviderUpstream { status: u16, message: String },

    #[error("timeout after {ms} ms")]
    Timeout { ms: u64 },

    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("deserialize error: {0}")]
    Deserialize(String),

    #[error("unsupported feature: {0}")]
    Unsupported(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl ProviderError {
    /// True if the error is retriable. The core layer applies backoff + jitter.
    pub fn is_retriable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. } => true,
            ProviderError::Timeout { .. } => true,
            ProviderError::Network(_) => true,
            ProviderError::ProviderUpstream { status, .. } => *status >= 500,
            _ => false,
        }
    }

    /// True if the error means we should try a fallback provider.
    ///
    /// Only upstream *server* errors (5xx) are fallback-eligible. A
    /// deterministic client error (400 invalid request, 403 forbidden, 422
    /// unprocessable) will fail identically on every provider, so failing
    /// over just burns extra upstream calls + spend. Matches the `>= 500`
    /// guard in [`Self::is_retriable`]. (429 maps to [`Self::RateLimited`]
    /// and timeouts to [`Self::Timeout`], which are handled separately.)
    pub fn is_fallback_eligible(&self) -> bool {
        match self {
            ProviderError::ModelNotFound { .. } => true,
            ProviderError::Timeout { .. } => true,
            ProviderError::ProviderUpstream { status, .. } => *status >= 500,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_5xx_is_fallback_eligible() {
        assert!(ProviderError::ProviderUpstream {
            status: 500,
            message: "boom".into()
        }
        .is_fallback_eligible());
        assert!(ProviderError::ProviderUpstream {
            status: 503,
            message: "unavailable".into()
        }
        .is_fallback_eligible());
    }

    #[test]
    fn upstream_4xx_is_not_fallback_eligible() {
        for status in [400u16, 403, 404, 422] {
            assert!(
                !ProviderError::ProviderUpstream {
                    status,
                    message: "client error".into()
                }
                .is_fallback_eligible(),
                "status {status} must not fail over"
            );
        }
    }

    #[test]
    fn model_not_found_and_timeout_still_fallback_eligible() {
        assert!(ProviderError::ModelNotFound { model: "x".into() }.is_fallback_eligible());
        assert!(ProviderError::Timeout { ms: 1000 }.is_fallback_eligible());
    }

    #[test]
    fn invalid_request_and_unauthorized_not_fallback_eligible() {
        assert!(!ProviderError::InvalidRequest("bad".into()).is_fallback_eligible());
        assert!(!ProviderError::Unauthorized("nope".into()).is_fallback_eligible());
    }
}
