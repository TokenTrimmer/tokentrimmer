//! Atomic Redis-backed fixed-window rate limiting shared across service replicas.
//!
//! The counter increment and first-write expiry are one Lua operation, so
//! concurrent replicas cannot over-admit because of a read/modify/write race.

use std::sync::Arc;
use std::time::Duration;

use crate::CacheError;

const INCREMENT_WINDOW_SCRIPT: &str = r#"
local count = redis.call('INCR', KEYS[1])
if count == 1 then
  redis.call('PEXPIRE', KEYS[1], ARGV[1])
end
local ttl = redis.call('PTTL', KEYS[1])
return {count, ttl}
"#;

/// Result of consuming one cell from a shared fixed-window limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitDecision {
    /// The counter remains at or below the configured limit.
    Allow,
    /// The counter exceeded the limit. The retry delay is always at least one
    /// second, including the narrow race where Redis expires the key between
    /// increment and response decoding.
    Reject { retry_after_secs: u64 },
}

/// Cloneable Redis limiter. `ConnectionManager` multiplexes commands over one
/// reconnecting backend connection; cloning this type does not open a socket.
#[derive(Clone)]
pub struct RedisRateLimiter {
    connection: redis::aio::ConnectionManager,
    namespace: Arc<str>,
}

impl RedisRateLimiter {
    /// Connect to a native Redis URL (`redis://` or `rediss://`).
    pub async fn connect(url: &str, namespace: impl Into<Arc<str>>) -> Result<Self, CacheError> {
        let client = redis::Client::open(url)?;
        let connection = redis::aio::ConnectionManager::new(client).await?;
        Ok(Self::from_connection_manager(connection, namespace))
    }

    /// Reuse an existing multiplexed connection manager.
    #[must_use]
    pub fn from_connection_manager(
        connection: redis::aio::ConnectionManager,
        namespace: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            connection,
            namespace: namespace.into(),
        }
    }

    /// Atomically consume one cell for `scope` + `key`.
    ///
    /// A fixed window begins with the first admitted or rejected attempt after
    /// expiry. Rejected attempts increment the same expiring counter but never
    /// extend its TTL.
    pub async fn check(
        &self,
        scope: &str,
        key: &str,
        limit: u32,
        window: Duration,
    ) -> Result<RateLimitDecision, CacheError> {
        let redis_key = format!("{}:{}:{}", self.namespace, scope, key);
        let window_ms = window.as_millis().clamp(1, i64::MAX as u128) as i64;
        let mut connection = self.connection.clone();
        let (count, ttl_ms): (i64, i64) = redis::cmd("EVAL")
            .arg(INCREMENT_WINDOW_SCRIPT)
            .arg(1)
            .arg(redis_key)
            .arg(window_ms)
            .query_async(&mut connection)
            .await?;

        Ok(decision_from_count(count, ttl_ms, limit.max(1)))
    }
}

fn decision_from_count(count: i64, ttl_ms: i64, limit: u32) -> RateLimitDecision {
    if count <= i64::from(limit) {
        RateLimitDecision::Allow
    } else {
        let retry_after_secs = u64::try_from(ttl_ms.max(1))
            .unwrap_or(u64::MAX)
            .saturating_add(999)
            / 1_000;
        RateLimitDecision::Reject {
            retry_after_secs: retry_after_secs.max(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_allows_through_limit_and_rounds_retry_up() {
        assert_eq!(decision_from_count(3, 60_000, 3), RateLimitDecision::Allow);
        assert_eq!(
            decision_from_count(4, 1_001, 3),
            RateLimitDecision::Reject {
                retry_after_secs: 2
            }
        );
    }

    #[test]
    fn expired_key_race_still_returns_positive_retry() {
        assert_eq!(
            decision_from_count(2, -1, 1),
            RateLimitDecision::Reject {
                retry_after_secs: 1
            }
        );
    }
}
