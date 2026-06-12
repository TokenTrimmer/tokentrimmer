//! L2 semantic cache — trait and implementations.
//!
//! The L2 cache stores compressed chat-completion responses alongside their
//! prompt embeddings and retrieves them by cosine similarity. This lets the
//! gateway serve cached responses for semantically equivalent queries even
//! when the exact request bytes differ.
//!
//! # Architecture
//!
//! ```text
//! Gateway request
//!   │
//!   ├─▶ L1 (Redis, exact SHA-256 key)  → hit: return cached bytes
//!   │
//!   └─▶ L2 (this module, cosine sim)   → hit: return if sim ≥ threshold
//!                                       → miss: call provider, write both L1 + L2
//! ```
//!
//! # Implementations
//!
//! - [`InMemoryL2Cache`] — `Vec` + `Mutex`, naïve O(n) cosine scan. Suitable
//!   for tests and local development only.
//! - [`PostgresL2Cache`] — HNSW pgvector index via `sqlx`. Production path.
//!   Its integration test is gated behind `#[ignore]` and requires a
//!   `DATABASE_URL` env var pointing at a Postgres instance with the `vector`
//!   extension installed.
//!
//! # Similarity threshold
//!
//! The default per-org threshold is **0.92** (ADR-008). Callers may override
//! it on a per-request basis by passing a different `threshold` value to
//! [`L2Cache::lookup`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tt_shared::{ChatCompletionRequest, ContentPart, Message, MessageContent};
use uuid::Uuid;

use crate::CacheError;

// ---------------------------------------------------------------------------
// Per-task-class thresholds
// ---------------------------------------------------------------------------

/// The global default L2 cosine-similarity threshold (ADR-008). This is the
/// floor for **every** task class: the per-class config can only ever raise a
/// class's threshold above this value, never lower it. Loosening below this
/// would serve cached answers at a similarity the gateway never accepted
/// before — a correctness regression, not a feature.
pub const DEFAULT_THRESHOLD: f32 = 0.92;

/// The class of request an L2 lookup belongs to. Mirrors (and is the cache-crate
/// home for) the higher-level `JudgeTaskClass` in `tt-core`, so the cache crate
/// can apply a per-class threshold without depending on `tt-core`. Callers in
/// `tt-core` map their `JudgeTaskClass` onto this enum.
///
/// `#[non_exhaustive]` so additional classes (embeddings re-rank, messages
/// ingress, …) can be added later without breaking match arms at call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TaskClass {
    /// `POST /v1/chat/completions` — the general chat class (the only one wired
    /// for v1; every other class falls back to the default threshold).
    ChatCompletions,
}

/// Per-class L2 threshold configuration. Globally-fixed for v1 (no per-org
/// config, no A/B). A class with no explicit entry resolves to
/// [`DEFAULT_THRESHOLD`]; an explicit entry is **floored** at
/// [`DEFAULT_THRESHOLD`] so no class can ever be configured below today's value.
///
/// Construct with [`ClassThresholds::new`] (every class at the default) and
/// optionally raise a specific class with [`ClassThresholds::with_class`].
#[derive(Debug, Clone)]
pub struct ClassThresholds {
    /// Floor + fallback for any class without an explicit (or `None`-class) entry.
    default: f32,
    /// Explicit per-class overrides. Always `>= default` (floored on insert).
    by_class: HashMap<TaskClass, f32>,
}

impl Default for ClassThresholds {
    fn default() -> Self {
        Self::new()
    }
}

impl ClassThresholds {
    /// A config where every task class resolves to [`DEFAULT_THRESHOLD`] (0.92).
    /// This is the safe v1 default: identical behaviour to the single-threshold
    /// gateway, just expressed per-class.
    #[must_use]
    pub fn new() -> Self {
        Self {
            default: DEFAULT_THRESHOLD,
            by_class: HashMap::new(),
        }
    }

    /// A config whose default floor is the gateway's configured global threshold.
    /// The floor is itself clamped up to [`DEFAULT_THRESHOLD`] so an operator who
    /// sets a *lower* global threshold can never drag the L2 bar below today's
    /// value — the per-class plumbing only ever tightens, never loosens.
    #[must_use]
    pub fn with_global_floor(global_threshold: f32) -> Self {
        Self {
            default: global_threshold.max(DEFAULT_THRESHOLD),
            by_class: HashMap::new(),
        }
    }

    /// Raise (never lower) the threshold for one class. The supplied value is
    /// floored at the config's `default` (== [`DEFAULT_THRESHOLD`] for a config
    /// built via [`ClassThresholds::new`]), so an accidental low value can never
    /// loosen a class below today's similarity bar.
    #[must_use]
    pub fn with_class(mut self, class: TaskClass, threshold: f32) -> Self {
        self.by_class.insert(class, threshold.max(self.default));
        self
    }

    /// The effective threshold for `class`. `None` (unknown / unclassified
    /// request) resolves to the default. Every result is `>= DEFAULT_THRESHOLD`.
    #[must_use]
    pub fn threshold_for(&self, class: Option<TaskClass>) -> f32 {
        match class.and_then(|c| self.by_class.get(&c).copied()) {
            Some(t) => t.max(self.default),
            None => self.default,
        }
    }
}

/// The safe per-class default threshold for `class`: always [`DEFAULT_THRESHOLD`]
/// for v1, for **every** class including unknown / unclassified (`None`). This is
/// the pure function the safety tests pin — no class may resolve below 0.92.
#[must_use]
pub fn class_threshold_for(class: Option<TaskClass>) -> f32 {
    ClassThresholds::new().threshold_for(class)
}

// ---------------------------------------------------------------------------
// Adaptive per-class thresholds (L2 false-positive gate, research Phase 2.2)
// ---------------------------------------------------------------------------

/// Hard ceiling for adaptive raises — a class threshold never exceeds this.
/// At 0.99 a near-exact paraphrase still hits; raising further would make the
/// L2 cache useless rather than safer.
pub const ADAPTIVE_THRESHOLD_CEILING: f32 = 0.99;

/// Tuning for the FP-rate → threshold controller. All defaults conservative.
///
/// The FP *tolerance* is deliberately NOT a field here: it is supplied per
/// recorded verdict ([`AdaptiveClassThresholds::record_judged_band_hit`]) from
/// the verify-gate config, so the tolerance knob lives in exactly one place —
/// a duplicate field would invite the two values to silently diverge.
#[derive(Debug, Clone, Copy)]
pub struct FpGateTuning {
    /// Judged in-band classified samples per adaptation batch (default 20).
    /// A batch smaller than this never adapts — one unlucky degraded sample
    /// must not move the threshold.
    pub min_samples: u32,
    /// Threshold raise per breaching batch (default 0.005). Small steps: the
    /// ratchet converges over batches instead of overshooting on one.
    pub step: f32,
}

impl Default for FpGateTuning {
    fn default() -> Self {
        Self {
            min_samples: 20,
            step: 0.005,
        }
    }
}

impl FpGateTuning {
    /// Construct with the documented clamps: `min_samples` floored at 1,
    /// `step` floored at 0 (a zero step disables adaptation without disabling
    /// measurement).
    #[must_use]
    pub fn new(min_samples: u32, step: f32) -> Self {
        Self {
            min_samples: min_samples.max(1),
            step: step.max(0.0),
        }
    }
}

/// Per-class FP batch state behind the [`AdaptiveClassThresholds`] mutex.
#[derive(Debug, Clone, Copy)]
struct ClassFpState {
    /// The adaptively-raised threshold for this class, when a batch has
    /// breached. Always `>= base.threshold_for(class)` and
    /// `<= ADAPTIVE_THRESHOLD_CEILING` by construction.
    raised: Option<f32>,
    /// Classified (non-Unclear) judged in-band samples in the current batch.
    judged: u32,
    /// Degraded verdicts in the current batch (the FP signal).
    degraded: u32,
    /// The strictest (minimum) tolerance seen across the batch — mixed
    /// tolerances adapt against the strictest, the conservative choice.
    min_tolerance_pct: f64,
}

impl Default for ClassFpState {
    fn default() -> Self {
        Self {
            raised: None,
            judged: 0,
            degraded: 0,
            min_tolerance_pct: f64::INFINITY,
        }
    }
}

/// Adaptive per-class thresholds: a **ratchet** over a static
/// [`ClassThresholds`] base.
///
/// `effective(class) = max(base.threshold_for(class), raised[class])` — raises
/// only, never lowers, never below the [`DEFAULT_THRESHOLD`] floor (inherited
/// from the base), capped at [`ADAPTIVE_THRESHOLD_CEILING`]. The raise signal
/// is the measured false-positive rate of judged *ambiguous-band* L2 hits: when
/// a batch of `min_samples` classified verdicts shows
/// `degraded/judged*100 > tolerance`, the class threshold steps up by
/// `tuning.step`.
///
/// State is in-process (resets on restart) — the static base config is the
/// durable floor, so a restart can only ever *loosen back to the configured
/// floor*, never below today's bar. Durable raises are a deliberate follow-up.
///
/// `Send + Sync` (mutex inside) so one instance can be shared between the
/// request path (threshold reads) and the detached judge tasks (verdict feeds).
pub struct AdaptiveClassThresholds {
    base: ClassThresholds,
    tuning: FpGateTuning,
    inner: Mutex<HashMap<Option<TaskClass>, ClassFpState>>,
}

impl std::fmt::Debug for AdaptiveClassThresholds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdaptiveClassThresholds")
            .field("base", &self.base)
            .field("tuning", &self.tuning)
            .finish_non_exhaustive()
    }
}

impl AdaptiveClassThresholds {
    /// Wrap `base` with the FP controller `tuning`. Until a batch breaches,
    /// `effective_threshold` equals `base.threshold_for` exactly.
    #[must_use]
    pub fn new(base: ClassThresholds, tuning: FpGateTuning) -> Self {
        Self {
            base,
            tuning,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// The effective threshold for `class`: always
    /// `>= base.threshold_for(class) >= DEFAULT_THRESHOLD`. This is the
    /// invariant the safety tests pin — adaptation can only tighten.
    #[must_use]
    pub fn effective_threshold(&self, class: Option<TaskClass>) -> f32 {
        let base = self.base.threshold_for(class);
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match guard.get(&class).and_then(|s| s.raised) {
            Some(raised) => raised.max(base),
            None => base,
        }
    }

    /// The adaptive raise currently applied to `class`, if any. Introspection
    /// for tests and metrics — `None` means the static base is in force.
    #[must_use]
    pub fn raised_for(&self, class: Option<TaskClass>) -> Option<f32> {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.get(&class).and_then(|s| s.raised)
    }

    /// Feed one judged AMBIGUOUS-BAND hit into the FP estimator.
    ///
    /// `degraded`: `Some(true)` = a confirmed false positive, `Some(false)` =
    /// a clean hit, `None` = an `Unclear` verdict — ignored entirely (excluded
    /// from the denominator, mirroring `VerdictTally`). `tolerance_pct` is the
    /// effective tolerance at judgment time; a mixed batch adapts against the
    /// MINIMUM seen (strictest wins — conservative).
    ///
    /// When the batch reaches `tuning.min_samples` classified verdicts:
    /// `fp_pct = degraded/judged*100`; a breach (`fp_pct > min_tolerance`)
    /// raises the effective threshold by `tuning.step` (capped at
    /// [`ADAPTIVE_THRESHOLD_CEILING`]); the batch resets either way.
    ///
    /// Returns `true` iff this call raised the threshold — the caller's
    /// metrics hook (tt-cache itself has no metrics dependency).
    pub fn record_judged_band_hit(
        &self,
        class: Option<TaskClass>,
        degraded: Option<bool>,
        tolerance_pct: f64,
    ) -> bool {
        let Some(is_degraded) = degraded else {
            return false; // Unclear: excluded from the denominator entirely.
        };
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let state = guard.entry(class).or_default();
        state.judged += 1;
        if is_degraded {
            state.degraded += 1;
        }
        state.min_tolerance_pct = state.min_tolerance_pct.min(tolerance_pct.clamp(0.5, 2.0));
        if state.judged < self.tuning.min_samples {
            return false;
        }
        let fp_pct = f64::from(state.degraded) / f64::from(state.judged) * 100.0;
        let breached = fp_pct > state.min_tolerance_pct;
        let mut raised_now = false;
        if breached {
            let base = self.base.threshold_for(class);
            let effective = state.raised.map_or(base, |r| r.max(base));
            let next = (effective + self.tuning.step).min(ADAPTIVE_THRESHOLD_CEILING);
            if next > effective {
                state.raised = Some(next);
                raised_now = true;
            }
        }
        // Reset the batch either way — each adaptation decision uses fresh
        // samples taken AT the (possibly new) threshold.
        state.judged = 0;
        state.degraded = 0;
        state.min_tolerance_pct = f64::INFINITY;
        raised_now
    }
}

// ---------------------------------------------------------------------------
// CacheEntry
// ---------------------------------------------------------------------------

/// A single entry in the L2 semantic cache.
///
/// Stored in Postgres (`cache_entries` table) and represented in memory when
/// working with [`InMemoryL2Cache`].
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Unique row identifier.
    pub id: Uuid,
    /// Organization that owns this entry. Used for tenant isolation.
    pub org_id: Uuid,
    /// Dense embedding vector for the cached prompt. Dimensionality matches
    /// the embedding model (e.g. 1536 for `text-embedding-3-small`).
    pub embedding: Vec<f32>,
    /// Serialized [`tt_shared::ChatCompletionResponse`] bytes (JSON).
    pub response: Vec<u8>,
    /// The **chat** model used to produce the response (e.g. `"gpt-4o"`),
    /// *not* the embedding model.
    pub model: String,
    /// The embedding model that produced [`CacheEntry::embedding`]
    /// (e.g. `"text-embedding-3-small"`). Lookup filters on this so that
    /// a switch in embedding models never compares vectors from different
    /// spaces (which would silently degrade or corrupt similarity scores).
    pub embedding_model: String,
    /// Number of prompt tokens consumed.
    pub input_tokens: u64,
    /// Number of completion tokens produced.
    pub output_tokens: u64,
    /// Catalog-derived baseline cost (USD) of producing this response — what
    /// the original request would have paid with no cache, computed at insert
    /// time from the versioned pricing catalog (the same math as the gateway's
    /// `compute_cost`). `None` for rows inserted before migration 0010, or
    /// when the model was absent from the catalog at insert time; the hit
    /// path then re-prices the stored model/token counts against the current
    /// catalog (or reports 0 saved) rather than fabricating a number.
    pub baseline_cost_usd: Option<f64>,
    /// How many times this entry has been served from cache.
    pub hit_count: u64,
    /// Latest judge quality score in `[0, 1]` for a response served from this
    /// entry (`1.0` = quality preserved, `0.0` = degraded). `None` until a judge
    /// has scored a response served from this entry (the common case — only a
    /// ~2% sample of downgraded traffic is judged). Additive + nullable
    /// (migration 0013); pre-0013 rows read as `None`.
    pub quality_score: Option<f32>,
    /// Latest judge verdict recorded against this entry
    /// (`acceptable` / `degraded` / `unclear`). `None` until a judge has scored a
    /// response served from this entry. Additive + nullable (migration 0013).
    pub judge_verdict: Option<String>,
    /// Wall-clock time the entry was created.
    pub created_at: DateTime<Utc>,
    /// Wall-clock time after which the entry must not be served.
    pub expires_at: DateTime<Utc>,
    /// One-way 64-bit SimHash ([`crate::lexical_sig`]) of the canonicalized
    /// embedded context text — NEVER the text itself (the privacy invariant of
    /// migration 0002 stands: vectors + responses + a 64-bit sketch, no source
    /// prompts). Read by the L2 verify gate to confirm an ambiguous-band hit
    /// lexically agrees with the incoming query. `None` for rows inserted
    /// before migration 0018; the verify gate fails open on `None`.
    pub lexical_sig: Option<i64>,
}

// ---------------------------------------------------------------------------
// Judge-driven eviction
// ---------------------------------------------------------------------------

/// The judge's risk band for a response that was **served from L2**. The cache
/// crate's own minimal mirror of `tt_plan_core::RiskBand`, so the cache can make
/// an eviction decision without depending on `tt-plan-core`. Callers map their
/// `RiskBand` onto this enum.
///
/// Only [`JudgeBand::High`] (a clearly degraded response) triggers a targeted
/// eviction; `Low` / `Medium` / unclassified verdicts only record the score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeBand {
    /// Degraded share ≤ 5% — quality preserved. Record only, never evict.
    Low,
    /// Degraded share in `(5%, 15%]`. Record only, never evict.
    Medium,
    /// Degraded share > 15% — clearly degraded. The single-entry eviction signal.
    High,
}

impl JudgeBand {
    /// Whether this band warrants evicting the specific entry that served the
    /// judged response. **Only** `High` does — the conservative, targeted rule.
    #[must_use]
    pub fn warrants_eviction(self) -> bool {
        matches!(self, JudgeBand::High)
    }
}

/// What [`L2Cache::record_judge_verdict`] did with a judged-from-L2 verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeRecordOutcome {
    /// The score/verdict was recorded on the entry; the entry was kept.
    Recorded,
    /// The band was `High`/Degraded: the specific entry was evicted (deleted by
    /// id) and is no longer servable. The verdict is recorded for audit where
    /// the backing store supports it.
    Evicted,
    /// The entry id was not found (already expired/evicted). No-op.
    NotFound,
}

// ---------------------------------------------------------------------------
// Paraphrase-dedup analytics (read-only)
// ---------------------------------------------------------------------------

/// One near-duplicate cluster found by [`L2Cache::analyze_dedup`]. A cluster is
/// a set of cache entries whose embeddings are mutually near-duplicate (cosine
/// `>= DEDUP_SIMILARITY`). Reported for analytics only — never mutated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DedupCluster {
    /// The entry id chosen as the cluster representative (the highest-`hit_count`
    /// member — the one worth keeping).
    pub representative_id: Uuid,
    /// All entry ids in the cluster, including the representative. Length ≥ 2
    /// (singletons are not reported as clusters).
    pub member_ids: Vec<Uuid>,
    /// Sum of `hit_count` across all members — the dedup opportunity's "weight".
    pub total_hit_count: u64,
}

/// The result of [`L2Cache::analyze_dedup`] for one org. Read-only analytics:
/// reports where near-duplicate embeddings cluster so a later admin tool can
/// quantify the dedup opportunity. Never deletes or updates anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DedupReport {
    /// Total non-expired entries considered (after any sampling bound).
    pub total_entries: u64,
    /// Number of multi-member near-duplicate clusters found.
    pub cluster_count: u64,
    /// Count of entries that are a *non-representative* member of some cluster —
    /// i.e. the entries that could be deduped away. `entries_deduped /
    /// total_entries` is the dedup ratio.
    pub entries_deduped: u64,
    /// Top clusters by `total_hit_count`, descending (bounded — see
    /// [`DEDUP_TOP_CLUSTERS`]).
    pub top_clusters: Vec<DedupCluster>,
}

/// Cosine-similarity threshold above which two embeddings are treated as
/// near-duplicates for dedup analytics. Deliberately higher than the L2 *serving*
/// threshold (0.92): dedup is about "these are effectively the same prompt",
/// which is a tighter bar than "close enough to serve a cached answer".
pub const DEDUP_SIMILARITY: f32 = 0.95;

/// Largest org the in-memory dedup analysis scans without sampling. Bounds the
/// O(N²) clustering so a huge org can't stall the analytics path.
pub const DEDUP_MAX_ENTRIES: usize = 5_000;

/// How many top clusters [`DedupReport::top_clusters`] retains.
pub const DEDUP_TOP_CLUSTERS: usize = 20;

// ---------------------------------------------------------------------------
// L2Cache trait
// ---------------------------------------------------------------------------

/// Semantic L2 cache contract.
///
/// Implementations must be `Send + Sync` so they can be shared across async
/// tasks via `Arc<dyn L2Cache>`.
#[async_trait]
pub trait L2Cache: Send + Sync {
    /// Insert a new [`CacheEntry`].
    ///
    /// Implementors should replace or ignore duplicate `id` values; callers
    /// generate a fresh UUID per insert.
    async fn insert(&self, entry: CacheEntry) -> Result<(), CacheError>;

    /// Find the nearest entry to `query_embedding` for `org_id`.
    ///
    /// Returns `Some((entry, similarity))` if the best match has a cosine
    /// similarity ≥ `threshold` **and** its `expires_at` is in the future
    /// **and** the entry was produced by `chat_model` and `embedding_model`.
    /// Returns `None` if no qualifying entry is found.
    ///
    /// Filtering on `chat_model` prevents a `gpt-4o` request from being
    /// served a response generated by a different model. Filtering on
    /// `embedding_model` ensures vectors from different embedding spaces are
    /// never compared — an embedding-model swap would otherwise silently
    /// degrade similarity scores.
    async fn lookup(
        &self,
        org_id: Uuid,
        query_embedding: &[f32],
        threshold: f32,
        chat_model: &str,
        embedding_model: &str,
    ) -> Result<Option<(CacheEntry, f32)>, CacheError>;

    /// Increment `hit_count` for the entry identified by `id`.
    ///
    /// This is a best-effort, idempotent operation. Implementations may
    /// silently swallow errors to avoid blocking the hot path.
    async fn bump_hit_count(&self, id: Uuid) -> Result<(), CacheError>;

    /// Delete the single entry identified by `id`. Targeted + idempotent: if the
    /// id no longer exists, this is a no-op `Ok(())`. Used by judge-driven
    /// eviction to remove **exactly** the entry that served a degraded response —
    /// never a bulk delete.
    async fn evict(&self, id: Uuid) -> Result<(), CacheError>;

    /// Record the latest judge `score` (and verdict string) against the entry
    /// `id` that served a judged-from-L2 response. The score is recorded
    /// regardless of band; **only** a `High`/Degraded `band` additionally
    /// evicts that one entry (see [`JudgeBand::warrants_eviction`]).
    ///
    /// Returns what happened: [`JudgeRecordOutcome::Evicted`] when the band was
    /// `High`, [`JudgeRecordOutcome::Recorded`] when it was kept, or
    /// [`JudgeRecordOutcome::NotFound`] when the entry was already gone.
    ///
    /// Conservative by construction: never bulk-deletes, never touches any other
    /// entry, and never evicts on `Low` / `Medium`.
    async fn record_judge_verdict(
        &self,
        id: Uuid,
        score: Option<f32>,
        verdict: &str,
        band: JudgeBand,
    ) -> Result<JudgeRecordOutcome, CacheError>;

    /// Read-only paraphrase-dedup analytics for `org_id`. Clusters non-expired
    /// entries whose embeddings are mutually near-duplicate (cosine
    /// `>= DEDUP_SIMILARITY`) and reports the dedup opportunity. Never mutates
    /// the cache (no DELETE/UPDATE). Large orgs are bounded/sampled.
    async fn analyze_dedup(&self, org_id: Uuid) -> Result<DedupReport, CacheError>;

    /// Look up the nearest entry, applying the **per-class** threshold for
    /// `task_class` from `thresholds`. Falls back to the config default when
    /// `task_class` is `None`. The resolved threshold is always `>= the config's
    /// default floor`, which is itself `>= DEFAULT_THRESHOLD` (see
    /// [`ClassThresholds::with_global_floor`]) — so a classed lookup can never
    /// serve a hit below today's similarity bar. This is the hard safety
    /// invariant.
    ///
    /// Default impl delegates to [`L2Cache::lookup`] with the resolved threshold,
    /// so every implementation gets per-class behaviour for free.
    async fn lookup_classed(
        &self,
        org_id: Uuid,
        query_embedding: &[f32],
        thresholds: &ClassThresholds,
        task_class: Option<TaskClass>,
        chat_model: &str,
        embedding_model: &str,
    ) -> Result<Option<(CacheEntry, f32)>, CacheError> {
        let effective = thresholds.threshold_for(task_class);
        self.lookup(
            org_id,
            query_embedding,
            effective,
            chat_model,
            embedding_model,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Cosine similarity
// ---------------------------------------------------------------------------

/// Compute the cosine similarity between two vectors.
///
/// Returns a value in `[-1.0, 1.0]`. Returns `0.0` if either vector has zero
/// magnitude so that zero-length vectors never match.
///
/// OpenAI `text-embedding-3` models return L2-normalized vectors, so for those
/// `dot(a, b) == cosine(a, b)`. We use the general form here so that
/// [`MockEmbedder`] vectors in tests do not need to be normalized.
///
/// [`MockEmbedder`]: crate::embed::MockEmbedder
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

/// True iff every component of the embedding is finite (no NaN/Inf). A
/// non-finite component would make cosine similarity NaN and corrupt ranking,
/// so such vectors are rejected at insert time (§4.15).
fn embedding_is_finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}

// ---------------------------------------------------------------------------
// l2_context_text — canonicalized embedding input  (fix §2.3)
// ---------------------------------------------------------------------------

/// Build the canonicalized text that is embedded for L2 cache lookup/insert.
///
/// Using only the last user message as the embedding key causes different
/// conversations that share a final turn (e.g. `"yes"`, `"continue"`) to
/// collide and return each other's cached responses.
///
/// Instead we build a compact, ordered representation of the semantically
/// meaningful context:
///
/// - The system prompt (if any) comes first, prefixed with `"[system] "`.
/// - Each non-system message is appended in order with a role prefix
///   (`"[user] "` or `"[assistant] "`).
///
/// `tool` messages are intentionally omitted — their content is typically
/// opaque JSON that adds noise without semantic signal.
///
/// Returns `None` only if `req.messages` is empty or contains no text at all
/// (purely-multimodal request).
///
/// This function is `pub` so the test suite can verify it directly; callers
/// inside `crates/core` should import it from `tt_cache`.
pub fn l2_context_text(req: &ChatCompletionRequest) -> Option<String> {
    fn message_text(content: &MessageContent) -> &str {
        match content {
            MessageContent::Text(s) => s.as_str(),
            MessageContent::Parts(parts) => {
                for p in parts {
                    if let ContentPart::Text { text } = p {
                        return text.as_str();
                    }
                }
                ""
            }
        }
    }

    let mut parts: Vec<String> = Vec::new();

    for msg in &req.messages {
        match msg {
            Message::System { content } => {
                let t = message_text(content);
                if !t.is_empty() {
                    parts.push(format!("[system] {t}"));
                }
            }
            Message::User { content, .. } => {
                let t = message_text(content);
                if !t.is_empty() {
                    parts.push(format!("[user] {t}"));
                }
            }
            Message::Assistant { content, .. } => {
                if let Some(c) = content {
                    let t = message_text(c);
                    if !t.is_empty() {
                        parts.push(format!("[assistant] {t}"));
                    }
                }
            }
            // Tool messages are omitted — opaque JSON, low semantic signal.
            Message::Tool { .. } => {}
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// InMemoryL2Cache
// ---------------------------------------------------------------------------

/// In-memory L2 cache for unit tests and local development.
///
/// Uses a `Vec<CacheEntry>` protected by a `Mutex`. Lookup is O(n) — not
/// suitable for production, where [`PostgresL2Cache`] provides an HNSW index.
///
/// # Example
///
/// ```rust
/// use tt_cache::l2::{InMemoryL2Cache, L2Cache, CacheEntry};
/// use uuid::Uuid;
/// use chrono::Utc;
///
/// # #[tokio::main]
/// # async fn main() {
/// let cache = InMemoryL2Cache::new();
/// let entry = CacheEntry {
///     id: Uuid::new_v4(),
///     org_id: Uuid::new_v4(),
///     embedding: vec![1.0, 0.0],
///     response: b"{}".to_vec(),
///     model: "gpt-4o".to_string(),
///     embedding_model: "text-embedding-3-small".to_string(),
///     input_tokens: 10,
///     output_tokens: 5,
///     baseline_cost_usd: Some(0.000045),
///     hit_count: 0,
///     quality_score: None,
///     judge_verdict: None,
///     created_at: Utc::now(),
///     expires_at: Utc::now() + chrono::Duration::seconds(3600),
///     lexical_sig: None,
/// };
/// cache.insert(entry).await.unwrap();
/// # }
/// ```
pub struct InMemoryL2Cache {
    entries: Arc<Mutex<Vec<CacheEntry>>>,
}

impl InMemoryL2Cache {
    /// Create a new, empty in-memory L2 cache.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Default for InMemoryL2Cache {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl L2Cache for InMemoryL2Cache {
    /// Insert `entry` into the in-memory store.
    async fn insert(&self, entry: CacheEntry) -> Result<(), CacheError> {
        if !embedding_is_finite(&entry.embedding) {
            return Err(CacheError::InvalidEmbedding);
        }
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        guard.push(entry);
        Ok(())
    }

    /// Scan all entries for `org_id`, filter by expiry, chat model, and
    /// embedding model, compute cosine similarity, and return the best match
    /// if it meets `threshold`.
    async fn lookup(
        &self,
        org_id: Uuid,
        query_embedding: &[f32],
        threshold: f32,
        chat_model: &str,
        embedding_model: &str,
    ) -> Result<Option<(CacheEntry, f32)>, CacheError> {
        let now = Utc::now();
        let guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());

        let best = guard
            .iter()
            .filter(|e| {
                e.org_id == org_id
                    && e.expires_at > now
                    && e.model == chat_model
                    && e.embedding_model == embedding_model
            })
            .map(|e| {
                let sim = cosine(&e.embedding, query_embedding);
                (e, sim)
            })
            .filter(|(_, sim)| sim.is_finite() && *sim >= threshold)
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        Ok(best.map(|(e, sim)| (e.clone(), sim)))
    }

    /// Increment `hit_count` for the entry with the given `id`.
    async fn bump_hit_count(&self, id: Uuid) -> Result<(), CacheError> {
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = guard.iter_mut().find(|e| e.id == id) {
            entry.hit_count += 1;
        }
        Ok(())
    }

    /// Remove exactly the entry with `id`. No-op if absent.
    async fn evict(&self, id: Uuid) -> Result<(), CacheError> {
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|e| e.id != id);
        Ok(())
    }

    /// Record the judge score/verdict on the entry, evicting it iff the band is
    /// `High`. Conservative: touches only the single entry identified by `id`.
    async fn record_judge_verdict(
        &self,
        id: Uuid,
        score: Option<f32>,
        verdict: &str,
        band: JudgeBand,
    ) -> Result<JudgeRecordOutcome, CacheError> {
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pos) = guard.iter().position(|e| e.id == id) else {
            return Ok(JudgeRecordOutcome::NotFound);
        };
        // Record on the live row first so an evicted entry's last-known verdict
        // is consistent if a store later persists it.
        guard[pos].quality_score = score;
        guard[pos].judge_verdict = Some(verdict.to_string());
        if band.warrants_eviction() {
            guard.remove(pos);
            return Ok(JudgeRecordOutcome::Evicted);
        }
        Ok(JudgeRecordOutcome::Recorded)
    }

    /// Cluster this org's non-expired entries by near-duplicate cosine
    /// similarity. Read-only — never mutates the store.
    async fn analyze_dedup(&self, org_id: Uuid) -> Result<DedupReport, CacheError> {
        let now = Utc::now();
        let guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let mut candidates: Vec<(Uuid, &[f32], u64)> = guard
            .iter()
            .filter(|e| e.org_id == org_id && e.expires_at > now)
            .map(|e| (e.id, e.embedding.as_slice(), e.hit_count))
            .collect();
        // Bound large orgs: deterministically keep the highest-hit_count entries
        // (the dedup signal that matters) so the O(N²) clustering stays cheap.
        // Sort by (hit_count desc, id) for a stable, reproducible sample.
        if candidates.len() > DEDUP_MAX_ENTRIES {
            candidates.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
            candidates.truncate(DEDUP_MAX_ENTRIES);
        }
        Ok(cluster_near_duplicates(&candidates))
    }
}

/// Pure near-duplicate clustering over `(id, embedding, hit_count)` rows. Single
/// linkage by cosine `>= DEDUP_SIMILARITY`: two entries land in the same cluster
/// if either is within the threshold of a member. Read-only — produces a
/// [`DedupReport`] without touching any cache. Shared by the in-memory impl and
/// directly unit-testable on synthetic vectors.
#[must_use]
fn cluster_near_duplicates(entries: &[(Uuid, &[f32], u64)]) -> DedupReport {
    let n = entries.len();
    let total_entries = n as u64;
    // Union-find over the entry indices.
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if cosine(entries[i].1, entries[j].1) >= DEDUP_SIMILARITY {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }
    // Group indices by root.
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }

    let mut clusters: Vec<DedupCluster> = Vec::new();
    let mut entries_deduped: u64 = 0;
    for members in groups.values() {
        if members.len() < 2 {
            continue; // singletons are not a dedup opportunity
        }
        // Representative = highest hit_count (ties broken by smallest id) — the
        // entry worth keeping.
        let rep_idx = *members
            .iter()
            .max_by(|&&a, &&b| {
                entries[a]
                    .2
                    .cmp(&entries[b].2)
                    .then_with(|| entries[b].0.cmp(&entries[a].0))
            })
            .expect("non-empty cluster");
        let representative_id = entries[rep_idx].0;
        let member_ids: Vec<Uuid> = members.iter().map(|&i| entries[i].0).collect();
        let total_hit_count: u64 = members.iter().map(|&i| entries[i].2).sum();
        entries_deduped += (members.len() - 1) as u64;
        clusters.push(DedupCluster {
            representative_id,
            member_ids,
            total_hit_count,
        });
    }

    let cluster_count = clusters.len() as u64;
    // Top clusters by total_hit_count desc (ties by representative id for
    // determinism), bounded.
    clusters.sort_by(|a, b| {
        b.total_hit_count
            .cmp(&a.total_hit_count)
            .then_with(|| a.representative_id.cmp(&b.representative_id))
    });
    clusters.truncate(DEDUP_TOP_CLUSTERS);

    DedupReport {
        total_entries,
        cluster_count,
        entries_deduped,
        top_clusters: clusters,
    }
}

// ---------------------------------------------------------------------------
// PostgresL2Cache
// ---------------------------------------------------------------------------

/// Production L2 cache backed by Postgres + pgvector HNSW index.
///
/// Uses the `<=>` cosine-distance operator from the `vector` extension.
/// The SQL query orders by cosine distance ascending (nearest first) and
/// applies the similarity filter `1 - distance >= threshold`.
///
/// # Setup
///
/// 1. Run `crates/core/migrations/0002_cache_entries.up.sql` against your
///    Postgres instance.
/// 2. Pass a connected [`sqlx::PgPool`] to [`PostgresL2Cache::new`].
///
/// # Note on `sqlx::query!` macro
///
/// We use the un-macro `sqlx::query(...)` form throughout to avoid the
/// compile-time `DATABASE_URL` requirement. This means type-checking of SQL
/// happens at runtime rather than compile time.
/// HNSW `ef_search` applied to org-filtered lookups.
///
/// pgvector's default is 40. With a `WHERE org_id = $1` filter, the HNSW graph
/// walk explores neighbours by vector distance and only *then* discards rows
/// that belong to other tenants — so under multi-tenant load the 40-candidate
/// list fills with other orgs' near vectors and the querying org's own nearest
/// neighbour can fall outside it, producing a false cache miss (poor recall).
/// Raising `ef_search` widens the candidate list so the org's vectors reliably
/// make the cut. 100 restores high recall at a small latency cost; tune via
/// [`PostgresL2Cache::with_ef_search`].
pub const DEFAULT_EF_SEARCH: i64 = 100;

pub struct PostgresL2Cache {
    pool: sqlx::PgPool,
    ef_search: i64,
}

impl PostgresL2Cache {
    /// Wrap an existing connected pool, using [`DEFAULT_EF_SEARCH`].
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            ef_search: DEFAULT_EF_SEARCH,
        }
    }

    /// Builder-style override of the HNSW `ef_search` used per lookup. Clamped
    /// to at least 1. Higher = better recall under multi-tenant load, slightly
    /// more work per query.
    #[must_use]
    pub fn with_ef_search(mut self, ef_search: i64) -> Self {
        self.ef_search = ef_search.max(1);
        self
    }

    /// The HNSW `ef_search` this cache applies per lookup.
    pub fn ef_search(&self) -> i64 {
        self.ef_search
    }
}

#[async_trait]
impl L2Cache for PostgresL2Cache {
    /// Insert a [`CacheEntry`] into the `cache_entries` table.
    ///
    /// Uses `ON CONFLICT DO NOTHING` so duplicate `id` values are silently
    /// ignored (callers always generate a fresh UUID per insert).
    async fn insert(&self, entry: CacheEntry) -> Result<(), CacheError> {
        if !embedding_is_finite(&entry.embedding) {
            return Err(CacheError::InvalidEmbedding);
        }
        // Convert Vec<f32> to pgvector::Vector for the Postgres `vector` column.
        let vec = pgvector::Vector::from(entry.embedding);

        sqlx::query(
            r#"
            INSERT INTO cache_entries
                (id, org_id, embedding, response, model, embedding_model,
                 input_tokens, output_tokens, baseline_cost_usd, hit_count,
                 quality_score, judge_verdict, created_at, expires_at,
                 lexical_sig)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(entry.id)
        .bind(entry.org_id)
        .bind(vec)
        .bind(
            serde_json::from_slice::<serde_json::Value>(&entry.response)
                .map_err(CacheError::Serde)?,
        )
        .bind(&entry.model)
        .bind(&entry.embedding_model)
        .bind(entry.input_tokens as i64)
        .bind(entry.output_tokens as i64)
        .bind(entry.baseline_cost_usd)
        .bind(entry.hit_count as i64)
        .bind(entry.quality_score)
        .bind(&entry.judge_verdict)
        .bind(entry.created_at)
        .bind(entry.expires_at)
        .bind(entry.lexical_sig)
        .execute(&self.pool)
        .await
        .map_err(CacheError::Sqlx)?;

        Ok(())
    }

    /// Query the `cache_entries` table for the nearest entry by cosine
    /// similarity, scoped to `org_id`, `chat_model`, `embedding_model`, and
    /// non-expired rows.
    ///
    /// The `<=>` operator is pgvector's cosine-distance operator (lower = more
    /// similar). `1 - distance` converts to cosine similarity.
    ///
    /// `chat_model` prevents serving a response produced by a different LLM.
    /// `embedding_model` prevents comparing vectors from different embedding
    /// spaces. Rows with a NULL `embedding_model` (inserted before migration
    /// 0007) are excluded — they cannot be safely matched.
    async fn lookup(
        &self,
        org_id: Uuid,
        query_embedding: &[f32],
        threshold: f32,
        chat_model: &str,
        embedding_model: &str,
    ) -> Result<Option<(CacheEntry, f32)>, CacheError> {
        let vec = pgvector::Vector::from(query_embedding.to_vec());

        // Run inside a transaction so `SET LOCAL hnsw.ef_search` scopes to this
        // query only. The raised ef_search keeps recall high once the org
        // filter discards other tenants' candidates (see [`DEFAULT_EF_SEARCH`]).
        // `SET` does not accept bind parameters, so the value is formatted in —
        // it is an `i64` we own (never user input), so this is injection-safe.
        let mut tx = self.pool.begin().await.map_err(CacheError::Sqlx)?;
        sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", self.ef_search))
            .execute(&mut *tx)
            .await
            .map_err(CacheError::Sqlx)?;

        let row = sqlx::query(
            r#"
            SELECT id, org_id, embedding, response, model, embedding_model,
                   input_tokens, output_tokens, baseline_cost_usd, hit_count,
                   quality_score, judge_verdict, created_at, expires_at,
                   lexical_sig,
                   CAST(1.0 - (embedding <=> $2) AS REAL) AS similarity
              FROM cache_entries
             WHERE org_id = $1
               AND expires_at > now()
               AND model = $4
               AND embedding_model = $5
               AND CAST(1.0 - (embedding <=> $2) AS REAL) >= $3
             ORDER BY embedding <=> $2
             LIMIT 1
            "#,
        )
        .bind(org_id)
        .bind(vec)
        .bind(threshold)
        .bind(chat_model)
        .bind(embedding_model)
        .fetch_optional(&mut *tx)
        .await
        .map_err(CacheError::Sqlx)?;

        tx.commit().await.map_err(CacheError::Sqlx)?;

        let Some(row) = row else {
            return Ok(None);
        };

        use sqlx::Row;
        let id: Uuid = row.try_get("id").map_err(CacheError::Sqlx)?;
        let org_id: Uuid = row.try_get("org_id").map_err(CacheError::Sqlx)?;
        let embedding_vec: pgvector::Vector = row.try_get("embedding").map_err(CacheError::Sqlx)?;
        let response_json: serde_json::Value = row.try_get("response").map_err(CacheError::Sqlx)?;
        let model: String = row.try_get("model").map_err(CacheError::Sqlx)?;
        let embedding_model_col: String =
            row.try_get("embedding_model").map_err(CacheError::Sqlx)?;
        // `input_tokens` / `output_tokens` are INT (INT4) in the schema
        // (migration 0002) — sqlx's strict decoding rejects an i64 read.
        let input_tokens: i32 = row.try_get("input_tokens").map_err(CacheError::Sqlx)?;
        let output_tokens: i32 = row.try_get("output_tokens").map_err(CacheError::Sqlx)?;
        // NULL for rows inserted before migration 0010 (or when the model was
        // missing from the catalog at insert) — surfaced as `None` so the hit
        // path can apply its honest fallback instead of a fabricated rate.
        let baseline_cost_usd: Option<f64> =
            row.try_get("baseline_cost_usd").map_err(CacheError::Sqlx)?;
        let hit_count: i64 = row.try_get("hit_count").map_err(CacheError::Sqlx)?;
        // NULL for rows inserted before migration 0013 (judge join) or never
        // judged — surfaced as `None`.
        let quality_score: Option<f32> = row.try_get("quality_score").map_err(CacheError::Sqlx)?;
        let judge_verdict: Option<String> =
            row.try_get("judge_verdict").map_err(CacheError::Sqlx)?;
        let created_at: DateTime<Utc> = row.try_get("created_at").map_err(CacheError::Sqlx)?;
        let expires_at: DateTime<Utc> = row.try_get("expires_at").map_err(CacheError::Sqlx)?;
        // NULL for rows inserted before migration 0018 — the verify gate fails
        // open on None.
        let lexical_sig: Option<i64> = row.try_get("lexical_sig").map_err(CacheError::Sqlx)?;
        let similarity: f32 = row.try_get("similarity").map_err(CacheError::Sqlx)?;

        let response_bytes = serde_json::to_vec(&response_json).map_err(CacheError::Serde)?;

        let entry = CacheEntry {
            id,
            org_id,
            embedding: embedding_vec.to_vec(),
            response: response_bytes,
            model,
            embedding_model: embedding_model_col,
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            baseline_cost_usd,
            hit_count: hit_count as u64,
            quality_score,
            judge_verdict,
            created_at,
            expires_at,
            lexical_sig,
        };

        Ok(Some((entry, similarity)))
    }

    /// Increment `hit_count` by 1 for the entry with the given `id`.
    ///
    /// Silently ignores the case where `id` no longer exists.
    async fn bump_hit_count(&self, id: Uuid) -> Result<(), CacheError> {
        sqlx::query("UPDATE cache_entries SET hit_count = hit_count + 1 WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(CacheError::Sqlx)?;

        Ok(())
    }

    /// Delete exactly one row by id. Targeted single-entry eviction — never a
    /// bulk delete. No-op when the id is absent.
    async fn evict(&self, id: Uuid) -> Result<(), CacheError> {
        sqlx::query("DELETE FROM cache_entries WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(CacheError::Sqlx)?;
        Ok(())
    }

    /// Record the judge score/verdict on the row, then evict that single row iff
    /// the band is `High`. Two statements, no bulk operations.
    async fn record_judge_verdict(
        &self,
        id: Uuid,
        score: Option<f32>,
        verdict: &str,
        band: JudgeBand,
    ) -> Result<JudgeRecordOutcome, CacheError> {
        let updated = sqlx::query(
            "UPDATE cache_entries SET quality_score = $2, judge_verdict = $3 WHERE id = $1",
        )
        .bind(id)
        .bind(score)
        .bind(verdict)
        .execute(&self.pool)
        .await
        .map_err(CacheError::Sqlx)?;

        if updated.rows_affected() == 0 {
            return Ok(JudgeRecordOutcome::NotFound);
        }
        if band.warrants_eviction() {
            self.evict(id).await?;
            return Ok(JudgeRecordOutcome::Evicted);
        }
        Ok(JudgeRecordOutcome::Recorded)
    }

    /// Read-only paraphrase-dedup analytics. Pulls a bounded sample of the org's
    /// non-expired `(id, embedding, hit_count)` rows (highest `hit_count` first)
    /// and clusters them in-process via [`cluster_near_duplicates`]. Issues no
    /// DELETE/UPDATE — strictly a SELECT.
    async fn analyze_dedup(&self, org_id: Uuid) -> Result<DedupReport, CacheError> {
        let rows = sqlx::query(
            r#"
            SELECT id, embedding, hit_count
              FROM cache_entries
             WHERE org_id = $1
               AND expires_at > now()
             ORDER BY hit_count DESC, id ASC
             LIMIT $2
            "#,
        )
        .bind(org_id)
        .bind(DEDUP_MAX_ENTRIES as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(CacheError::Sqlx)?;

        use sqlx::Row;
        let mut decoded: Vec<(Uuid, Vec<f32>, u64)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let id: Uuid = row.try_get("id").map_err(CacheError::Sqlx)?;
            let emb: pgvector::Vector = row.try_get("embedding").map_err(CacheError::Sqlx)?;
            let hit_count: i64 = row.try_get("hit_count").map_err(CacheError::Sqlx)?;
            decoded.push((id, emb.to_vec(), hit_count.max(0) as u64));
        }
        let view: Vec<(Uuid, &[f32], u64)> = decoded
            .iter()
            .map(|(id, emb, hc)| (*id, emb.as_slice(), *hc))
            .collect();
        Ok(cluster_near_duplicates(&view))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_unit_vectors() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let sim = cosine(&a, &a);
        assert!((sim - 1.0).abs() < 1e-6, "identical vectors → sim ≈ 1");
    }

    #[test]
    fn cosine_orthogonal_vectors() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        let sim = cosine(&a, &b);
        assert!(sim.abs() < 1e-6, "orthogonal vectors → sim ≈ 0");
    }

    #[test]
    fn cosine_zero_vector_returns_zero() {
        let a = vec![0.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0];
        assert_eq!(cosine(&a, &b), 0.0);
        assert_eq!(cosine(&b, &a), 0.0);
    }

    fn entry_at(id: Uuid, org_id: Uuid, embedding: Vec<f32>, now: DateTime<Utc>) -> CacheEntry {
        CacheEntry {
            id,
            org_id,
            embedding,
            response: b"{}".to_vec(),
            model: "gpt-4o".into(),
            embedding_model: "mock-v1".into(),
            input_tokens: 1,
            output_tokens: 1,
            baseline_cost_usd: None,
            hit_count: 0,
            quality_score: None,
            judge_verdict: None,
            created_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
            lexical_sig: None,
        }
    }

    /// Recall regression under multi-tenant load: with N orgs each holding a
    /// planted near-duplicate of the query (plus noise), every org's lookup
    /// must recall ITS OWN planted entry — never another tenant's, even though
    /// every org stores an identical match vector. This is the contract the
    /// Postgres HNSW path must also uphold (which is why `PostgresL2Cache`
    /// raises `hnsw.ef_search` so the org filter doesn't starve recall).
    #[tokio::test]
    async fn multi_tenant_recall_each_org_finds_its_own_match() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let query = vec![1.0_f32, 0.0, 0.0, 0.0];
        let noise = vec![0.0_f32, 1.0, 0.0, 0.0]; // orthogonal → sim 0

        const N_ORGS: usize = 50;
        let mut orgs = Vec::new();
        let mut match_ids = Vec::new();
        for _ in 0..N_ORGS {
            let org = Uuid::new_v4();
            let match_id = Uuid::new_v4();
            // Planted near-duplicate of the query for this org.
            cache
                .insert(entry_at(match_id, org, query.clone(), now))
                .await
                .unwrap();
            // Noise entries for the same org that must never out-rank the match.
            for _ in 0..3 {
                cache
                    .insert(entry_at(Uuid::new_v4(), org, noise.clone(), now))
                    .await
                    .unwrap();
            }
            orgs.push(org);
            match_ids.push(match_id);
        }

        for (k, org) in orgs.iter().enumerate() {
            let (hit, sim) = cache
                .lookup(*org, &query, 0.9, "gpt-4o", "mock-v1")
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("org {k} should recall its planted match"));
            assert_eq!(hit.id, match_ids[k], "org {k} recalled the wrong entry");
            assert_eq!(hit.org_id, *org, "lookup must never cross tenants");
            assert!(sim > 0.99, "org {k} similarity too low: {sim}");
        }
    }

    /// Tenant isolation: an org with NO matching entry must miss, even when a
    /// different org holds a perfect match for the same vector.
    #[tokio::test]
    async fn lookup_does_not_leak_across_orgs() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let q = vec![1.0_f32, 0.0];
        let org_with_match = Uuid::new_v4();
        let org_without = Uuid::new_v4();
        cache
            .insert(entry_at(Uuid::new_v4(), org_with_match, q.clone(), now))
            .await
            .unwrap();

        assert!(cache
            .lookup(org_with_match, &q, 0.9, "gpt-4o", "mock-v1")
            .await
            .unwrap()
            .is_some());
        assert!(
            cache
                .lookup(org_without, &q, 0.9, "gpt-4o", "mock-v1")
                .await
                .unwrap()
                .is_none(),
            "an org must not see another org's cached entry"
        );
    }

    /// §4.15: an embedding with a non-finite (NaN/Inf) component is rejected at
    /// insert so it can never poison similarity ranking; finite vectors insert.
    #[tokio::test]
    async fn insert_rejects_non_finite_embedding() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let org = Uuid::new_v4();

        let nan = cache
            .insert(entry_at(Uuid::new_v4(), org, vec![1.0, f32::NAN, 0.0], now))
            .await;
        assert!(matches!(nan, Err(CacheError::InvalidEmbedding)));

        let inf = cache
            .insert(entry_at(Uuid::new_v4(), org, vec![f32::INFINITY, 0.0], now))
            .await;
        assert!(matches!(inf, Err(CacheError::InvalidEmbedding)));

        // A finite vector still inserts fine.
        cache
            .insert(entry_at(Uuid::new_v4(), org, vec![1.0, 0.0], now))
            .await
            .expect("finite embedding inserts");
    }

    // ── Per-class thresholds (SAFETY: never below today's 0.92) ─────────────

    #[test]
    fn class_threshold_defaults_to_global_for_every_class() {
        // Every known class — and the unknown / unclassified `None` case —
        // resolves to exactly the current global default. No loosening.
        assert_eq!(class_threshold_for(None), DEFAULT_THRESHOLD);
        assert_eq!(
            class_threshold_for(Some(TaskClass::ChatCompletions)),
            DEFAULT_THRESHOLD
        );
        assert!(
            (DEFAULT_THRESHOLD - 0.92).abs() < f32::EPSILON,
            "default is 0.92"
        );
    }

    #[test]
    fn class_thresholds_floor_prevents_loosening() {
        // An accidental low per-class value is floored at the default — the bar
        // can be raised, never lowered.
        let cfg = ClassThresholds::new().with_class(TaskClass::ChatCompletions, 0.50);
        assert_eq!(
            cfg.threshold_for(Some(TaskClass::ChatCompletions)),
            DEFAULT_THRESHOLD,
            "0.50 must be floored up to 0.92"
        );
        // A higher value is honoured.
        let strict = ClassThresholds::new().with_class(TaskClass::ChatCompletions, 0.97);
        assert!((strict.threshold_for(Some(TaskClass::ChatCompletions)) - 0.97).abs() < 1e-6);
    }

    #[test]
    fn with_global_floor_never_drops_below_default() {
        // A LOWER global threshold is clamped up to the 0.92 default — the floor
        // can be raised, never lowered.
        let loose = ClassThresholds::with_global_floor(0.50);
        assert_eq!(loose.threshold_for(None), DEFAULT_THRESHOLD);
        assert_eq!(
            loose.threshold_for(Some(TaskClass::ChatCompletions)),
            DEFAULT_THRESHOLD
        );
        // A HIGHER global threshold becomes the floor for every class.
        let strict = ClassThresholds::with_global_floor(0.96);
        assert!((strict.threshold_for(None) - 0.96).abs() < 1e-6);
    }

    /// A classed lookup must never return a hit below the class threshold — and
    /// the global floor wins even if the config somehow held a lower value.
    #[tokio::test]
    async fn classed_lookup_never_returns_hit_below_threshold() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let org = Uuid::new_v4();
        // Two vectors ~0.93 apart in cosine (below 0.95, above 0.92).
        let stored = vec![1.0_f32, 0.0];
        let query = vec![0.93_f32, 0.37]; // cosine ≈ 0.93
        cache
            .insert(entry_at(Uuid::new_v4(), org, stored, now))
            .await
            .unwrap();

        let sim = cosine(&[1.0, 0.0], &[0.93, 0.37]);
        assert!(
            (0.92..0.95).contains(&sim),
            "fixture must sit between 0.92 and 0.95; got {sim}"
        );

        // Default class threshold (0.92) → this 0.93 match is a hit.
        let cfg = ClassThresholds::new();
        let hit = cache
            .lookup_classed(
                org,
                &query,
                &cfg,
                Some(TaskClass::ChatCompletions),
                "gpt-4o",
                "mock-v1",
            )
            .await
            .unwrap();
        assert!(hit.is_some(), "0.93 ≥ 0.92 default → hit");
        assert!(hit.unwrap().1 >= DEFAULT_THRESHOLD);

        // Raise the class threshold to 0.95 → the same 0.93 match must miss.
        let strict = ClassThresholds::new().with_class(TaskClass::ChatCompletions, 0.95);
        let miss = cache
            .lookup_classed(
                org,
                &query,
                &strict,
                Some(TaskClass::ChatCompletions),
                "gpt-4o",
                "mock-v1",
            )
            .await
            .unwrap();
        assert!(miss.is_none(), "0.93 < 0.95 class threshold → miss");
    }

    // ── Judge-driven eviction (targeted + conservative) ─────────────────────

    #[tokio::test]
    async fn eviction_fires_only_on_high_band_and_removes_exactly_that_entry() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let org = Uuid::new_v4();
        let victim = Uuid::new_v4();
        let bystander = Uuid::new_v4();
        cache
            .insert(entry_at(victim, org, vec![1.0, 0.0], now))
            .await
            .unwrap();
        cache
            .insert(entry_at(bystander, org, vec![0.0, 1.0], now))
            .await
            .unwrap();

        // High band → evicts exactly the victim.
        let outcome = cache
            .record_judge_verdict(victim, Some(0.0), "degraded", JudgeBand::High)
            .await
            .unwrap();
        assert_eq!(outcome, JudgeRecordOutcome::Evicted);

        // Victim gone; bystander untouched.
        assert!(cache
            .lookup(org, &[1.0, 0.0], 0.99, "gpt-4o", "mock-v1")
            .await
            .unwrap()
            .is_none());
        let (still, _) = cache
            .lookup(org, &[0.0, 1.0], 0.99, "gpt-4o", "mock-v1")
            .await
            .unwrap()
            .expect("bystander must survive");
        assert_eq!(still.id, bystander);
    }

    #[tokio::test]
    async fn eviction_does_not_fire_on_low_medium_or_records_only() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let org = Uuid::new_v4();
        for band in [JudgeBand::Low, JudgeBand::Medium] {
            let id = Uuid::new_v4();
            cache
                .insert(entry_at(id, org, vec![1.0, 0.0], now))
                .await
                .unwrap();
            let outcome = cache
                .record_judge_verdict(id, Some(1.0), "acceptable", band)
                .await
                .unwrap();
            assert_eq!(
                outcome,
                JudgeRecordOutcome::Recorded,
                "{band:?} records only"
            );
            // Entry survives and carries the recorded verdict.
            let (entry, _) = cache
                .lookup(org, &[1.0, 0.0], 0.99, "gpt-4o", "mock-v1")
                .await
                .unwrap()
                .expect("entry must survive a non-High verdict");
            assert_eq!(entry.quality_score, Some(1.0));
            assert_eq!(entry.judge_verdict.as_deref(), Some("acceptable"));
            cache.evict(id).await.unwrap(); // clean up for the next loop
        }
    }

    #[tokio::test]
    async fn record_verdict_on_missing_entry_is_not_found() {
        let cache = InMemoryL2Cache::new();
        let outcome = cache
            .record_judge_verdict(Uuid::new_v4(), Some(0.0), "degraded", JudgeBand::High)
            .await
            .unwrap();
        assert_eq!(outcome, JudgeRecordOutcome::NotFound);
    }

    #[test]
    fn judge_band_eviction_policy() {
        assert!(JudgeBand::High.warrants_eviction());
        assert!(!JudgeBand::Medium.warrants_eviction());
        assert!(!JudgeBand::Low.warrants_eviction());
    }

    // ── Paraphrase-dedup analytics (read-only) ──────────────────────────────

    #[tokio::test]
    async fn dedup_clusters_near_duplicates_and_is_read_only() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let org = Uuid::new_v4();
        // Cluster A: three near-identical vectors (cosine ≈ 1.0 ≥ 0.95).
        let a1 = Uuid::new_v4();
        let a2 = Uuid::new_v4();
        let a3 = Uuid::new_v4();
        for (id, v, hits) in [
            (a1, vec![1.0_f32, 0.0, 0.0], 10_u64),
            (a2, vec![0.999_f32, 0.01, 0.0], 3),
            (a3, vec![0.998_f32, 0.02, 0.0], 1),
        ] {
            let mut e = entry_at(id, org, v, now);
            e.hit_count = hits;
            cache.insert(e).await.unwrap();
        }
        // A lone, orthogonal singleton — not a cluster.
        cache
            .insert(entry_at(Uuid::new_v4(), org, vec![0.0, 1.0, 0.0], now))
            .await
            .unwrap();

        let before = cache
            .lookup(org, &[1.0, 0.0, 0.0], 0.0, "gpt-4o", "mock-v1")
            .await
            .unwrap();
        let report = cache.analyze_dedup(org).await.unwrap();

        assert_eq!(report.total_entries, 4);
        assert_eq!(report.cluster_count, 1, "one near-dup cluster");
        assert_eq!(report.entries_deduped, 2, "3-member cluster → 2 deduped");
        let cluster = &report.top_clusters[0];
        assert_eq!(cluster.member_ids.len(), 3);
        assert_eq!(
            cluster.representative_id, a1,
            "highest hit_count is the rep"
        );
        assert_eq!(cluster.total_hit_count, 14);

        // READ-ONLY: nothing was deleted/modified — the same lookup still works.
        let after = cache
            .lookup(org, &[1.0, 0.0, 0.0], 0.0, "gpt-4o", "mock-v1")
            .await
            .unwrap();
        assert_eq!(before.is_some(), after.is_some());
        assert_eq!(
            cache.analyze_dedup(org).await.unwrap().total_entries,
            4,
            "re-running analysis still sees all 4 entries (no mutation)"
        );
    }

    #[tokio::test]
    async fn dedup_does_not_cross_orgs() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let org_a = Uuid::new_v4();
        let org_b = Uuid::new_v4();
        // Two near-dups, but in DIFFERENT orgs → not a cluster.
        cache
            .insert(entry_at(Uuid::new_v4(), org_a, vec![1.0, 0.0], now))
            .await
            .unwrap();
        cache
            .insert(entry_at(Uuid::new_v4(), org_b, vec![1.0, 0.0], now))
            .await
            .unwrap();
        let report = cache.analyze_dedup(org_a).await.unwrap();
        assert_eq!(report.total_entries, 1);
        assert_eq!(report.cluster_count, 0);
    }

    // ── Adaptive thresholds (FP gate ratchet — SAFETY: never below 0.92) ─────

    fn gate(min_samples: u32, step: f32) -> AdaptiveClassThresholds {
        AdaptiveClassThresholds::new(ClassThresholds::new(), FpGateTuning::new(min_samples, step))
    }

    /// For every class (incl. unclassified `None`), the effective threshold is
    /// `>= the ClassThresholds floor >= 0.92` — before AND after any record
    /// sequence, including adversarial all-clean batches.
    #[test]
    fn effective_threshold_never_below_class_floor() {
        let g = gate(5, 0.005);
        for class in [None, Some(TaskClass::ChatCompletions)] {
            assert!(g.effective_threshold(class) >= DEFAULT_THRESHOLD);
            assert_eq!(g.effective_threshold(class), class_threshold_for(class));
        }
        // Feed batches of every shape; the floor must hold throughout.
        for class in [None, Some(TaskClass::ChatCompletions)] {
            for verdict in [Some(false), Some(true), None] {
                for _ in 0..25 {
                    g.record_judged_band_hit(class, verdict, 1.0);
                    assert!(
                        g.effective_threshold(class) >= DEFAULT_THRESHOLD,
                        "effective threshold dropped below the 0.92 floor"
                    );
                }
            }
        }
    }

    /// A breaching batch (FP% > tolerance over min_samples) raises the
    /// effective threshold by exactly `step`; repeated breaches ratchet up but
    /// never exceed ADAPTIVE_THRESHOLD_CEILING.
    #[test]
    fn fp_breach_raises_threshold_by_step_and_caps_at_ceiling() {
        let class = Some(TaskClass::ChatCompletions);
        let g = gate(20, 0.005);
        // 20 degraded samples → 100% FP > 1% tolerance → one step.
        for i in 0..20 {
            let raised = g.record_judged_band_hit(class, Some(true), 1.0);
            assert_eq!(raised, i == 19, "raise fires exactly at the batch close");
        }
        let after_one = g.effective_threshold(class);
        assert!(
            (after_one - (DEFAULT_THRESHOLD + 0.005)).abs() < 1e-6,
            "one breach raises by step: got {after_one}"
        );
        assert_eq!(g.raised_for(class), Some(after_one));
        // Hammer it with breaching batches; the ceiling must hold.
        for _ in 0..100 {
            for _ in 0..20 {
                g.record_judged_band_hit(class, Some(true), 1.0);
            }
        }
        let capped = g.effective_threshold(class);
        assert!(
            capped <= ADAPTIVE_THRESHOLD_CEILING + 1e-6,
            "ratchet must cap at the ceiling; got {capped}"
        );
        assert!((capped - ADAPTIVE_THRESHOLD_CEILING).abs() < 1e-6);
        // Once at the ceiling, a further breach reports no raise.
        for _ in 0..19 {
            g.record_judged_band_hit(class, Some(true), 1.0);
        }
        assert!(
            !g.record_judged_band_hit(class, Some(true), 1.0),
            "at the ceiling, no further raise is reported"
        );
    }

    /// Fewer than min_samples classified verdicts never adapt — even when
    /// every one of them is degraded.
    #[test]
    fn no_adaptation_below_min_samples() {
        let class = Some(TaskClass::ChatCompletions);
        let g = gate(20, 0.005);
        for _ in 0..19 {
            assert!(!g.record_judged_band_hit(class, Some(true), 1.0));
        }
        assert_eq!(g.raised_for(class), None, "19 < 20 must not adapt");
        assert_eq!(g.effective_threshold(class), DEFAULT_THRESHOLD);
    }

    /// The ratchet never lowers: after a raise, clean batches leave the
    /// effective threshold unchanged.
    #[test]
    fn ratchet_never_lowers() {
        let class = Some(TaskClass::ChatCompletions);
        let g = gate(5, 0.005);
        for _ in 0..5 {
            g.record_judged_band_hit(class, Some(true), 1.0);
        }
        let raised = g.effective_threshold(class);
        assert!(raised > DEFAULT_THRESHOLD, "precondition: a raise happened");
        for _ in 0..50 {
            g.record_judged_band_hit(class, Some(false), 1.0);
        }
        assert_eq!(
            g.effective_threshold(class),
            raised,
            "clean batches must never lower a raised threshold"
        );
    }

    /// Unclear verdicts (`None`) are excluded from the denominator: a stream
    /// of Unclear-only feeds never completes a batch, never adapts.
    #[test]
    fn unclear_verdicts_excluded_from_denominator() {
        let class = Some(TaskClass::ChatCompletions);
        let g = gate(5, 0.005);
        for _ in 0..100 {
            assert!(!g.record_judged_band_hit(class, None, 1.0));
        }
        assert_eq!(g.raised_for(class), None);
        // And Unclear mixed into a real batch doesn't count toward min_samples:
        // 4 classified + many Unclear stays below the 5-sample batch size.
        for _ in 0..4 {
            g.record_judged_band_hit(class, Some(true), 1.0);
        }
        for _ in 0..50 {
            g.record_judged_band_hit(class, None, 1.0);
        }
        assert_eq!(
            g.raised_for(class),
            None,
            "Unclear must not fill the batch denominator"
        );
    }

    /// Mixed tolerances in one batch adapt against the MINIMUM seen
    /// (strictest wins): one degraded in five (20% FP) breaches a 0.5%
    /// tolerance even when later samples arrive with a loose 2.0%.
    #[test]
    fn strictest_tolerance_in_batch_wins() {
        let class = Some(TaskClass::ChatCompletions);
        let g = gate(5, 0.005);
        // First sample carries the strict tolerance; the rest are loose.
        g.record_judged_band_hit(class, Some(true), 0.5);
        for _ in 0..3 {
            g.record_judged_band_hit(class, Some(false), 2.0);
        }
        let raised = g.record_judged_band_hit(class, Some(false), 2.0);
        assert!(
            raised,
            "20% FP must breach the strictest (0.5%) tolerance in the batch"
        );
        assert!(g.effective_threshold(class) > DEFAULT_THRESHOLD);
    }
}
