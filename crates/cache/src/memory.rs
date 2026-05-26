//! In-memory [`L1Cache`] implementation for unit tests.
//!
//! [`InMemoryL1Cache`] uses a [`DashMap`] so that it can be shared across
//! tasks without a `Mutex`.  Expiry is checked lazily on [`get`]: expired
//! entries are removed and `None` is returned.
//!
//! This implementation is **not** suitable for production use — it has no
//! background eviction, no memory bounds, and no persistence.
//!
//! [`L1Cache`]: crate::L1Cache
//! [`DashMap`]: dashmap::DashMap
//! [`get`]: InMemoryL1Cache::get

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;

use crate::{CacheError, L1Cache};

/// An in-memory L1 cache backed by a [`DashMap`].
///
/// Cheap to clone — the inner map is reference-counted.
///
/// # Example
///
/// ```rust
/// use tt_cache::memory::InMemoryL1Cache;
/// use tt_cache::L1Cache;
///
/// # #[tokio::main]
/// # async fn main() {
/// let cache = InMemoryL1Cache::new();
/// cache.set("k", b"hello", 60).await.unwrap();
/// assert_eq!(cache.get("k").await.unwrap(), Some(b"hello".to_vec()));
/// # }
/// ```
#[derive(Clone)]
pub struct InMemoryL1Cache {
    inner: Arc<DashMap<String, (Vec<u8>, Instant)>>,
}

impl InMemoryL1Cache {
    /// Create a new, empty cache.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }
}

impl Default for InMemoryL1Cache {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl L1Cache for InMemoryL1Cache {
    /// Retrieve a cached value.  Expired entries are evicted and `None` is
    /// returned.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        if let Some(entry) = self.inner.get(key) {
            let (value, expires_at) = entry.value();
            if Instant::now() < *expires_at {
                return Ok(Some(value.clone()));
            }
            // Entry has expired — drop the read guard before removing.
            drop(entry);
            self.inner.remove(key);
        }
        Ok(None)
    }

    /// Store a value with the given TTL.  Any existing entry for `key` is
    /// replaced.
    async fn set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), CacheError> {
        let expires_at = Instant::now() + Duration::from_secs(ttl_secs);
        self.inner
            .insert(key.to_owned(), (value.to_vec(), expires_at));
        Ok(())
    }

    /// Remove `key` from the cache.  A no-op when the key is absent.
    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        self.inner.remove(key);
        Ok(())
    }
}
