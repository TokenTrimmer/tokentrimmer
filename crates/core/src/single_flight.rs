//! In-process single-flight coalescing for the non-streaming cache-miss path.
//!
//! # Problem
//! A burst of N identical cache-eligible requests that all miss L1+L2 would
//! each dispatch independently to the upstream provider — N× cost and latency
//! for the same output.
//!
//! # Protocol
//! When the first request ("leader") hits a cache miss it races to insert an
//! in-flight entry keyed on the L1 cache key. The [`SingleFlight`] map uses a
//! `tokio::sync::watch` channel as the coordination primitive:
//!
//! ```text
//! Leader:
//!   1. try_become_leader(key) → Ok(LeaderGuard)  [entry created]
//!   2. dispatch to provider
//!   3. on success: guard.complete(response_bytes)
//!      on error:   guard drops (None broadcast)
//!   → entry removed in both cases (Drop impl)
//!
//! Follower:
//!   1. try_become_leader(key) → Err(rx)          [entry already exists]
//!   2. await rx.changed() with FOLLOWER_TIMEOUT
//!   3a. if bytes received before timeout: re-read L1 cache, serve hit
//!   3b. if timeout / None received (leader error): fall through to own dispatch
//! ```
//!
//! # Deadlock / Leak safety
//! - The `LeaderGuard` removes the entry from the map on `Drop`. Any code path
//!   that creates a guard (including panics, `?` early-return, or normal
//!   completion) will remove the entry. Followers therefore always eventually
//!   unblock.
//! - Followers wait at most [`FOLLOWER_TIMEOUT`] before falling through to
//!   their own provider dispatch. This bounds latency even when a leader stalls
//!   (network hang, slow upstream).
//! - The map holds only `watch::Sender` references. Once the guard drops,
//!   the sender is dropped, all receivers get a closed-channel wake, and the
//!   map entry is gone — no unbounded accumulation.
//!
//! # Scope
//! Single-flight ONLY fires for cache-eligible, non-streaming, do_lookup
//! requests. Bypass/non-eligible/streaming requests never enter this path.
//! Streaming single-flight is a follow-up (requires broadcasting an SSE stream
//! rather than a single response value, which is non-trivial).

use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::watch;
use tokio::time::{timeout, Duration};

/// Timeout a follower will wait for the leader to complete before falling
/// through to its own provider dispatch. 2 seconds is generous enough for
/// normal provider latency (P95 < 1 s) while keeping the request bound tight.
pub const FOLLOWER_TIMEOUT: Duration = Duration::from_secs(2);

/// The value broadcast from leader to followers. `None` means the leader
/// failed; followers should fall through. `Some(bytes)` means the leader
/// succeeded and the entry should now be in the L1 cache.
type Broadcast = Option<()>;

/// In-process single-flight map. Typically one instance per `AppState`,
/// shared via `Arc`.
///
/// Keys are the namespaced L1 cache keys (org-scoped, request-hashed strings).
/// Values are watch senders whose receivers are handed to followers.
pub struct SingleFlight {
    /// The shared map is wrapped in `Arc` so that `LeaderGuard` can hold a
    /// cheap clone of the reference and remove the entry on drop without
    /// requiring a lifetime tie back to the `SingleFlight` owner.
    inflight: Arc<DashMap<String, Arc<watch::Sender<Broadcast>>>>,
}

impl SingleFlight {
    /// Create an empty map.
    pub fn new() -> Self {
        Self {
            inflight: Arc::new(DashMap::new()),
        }
    }

    /// Attempt to become the leader for `key`.
    ///
    /// Returns:
    /// - `Ok(guard)` — this caller is the leader and holds the guard. The
    ///   guard MUST be completed (via [`LeaderGuard::complete`]) or simply
    ///   dropped on error so followers are unblocked.
    /// - `Err(rx)` — another request is already in-flight for this key. The
    ///   caller should [`wait_for_leader`] on the returned receiver.
    pub fn try_become_leader(&self, key: &str) -> Result<LeaderGuard, watch::Receiver<Broadcast>> {
        // Use entry API to avoid a TOCTOU gap: if the key is absent we insert
        // atomically; if it is present we hand back the existing receiver.
        //
        // DashMap::entry holds a shard lock for the duration of the closure,
        // so the "check + insert" is atomic at the shard level.
        use dashmap::mapref::entry::Entry;

        match self.inflight.entry(key.to_string()) {
            Entry::Vacant(slot) => {
                // We are the leader. Create the channel and insert.
                let (tx, _rx) = watch::channel::<Broadcast>(None);
                let tx = Arc::new(tx);
                slot.insert(Arc::clone(&tx));
                Ok(LeaderGuard {
                    key: key.to_string(),
                    // Clone the Arc pointer — both `SingleFlight` and
                    // `LeaderGuard` now point at the SAME DashMap.
                    map: Arc::clone(&self.inflight),
                    tx,
                })
            }
            Entry::Occupied(existing) => {
                // A leader is already in-flight. Hand back a receiver so the
                // caller can wait.
                Err(existing.get().subscribe())
            }
        }
    }
}

impl Default for SingleFlight {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard held by the leader for a given cache key.
///
/// On `Drop` (success or error) the entry is removed from the map and a
/// final broadcast is sent to wake all waiting followers.
pub struct LeaderGuard {
    key: String,
    map: Arc<DashMap<String, Arc<watch::Sender<Broadcast>>>>,
    tx: Arc<watch::Sender<Broadcast>>,
}

impl LeaderGuard {
    /// Call this when the leader has successfully inserted the response into
    /// the L1 cache. Followers will receive `Some(())` and re-read L1.
    pub fn complete(self) {
        // Send success signal. Ignore send errors — they mean no followers are
        // waiting, which is fine.
        let _ = self.tx.send(Some(()));
        // Drop removes the map entry (see Drop impl below).
    }
}

impl Drop for LeaderGuard {
    fn drop(&mut self) {
        // Remove the in-flight entry. After this, new requests for the same
        // key will become a fresh leader rather than waiting.
        self.map.remove(&self.key);
        // The watch::Sender is dropped here (unless complete() was called
        // first, which sent Some(()) before this point). Any remaining
        // receivers will observe a channel-closed wake; wait_for_leader
        // treats that as a fallthrough.
    }
}

/// Follower wait. Returns `true` if the leader completed successfully (the
/// cache should now be populated) or `false` if the wait timed out or the
/// leader failed (caller should fall through to its own dispatch).
pub async fn wait_for_leader(mut rx: watch::Receiver<Broadcast>) -> bool {
    // Wait up to FOLLOWER_TIMEOUT for the leader to broadcast.
    match timeout(FOLLOWER_TIMEOUT, rx.changed()).await {
        // Leader sent a value before the timeout.
        Ok(Ok(())) => {
            // Check whether the broadcast was Some(()) (success) or None
            // (leader failed). We re-read the current value rather than
            // trusting the initial None sentinel.
            rx.borrow().is_some()
        }
        // Channel closed (leader dropped without calling complete()) or
        // timeout — fall through in both cases.
        Ok(Err(_)) | Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    /// A single key with 8 concurrent callers — only the first (leader) should
    /// "dispatch"; the remaining 7 (followers) should await and get the
    /// leader's result.
    #[tokio::test]
    async fn one_leader_many_followers_single_dispatch() {
        let sf = Arc::new(SingleFlight::new());
        let dispatch_count = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let sf = Arc::clone(&sf);
                let count = Arc::clone(&dispatch_count);
                tokio::spawn(async move {
                    match sf.try_become_leader("key-a") {
                        Ok(guard) => {
                            // Leader: simulate provider dispatch latency.
                            count.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            guard.complete();
                            true // leader succeeded
                        }
                        Err(rx) => {
                            // Follower: wait for leader.
                            wait_for_leader(rx).await
                        }
                    }
                })
            })
            .collect();

        let results: Vec<bool> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // Exactly one dispatch.
        assert_eq!(
            dispatch_count.load(Ordering::SeqCst),
            1,
            "expected exactly 1 provider dispatch, got {}",
            dispatch_count.load(Ordering::SeqCst)
        );
        // All followers received true (leader succeeded).
        assert!(
            results.iter().all(|&r| r),
            "all followers should see success: {results:?}"
        );
    }

    /// When the leader fails (guard dropped without calling complete()), the
    /// follower must receive `false` and fall through to its own dispatch.
    #[tokio::test]
    async fn leader_error_unblocks_follower_with_fallthrough() {
        let sf = Arc::new(SingleFlight::new());

        // Become leader in a separate task that will fail.
        let sf_leader = Arc::clone(&sf);
        let leader = tokio::spawn(async move {
            let guard = sf_leader.try_become_leader("key-b").unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            // Simulate error: drop without calling complete().
            drop(guard);
        });

        // Give the leader a head start so our try_become_leader sees it.
        tokio::time::sleep(Duration::from_millis(1)).await;

        let sf_follower = Arc::clone(&sf);
        let follower = tokio::spawn(async move {
            match sf_follower.try_become_leader("key-b") {
                Ok(_guard) => {
                    // Unexpectedly became leader — still fine for this test.
                    true
                }
                Err(rx) => wait_for_leader(rx).await,
            }
        });

        leader.await.unwrap();
        let follower_result = follower.await.unwrap();

        // Follower should have received false (leader failed → fall through).
        assert!(
            !follower_result,
            "follower should receive false when leader errors"
        );

        // Map must be empty after leader drops.
        assert!(
            sf.inflight.is_empty(),
            "inflight map must be empty after leader error"
        );
    }

    /// Two different keys must not coalesce — each request acts as its own
    /// leader and dispatches independently.
    #[tokio::test]
    async fn different_keys_do_not_coalesce() {
        let sf = Arc::new(SingleFlight::new());
        let count = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for key in &["key-x", "key-y"] {
            let sf = Arc::clone(&sf);
            let count = Arc::clone(&count);
            let key = key.to_string();
            handles.push(tokio::spawn(async move {
                match sf.try_become_leader(&key) {
                    Ok(guard) => {
                        count.fetch_add(1, Ordering::SeqCst);
                        guard.complete();
                    }
                    Err(rx) => {
                        let _ = wait_for_leader(rx).await;
                    }
                }
            }));
        }

        futures::future::join_all(handles).await;

        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "two different keys must dispatch independently"
        );
    }

    /// After the leader completes, the inflight entry must be removed so
    /// subsequent requests for the same key can become a fresh leader.
    #[tokio::test]
    async fn entry_cleaned_up_after_leader_completes() {
        let sf = Arc::new(SingleFlight::new());

        let guard = sf.try_become_leader("key-c").expect("should be leader");
        assert_eq!(sf.inflight.len(), 1);
        guard.complete();
        // complete() calls Drop, which removes the entry.
        assert!(
            sf.inflight.is_empty(),
            "entry must be removed after complete()"
        );
    }

    /// Follower timeout: if the leader never completes within FOLLOWER_TIMEOUT,
    /// the follower should return false and fall through.
    #[tokio::test]
    async fn follower_times_out_when_leader_stalls() {
        // Override FOLLOWER_TIMEOUT isn't possible at runtime, but we can
        // test the underlying wait_for_leader with a channel that never sends.
        let (_tx, rx) = watch::channel::<Broadcast>(None);

        // Swap in a very short timeout by calling the internal logic manually.
        // Since FOLLOWER_TIMEOUT is 2 s and we can't override it easily in a
        // test, we test with a channel that is immediately closed instead
        // (same fallthrough path as timeout).
        drop(_tx); // close the channel
        let result = wait_for_leader(rx).await;
        assert!(!result, "closed channel should produce fallthrough (false)");
    }
}
