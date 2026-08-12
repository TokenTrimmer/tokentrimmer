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
use crate::{CacheError, L1Cache, L1PurgeResult};

/// Purge fences outlive the longest accepted L1 TTL (30 days) so an unindexed
/// pre-rollout value cannot become readable again after its account is erased.
const ORG_PURGE_FENCE_SECS: u64 = 35 * 24 * 60 * 60;
const PURGE_KEYS_PER_BATCH: usize = 1_000;
const PURGE_BATCHES_PER_CALL: usize = 10;

const INDEXED_GET_LUA: &str = r#"
if redis.call('EXISTS', KEYS[2]) == 1 then
  return nil
end
return redis.call('GET', KEYS[1])
"#;

const INDEXED_SET_LUA: &str = r#"
if redis.call('EXISTS', KEYS[3]) == 1 then
  return 0
end
local ttl = tonumber(ARGV[2])
local now = tonumber(ARGV[3])
redis.call('SET', KEYS[1], ARGV[1], 'EX', ttl)
redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now)
redis.call('ZADD', KEYS[2], now + ttl, KEYS[1])
local index_ttl = redis.call('TTL', KEYS[2])
if index_ttl < ttl + 86400 then
  redis.call('EXPIRE', KEYS[2], ttl + 86400)
end
return 1
"#;

const INDEXED_DELETE_LUA: &str = r#"
local deleted = redis.call('DEL', KEYS[1])
redis.call('ZREM', KEYS[2], KEYS[1])
return deleted
"#;

const ORG_PURGE_LUA: &str = r#"
redis.call('SET', KEYS[2], '1', 'EX', ARGV[1])
local members = redis.call('ZRANGE', KEYS[1], 0, tonumber(ARGV[2]) - 1)
local deleted = 0
if #members > 0 then
  deleted = redis.call('DEL', unpack(members))
  redis.call('ZREM', KEYS[1], unpack(members))
end
local remaining = redis.call('ZCARD', KEYS[1])
if remaining == 0 then
  redis.call('DEL', KEYS[1])
end
return {remaining == 0 and 1 or 0, deleted}
"#;

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

    /// Clone the multiplexed connection manager for another atomic Redis
    /// primitive in the same process. This reuses the existing socket pool.
    #[must_use]
    pub fn connection_manager(&self) -> redis::aio::ConnectionManager {
        self.conn.clone()
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

    /// Build the legacy pre-index key. New tenant values deliberately use a
    /// Redis hash tag so value/index/fence scripts remain cluster-safe.
    fn legacy_full_key(&self, key: &str) -> String {
        format!("{}:{}", self.namespace, key)
    }

    fn full_key(&self, key: &str, org_id: Uuid) -> String {
        if org_id.is_nil() {
            self.legacy_full_key(key)
        } else {
            format!("{}:{{{org_id}}}:{key}", self.namespace)
        }
    }

    fn org_index_key(&self, org_id: Uuid) -> String {
        format!("{}:{{{org_id}}}:org-index:v1", self.namespace)
    }

    fn org_fence_key(&self, org_id: Uuid) -> String {
        format!("{}:{{{org_id}}}:org-purge-fence:v1", self.namespace)
    }
}

#[async_trait]
impl L1Cache for RedisL1Cache {
    /// Fetch a cached value by `key`.  Returns `None` when the key is absent
    /// or has expired. With a wired codec, decrypts the value; a sealed value
    /// that does not authenticate is treated as a miss, a legacy plaintext value
    /// is returned as-is.
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let org_id = org_from_l1_key(key);
        let full = self.full_key(key, org_id);
        // ConnectionManager requires &mut; clone is O(1) (Arc clone).
        let result: Option<Vec<u8>> = if org_id.is_nil() {
            self.conn.clone().get(&full).await?
        } else {
            redis::Script::new(INDEXED_GET_LUA)
                .key(&full)
                .key(self.org_fence_key(org_id))
                .invoke_async(&mut self.conn.clone())
                .await?
        };
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
        let full = self.full_key(key, org_id);
        if org_id.is_nil() {
            let _: () = self.conn.clone().set_ex(&full, payload, ttl_secs).await?;
        } else {
            let now = chrono::Utc::now().timestamp();
            let _: i64 = redis::Script::new(INDEXED_SET_LUA)
                .key(&full)
                .key(self.org_index_key(org_id))
                .key(self.org_fence_key(org_id))
                .arg(payload)
                .arg(ttl_secs)
                .arg(now)
                .invoke_async(&mut self.conn.clone())
                .await?;
        }
        Ok(())
    }

    /// Delete `key` from the cache.  A no-op if the key does not exist.
    async fn delete(&self, key: &str) -> Result<(), CacheError> {
        let org_id = org_from_l1_key(key);
        let full = self.full_key(key, org_id);
        if org_id.is_nil() {
            let _: () = self.conn.clone().del(&full).await?;
        } else {
            let _: i64 = redis::Script::new(INDEXED_DELETE_LUA)
                .key(&full)
                .key(self.org_index_key(org_id))
                .invoke_async(&mut self.conn.clone())
                .await?;
            // Best-effort removal of the pre-index rollout key. New readers do
            // not fall back to it, and the account fence covers indexed reads.
            let _: () = self.conn.clone().del(self.legacy_full_key(key)).await?;
        }
        Ok(())
    }

    async fn purge_org(&self, org_id: Uuid) -> Result<L1PurgeResult, CacheError> {
        if org_id.is_nil() {
            return Ok(L1PurgeResult {
                complete: true,
                deleted: 0,
            });
        }
        let index = self.org_index_key(org_id);
        let fence = self.org_fence_key(org_id);
        let mut deleted = 0usize;
        for _ in 0..PURGE_BATCHES_PER_CALL {
            let result: (i64, i64) = redis::Script::new(ORG_PURGE_LUA)
                .key(&index)
                .key(&fence)
                .arg(ORG_PURGE_FENCE_SECS)
                .arg(PURGE_KEYS_PER_BATCH)
                .invoke_async(&mut self.conn.clone())
                .await?;
            deleted = deleted.saturating_add(usize::try_from(result.1).unwrap_or(usize::MAX));
            if result.0 == 1 {
                return Ok(L1PurgeResult {
                    complete: true,
                    deleted,
                });
            }
        }
        Ok(L1PurgeResult {
            complete: false,
            deleted,
        })
    }
}
