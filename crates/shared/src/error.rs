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
    /// A durable pre-dispatch reservation could not fit within the caller's
    /// configured monthly USD cap. Core maps this to the gateway's typed 429
    /// budget response; adapters never construct it themselves.
    #[error(
        "monthly spend cap cannot admit estimated cost ${estimated_usd:.6}; remaining ${remaining_usd:.6}"
    )]
    BudgetExceeded {
        estimated_usd: f64,
        remaining_usd: f64,
    },

    /// A USD-capped call has no catalog price and/or bounded output size, so a
    /// safe reservation cannot be computed.
    #[error("a capped request has no enforceable cost bound for model {model}")]
    BudgetPriceUnknown { model: String },

    /// The durable reservation backend failed before provider dispatch. Failing
    /// closed here protects a configured hard cap during database degradation.
    #[error("durable budget reservation unavailable: {0}")]
    BudgetUnavailable(String),

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
    /// Transient/load and server-side conditions are fallback-eligible:
    /// upstream *server* errors (5xx), timeouts, model-not-found, and rate
    /// limiting (429). A 429 is also [`Self::is_retriable`], so the dispatch
    /// loop first exhausts the same-provider retry budget (honoring
    /// `retry_after_ms`); only then does failover advance to a candidate that
    /// may have spare quota — a common real-world recovery during a primary
    /// provider's capacity crunch.
    ///
    /// A *deterministic* client error (400 invalid request, 403 forbidden, 422
    /// unprocessable) is NOT eligible: it would fail identically on every
    /// provider, so failing over just burns extra upstream calls + spend.
    pub fn is_fallback_eligible(&self) -> bool {
        match self {
            ProviderError::ModelNotFound { .. } => true,
            ProviderError::Timeout { .. } => true,
            ProviderError::RateLimited { .. } => true,
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
    fn rate_limited_is_fallback_eligible() {
        // A sustained 429 on the primary should fail over to a provider with
        // spare quota (after same-provider retries exhaust). 429 is also
        // retriable, so the dispatch loop retries it in place first.
        let e = ProviderError::RateLimited { retry_after_ms: 0 };
        assert!(e.is_fallback_eligible());
        assert!(e.is_retriable());
    }

    #[test]
    fn invalid_request_and_unauthorized_not_fallback_eligible() {
        assert!(!ProviderError::InvalidRequest("bad".into()).is_fallback_eligible());
        assert!(!ProviderError::Unauthorized("nope".into()).is_fallback_eligible());
    }
}
