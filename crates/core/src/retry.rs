//! Bounded retry/backoff for provider dispatch.
//!
//! Adapters deliberately don't retry — this is the core layer's policy. On a
//! retriable [`ProviderError`] (rate limit, timeout, network, 5xx) the op is
//! re-tried with exponential backoff, honoring a rate-limit's `retry_after_ms`,
//! up to `max_attempts`. Non-retriable errors return immediately.
//!
//! Falling back to an *alternate* provider ([`ProviderError::is_fallback_eligible`])
//! needs an ordered fallback chain, which lands with the `provider-failover`
//! work; this layer retries the same provider (which covers transient 429/5xx).

use std::future::Future;
use std::time::Duration;

use tt_shared::ProviderError;

/// Upper bound on any single backoff sleep, so a hostile `retry_after_ms` or a
/// deep exponential can't stall a request indefinitely.
const MAX_BACKOFF: Duration = Duration::from_secs(20);

/// Retry/backoff parameters.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts including the first (`1` = no retries).
    pub max_attempts: u32,
    /// Base delay for exponential backoff: `delay = base * 2^(attempt-1)`.
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
        }
    }
}

impl RetryPolicy {
    /// Backoff before the next attempt. `attempt` is the (1-based) attempt that
    /// just failed. Honors a rate-limit's `retry_after_ms`; otherwise applies
    /// capped exponential backoff.
    fn backoff(&self, attempt: u32, err: &ProviderError) -> Duration {
        if let ProviderError::RateLimited { retry_after_ms } = err {
            return Duration::from_millis(*retry_after_ms).min(MAX_BACKOFF);
        }
        let mult = 2u32.saturating_pow(attempt.saturating_sub(1));
        self.base_delay.saturating_mul(mult).min(MAX_BACKOFF)
    }
}

/// Run `op` with bounded retry/backoff. Retries only
/// [`ProviderError::is_retriable`] errors, up to `policy.max_attempts`.
pub async fn with_retry<T, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut attempt: u32 = 1;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= policy.max_attempts || !e.is_retriable() {
                    return Err(e);
                }
                let delay = policy.backoff(attempt, &e);
                tracing::warn!(attempt, error = %e, backoff_ms = delay.as_millis() as u64, "provider dispatch failed; retrying");
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn fast(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn succeeds_on_first_try() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let r: Result<u8, _> = with_retry(&fast(3), || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(7u8)
            }
        })
        .await;
        assert_eq!(r.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let r: Result<u8, _> = with_retry(&fast(5), || {
            let c = c.clone();
            async move {
                let n = c.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(ProviderError::Timeout { ms: 1 })
                } else {
                    Ok(9u8)
                }
            }
        })
        .await;
        assert_eq!(r.unwrap(), 9);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn non_retriable_returns_immediately() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let r: Result<u8, _> = with_retry(&fast(5), || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::InvalidRequest("bad".into()))
            }
        })
        .await;
        assert!(matches!(r, Err(ProviderError::InvalidRequest(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausts_attempts_then_returns_last_error() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let r: Result<u8, _> = with_retry(&fast(3), || {
            let c = c.clone();
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::Timeout { ms: 1 })
            }
        })
        .await;
        assert!(matches!(r, Err(ProviderError::Timeout { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn backoff_honors_retry_after() {
        let p = RetryPolicy::default();
        assert_eq!(
            p.backoff(
                1,
                &ProviderError::RateLimited {
                    retry_after_ms: 1500
                }
            ),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn backoff_is_exponential() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
        };
        let e = ProviderError::Timeout { ms: 0 };
        assert_eq!(p.backoff(1, &e), Duration::from_millis(100));
        assert_eq!(p.backoff(2, &e), Duration::from_millis(200));
        assert_eq!(p.backoff(3, &e), Duration::from_millis(400));
    }

    #[test]
    fn backoff_is_capped() {
        let p = RetryPolicy {
            max_attempts: 99,
            base_delay: Duration::from_secs(10),
        };
        assert_eq!(
            p.backoff(10, &ProviderError::Timeout { ms: 0 }),
            MAX_BACKOFF
        );
    }
}
