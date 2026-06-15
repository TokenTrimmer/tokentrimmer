//! Redis-backed [`L1Cache`] implementation.
//!
//! [`RedisL1Cache`] wraps a [`redis::aio::ConnectionManager`] (which handles
//! automatic reconnection) and prefixes every key with a configurable
//! namespace string (e.g. `"tt:l1"`) so multiple environments can share a
//! single Redis instance without key collisions.
//!
//! # At-rest encryption (SEC-2)
//!
//! By default the L1 value is the plaintext serialized [`crate::L1Entry`]
//! envelope (which carries the verbatim provider response). Wiring a
//! [`ResponseCodec`] via [`RedisL1Cache::with_response_codec`] seals the value
//! bytes on [`set`](RedisL1Cache::set) and opens them on
//! [`get`](RedisL1Cache::get). The per-org key is derived from the org the
//! gateway namespaces into the L1 key (`"{org_id}:{request_hash}"`; see
//! [`org_from_l1_key`]) — the [`L1Cache`] trait carries no org argument, so the
//! key is the org signal. Legacy plaintext values stay readable (fail-open).
//!
//! [`L1Cache`]: crate::L1Cache
//! [`org_from_l1_key`]: crate::org_from_l1_key

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use redis::AsyncCommands;
use uuid::Uuid;

use crate::response_codec::{org_from_l1_key, L1Open, ResponseCodec};
use crate::{CacheError, L1Cache};

/// A Redis-backed L1 exact-match cache.
///
/// Construct via [`RedisL1Cache::connect`].
pub struct RedisL1Cache {
    /// Connection manager — cheap to clone; each command clones internally.
    conn: redis::aio::ConnectionManager,
    /// Key namespace prefix, e.g. `"tt:l1"`.
    namespace: String,
    /// Optional at-rest value codec (SEC-2). `None` = plaintext (default).
    response_codec: Option<ResponseCodec>,
    /// Orgs whose writes are dropped (the per-org "do not cache" hook).
    no_cache_orgs: Arc<HashSet<Uuid>>,
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
            response_codec: None,
            no_cache_orgs: Arc::new(HashSet::new()),
        })
    }

    /// Enable at-rest encryption of L1 values (SEC-2). The per-org key is
    /// derived from the org parsed out of the (gateway-namespaced) L1 key. See
    /// the module docs. Default (un-wired) is plaintext, identical to today.
    /// Wiring this ON at the gateway is a one-line follow-up in `tt-cli`.
    #[must_use]
    pub fn with_response_codec(mut self, codec: ResponseCodec) -> Self {
        self.response_codec = Some(codec);
        self
    }

    /// Skip caching entirely for `orgs` (the per-org "do not cache" hook):
    /// [`set`](RedisL1Cache::set) is a silent no-op for a key whose namespaced
    /// org (see [`org_from_l1_key`]) is listed.
    #[must_use]
    pub fn with_no_cache_orgs(mut self, orgs: HashSet<Uuid>) -> Self {
        self.no_cache_orgs = Arc::new(orgs);
        self
    }

    /// Build the full namespaced key string.
    fn full_key(&self, key: &str) -> String {
        format!("{}:{}", self.namespace, key)
    }
}

#[async_trait]
impl L1Cache for RedisL1Cache {
    /// Fetch a cached value by `key`.  Returns `None` when the key is absent
    /// or has expired. With a wired codec, decrypts the value; a sealed value
    /// that does not authenticate is treated as a miss, a legacy plaintext value
    /// is returned as-is.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let full = self.full_key(key);
        // ConnectionManager requires &mut; clone is O(1) (Arc clone).
        let result: Option<Vec<u8>> = self.conn.clone().get(&full).await?;
        let Some(bytes) = result else {
            return Ok(None);
        };
        let Some(codec) = self.response_codec.as_ref() else {
            return Ok(Some(bytes));
        };
        match codec.open_l1_value(org_from_l1_key(key), key, &bytes) {
            L1Open::Plaintext => Ok(Some(bytes)),
            L1Open::Decrypted(plain) => Ok(Some(plain)),
            // Sealed but unreadable (wrong key) → treat as a miss.
            L1Open::Undecryptable => Ok(None),
        }
    }

    /// Store `value` under `key` with a TTL of `ttl_secs` seconds. With a wired
    /// codec, the value is sealed at rest before storage.
    async fn set(&self, key: &str, value: &[u8], ttl_secs: u64) -> Result<(), CacheError> {
        let org_id = org_from_l1_key(key);
        // Per-org "do not cache" hook.
        if self.no_cache_orgs.contains(&org_id) {
            return Ok(());
        }
        let sealed;
        let payload: &[u8] = match self.response_codec.as_ref() {
            Some(codec) => {
                sealed = codec.seal_l1_value(org_id, key, value)?;
                &sealed
            }
            None => value,
        };
        let full = self.full_key(key);
        let _: () = self.conn.clone().set_ex(&full, payload, ttl_secs).await?;
        Ok(())
    }

    /// Delete `key` from the cache.  A no-op if the key does not exist.
    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let full = self.full_key(key);
        let _: () = self.conn.clone().del(&full).await?;
        Ok(())
    }
}
