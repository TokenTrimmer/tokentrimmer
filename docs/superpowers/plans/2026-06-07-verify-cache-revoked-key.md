# Verify-cache revoked-key eviction hook Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a key_id-based eviction hook to the verify cache so a revoked key can be invalidated immediately (revocation knows the key_id, not the token plaintext).

**Architecture:** Add `KeyVerifyCache::evict_key_id(key_id)` — a `retain` that drops positive entries whose `ApiKeyContext.key_id` matches. After eviction the next request misses → full argon2 verify → store reports revoked → 401. Public mechanism only; the cloud revoke route wires it (separate slice).

**Tech Stack:** Rust, `dashmap`, `uuid`, `crates/core/src/middleware/key_cache.rs` (+ an end-to-end test in `auth.rs`).

Spec: `docs/superpowers/specs/2026-06-07-verify-cache-revoked-key-design.md`

> **CI note (`ci-verify-all-targets`):** before push run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-run`.

---

### Task 1: `evict_key_id` hook + unit tests + doc

**Files:**
- Modify: `crates/core/src/middleware/key_cache.rs`

- [ ] **Step 1: Write the failing unit tests**

In `crates/core/src/middleware/key_cache.rs`, add these tests inside the `#[cfg(test)] mod tests` block, right after the existing `evict_removes_entry` test (which sits under the `// Eviction` banner):

```rust
    #[test]
    fn evict_key_id_removes_matching_hit() {
        let (cache, _clock) = cache_with_fake_clock();
        let c = ctx();
        let hash = hash_token("tt_live_by_keyid");
        cache.insert_hit(hash, c.clone());
        assert!(matches!(cache.get(&hash), CacheLookup::Hit(_)));
        cache.evict_key_id(c.key_id);
        assert!(matches!(cache.get(&hash), CacheLookup::Miss));
    }

    #[test]
    fn evict_key_id_is_targeted() {
        let (cache, _clock) = cache_with_fake_clock();
        let a = ctx();
        let b = ctx();
        let ha = hash_token("tt_live_keyA");
        let hb = hash_token("tt_live_keyB");
        cache.insert_hit(ha, a.clone());
        cache.insert_hit(hb, b.clone());

        cache.evict_key_id(a.key_id);

        assert!(matches!(cache.get(&ha), CacheLookup::Miss), "A should be evicted");
        assert!(matches!(cache.get(&hb), CacheLookup::Hit(_)), "B should remain");
    }

    #[test]
    fn evict_key_id_leaves_negative_entries() {
        let (cache, _clock) = cache_with_fake_clock();
        let hn = hash_token("tt_live_wrong_secret");
        cache.insert_failure(hn);
        assert!(matches!(cache.get(&hn), CacheLookup::Failure));
        // A negative entry carries no key_id — eviction by any key_id leaves it.
        cache.evict_key_id(Uuid::new_v4());
        assert!(matches!(cache.get(&hn), CacheLookup::Failure));
    }
```

- [ ] **Step 2: Run to confirm they fail**

Run: `cargo test -p tt-core --lib middleware::key_cache::tests 2>&1 | tail -15`
Expected: FAIL — compile error: no method `evict_key_id` on `KeyVerifyCache`.

- [ ] **Step 3: Add the `Uuid` import**

At the top of `crates/core/src/middleware/key_cache.rs`, in the `use` block (currently `use dashmap::DashMap;` / `use tt_auth::ApiKeyContext;`), add:

```rust
use uuid::Uuid;
```

- [ ] **Step 4: Add the `evict_key_id` method**

In the `impl<C: Clock> KeyVerifyCache<C>` block, immediately after the existing `evict` method (`pub fn evict(&self, hash: &TokenHash) { self.map.remove(hash); }`), add:

```rust
    /// Evict every cached positive entry for `key_id`. Called when a key is
    /// revoked: revocation knows the key_id (not the token plaintext), so the
    /// `blake3(token)` cache key cannot be recomputed — we match on the cached
    /// `ApiKeyContext.key_id` instead. After eviction the next request for that
    /// key is a cache miss → full argon2 verify → store reports revoked → 401.
    ///
    /// Negative entries carry no key_id and are left as-is (they expire in
    /// `NEGATIVE_TTL_SECS`). O(n) over a bounded cache; revocation is rare.
    pub fn evict_key_id(&self, key_id: Uuid) {
        self.map
            .retain(|_, entry| !matches!(entry, CacheEntry::Hit { ctx, .. } if ctx.key_id == key_id));
    }
```

(Confirm the surrounding `impl` block is the one bounded `impl<C: Clock> KeyVerifyCache<C>` that also holds `get`/`insert_hit`/`evict` — place `evict_key_id` next to `evict`. If `evict` lives in a different `impl` block, add `evict_key_id` into the same block as `evict`.)

- [ ] **Step 5: Run to confirm they pass**

Run: `cargo test -p tt-core --lib middleware::key_cache::tests 2>&1 | tail -15`
Expected: PASS — all `key_cache::tests` green (the three new + all existing).

- [ ] **Step 6: Update the module doc "Revocation staleness" section**

In the top module doc-comment of `crates/core/src/middleware/key_cache.rs`, replace the `## Revocation staleness` block:

```rust
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
```

with:

```rust
//! ## Revocation staleness
//!
//! Because we skip argon2 for cached-positive entries, a key that is revoked
//! in the store would otherwise remain usable for up to `POSITIVE_TTL_SECS`
//! seconds from its last cache insertion.
//!
//! [`KeyVerifyCache::evict_key_id`] closes that window on the instance that
//! handles the revoke: the key-revocation path (the cloud admin route) must
//! call `verify_cache.evict_key_id(key_id)` after a successful `revoke_key`.
//! Revocation knows the key_id but never the token plaintext, so eviction is
//! by key_id (matching the cached `ApiKeyContext`), not by `blake3(token)`.
//! After eviction the next request re-runs the full argon2 verify, which the
//! revoked store row then rejects.
//!
//! **Multi-instance:** an in-process evict only invalidates the instance that
//! processed the revoke; other gateway instances remain bounded by
//! `POSITIVE_TTL_SECS` until a cross-instance invalidation channel (e.g. a
//! Redis pub/sub `keyrevoked` event) is added. That channel is the remaining
//! future work and is out of scope for this crate.
```

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/middleware/key_cache.rs
git commit -m "feat(auth): add evict_key_id hook to verify cache for revocation

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: End-to-end test — revoke + `evict_key_id` rejects next request

**Files:**
- Modify: `crates/core/src/middleware/auth.rs`

The existing test `revoked_key_accepted_within_positive_ttl_then_rejected_after_eviction` (in `auth.rs`'s `#[cfg(test)] mod tests`) proves the by-hash eviction path. This task adds a sibling that exercises the new by-**key_id** hook, proving the revoke route's intended call rejects the next request and forces re-verification.

- [ ] **Step 1: Write the end-to-end test**

In `crates/core/src/middleware/auth.rs`, add this test immediately after `revoked_key_accepted_within_positive_ttl_then_rejected_after_eviction` (it reuses the same helpers in that module: `CountingKeyStore`, `seed_key`, `live_bearer`, `build_router`, `hash_token` is NOT needed here):

```rust
    #[tokio::test]
    async fn revoked_key_rejected_after_evict_by_key_id() {
        let org = Uuid::new_v4();
        let plaintext = "tt_live_revoke_by_id99";

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext).await;

        let cache = Arc::new(KeyVerifyCache::new());

        let mut app = AppState::new(crate::registry::ProviderRegistry::new());
        app.verify_cache = cache.clone();
        app.key_store = Some(store.clone());
        let router = build_router(app);

        // First request — cache miss → argon2 → 200; warms the positive entry.
        let r1 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r1");
        assert_eq!(r1.status(), StatusCode::OK, "pre-revoke should succeed");
        assert_eq!(store.find_count.load(Ordering::SeqCst), 1);

        // Revoke in the store + evict the cache BY KEY_ID (what the revoke route does).
        let (key_id, key_org_id) = {
            let g = store.by_prefix.lock().unwrap();
            let k = g.get(&plaintext[..12]).unwrap();
            (k.id, k.org_id)
        };
        store
            .revoke(key_id, key_org_id, Utc::now())
            .await
            .expect("revoke");
        cache.evict_key_id(key_id);

        // Next request — cache miss (evicted) → argon2 re-runs → revoked → 401.
        let r2 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r2");
        assert_eq!(
            r2.status(),
            StatusCode::UNAUTHORIZED,
            "after evict_key_id, revoked key must be rejected"
        );
        // find_by_prefix ran a second time (proves re-verification, not a cache hit).
        assert_eq!(store.find_count.load(Ordering::SeqCst), 2);
    }
```

- [ ] **Step 2: Run to confirm it passes**

Run: `cargo test -p tt-core --lib middleware::auth::tests::revoked_key_rejected_after_evict_by_key_id 2>&1 | tail -15`
Expected: PASS. (If it fails to compile because `KeyVerifyCache`/`hash_token`/`Arc`/`Ordering`/`Utc` aren't in scope, mirror the imports already used by the adjacent `revoked_key_accepted_..._after_eviction` test — they are in the same module, so `use super::*` plus that test's existing imports cover it. Do NOT add new top-level imports beyond what that sibling test already relies on.)

- [ ] **Step 3: Run the full auth + key_cache modules**

Run: `cargo test -p tt-core --lib middleware::auth middleware::key_cache 2>&1 | tail -15`
Expected: PASS — both modules green.

- [ ] **Step 4: Gates**

Run: `cargo clippy -p tt-core --all-targets -- -D warnings 2>&1 | grep -v "Permission denied\|auto-clean" | tail -15`
Expected: no warnings. (`evict_key_id` is `pub`, so no dead-code warning despite no in-crate caller.)

Run: `cargo fmt -p tt-core -- --check 2>&1 | tail -5`
Expected: clean. If a diff, run `cargo fmt -p tt-core`.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/middleware/auth.rs
git commit -m "test(auth): revoked key rejected after evict_key_id (e2e)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)

Per `ci-verify-all-targets`:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
cargo test -p tt-core
```
Expected: all clean/green.

## Notes for the implementer
- `evict_key_id` must live in the SAME `impl` block as the existing `evict` (the `impl<C: Clock> KeyVerifyCache<C>` block) so it has access to `self.map`.
- `CacheEntry::Hit { ctx, .. }` — `ctx` is the `ApiKeyContext`; match its `.key_id`. Negative entries are `CacheEntry::Failure { .. }` (no key_id) and must be retained.
- Stage only the one file per task; no whole-workspace `cargo fmt`.
- This is the public mechanism only — wiring into the cloud revoke route + multi-instance propagation are a separate cloud slice.
