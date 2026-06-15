//! L2 (semantic) cache projection. Mirrors the contract of the production
//! [`tt_cache::l2::L2Cache`] — cosine-similarity search over not-yet-expired
//! entries that share the same `(provider, model)` key — but operates on a
//! historical request stream so the Plan can answer: "what hit rate would
//! the proposed `(threshold, ttl)` config have yielded?"
//!
//! ## Algorithm
//!
//! For each cosine-similarity threshold `T` in
//! [`crate::types::PlanConfig::l2_threshold_sweep`]:
//!
//! 1. Walk the requests in deterministic `(ts, id)` order.
//! 2. For each request with a non-`None` embedding, search the live entries
//!    bucket keyed by `(provider, model)` for the nearest neighbour whose
//!    cosine similarity is `>= T`.
//! 3. **Hit:** count the hit and apply the poisoning heuristic against the
//!    matched source entry. The cache is not mutated (a hit serves the
//!    cached response, it doesn't insert a new one).
//! 4. **Miss:** insert the current request's `(ts, embedding, finish_reason,
//!    output_tokens)` into the bucket.
//! 5. Evict entries whose `ts` is older than `req.ts - ttl` lazily on each
//!    bucket touch.
//!
//! Each threshold pass runs **independently** against an empty starting
//! cache — this is the standard sensitivity sweep described in
//! `docs/03-plan-replay-design.md` §6.2. The cache contents diverge across
//! thresholds: a hit reuses (no insert), a miss inserts; a higher threshold
//! produces more misses and therefore more inserts, so the live entry sets
//! are genuinely different per pass.
//!
//! ## Cache poisoning heuristic
//!
//! On every projected L2 hit we ask: did the historical outcome of the
//! matched source diverge enough from the historical outcome of the current
//! request to suggest serving the cached response would have been *wrong*?
//! Two signals (either trips the candidate count):
//!
//! - `finish_reason` differs (e.g. `"stop"` vs `"length"`), and both sides
//!   have a finish reason recorded.
//! - `output_tokens` differs by more than
//!   `max(20, current.output_tokens / 4)` — a 25%-or-20-tokens tolerance.
//!
//! When either side has no `finish_reason` recorded the divergence check
//! degrades to "skip" — the token-count check still runs because it's
//! available on every row. This keeps the heuristic useful on historical
//! data that predates the `finish_reason` column.
//!
//! ## Complexity
//!
//! Naive `O(N^2)` per threshold (linear scan of the bucket per request).
//! Fine for the v1 corpus sizes the CLI replays. An HNSW upgrade lands in a
//! later iteration when the bucket size becomes a bottleneck.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::types::{
    L2Projection, L2SweepResult, L2TaskClass, PerClassL2Metrics, PlanConfig, RequestLog,
};

/// Live entry in a per-`(provider, model)` bucket. Kept lightweight — just
/// the fields the cosine search + poisoning heuristic need.
#[derive(Clone)]
struct LiveEntry {
    ts: DateTime<Utc>,
    embedding: Vec<f32>,
    finish_reason: Option<String>,
    output_tokens: u32,
}

/// Project L2 (semantic) cache hits over a request window under the
/// proposed configuration. See the module docs for the algorithm.
///
/// Returns an empty sweep when:
///
/// - `requests` is empty,
/// - `config.l2_threshold_sweep` is empty,
/// - `config.l2_ttl_seconds` is `None`, or
/// - no request in the window carries an `embedding`.
///
/// In any of these cases callers can rely on
/// [`L2SweepResult::per_threshold`] being empty and
/// [`L2SweepResult::poisoning_candidates`] being `0`.
#[must_use]
pub fn project_l2_hits(requests: &[RequestLog], config: &PlanConfig) -> L2SweepResult {
    if requests.is_empty() || config.l2_threshold_sweep.is_empty() {
        return L2SweepResult::default();
    }
    let Some(ttl_secs) = config.l2_ttl_seconds else {
        return L2SweepResult::default();
    };
    let any_embedding = requests.iter().any(|r| r.embedding.is_some());
    if !any_embedding {
        return L2SweepResult::default();
    }
    let ttl = chrono::Duration::seconds(i64::from(ttl_secs));

    // Stable order so determinism is preserved across input permutations.
    let mut sorted: Vec<&RequestLog> = requests.iter().collect();
    sorted.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));

    let mut per_threshold: Vec<L2Projection> = Vec::with_capacity(config.l2_threshold_sweep.len());
    // DEDUP: union of the distinct request ids flagged at *any* threshold.
    // A request that poisons at multiple thresholds lands in the set once, so
    // the aggregate reflects distinct candidate requests rather than the
    // cross-sweep sum (which over-counts up to N× for an N-threshold sweep).
    // BTreeSet (not HashSet) keeps the cardinality deterministic regardless of
    // insertion order.
    let mut distinct_poisoning: BTreeSet<Uuid> = BTreeSet::new();
    // Per-class accumulators. `considered`/`hits` sum across thresholds; the
    // poisoning set is deduped per class across the whole sweep (mirroring the
    // top-level aggregate). BTreeMap keeps the emitted order deterministic.
    let mut per_class: BTreeMap<L2TaskClass, ClassAccum> = BTreeMap::new();

    for &threshold in &config.l2_threshold_sweep {
        let proj = run_single_threshold(
            &sorted,
            threshold,
            ttl,
            &mut distinct_poisoning,
            &mut per_class,
        );
        per_threshold.push(proj);
    }

    let poisoning_candidates = u32::try_from(distinct_poisoning.len()).unwrap_or(u32::MAX);

    let per_class = per_class
        .into_iter()
        .map(|(task_class, acc)| {
            let considered = acc.considered;
            let hits = acc.hits;
            PerClassL2Metrics {
                task_class,
                considered,
                hits,
                misses: considered.saturating_sub(hits),
                poisoning_candidates: u32::try_from(acc.poisoning.len()).unwrap_or(u32::MAX),
            }
        })
        .collect();

    L2SweepResult {
        per_threshold,
        poisoning_candidates,
        per_class,
    }
}

/// Per-class running totals accumulated across the threshold sweep. `considered`
/// and `hits` sum every threshold's contribution; `poisoning` is a deduped set
/// of distinct flagged request ids for that class.
#[derive(Default)]
struct ClassAccum {
    considered: u32,
    hits: u32,
    poisoning: BTreeSet<Uuid>,
}

/// Run one threshold pass. The requests slice MUST already be sorted by
/// `(ts, id)` — sorting is done once in [`project_l2_hits`] and reused
/// across thresholds so the per-pass cost stays linear in the bucket size.
///
/// The returned [`L2Projection`] carries this threshold's own poisoning
/// count. Each flagged request's id is also inserted into
/// `distinct_poisoning` so the caller can report the deduplicated
/// cross-sweep aggregate.
fn run_single_threshold(
    sorted: &[&RequestLog],
    threshold: f32,
    ttl: chrono::Duration,
    distinct_poisoning: &mut BTreeSet<Uuid>,
    per_class: &mut BTreeMap<L2TaskClass, ClassAccum>,
) -> L2Projection {
    // BTreeMap (rather than HashMap) so iteration order — and thus any
    // future bucket-mutation order — stays deterministic. The buckets
    // themselves are Vecs kept in `ts`-ascending insertion order; we evict
    // the head when it falls outside the TTL window.
    let mut active: BTreeMap<(String, String), Vec<LiveEntry>> = BTreeMap::new();

    let mut total_considered: u32 = 0;
    let mut hits: u32 = 0;
    let mut poisoning: u32 = 0;

    for req in sorted {
        let Some(embedding) = req.embedding.as_ref() else {
            // No embedding → can't participate in L2 projection.
            continue;
        };
        total_considered = total_considered.saturating_add(1);
        let class_acc = per_class.entry(req.task_class).or_default();
        class_acc.considered = class_acc.considered.saturating_add(1);

        let key = (req.provider.clone(), req.model.clone());
        let bucket = active.entry(key).or_default();

        // Evict stale entries (entries whose ts < req.ts - ttl).
        let cutoff = req.ts - ttl;
        bucket.retain(|e| e.ts >= cutoff);

        // Naive O(N) nearest-neighbour search at this threshold.
        let mut best: Option<(usize, f32)> = None;
        for (idx, entry) in bucket.iter().enumerate() {
            let sim = cosine(embedding, &entry.embedding);
            if sim >= threshold && best.is_none_or(|(_, b)| sim > b) {
                best = Some((idx, sim));
            }
        }

        if let Some((idx, _sim)) = best {
            hits = hits.saturating_add(1);
            class_acc.hits = class_acc.hits.saturating_add(1);
            // Cache-poisoning heuristic: did the matched source diverge
            // from the current request's historical outcome?
            let source = &bucket[idx];
            if outcomes_diverged(source, req) {
                poisoning = poisoning.saturating_add(1);
                // Record this request id for the deduplicated aggregate. The
                // per-threshold `poisoning` count above still increments every
                // time so each threshold reports its own (possibly repeated)
                // candidate.
                distinct_poisoning.insert(req.id);
                class_acc.poisoning.insert(req.id);
            }
            // Hits do not reset / re-insert — the original miss already
            // populated the cache. The source entry keeps its `ts`.
        } else {
            bucket.push(LiveEntry {
                ts: req.ts,
                embedding: embedding.clone(),
                finish_reason: req.finish_reason.clone(),
                output_tokens: req.output_tokens,
            });
        }
    }

    let rate = if total_considered == 0 {
        0.0
    } else {
        f64::from(hits) / f64::from(total_considered)
    };
    L2Projection {
        threshold,
        total: total_considered,
        projected_l2_hits: hits,
        projected_l2_hit_rate: rate,
        poisoning_candidates: poisoning,
    }
}

/// Cosine similarity between two equal-length vectors. The production
/// embedder (`text-embedding-3-small`) returns L2-normalized vectors so
/// `dot == cosine`, but we keep the full formula so tests with raw
/// vectors (the `MockEmbedder`, hand-rolled fixtures) work without
/// pre-normalizing.
///
/// Returns `0.0` for mismatched lengths or zero-magnitude inputs so
/// pathological pairs never spuriously hit.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

/// Cache-poisoning heuristic. See the module-level docs for the policy.
fn outcomes_diverged(source: &LiveEntry, req: &RequestLog) -> bool {
    let finish_diverged = match (
        source.finish_reason.as_deref(),
        req.finish_reason.as_deref(),
    ) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    };
    let tolerance = std::cmp::max(20, req.output_tokens / 4);
    let token_delta =
        (i64::from(source.output_tokens) - i64::from(req.output_tokens)).unsigned_abs();
    let tokens_diverged = token_delta > u64::from(tolerance);
    finish_diverged || tokens_diverged
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use uuid::Uuid;

    fn req_with(
        id_seed: u128,
        secs: i64,
        embedding: Option<Vec<f32>>,
        finish_reason: Option<&str>,
        output_tokens: u32,
    ) -> RequestLog {
        RequestLog {
            id: Uuid::from_u128(id_seed),
            org_id: Uuid::nil(),
            ts: Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap()
                + chrono::Duration::seconds(secs),
            provider: "anthropic".into(),
            model: "claude".into(),
            input_tokens: 100,
            output_tokens,
            cached_tokens: 0,
            cost_usd: 0.0,
            baseline_cost_usd: 0.0,
            cached: false,
            cache_layer: None,
            matched_route_id: None,
            latency_ms: 0,
            upstream_latency_ms: None,
            status: 200,
            tag: None,
            embedding,
            finish_reason: finish_reason.map(String::from),
            body: None,
            response_body: None,
            task_class: Default::default(),
            diff_saved_usd: None,
            minify_saved_est_usd: None,
        }
    }

    #[test]
    fn cosine_identical_vectors_is_one() {
        let a = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!(cosine(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_mismatched_length_returns_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
    }

    #[test]
    fn outcomes_diverged_token_delta() {
        let src = LiveEntry {
            ts: Utc::now(),
            embedding: vec![],
            finish_reason: None,
            output_tokens: 100,
        };
        let req = req_with(1, 0, None, None, 100);
        // delta = 0 → not divergent
        assert!(!outcomes_diverged(&src, &req));

        // delta = 30 > max(20, 25) → divergent (25% of 100 = 25, max is 25)
        let mut src2 = src.clone();
        src2.output_tokens = 130;
        assert!(outcomes_diverged(&src2, &req));
    }

    #[test]
    fn outcomes_diverged_finish_reason() {
        let src = LiveEntry {
            ts: Utc::now(),
            embedding: vec![],
            finish_reason: Some("length".into()),
            output_tokens: 100,
        };
        let req = req_with(1, 0, None, Some("stop"), 100);
        assert!(outcomes_diverged(&src, &req));
    }

    #[test]
    fn outcomes_diverged_missing_finish_reason_does_not_flag() {
        let src = LiveEntry {
            ts: Utc::now(),
            embedding: vec![],
            finish_reason: None,
            output_tokens: 100,
        };
        let req = req_with(1, 0, None, Some("stop"), 100);
        // Only one side has a finish reason → tokens equal → not divergent.
        assert!(!outcomes_diverged(&src, &req));
    }

    #[test]
    fn empty_sweep_when_no_embeddings() {
        let reqs = vec![
            req_with(1, 0, None, None, 10),
            req_with(2, 1, None, None, 10),
        ];
        let cfg = PlanConfig {
            l2_ttl_seconds: Some(60),
            ..PlanConfig::default()
        };
        let result = project_l2_hits(&reqs, &cfg);
        assert!(result.per_threshold.is_empty());
        assert_eq!(result.poisoning_candidates, 0);
    }

    #[test]
    fn poisoning_reported_per_threshold_and_deduped_in_aggregate() {
        // Two requests with identical embeddings (cosine == 1.0), so the
        // second hits the first at *every* threshold in the sweep. Their
        // outcomes diverge (finish_reason stop vs length) → the second is a
        // poisoning candidate at every threshold it hits.
        let emb = Some(vec![1.0_f32, 0.0, 0.0]);
        let reqs = vec![
            req_with(1, 0, emb.clone(), Some("length"), 100),
            req_with(2, 1, emb.clone(), Some("stop"), 100),
        ];
        let cfg = PlanConfig {
            l2_ttl_seconds: Some(600),
            // Three thresholds the identical pair clears at all of them.
            l2_threshold_sweep: vec![0.80, 0.90, 0.95],
            ..PlanConfig::default()
        };
        let result = project_l2_hits(&reqs, &cfg);

        // (a) Each per-threshold projection reports its OWN count. The pair
        // hits + diverges at every threshold, so every row reports exactly 1.
        assert_eq!(result.per_threshold.len(), 3);
        for proj in &result.per_threshold {
            assert_eq!(proj.projected_l2_hits, 1);
            assert_eq!(
                proj.poisoning_candidates, 1,
                "threshold {} should report its own poisoning count",
                proj.threshold
            );
        }

        // (b) The aggregate counts the candidate request ONCE — not 3× (the
        // old summed behaviour). One distinct poisoning-candidate request id.
        assert_eq!(
            result.poisoning_candidates, 1,
            "aggregate must dedup across the sweep, not sum (would be 3)"
        );
    }

    #[test]
    fn poisoning_aggregate_counts_distinct_requests() {
        // Two independent poisoning candidates (req 2 matches req 1, req 4
        // matches req 3) — distinct ids → aggregate is 2. Each threshold
        // reports both, but the aggregate stays 2 across the multi-threshold
        // sweep rather than 2 * number_of_thresholds.
        let a = Some(vec![1.0_f32, 0.0]);
        let b = Some(vec![0.0_f32, 1.0]);
        let reqs = vec![
            req_with(1, 0, a.clone(), Some("length"), 100),
            req_with(2, 1, a.clone(), Some("stop"), 100),
            req_with(3, 2, b.clone(), Some("length"), 100),
            req_with(4, 3, b.clone(), Some("stop"), 100),
        ];
        let cfg = PlanConfig {
            l2_ttl_seconds: Some(600),
            l2_threshold_sweep: vec![0.90, 0.95],
            ..PlanConfig::default()
        };
        let result = project_l2_hits(&reqs, &cfg);
        for proj in &result.per_threshold {
            assert_eq!(proj.poisoning_candidates, 2);
        }
        assert_eq!(result.poisoning_candidates, 2);
    }

    #[test]
    fn empty_sweep_when_ttl_none() {
        let reqs = vec![req_with(1, 0, Some(vec![1.0, 0.0]), None, 10)];
        let cfg = PlanConfig::default(); // l2_ttl_seconds is None
        let result = project_l2_hits(&reqs, &cfg);
        assert!(result.per_threshold.is_empty());
    }

    #[test]
    fn per_class_metrics_report_hits_misses_and_poisoning() {
        // Identical-embedding pair: req 2 hits req 1 at every threshold, and
        // their outcomes diverge → req 2 is a poisoning candidate. Both are
        // ChatCompletions (the v1 default class).
        let emb = Some(vec![1.0_f32, 0.0, 0.0]);
        let reqs = vec![
            req_with(1, 0, emb.clone(), Some("length"), 100),
            req_with(2, 1, emb.clone(), Some("stop"), 100),
        ];
        let cfg = PlanConfig {
            l2_ttl_seconds: Some(600),
            l2_threshold_sweep: vec![0.90, 0.92],
            ..PlanConfig::default()
        };
        let result = project_l2_hits(&reqs, &cfg);

        assert_eq!(result.per_class.len(), 1, "one class present (chat)");
        let m = &result.per_class[0];
        assert_eq!(m.task_class, L2TaskClass::ChatCompletions);
        // 2 thresholds × 2 considered = 4 considered; req 2 hits at both → 2 hits.
        assert_eq!(m.considered, 4);
        assert_eq!(m.hits, 2);
        assert_eq!(m.misses, 2);
        // Distinct poisoning request ids for the class, deduped across the sweep.
        assert_eq!(m.poisoning_candidates, 1);
    }

    #[test]
    fn per_class_metrics_empty_without_embeddings() {
        let reqs = vec![req_with(1, 0, None, None, 10)];
        let cfg = PlanConfig {
            l2_ttl_seconds: Some(60),
            ..PlanConfig::default()
        };
        let result = project_l2_hits(&reqs, &cfg);
        assert!(result.per_class.is_empty());
    }
}
