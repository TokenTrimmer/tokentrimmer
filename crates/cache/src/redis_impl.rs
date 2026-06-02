//! Redis-backed [`L1Cache`] implementation.
//!
//! [`RedisL1Cache`] wraps a [`redis::aio::ConnectionManager`] (which handles
//! automatic reconnection) and prefixes every key with a configurable
//! namespace string (e.g. `"tt:l1"`) so multiple environments can share a
//! single Redis instance without key collisions.
//!
//! [`L1Cache`]: crate::L1Cache

use async_trait::async_trait;
use redis::AsyncCommands;

use crate::{CacheError, L1Cache};

/// A Redis-backed L1 exact-match cache.
///
/// Construct via [`RedisL1Cache::connect`].
pub struct RedisL1Cache {
    /// Connection manager — cheap to clone; each command clones internally.
    conn: redis::aio::ConnectionManager,
    /// Key namespace prefix, e.g. `"tt:l1"`.
    namespace: String,
}

impl RedisL1Cache {
    /// Connect to the Redis instance at `url` and return a ready cache handle.
    ///
    /// `namespace` is prepended to every key as `{namespace}:{key}`.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::Redis`] if the URL is invalid or the initial
    /// connection attempt fails.
    pub async fn connect(url: &str, namespace: impl Into<String>) -> Result<Self, CacheError> {
        let client = redis::Client::open(url)?;
        let conn = redis::aio::ConnectionManager::new(client).await?;
        Ok(Self {
            conn,
            namespace: namespace.into(),
        })
    }

    /// Build the full namespaced key string.
    fn full_key(&self, key: &str) -> String {
        format!("{}:{}", self.namespace, key)
    }
}

#[async_trait]
impl L1Cache for RedisL1Cache {
    /// Fetch a cached value by `key`.  Returns `None` when the key is absent
    /// or has expired.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let full = self.full_key(key);
        // ConnectionManager requires &mut; clone is O(1) (Arc clone).
        let result: Option<Vec<u8>> = self.conn.clone().get(&full).await?;
        Ok(result)
    }

    /// Store `value` under `key` with a TTL of `ttl_secs` seconds.
    async fn set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), CacheError> {
        let full = self.full_key(key);
        let _: () = self.conn.clone().set_ex(&full, value, ttl_secs).await?;
        Ok(())
    }

    /// Delete `key` from the cache.  A no-op if the key does not exist.
    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let full = self.full_key(key);
        let _: () = self.conn.clone().del(&full).await?;
        Ok(())
    }
}
