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

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::response_codec::{org_from_l1_key, L1Open, ResponseCodec};
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
///
/// Like [`RedisL1Cache`](crate::redis_impl::RedisL1Cache) it can opt into
/// at-rest value encryption via [`with_response_codec`](Self::with_response_codec)
/// (SEC-2), deriving the per-org key from the gateway-namespaced L1 key.
#[derive(Clone)]
pub struct InMemoryL1Cache {
    inner: Arc<DashMap<String, (Vec<u8>, Instant)>>,
    /// Optional at-rest value codec (SEC-2). `None` = plaintext (default).
    response_codec: Option<ResponseCodec>,
    /// Orgs whose writes are dropped (the per-org "do not cache" hook).
    no_cache_orgs: Arc<HashSet<Uuid>>,
}

impl InMemoryL1Cache {
    /// Create a new, empty cache.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            response_codec: None,
            no_cache_orgs: Arc::new(HashSet::new()),
        }
    }

    /// Enable at-rest encryption of L1 values (SEC-2). The per-org key is
    /// derived from the org parsed out of the (gateway-namespaced) L1 key.
    /// Default (un-wired) is plaintext, identical to today.
    #[must_use]
    pub fn with_response_codec(mut self, codec: ResponseCodec) -> Self {
        self.response_codec = Some(codec);
        self
    }

    /// Skip caching entirely for `orgs` (the per-org "do not cache" hook):
    /// [`set`](InMemoryL1Cache::set) is a silent no-op for a key whose
    /// namespaced org is listed.
    #[must_use]
    pub fn with_no_cache_orgs(mut self, orgs: HashSet<Uuid>) -> Self {
        self.no_cache_orgs = Arc::new(orgs);
        self
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
    /// returned. With a wired codec, decrypts the value (a sealed value that
    /// does not authenticate is a miss; a legacy plaintext value is returned
    /// as-is).
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let stored = {
            if let Some(entry) = self.inner.get(key) {
                let (value, expires_at) = entry.value();
                if Instant::now() < *expires_at {
                    Some(value.clone())
                } else {
                    drop(entry);
                    self.inner.remove(key);
                    None
                }
            } else {
                None
            }
        };
        let Some(bytes) = stored else {
            return Ok(None);
        };
        let Some(codec) = self.response_codec.as_ref() else {
            return Ok(Some(bytes));
        };
        match codec.open_l1_value(org_from_l1_key(key), key, &bytes) {
            L1Open::Plaintext => Ok(Some(bytes)),
            L1Open::Decrypted(plain) => Ok(Some(plain)),
            L1Open::Undecryptable => Ok(None),
        }
    }

    /// Store a value with the given TTL.  Any existing entry for `key` is
    /// replaced. With a wired codec, the value is sealed at rest first.
    async fn set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), CacheError> {
        let org_id = org_from_l1_key(key);
        if self.no_cache_orgs.contains(&org_id) {
            return Ok(());
        }
        let payload = match self.response_codec.as_ref() {
            Some(codec) => codec.seal_l1_value(org_id, key, value)?,
            None => value.to_vec(),
        };
        let expires_at = Instant::now() + Duration::from_secs(ttl_secs);
        self.inner.insert(key.to_owned(), (payload, expires_at));
        Ok(())
    }

    /// Remove `key` from the cache.  A no-op when the key is absent.
    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        self.inner.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_for(org: Uuid) -> String {
        format!("{org}:deadbeefhash")
    }

    /// Encrypt-on-write / decrypt-on-read round-trip for L1: a wired codec seals
    /// the value (the serialized L1 envelope) at rest and `get` opens it,
    /// returning the original bytes; the stored bytes never contain the plaintext.
    #[tokio::test]
    async fn l1_codec_round_trips_value() {
        let org = Uuid::from_u128(42);
        let cache = InMemoryL1Cache::new().with_response_codec(ResponseCodec::new([3u8; 32]));
        let key = key_for(org);
        let value = br#"{"response":{"choices":[{"message":{"content":"secret"}}]}}"#;

        cache.set(&key, value, 60).await.unwrap();

        // Stored bytes are an encrypted envelope, not the plaintext.
        let raw = cache.inner.get(&key).unwrap().value().0.clone();
        assert!(ResponseCodec::is_encrypted_l1(&raw));
        assert!(!raw.windows(6).any(|w| w == b"secret"));

        assert_eq!(cache.get(&key).await.unwrap(), Some(value.to_vec()));
    }

    /// A pre-codec plaintext value stays readable when a codec is later wired
    /// (fail-open back-compat).
    #[tokio::test]
    async fn l1_codec_reads_legacy_plaintext() {
        let org = Uuid::from_u128(42);
        let key = key_for(org);
        let value = br#"{"response":{},"version":1}"#;

        // Write plaintext (no codec), then read through a codec-wired cache that
        // shares the same backing map.
        let plain = InMemoryL1Cache::new();
        plain.set(&key, value, 60).await.unwrap();
        let encrypted = InMemoryL1Cache {
            inner: plain.inner.clone(),
            response_codec: Some(ResponseCodec::new([4u8; 32])),
            no_cache_orgs: Arc::new(HashSet::new()),
        };
        assert_eq!(encrypted.get(&key).await.unwrap(), Some(value.to_vec()));
    }

    /// A sealed value that does not authenticate under the reader's key is a
    /// miss (never served as garbage).
    #[tokio::test]
    async fn l1_wrong_key_is_a_miss() {
        let org = Uuid::from_u128(42);
        let key = key_for(org);
        let writer = InMemoryL1Cache::new().with_response_codec(ResponseCodec::new([5u8; 32]));
        writer.set(&key, b"payload", 60).await.unwrap();

        let reader = InMemoryL1Cache {
            inner: writer.inner.clone(),
            response_codec: Some(ResponseCodec::new([6u8; 32])),
            no_cache_orgs: Arc::new(HashSet::new()),
        };
        assert_eq!(reader.get(&key).await.unwrap(), None);
    }

    /// The per-org "do not cache" hook drops L1 writes for listed orgs.
    #[tokio::test]
    async fn l1_no_cache_org_skips_write() {
        let blocked = Uuid::from_u128(99);
        let cache = InMemoryL1Cache::new().with_no_cache_orgs(HashSet::from([blocked]));
        let key = key_for(blocked);
        cache.set(&key, b"payload", 60).await.unwrap();
        assert_eq!(cache.get(&key).await.unwrap(), None);
    }
}
