//! In-process TTL cache for API key verification results.
//!
//! ## Purpose
//!
//! Argon2 is deliberately slow (~100 ms on default params). Running it on every
//! authenticated request would:
//!
//! * Blow the sub-30 ms p50 overhead target for the gateway.
//! * Create a CPU-exhaustion amplification vector: an adversary who knows any
//!   live `tt_live_*` prefix can spam requests with wrong suffixes and force
//!   argon2 work on every one.
//!
//! This cache short-circuits argon2 for both successful and failed verifications
//! within their respective TTL windows.
//!
//! ## Cache key
//!
//! The cache is keyed by the **blake3 hash** of the presented bearer token.
//! The plaintext token and the argon2 hash are NEVER stored — only the non-secret
//! `ApiKeyContext` (org_id, key_id) on the positive side, and nothing on the
//! negative side (the failure fact itself is the value).
//!
//! ## TTLs
//!
//! * Positive (verified OK): [`POSITIVE_TTL_SECS`] = 60 s.
//! * Negative (wrong secret or not-found): [`NEGATIVE_TTL_SECS`] = 10 s.
//!
//! ## Revocation staleness
//!
//! Because we skip argon2 for cached-positive entries, a key that is revoked
//! in the store remains usable for up to `POSITIVE_TTL_SECS` seconds from its
//! last cache insertion. This is an explicit, documented tradeoff: we prioritise
//! p50 latency and DoS resistance over instant revocation propagation.
//!
//! **Future work:** a cross-instance invalidation channel (e.g. a Redis pub/sub
//! `keyrevoked` event) could shrink the staleness window to near-zero without
//! re-introducing per-request argon2 cost. That is out of scope here.
//!
//! ## Clock injection
//!
//! The cache accepts a `now_fn: impl Fn() -> std::time::Instant` at construction
//! time so tests can advance time deterministically without `thread::sleep`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tt_auth::ApiKeyContext;

/// TTL for a successfully-verified key (positive cache).
pub const POSITIVE_TTL_SECS: u64 = 60;

/// TTL for a failed verification (negative cache / DoS mitigation).
///
/// Kept short so a newly-issued or rotated key is usable quickly after a stale
/// failure entry expires.
pub const NEGATIVE_TTL_SECS: u64 = 10;

/// A single entry in the verify cache.
#[derive(Clone, Debug)]
enum CacheEntry {
    /// Argon2 said OK. Carries the non-secret context and the insertion instant.
    Hit {
        ctx: ApiKeyContext,
        inserted_at: Instant,
    },
    /// Argon2 said NO (wrong secret, not-found, revoked). Only stores insertion time.
    Failure { inserted_at: Instant },
}

impl CacheEntry {
    fn inserted_at(&self) -> Instant {
        match self {
            CacheEntry::Hit { inserted_at, .. } | CacheEntry::Failure { inserted_at } => {
                *inserted_at
            }
        }
    }
}

/// The result of a cache lookup — what the caller can do with it.
#[derive(Debug, Clone)]
pub enum CacheLookup {
    /// Cache says this token is valid; use the context, skip argon2.
    Hit(ApiKeyContext),
    /// Cache says this token recently failed; return 401, skip argon2.
    Failure,
    /// Cache miss — must run argon2.
    Miss,
}

type TokenHash = [u8; 32];

/// In-process verified-key cache with configurable clock.
///
/// Shared across requests via `Arc`; `DashMap` provides lock-free per-shard
/// concurrent access. The cache never stores the plaintext token or the argon2
/// hash string — only the `ApiKeyContext` (key_id, org_id) on hits, and nothing
/// but the timestamp on failures.
pub struct KeyVerifyCache<C = SystemClock> {
    map: DashMap<TokenHash, CacheEntry>,
    positive_ttl: Duration,
    negative_ttl: Duration,
    clock: C,
}

/// Marker trait for clock providers (makes `C` bounds readable).
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

/// Production clock — delegates to `Instant::now()`.
#[derive(Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock — wraps an `Arc<Mutex<Instant>>` so tests can advance time.
#[cfg(test)]
#[derive(Clone)]
pub struct FakeClock(pub Arc<std::sync::Mutex<Instant>>);

#[cfg(test)]
impl FakeClock {
    pub fn new(start: Instant) -> Self {
        Self(Arc::new(std::sync::Mutex::new(start)))
    }

    /// Advance the clock by `dur`.
    pub fn advance(&self, dur: Duration) {
        let mut t = self.0.lock().expect("FakeClock poisoned");
        *t += dur;
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.0.lock().expect("FakeClock poisoned")
    }
}

impl KeyVerifyCache<SystemClock> {
    /// Create a production cache with the system clock and default TTLs.
    pub fn new() -> Self {
        Self::with_clock(SystemClock)
    }
}

impl Default for KeyVerifyCache<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

/// Hash a bearer token to its 32-byte blake3 cache key.
///
/// This is a free function (not a method) so it can be called without knowing
/// the clock type parameter of [`KeyVerifyCache`]. The plaintext never appears
/// in the cache map — only this digest is stored as the key.
pub fn hash_token(token: &str) -> TokenHash {
    *blake3::hash(token.as_bytes()).as_bytes()
}

impl<C: Clock> KeyVerifyCache<C> {
    /// Create with a custom clock (useful for tests).
    pub fn with_clock(clock: C) -> Self {
        Self {
            map: DashMap::new(),
            positive_ttl: Duration::from_secs(POSITIVE_TTL_SECS),
            negative_ttl: Duration::from_secs(NEGATIVE_TTL_SECS),
            clock,
        }
    }

    /// Look up the cache for a given token hash.
    ///
    /// Expired entries are treated as misses (and lazily removed).
    pub fn get(&self, hash: &TokenHash) -> CacheLookup {
        let now = self.clock.now();
        if let Some(entry) = self.map.get(hash) {
            let age = now.duration_since(entry.inserted_at());
            match entry.value() {
                CacheEntry::Hit { ctx, .. } => {
                    if age <= self.positive_ttl {
                        return CacheLookup::Hit(ctx.clone());
                    }
                    // Expired — fall through to Miss (lazy eviction below)
                }
                CacheEntry::Failure { .. } => {
                    if age <= self.negative_ttl {
                        return CacheLookup::Failure;
                    }
                }
            }
        }
        CacheLookup::Miss
    }

    /// Insert a successful verification result.
    pub fn insert_hit(&self, hash: TokenHash, ctx: ApiKeyContext) {
        let inserted_at = self.clock.now();
        self.map.insert(
            hash,
            CacheEntry::Hit { ctx, inserted_at },
        );
    }

    /// Insert a failed verification (wrong secret, not found, revoked).
    pub fn insert_failure(&self, hash: TokenHash) {
        let inserted_at = self.clock.now();
        self.map.insert(hash, CacheEntry::Failure { inserted_at });
    }

    /// Remove a specific entry — used after explicit revocation to shorten
    /// the staleness window when the caller knows a key was just revoked.
    pub fn evict(&self, hash: &TokenHash) {
        self.map.remove(hash);
    }

    /// Return the number of entries currently in the map (including expired).
    /// Intended for metrics / tests only.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Return `true` when the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Type alias for the production cache used by [`AppState`].
pub type VerifyCache = Arc<KeyVerifyCache<SystemClock>>;

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tt_auth::ApiKeyContext;
    use uuid::Uuid;

    use super::*;

    fn ctx() -> ApiKeyContext {
        ApiKeyContext {
            key_id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
        }
    }

    fn cache_with_fake_clock() -> (KeyVerifyCache<FakeClock>, FakeClock) {
        let start = Instant::now();
        let clock = FakeClock::new(start);
        let cache = KeyVerifyCache::with_clock(clock.clone());
        (cache, clock)
    }

    // -----------------------------------------------------------------------
    // Basic hit / failure / miss
    // -----------------------------------------------------------------------

    #[test]
    fn hit_within_ttl_returns_hit() {
        let (cache, _clock) = cache_with_fake_clock();
        let hash = hash_token("tt_live_abc123");
        let c = ctx();
        cache.insert_hit(hash, c.clone());

        match cache.get(&hash) {
            CacheLookup::Hit(got) => {
                assert_eq!(got.key_id, c.key_id);
                assert_eq!(got.org_id, c.org_id);
            }
            other => panic!("expected Hit, got {other:?}"),
        }
    }

    #[test]
    fn failure_within_negative_ttl_returns_failure() {
        let (cache, _clock) = cache_with_fake_clock();
        let hash = hash_token("tt_live_bad");
        cache.insert_failure(hash);

        assert!(matches!(cache.get(&hash), CacheLookup::Failure));
    }

    #[test]
    fn unknown_hash_returns_miss() {
        let (cache, _clock) = cache_with_fake_clock();
        let hash = hash_token("tt_live_unknown");
        assert!(matches!(cache.get(&hash), CacheLookup::Miss));
    }

    // -----------------------------------------------------------------------
    // TTL expiry
    // -----------------------------------------------------------------------

    #[test]
    fn hit_expires_after_positive_ttl() {
        let (cache, clock) = cache_with_fake_clock();
        let hash = hash_token("tt_live_expiring");
        cache.insert_hit(hash, ctx());

        // Advance just past positive TTL.
        clock.advance(Duration::from_secs(POSITIVE_TTL_SECS + 1));

        assert!(matches!(cache.get(&hash), CacheLookup::Miss));
    }

    #[test]
    fn failure_expires_after_negative_ttl() {
        let (cache, clock) = cache_with_fake_clock();
        let hash = hash_token("tt_live_failexpire");
        cache.insert_failure(hash);

        // Advance just past negative TTL but within positive TTL (to show the
        // two TTLs are independent).
        clock.advance(Duration::from_secs(NEGATIVE_TTL_SECS + 1));

        assert!(matches!(cache.get(&hash), CacheLookup::Miss));
    }

    #[test]
    fn hit_still_valid_just_before_ttl() {
        let (cache, clock) = cache_with_fake_clock();
        let hash = hash_token("tt_live_edge");
        cache.insert_hit(hash, ctx());

        // Advance to exactly the TTL boundary (≤ positive_ttl is fresh).
        clock.advance(Duration::from_secs(POSITIVE_TTL_SECS));

        assert!(matches!(cache.get(&hash), CacheLookup::Hit(_)));
    }

    // -----------------------------------------------------------------------
    // Eviction
    // -----------------------------------------------------------------------

    #[test]
    fn evict_removes_entry() {
        let (cache, _clock) = cache_with_fake_clock();
        let hash = hash_token("tt_live_evict_me");
        cache.insert_hit(hash, ctx());
        assert!(matches!(cache.get(&hash), CacheLookup::Hit(_)));
        cache.evict(&hash);
        assert!(matches!(cache.get(&hash), CacheLookup::Miss));
    }

    // -----------------------------------------------------------------------
    // Token → hash is deterministic and doesn't store plaintext
    // -----------------------------------------------------------------------

    #[test]
    fn same_token_yields_same_hash() {
        let token = "tt_live_deterministic_token";
        let h1 = hash_token(token);
        let h2 = hash_token(token);
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_tokens_yield_different_hashes() {
        let h1 = hash_token("tt_live_aaa");
        let h2 = hash_token("tt_live_bbb");
        assert_ne!(h1, h2);
    }
}
