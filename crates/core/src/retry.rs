//! Bounded retry/backoff with full-jitter for provider dispatch.
//!
//! Adapters deliberately don't retry — this is the core layer's policy. On a
//! retriable [`ProviderError`] (rate limit, timeout, network, 5xx) the op is
//! re-tried with **full-jitter** exponential backoff, honoring a rate-limit's
//! `retry_after_ms`, up to `max_attempts`. Non-retriable errors return
//! immediately.
//!
//! ## Jitter seam
//!
//! [`RetryPolicy`] holds a [`JitterFn`] that maps an uncapped delay to a
//! jittered one. The production default applies full jitter:
//! `delay = rand(0 ..= min(base * 2^(n-1), MAX_BACKOFF))`.
//! Tests inject a deterministic no-op (`JitterFn::none()`) so timing is
//! reproducible and the test suite stays fast and non-flaky.
//!
//! Falling back to an *alternate* provider ([`ProviderError::is_fallback_eligible`])
//! needs an ordered fallback chain, which is handled by the `provider-failover`
//! module; this layer retries the same provider.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use tt_shared::ProviderError;

/// Upper bound on any single backoff sleep, so a hostile `retry_after_ms` or a
/// deep exponential can't stall a request indefinitely.
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(20);

/// A pluggable jitter function. Takes the deterministic (capped) backoff
/// duration and returns the actual sleep duration, which must be `<= input`.
///
/// Use [`JitterFn::full()`] (production default) for full-jitter randomization.
/// Use [`JitterFn::none()`] in tests for deterministic behaviour.
#[derive(Clone)]
pub struct JitterFn(Arc<dyn Fn(Duration) -> Duration + Send + Sync>);

impl JitterFn {
    /// Full-jitter: returns `rand(0 ..= cap)` using the OS RNG. This prevents
    /// thundering-herd synchronization when many requests share the same
    /// provider incident and backoff schedule.
    pub fn full() -> Self {
        Self(Arc::new(|cap: Duration| {
            let cap_nanos = cap.as_nanos() as u64;
            if cap_nanos == 0 {
                return Duration::ZERO;
            }
            // Draw a 64-bit uniform random sample from the OS CSPRNG and reduce
            // it into [0, cap_nanos] with a modulo. The bias is negligible for
            // practical Duration values (max 20 s → 2×10^10 ns << 2^64).
            let mut buf = [0u8; 8];
            OsRng.fill_bytes(&mut buf);
            let r = u64::from_le_bytes(buf);
            Duration::from_nanos(r % (cap_nanos + 1))
        }))
    }

    /// No-op jitter: returns the input unchanged. Use in unit tests for
    /// deterministic, fast backoff assertions.
    pub fn none() -> Self {
        Self(Arc::new(|d: Duration| d))
    }

    /// Apply the jitter function to `cap`.
    #[inline]
    pub fn apply(&self, cap: Duration) -> Duration {
        (self.0)(cap)
    }
}

impl std::fmt::Debug for JitterFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JitterFn")
    }
}

/// Retry/backoff parameters.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first (`1` = no retries).
    pub max_attempts: u32,
    /// Base delay for exponential backoff: uncapped delay = `base * 2^(attempt-1)`.
    pub base_delay: Duration,
    /// Jitter applied to the capped deterministic delay. Defaults to full jitter.
    pub jitter: JitterFn,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
            jitter: JitterFn::full(),
        }
    }
}

impl RetryPolicy {
    /// Convenience constructor: zero base delay, no jitter, given max attempts.
    /// Intended for tests that need a fast, deterministic policy.
    #[cfg(test)]
    pub fn fast_no_jitter(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            base_delay: Duration::ZERO,
            jitter: JitterFn::none(),
        }
    }

    /// Return a clone of this policy with `max_attempts` clamped to `limit`.
    /// Used by the failover layer to bound per-candidate retries in a chain.
    pub fn capped(&self, limit: u32) -> Self {
        Self {
            max_attempts: self.max_attempts.min(limit),
            base_delay: self.base_delay,
            jitter: self.jitter.clone(),
        }
    }

    /// Compute the backoff sleep before the next attempt.
    ///
    /// `attempt` is the **1-based** attempt number that just failed. The
    /// returned duration respects `MAX_BACKOFF` and the configured jitter:
    ///
    /// - `RateLimited`: `min(retry_after_ms, MAX_BACKOFF)`, no jitter (the
    ///   server told us exactly how long to wait).
    /// - Everything else: `jitter(min(base * 2^(attempt-1), MAX_BACKOFF))`,
    ///   which is `rand(0 ..= cap)` with the default full-jitter function.
    pub(crate) fn backoff(&self, attempt: u32, err: &ProviderError) -> Duration {
        if let ProviderError::RateLimited { retry_after_ms } = err {
            // Server-specified backoff: honor it exactly (still capped).
            return Duration::from_millis(*retry_after_ms).min(MAX_BACKOFF);
        }
        let mult = 2u32.saturating_pow(attempt.saturating_sub(1));
        let cap = self.base_delay.saturating_mul(mult).min(MAX_BACKOFF);
        self.jitter.apply(cap)
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
                tracing::warn!(
                    attempt,
                    error = %e,
                    backoff_ms = delay.as_millis() as u64,
                    "provider dispatch failed; retrying"
                );
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
            jitter: JitterFn::none(),
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
        let p = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(200),
            jitter: JitterFn::none(),
        };
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
    fn backoff_no_jitter_is_exponential() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
            jitter: JitterFn::none(),
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
            jitter: JitterFn::none(),
        };
        assert_eq!(
            p.backoff(10, &ProviderError::Timeout { ms: 0 }),
            MAX_BACKOFF
        );
    }

    /// Full-jitter delay for attempt n is within [0, min(base*2^(n-1), MAX_BACKOFF)].
    #[test]
    fn full_jitter_delay_in_bounds() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
            jitter: JitterFn::full(),
        };
        let e = ProviderError::Timeout { ms: 0 };

        // Run many samples; each must be within [0, cap].
        for attempt in 1u32..=4 {
            let mult = 2u32.saturating_pow(attempt.saturating_sub(1));
            let cap = p.base_delay.saturating_mul(mult).min(MAX_BACKOFF);
            for _ in 0..50 {
                let d = p.backoff(attempt, &e);
                assert!(
                    d <= cap,
                    "attempt {attempt}: jitter {d:?} exceeds cap {cap:?}"
                );
            }
        }
    }

    /// Deterministic (no-jitter) backoff for attempt n equals base*2^(n-1).
    #[test]
    fn deterministic_jitter_none_exact() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(200),
            jitter: JitterFn::none(),
        };
        let e = ProviderError::Timeout { ms: 0 };
        assert_eq!(p.backoff(1, &e), Duration::from_millis(200));
        assert_eq!(p.backoff(2, &e), Duration::from_millis(400));
        assert_eq!(p.backoff(3, &e), Duration::from_millis(800));
    }

    /// `RetryPolicy::capped` reduces max_attempts correctly.
    #[test]
    fn capped_reduces_attempts() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_attempts, 3);
        let capped = p.capped(2);
        assert_eq!(capped.max_attempts, 2);
        let capped_same = capped.capped(5); // min(2, 5) = 2
        assert_eq!(capped_same.max_attempts, 2);
    }
}
