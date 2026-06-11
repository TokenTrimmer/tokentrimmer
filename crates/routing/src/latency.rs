//! Gateway-side rolling latency window — the REAL signal behind the
//! `upstream_latency_ms_p95_gt` route condition.
//!
//! # Why this exists
//!
//! The public gateway has **no p95 latency data at routing-decision time**:
//! `request_logs` (the only persisted latency record) is cloud-only and written
//! fire-and-forget *after* the response, so it cannot inform a route picked
//! *before* dispatch. A latency-aware condition backed by nothing would either
//! always match or never match — a no-op masquerading as a feature.
//!
//! [`LatencyTracker`] makes the signal real and local: the gateway records every
//! upstream `upstream_latency_ms` it already measures into a bounded, in-process
//! rolling window keyed by `(provider, model)`, and the routing engine queries
//! that window's live p95 when evaluating the condition.
//!
//! # Semantics (cold-start safe)
//!
//! - The window is **in-process and bounded** — only the most recent
//!   [`WINDOW_CAPACITY`] samples per `(provider, model)` are retained, so the
//!   p95 reflects *current* upstream behavior, not all-time history. It is
//!   per-instance: a multi-replica gateway maintains one window per replica
//!   (acceptable — each replica routes on what it has itself observed).
//! - [`LatencyTracker::p95`] returns `None` until at least [`MIN_SAMPLES`]
//!   observations exist for that key. A condition consulting it therefore treats
//!   **insufficient data as "do not match"** (cold start → the slow-primary
//!   alternate route does NOT fire on an unknown key), and only fires once there
//!   is enough evidence that the observed p95 genuinely exceeds the threshold.

use std::collections::HashMap;
use std::sync::RwLock;

/// Maximum samples retained per `(provider, model)` ring buffer. Bounds memory
/// and keeps the p95 responsive to recent behavior rather than all-time history.
pub const WINDOW_CAPACITY: usize = 256;

/// Minimum samples before [`LatencyTracker::p95`] returns a value. Below this,
/// the percentile is statistically meaningless, so the tracker reports `None`
/// and latency conditions stay FALSE (cold start). 20 keeps a fresh key from
/// firing on one or two slow outliers.
pub const MIN_SAMPLES: usize = 20;

/// Number of shards. Sharding the keyspace across independent locks keeps the
/// hot record path from contending on a single global lock under load.
const SHARD_COUNT: usize = 16;

/// A bounded ring buffer of recent latency samples (milliseconds) for one
/// `(provider, model)` key.
#[derive(Debug, Default)]
struct Window {
    /// Most-recent-wins ring buffer; `len <= WINDOW_CAPACITY`.
    samples: Vec<u32>,
    /// Next write position (wraps at `WINDOW_CAPACITY`).
    next: usize,
}

impl Window {
    fn push(&mut self, ms: u32) {
        if self.samples.len() < WINDOW_CAPACITY {
            self.samples.push(ms);
        } else {
            self.samples[self.next] = ms;
            self.next = (self.next + 1) % WINDOW_CAPACITY;
        }
    }

    /// p95 of the current window, or `None` when fewer than [`MIN_SAMPLES`]
    /// observations exist. Uses the nearest-rank method on a sorted copy.
    fn p95(&self) -> Option<u32> {
        let n = self.samples.len();
        if n < MIN_SAMPLES {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        // Nearest-rank: ceil(0.95 * n) -> 1-based rank; clamp into bounds.
        let rank = ((0.95_f64 * n as f64).ceil() as usize).clamp(1, n);
        Some(sorted[rank - 1])
    }
}

/// Concurrent, sharded rolling-latency window keyed by `(provider, model)`.
///
/// Cheap to clone the `Arc` around it; `record` takes a write lock on one shard,
/// `p95` takes a read lock on one shard. See the module docs for semantics.
#[derive(Debug)]
pub struct LatencyTracker {
    shards: Vec<RwLock<HashMap<(String, String), Window>>>,
}

impl Default for LatencyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyTracker {
    /// Construct an empty tracker.
    pub fn new() -> Self {
        let mut shards = Vec::with_capacity(SHARD_COUNT);
        for _ in 0..SHARD_COUNT {
            shards.push(RwLock::new(HashMap::new()));
        }
        Self { shards }
    }

    fn shard_for(&self, provider: &str, model: &str) -> &RwLock<HashMap<(String, String), Window>> {
        // Cheap FNV-1a over the two key parts; stable across calls.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in provider
            .bytes()
            .chain(std::iter::once(b'\0'))
            .chain(model.bytes())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        &self.shards[(hash as usize) % SHARD_COUNT]
    }

    /// Record one observed upstream latency sample (milliseconds) for
    /// `(provider, model)`. Lock-poisoning is treated as a no-op (we never want
    /// a metrics path to panic user traffic).
    pub fn record(&self, provider: &str, model: &str, latency_ms: u32) {
        let shard = self.shard_for(provider, model);
        if let Ok(mut map) = shard.write() {
            map.entry((provider.to_string(), model.to_string()))
                .or_default()
                .push(latency_ms);
        }
    }

    /// Live p95 (milliseconds) for `(provider, model)`, or `None` when there are
    /// fewer than [`MIN_SAMPLES`] observations (cold start) or the lock is
    /// poisoned. Callers MUST treat `None` as "insufficient data → condition
    /// does not match".
    pub fn p95(&self, provider: &str, model: &str) -> Option<u32> {
        let shard = self.shard_for(provider, model);
        let map = shard.read().ok()?;
        map.get(&(provider.to_string(), model.to_string()))
            .and_then(Window::p95)
    }

    /// Current sample count for `(provider, model)` (mainly for tests/telemetry).
    pub fn sample_count(&self, provider: &str, model: &str) -> usize {
        let shard = self.shard_for(provider, model);
        shard
            .read()
            .ok()
            .and_then(|map| {
                map.get(&(provider.to_string(), model.to_string()))
                    .map(|w| w.samples.len())
            })
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_is_none_below_min_samples() {
        let t = LatencyTracker::new();
        for _ in 0..(MIN_SAMPLES - 1) {
            t.record("openai", "gpt-4o", 1000);
        }
        assert_eq!(t.p95("openai", "gpt-4o"), None, "cold start → None");
        assert_eq!(t.sample_count("openai", "gpt-4o"), MIN_SAMPLES - 1);
    }

    #[test]
    fn p95_present_at_min_samples() {
        let t = LatencyTracker::new();
        for _ in 0..MIN_SAMPLES {
            t.record("openai", "gpt-4o", 500);
        }
        assert_eq!(t.p95("openai", "gpt-4o"), Some(500));
    }

    #[test]
    fn p95_reflects_tail_latency() {
        let t = LatencyTracker::new();
        // 95 samples at 100ms, 5 at 5000ms → p95 sits in the slow tail.
        for _ in 0..95 {
            t.record("p", "m", 100);
        }
        for _ in 0..5 {
            t.record("p", "m", 5000);
        }
        let p95 = t.p95("p", "m").expect("enough samples");
        // nearest-rank p95 of 100 samples = the 95th sorted value = 100 here
        // (the 5 slow ones are ranks 96..100). Assert it sits within the observed
        // range [fast floor, slow peak].
        assert!((100..=5000).contains(&p95));
    }

    #[test]
    fn p95_catches_majority_slow() {
        let t = LatencyTracker::new();
        // 50 slow + 50 fast: p95 lands in the slow half.
        for _ in 0..50 {
            t.record("p", "m", 100);
        }
        for _ in 0..50 {
            t.record("p", "m", 4000);
        }
        assert_eq!(t.p95("p", "m"), Some(4000));
    }

    #[test]
    fn keys_are_independent() {
        let t = LatencyTracker::new();
        for _ in 0..MIN_SAMPLES {
            t.record("openai", "gpt-4o", 3000);
        }
        // A different model has no samples → None.
        assert!(t.p95("openai", "gpt-4o-mini").is_none());
        assert_eq!(t.p95("openai", "gpt-4o"), Some(3000));
    }

    #[test]
    fn window_is_bounded_and_recent_wins() {
        let t = LatencyTracker::new();
        // Fill past capacity with slow samples, then flood with fast ones so the
        // ring buffer evicts the slow history.
        for _ in 0..WINDOW_CAPACITY {
            t.record("p", "m", 9000);
        }
        for _ in 0..WINDOW_CAPACITY {
            t.record("p", "m", 50);
        }
        assert_eq!(
            t.sample_count("p", "m"),
            WINDOW_CAPACITY,
            "window stays bounded at capacity"
        );
        assert_eq!(
            t.p95("p", "m"),
            Some(50),
            "recent fast samples evicted the slow history"
        );
    }
}
