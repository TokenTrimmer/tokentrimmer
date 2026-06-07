# Verify-cache revoked-key eviction hook — Design

**Status:** approved (design)
**Date:** 2026-06-07
**Slice:** Audit-remediation Wave 3 (public repo, `crates/core/src/middleware/key_cache.rs`). Closes the public half of the security/medium finding: a revoked API key keeps authenticating for up to the positive-TTL window (60 s) because `revoke_key` does not invalidate the in-process verify cache.

## Problem & constraint

`auth.rs` caches a successful argon2 verification as a positive `ApiKeyContext` for `POSITIVE_TTL_SECS = 60`, keyed by `blake3(token)`. `revoke_key` (`crates/auth/src/keys.rs`) marks the store row revoked but never evicts the cache, so a warm entry keeps returning 200 for up to 60 s per gateway instance.

**Hard constraint:** the existing `KeyVerifyCache::evict(token_hash)` is unusable at revoke time — revocation only ever knows the **key_id**, never the token plaintext, so `blake3(token)` cannot be recomputed. The positive cache entry *does* carry `ApiKeyContext { key_id, .. }`, so entries are evictable **by key_id**.

**Repo split (forced):** the public repo is a library with **no key-revocation route** (key management lives in the cloud), and the cloud cannot edit this path-dependency crate. So:
- **This (public) slice:** add the key_id-based eviction *mechanism* + docs + tests.
- **Cloud slice (tracked follow-up):** call the hook from the cloud revoke route + add a multi-instance revocation channel.

## Architecture (`crates/core/src/middleware/key_cache.rs`)

### Eviction hook
```rust
/// Evict every cached positive entry for `key_id`. Called when a key is
/// revoked: revocation knows the key_id (not the token plaintext), so the
/// blake3(token) cache key cannot be recomputed — we match on the cached
/// `ApiKeyContext.key_id` instead. After eviction the next request for that
/// key is a cache miss → full argon2 verify → store reports revoked → 401.
///
/// Negative entries carry no key_id and are left as-is (they expire in
/// `NEGATIVE_TTL_SECS`). O(n) over a bounded cache; revocation is rare.
pub fn evict_key_id(&self, key_id: Uuid) {
    self.map.retain(|_, entry| {
        !matches!(entry, CacheEntry::Hit { ctx, .. } if ctx.key_id == key_id)
    });
}
```
- `Uuid` is already in scope (via `tt_auth::ApiKeyContext`'s `key_id: Uuid`); add `use uuid::Uuid;` if not already imported.
- Marked `pub` so the cloud (the consuming binary) can call it; being `pub` it does not trip `dead_code` despite having no in-crate caller yet.
- The existing `evict(token_hash)` stays (test helper / by-hash path).

### Why a single evict is sufficient (no persistent revoked-set)
After `evict_key_id` removes the positive entry, the next request for that token is a cache miss → cold argon2 path → the store (now revoked) returns failure → a **negative** cache entry is written and 401 returned. The store is the source of truth on the cold path, so the key cannot re-enter the positive cache. A one-shot eviction therefore fully closes the window on that instance — no generation counter / revoked-id set needed.

## Data flow (intended end-to-end, completed by the cloud slice)
`admin revokes key_id` → cloud revoke route calls `revoke_key(store, audit, key_id, org)` → on success calls `state.verify_cache.evict_key_id(key_id)` → that instance rejects immediately; other instances rely on the multi-instance channel (cloud follow-up) or the 60 s TTL as the backstop.

## Error handling
None — `evict_key_id` is infallible (a `retain` over a `DashMap`). Concurrent inserts during eviction are safe (DashMap per-shard locking); a request mid-verify that re-inserts after eviction would only re-insert a *positive* entry if the store still said OK, which it won't post-revoke.

## Documentation
Update the `key_cache.rs` module doc "## Revocation staleness" section:
- Document `evict_key_id(key_id)` as the revoke-time hook and that the cloud revoke route must call it after a successful `revoke_key`.
- State the single-instance semantics (instant on the instance that handles the revoke; other instances bounded by `POSITIVE_TTL_SECS` until the multi-instance channel lands).
- Keep the existing "Future work: cross-instance invalidation channel (Redis pub/sub)" note, now framed as the multi-instance completion of this hook.

## Testing (`key_cache.rs` unit tests + one `auth.rs` end-to-end test)
- `evict_key_id` removes the matching positive entry: insert a hit for `key_id = X` (hash H), `evict_key_id(X)`, assert `get(&H) == Miss`.
- `evict_key_id` is targeted: insert hits for `X` (hash H1) and `Y` (hash H2); `evict_key_id(X)`; assert `get(&H1) == Miss` and `get(&H2) == Hit`.
- `evict_key_id` leaves negative entries: `insert_failure(Hn)`, `evict_key_id(anything)`, assert `get(&Hn) == Failure`.
- End-to-end (mirror the existing `revoked_key_accepted_within_positive_ttl_then_rejected_after_eviction` in `auth.rs`, which currently evicts by hash): add a variant that revokes in the store, calls `cache.evict_key_id(key_id)`, and asserts the next request returns **401** and that argon2 re-ran (the store's verify counter incremented) — proving the by-key_id hook forces re-verification.

Gates: `cargo test -p tt-core`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all -- --check`; `cargo test --workspace --no-run` (per `ci-verify-all-targets`).

## Out of scope (→ cloud slice, tracked)
- Wiring `evict_key_id` into the cloud's key-revoke route.
- Multi-instance revocation propagation (Redis pub/sub or a DB revocation epoch) — eliminates the cross-instance window.
- Changing `POSITIVE_TTL_SECS` or making it tier-configurable (the latency/DoS tradeoff is deliberate; the hook makes same-instance revocation instant).
- Any change to `revoke_key`'s signature (the caller already holds the key_id it passed in).
