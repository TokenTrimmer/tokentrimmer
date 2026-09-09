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
use std::time::{Duration, Instant};

/// Maximum samples retained per `(provider, model)` ring buffer. Bounds memory
/// and keeps the p95 responsive to recent behavior rather than all-time history.
pub const WINDOW_CAPACITY: usize = 256;

/// Minimum samples before [`LatencyTracker::p95`] returns a value. Below this,
/// the percentile is statistically meaningless, so the tracker reports `None`
/// and latency conditions stay FALSE (cold start). 20 keeps a fresh key from
/// firing on one or two slow outliers.
pub const MIN_SAMPLES: usize = 20;

/// Maximum age of a latency sample before it is excluded from the p95.
/// Old samples represent stale upstream behavior, not current. The tracking
/// is per-process: each gateway replica observes its own window.
pub const MAX_SAMPLE_AGE: Duration = Duration::from_secs(300);

/// Number of shards. Sharding the keyspace across independent locks keeps the
/// hot record path from contending on a single global lock under load.
const SHARD_COUNT: usize = 16;

/// One observed latency sample with its capture time.
#[derive(Debug, Clone, Copy)]
struct Sample {
    ms: u32,
    at: Instant,
}

/// A bounded ring buffer of recent latency samples for one
/// `(provider, model, operation)` key.
#[derive(Debug, Default)]
struct Window {
    /// Most-recent-wins ring buffer; `len <= WINDOW_CAPACITY`.
    samples: Vec<Sample>,
    /// Next write position (wraps at `WINDOW_CAPACITY`).
    next: usize,
}

impl Window {
    fn push(&mut self, ms: u32, at: Instant) {
        let sample = Sample { ms, at };
        if self.samples.len() < WINDOW_CAPACITY {
            self.samples.push(sample);
        } else {
            self.samples[self.next] = sample;
            self.next = (self.next + 1) % WINDOW_CAPACITY;
        }
    }

    /// p95 of the current **fresh** (under [`MAX_SAMPLE_AGE`]) samples,
    /// or `None` when fewer than [`MIN_SAMPLES`] fresh observations exist.
    /// Uses the nearest-rank method on a sorted copy.
    fn p95(&self) -> Option<u32> {
        let now = Instant::now();
        let fresh: Vec<u32> = self
            .samples
            .iter()
            .filter(|s| now.duration_since(s.at) < MAX_SAMPLE_AGE)
            .map(|s| s.ms)
            .collect();
        let n = fresh.len();
        if n < MIN_SAMPLES {
            return None;
        }
        let mut sorted = fresh;
        sorted.sort_unstable();
        let rank = ((0.95_f64 * n as f64).ceil() as usize).clamp(1, n);
        Some(sorted[rank - 1])
    }

    /// Count of *fresh* samples (mainly for tests/telemetry).
    fn fresh_count(&self) -> usize {
        let now = Instant::now();
        self.samples
            .iter()
            .filter(|s| now.duration_since(s.at) < MAX_SAMPLE_AGE)
            .count()
    }
}

/// Type of upstream operation being measured. Prevents mixing fundamentally
/// different latency signals (time-to-first-byte vs full completion) into one
/// distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LatencyOperation {
    /// Time to establish a streaming response (first chunk / stream ready).
    /// Measures connection + upstream queue, not generation time.
    StreamEstablishment,
    /// Time to receive a full buffered (non-streaming) completion. Includes
    /// generation time; NOT comparable with [`StreamEstablishment`].
    BufferedCompletion,
}

impl LatencyOperation {
    fn key_fragment(self) -> &'static str {
        match self {
            Self::StreamEstablishment => "stream",
            Self::BufferedCompletion => "chat",
        }
    }
}

/// Internal key: (provider, model, operation-fragment).
type ShardKey = (String, String, String);
type ShardMap = HashMap<ShardKey, Window>;

/// Concurrent, sharded rolling-latency window.
///
/// Cheap to clone the `Arc` around it; `record` takes a write lock on one shard,
/// `p95` takes a read lock on one shard. See the module docs for semantics.
#[derive(Debug)]
pub struct LatencyTracker {
    shards: Vec<RwLock<ShardMap>>,
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

    fn shard_for(&self, provider: &str, model: &str) -> &RwLock<ShardMap> {
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
    /// `(provider, model, operation)`. Lock-poisoning is treated as a no-op (we never want
    /// a metrics path to panic user traffic).
    pub fn record(
        &self,
        provider: &str,
        model: &str,
        operation: LatencyOperation,
        latency_ms: u32,
    ) {
        let shard = self.shard_for(provider, model);
        if let Ok(mut map) = shard.write() {
            map.entry((
                provider.to_string(),
                model.to_string(),
                operation.key_fragment().to_string(),
            ))
            .or_default()
            .push(latency_ms, Instant::now());
        }
    }

    /// Live p95 (milliseconds) for `(provider, model, operation)`, or `None` when there are
    /// fewer than [`MIN_SAMPLES`] fresh observations (cold start or all samples
    /// aged out) or the lock is poisoned. Callers MUST treat `None` as
    /// "insufficient data → condition does not match".
    pub fn p95(&self, provider: &str, model: &str, operation: LatencyOperation) -> Option<u32> {
        let shard = self.shard_for(provider, model);
        let map = shard.read().ok()?;
        map.get(&(
            provider.to_string(),
            model.to_string(),
            operation.key_fragment().to_string(),
        ))
        .and_then(Window::p95)
    }

    /// Current fresh sample count for `(provider, model, operation)` (mainly for tests/telemetry).
    pub fn sample_count(&self, provider: &str, model: &str, operation: LatencyOperation) -> usize {
        let shard = self.shard_for(provider, model);
        shard
            .read()
            .ok()
            .and_then(|map| {
                map.get(&(
                    provider.to_string(),
                    model.to_string(),
                    operation.key_fragment().to_string(),
                ))
                .map(Window::fresh_count)
            })
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const OP: LatencyOperation = LatencyOperation::StreamEstablishment;

    #[test]
    fn p95_is_none_below_min_samples() {
        let t = LatencyTracker::new();
        for _ in 0..(MIN_SAMPLES - 1) {
            t.record("openai", "gpt-4o", OP, 1000);
        }
        assert_eq!(t.p95("openai", "gpt-4o", OP), None, "cold start → None");
        assert_eq!(t.sample_count("openai", "gpt-4o", OP), MIN_SAMPLES - 1);
    }

    #[test]
    fn p95_present_at_min_samples() {
        let t = LatencyTracker::new();
        for _ in 0..MIN_SAMPLES {
            t.record("openai", "gpt-4o", OP, 500);
        }
        assert_eq!(t.p95("openai", "gpt-4o", OP), Some(500));
    }

    #[test]
    fn stream_establishment_and_buffered_completion_are_tracked_separately() {
        let t = LatencyTracker::new();
        // 20 fast stream-establishment samples
        for _ in 0..MIN_SAMPLES {
            t.record("p", "m", LatencyOperation::StreamEstablishment, 50);
        }
        // 2 slow buffered-completion samples (not enough for p95)
        t.record("p", "m", LatencyOperation::BufferedCompletion, 5000);
        t.record("p", "m", LatencyOperation::BufferedCompletion, 6000);
        assert!(
            t.p95("p", "m", LatencyOperation::StreamEstablishment)
                .is_some(),
            "stream establishment has enough fresh samples"
        );
        assert!(
            t.p95("p", "m", LatencyOperation::BufferedCompletion)
                .is_none(),
            "buffered completion must not mix with stream establishment"
        );
        assert!(t.sample_count("p", "m", LatencyOperation::StreamEstablishment) > 0);
        assert_eq!(
            t.sample_count("p", "m", LatencyOperation::BufferedCompletion),
            2
        );
    }

    #[test]
    fn old_samples_expire() {
        let t = LatencyTracker::new();
        let shard = t.shard_for("p", "m");
        // Simulate old samples that should be expired out
        if let Ok(mut map) = shard.write() {
            let window = map
                .entry(("p".into(), "m".into(), "stream".into()))
                .or_default();
            for _ in 0..MIN_SAMPLES {
                window.push(
                    100,
                    Instant::now() - MAX_SAMPLE_AGE - Duration::from_secs(1),
                );
            }
        }
        assert!(
            t.p95("p", "m", OP).is_none(),
            "expired samples must not contribute to the p95"
        );
        assert_eq!(t.sample_count("p", "m", OP), 0);
    }

    #[test]
    fn fresh_samples_not_expired_by_adjacent_old() {
        let t = LatencyTracker::new();
        let shard = t.shard_for("p", "m");
        if let Ok(mut map) = shard.write() {
            let window = map
                .entry(("p".into(), "m".into(), "stream".into()))
                .or_default();
            // Push some expired samples
            for _ in 0..30 {
                window.push(
                    100,
                    Instant::now() - MAX_SAMPLE_AGE - Duration::from_secs(1),
                );
            }
            // Push fresh samples
            for _ in 0..MIN_SAMPLES {
                window.push(200, Instant::now());
            }
        }
        assert!(
            t.p95("p", "m", OP).is_some(),
            "fresh samples must still count alongside expired ones"
        );
    }

    #[test]
    fn p95_reflects_tail_latency() {
        let t = LatencyTracker::new();
        // 95 samples at 100ms, 5 at 5000ms → p95 sits in the slow tail.
        for _ in 0..95 {
            t.record("p", "m", OP, 100);
        }
        for _ in 0..5 {
            t.record("p", "m", OP, 5000);
        }
        let p95 = t.p95("p", "m", OP).expect("enough samples");
        assert!((100..=5000).contains(&p95));
    }

    #[test]
    fn p95_catches_majority_slow() {
        let t = LatencyTracker::new();
        // 50 slow + 50 fast: p95 lands in the slow half.
        for _ in 0..50 {
            t.record("p", "m", OP, 100);
        }
        for _ in 0..50 {
            t.record("p", "m", OP, 4000);
        }
        assert_eq!(t.p95("p", "m", OP), Some(4000));
    }

    #[test]
    fn keys_are_independent() {
        let t = LatencyTracker::new();
        for _ in 0..MIN_SAMPLES {
            t.record("openai", "gpt-4o", OP, 3000);
        }
        // A different model has no samples → None.
        assert!(t.p95("openai", "gpt-4o-mini", OP).is_none());
        assert_eq!(t.p95("openai", "gpt-4o", OP), Some(3000));
    }

    #[test]
    fn window_is_bounded_and_recent_wins() {
        let t = LatencyTracker::new();
        // Fill past capacity with slow samples, then flood with fast ones so the
        // ring buffer evicts the slow history.
        for _ in 0..WINDOW_CAPACITY {
            t.record("p", "m", OP, 9000);
        }
        for _ in 0..WINDOW_CAPACITY {
            t.record("p", "m", OP, 50);
        }
        assert_eq!(
            t.sample_count("p", "m", OP),
            WINDOW_CAPACITY,
            "window stays bounded at capacity"
        );
        assert_eq!(
            t.p95("p", "m", OP),
            Some(50),
            "recent fast samples evicted the slow history"
        );
    }
}
