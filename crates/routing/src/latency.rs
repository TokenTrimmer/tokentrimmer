//! Gateway-side rolling latency window — the REAL signal behind the
//! `upstream_latency_ms_p95_gt` route condition.
//!
//! # Why this exists
//!
//! Routing uses a live in-process observation window rather than querying a
//! persisted request-log aggregate for each decision. A missing signal must be
//! unknown, not an invented zero or a measurement from another operation.
//!
//! [`LatencyTracker`] makes the signal real and local: the gateway records every
//! observed dispatch duration into an in-process window keyed by provider,
//! model and operation. Routing consults the incoming request's operation.
//! Streaming establishment is stream-handle readiness, not first output token.
//! Gateway wiring currently records successful direct dispatch groups including
//! retries/backoff, not every attempt, timeout or failover. This is not an SLO.
//!
//! # Semantics (cold-start safe)
//!
//! - The window is **in-process and bounded** — only the most recent
//!   [`WINDOW_CAPACITY`] samples per `(provider, model)` are retained, so the
//!   p95 reflects *current* upstream behavior, not all-time history. It is
//!   per-instance: a multi-replica gateway maintains one window per replica
//!   (each replica routes on what it has itself observed). This bounds each
//!   window, not the total keyspace; expired keys are not reclaimed here.
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

/// Policy minimum before [`LatencyTracker::p95`] returns a value. Below this,
/// the tracker reports None and conditions stay false. This floor is not a
/// calibrated confidence bound or a guarantee against noisy tail estimates.
pub const MIN_SAMPLES: usize = 20;

/// Maximum age of a latency sample before it is excluded from the p95.
/// Old samples represent stale upstream behavior, not current. The tracking
/// is per-process: each gateway replica observes its own window.
pub const MAX_SAMPLE_AGE: Duration = Duration::from_secs(300);

/// Number of shards. Sharding the keyspace across independent locks keeps the
/// hot record path from contending on a single global lock under load.
const SHARD_COUNT: usize = 16;

/// One consistent, instance-local view of the retained/fresh window. Ages are
/// relative to a monotonic clock and cover fresh samples only. This does not
/// manufacture first-token, failure/timeout-rate or fleet-wide evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyEvidence {
    pub scope: &'static str,
    pub operation: LatencyOperation,
    pub window_seconds: u64,
    pub capacity: usize,
    pub minimum_samples: usize,
    pub retained_samples: usize,
    pub sample_count: usize,
    pub newest_sample_age_ms: Option<u64>,
    pub oldest_sample_age_ms: Option<u64>,
    pub p95_ms: Option<u32>,
}

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

    fn evidence_at(&self, operation: LatencyOperation, now: Instant) -> LatencyEvidence {
        let mut fresh = Vec::with_capacity(self.samples.len());
        let mut newest: Option<u64> = None;
        let mut oldest: Option<u64> = None;
        for sample in &self.samples {
            let Some(age) = now.checked_duration_since(sample.at) else {
                continue;
            };
            if age >= MAX_SAMPLE_AGE {
                continue;
            }
            let age_ms = u64::try_from(age.as_millis()).unwrap_or(u64::MAX);
            newest = Some(newest.map_or(age_ms, |value| value.min(age_ms)));
            oldest = Some(oldest.map_or(age_ms, |value| value.max(age_ms)));
            fresh.push(sample.ms);
        }
        let n = fresh.len();
        let p95_ms = if n < MIN_SAMPLES {
            None
        } else {
            fresh.sort_unstable();
            let rank = ((0.95_f64 * n as f64).ceil() as usize).clamp(1, n);
            Some(fresh[rank - 1])
        };
        LatencyEvidence {
            scope: "instance",
            operation,
            window_seconds: MAX_SAMPLE_AGE.as_secs(),
            capacity: WINDOW_CAPACITY,
            minimum_samples: MIN_SAMPLES,
            retained_samples: self.samples.len(),
            sample_count: n,
            newest_sample_age_ms: newest,
            oldest_sample_age_ms: oldest,
            p95_ms,
        }
    }
}

/// Type of upstream operation being measured. Prevents mixing fundamentally
/// different signals (stream-handle readiness vs buffered completion) into one
/// distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LatencyOperation {
    /// Time until the provider returns a stream handle, including configured
    /// retries/backoff. This is not TCP connect time or first output token.
    StreamEstablishment,
    /// Time to receive a full buffered (non-streaming) completion. Includes
    /// generation time; NOT comparable with [`StreamEstablishment`].
    BufferedCompletion,
}

impl LatencyOperation {
    /// Match the operation recorded by dispatch, without borrowing another
    /// population when the matching one is cold or unavailable.
    pub const fn for_streaming(stream: bool) -> Self {
        if stream {
            Self::StreamEstablishment
        } else {
            Self::BufferedCompletion
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StreamEstablishment => "stream_establishment",
            Self::BufferedCompletion => "buffered_completion",
        }
    }

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
        self.evidence(provider, model, operation)
            .and_then(|e| e.p95_ms)
    }

    /// One snapshot for p95, sample count and age. A missing key is a known
    /// empty window; a poisoned lock is unavailable (None), not healthy zero.
    pub fn evidence(
        &self,
        provider: &str,
        model: &str,
        operation: LatencyOperation,
    ) -> Option<LatencyEvidence> {
        let map = self.shard_for(provider, model).read().ok()?;
        let now = Instant::now(); // after acquiring the lock, not before a wait
        let empty = Window::default();
        Some(
            map.get(&(
                provider.to_string(),
                model.to_string(),
                operation.key_fragment().to_string(),
            ))
            .unwrap_or(&empty)
            .evidence_at(operation, now),
        )
    }

    /// Legacy count helper; unavailable locks still map to zero for API
    /// compatibility. Use evidence() to distinguish unavailable from empty.
    pub fn sample_count(&self, provider: &str, model: &str, operation: LatencyOperation) -> usize {
        self.evidence(provider, model, operation)
            .map_or(0, |e| e.sample_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const OP: LatencyOperation = LatencyOperation::StreamEstablishment;

    #[test]
    fn evidence_uses_one_clock_and_excludes_expired_boundary_and_future_samples() {
        let now = Instant::now();
        let mut window = Window::default();
        for i in 1..=20 {
            window.push(i, now - Duration::from_secs(u64::from(i)));
        }
        window.push(9999, now - MAX_SAMPLE_AGE);
        window.push(9999, now + Duration::from_secs(1));
        let evidence = window.evidence_at(OP, now);
        assert_eq!(evidence.scope, "instance");
        assert_eq!(evidence.operation, OP);
        assert_eq!(evidence.window_seconds, 300);
        assert_eq!(evidence.capacity, WINDOW_CAPACITY);
        assert_eq!(evidence.minimum_samples, MIN_SAMPLES);
        assert_eq!(evidence.retained_samples, 22);
        assert_eq!(evidence.sample_count, 20);
        assert_eq!(evidence.newest_sample_age_ms, Some(1000));
        assert_eq!(evidence.oldest_sample_age_ms, Some(20_000));
        assert_eq!(evidence.p95_ms, Some(19));
        let expired = window.evidence_at(OP, now + MAX_SAMPLE_AGE + Duration::from_secs(1));
        assert_eq!(expired.retained_samples, 22);
        assert_eq!(expired.sample_count, 0);
        assert_eq!(expired.p95_ms, None);
        assert_eq!(expired.newest_sample_age_ms, None);
        assert_eq!(expired.oldest_sample_age_ms, None);
    }

    #[test]
    fn evidence_exposes_cold_scope_without_borrowing_another_operation_or_tracker() {
        let tracker = LatencyTracker::new();
        let operation = LatencyOperation::for_streaming(false);
        assert_eq!(operation, LatencyOperation::BufferedCompletion);
        assert_eq!(operation.as_str(), "buffered_completion");
        assert_eq!(LatencyOperation::for_streaming(true), OP);
        let cold = tracker.evidence("p", "m", operation).unwrap();
        assert_eq!(cold.scope, "instance");
        assert_eq!(cold.sample_count, 0);
        assert_eq!(cold.retained_samples, 0);
        assert_eq!(cold.newest_sample_age_ms, None);
        tracker.record("p", "m", operation, 42);
        assert_eq!(
            tracker.evidence("p", "m", operation).unwrap().sample_count,
            1
        );
        assert_eq!(tracker.evidence("p", "m", OP).unwrap().sample_count, 0);
        assert_eq!(
            LatencyTracker::new()
                .evidence("p", "m", operation)
                .unwrap()
                .sample_count,
            0
        );
    }

    #[test]
    fn evidence_reports_nearest_rank_and_retained_capacity() {
        let now = Instant::now();
        let mut window = Window::default();
        for ms in 1..=100 {
            window.push(ms, now);
        }
        assert_eq!(window.evidence_at(OP, now).p95_ms, Some(95));
        for _ in 0..WINDOW_CAPACITY {
            window.push(10, now);
        }
        let evidence = window.evidence_at(OP, now);
        assert_eq!(evidence.retained_samples, WINDOW_CAPACITY);
        assert_eq!(evidence.sample_count, WINDOW_CAPACITY);
        assert_eq!(evidence.p95_ms, Some(10));
    }

    #[test]
    fn poisoned_evidence_is_unavailable_not_a_healthy_empty_snapshot() {
        let tracker = std::sync::Arc::new(LatencyTracker::new());
        let other = tracker.clone();
        assert!(std::thread::spawn(move || {
            let _guard = other.shard_for("p", "m").write().unwrap();
            panic!("fixture poison");
        })
        .join()
        .is_err());
        assert!(tracker.evidence("p", "m", OP).is_none());
        assert_eq!(tracker.p95("p", "m", OP), None);
        tracker.record("p", "m", OP, 10); // no panic on poisoned telemetry
    }

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
        // Nearest-rank p95 is the 95th sample: exactly five slow outliers
        // among 100 samples remain above that rank, not in the reported p95.
        for _ in 0..95 {
            t.record("p", "m", OP, 100);
        }
        for _ in 0..5 {
            t.record("p", "m", OP, 5000);
        }
        let p95 = t.p95("p", "m", OP).expect("enough samples");
        assert_eq!(p95, 100);
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
