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
    pub fn is_fallback_eligible(&self) -> bool {
        matches!(
            self,
            ProviderError::ModelNotFound { .. }
                | ProviderError::ProviderUpstream { .. }
                | ProviderError::Timeout { .. }
        )
    }
}
