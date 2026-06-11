//! Sampled async quality judge on rerouted (downgraded) traffic.
//!
//! When the routing engine rewrites a request to a model *cheaper* than the one
//! the caller asked for, we want to confirm the optimization preserved quality.
//! This module samples ~2% of those rerouted-down requests and, **after the user
//! response has already been returned**, dispatches a cheap-model quality judge
//! that compares the served (cheaper-model) output against a one-shot original-
//! model reference. The verdict is recorded as a [`tt_plan_core::SampleScore`] +
//! aggregated [`tt_plan_core::RiskBand`] — the trust signal that gates aggressive
//! optimization.
//!
//! # Hard invariants
//!
//! 1. **Zero added user latency.** The judge runs in a detached
//!    [`tokio::spawn`] kicked off *after* the HTTP response is constructed. The
//!    response path never awaits the judge. This is enforced structurally: the
//!    only public entry point, [`spawn_quality_judge`], takes owned data and
//!    returns immediately.
//! 2. **Reroute-down only.** A request is eligible only when the routing engine
//!    rewrote `req.model` AND the served model is cheaper than the originally
//!    requested one (see [`is_downgrade`]). A non-rerouted request — or a reroute
//!    that did not lower cost — is never judged.
//! 3. **One task class (MVP).** Only [`JudgeTaskClass::ChatCompletions`] is
//!    sampled. The class filter is explicit + extensible so a follow-up can opt
//!    additional classes in.
//! 4. **Deterministic-but-uniform sampling.** [`should_sample`] hashes the trace
//!    id to a uniform `[0, 1)` fraction and compares it to the rate. Same trace +
//!    rate → same decision; across traces the keep-set is uniform. Rate `1.0`
//!    judges every eligible request, `0.0` judges none.
//! 5. **Record only.** The judge records a score + risk band. It never pauses a
//!    route — auto-pause is a deliberate follow-up.

use std::sync::Arc;

use async_trait::async_trait;
use tt_cache::{JudgeBand, JudgeRecordOutcome, L2Cache};
use tt_plan_core::{JudgeProvider, JudgeVerdict, QualityError, RiskBand, SampleScore};
use tt_shared::{
    messages::Message, ChatCompletionRequest, ChatCompletionResponse, MessageContent, ModelPricing,
    Provider, RequestContext, Usage,
};
use uuid::Uuid;

/// Default fraction of eligible (rerouted-down) requests to judge. Spec target
/// is "~2% of rerouted traffic". Overridable via `TT_JUDGE_SAMPLE_RATE`.
pub const DEFAULT_JUDGE_SAMPLE_RATE: f64 = 0.02;

/// Default judge model — a cheap rubric scorer. Overridable via `TT_JUDGE_MODEL`.
/// `gpt-4o-mini` is the cheapest broadly-available OpenAI chat model and a
/// reasonable default judge; operators pick their own cheap model in config.
pub const DEFAULT_JUDGE_MODEL: &str = "gpt-4o-mini";

/// Default deadline for the baseline reference dispatch inside the detached
/// judge task (`TT_JUDGE_BASELINE_TIMEOUT_SECS`). Deliberately NOT the 2s
/// canary `shadow_timeout`: nobody is waiting on the detached judge task, and
/// 2s would starve real baseline answers from slower flagship models.
pub const DEFAULT_JUDGE_BASELINE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The task classes the sampled judge can score. MVP enables only
/// [`JudgeTaskClass::ChatCompletions`]; the enum exists so additional classes
/// (embeddings re-rank, messages-ingress, …) can be opted in without reshaping
/// the call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JudgeTaskClass {
    /// `POST /v1/chat/completions` — the most general class, enabled for the MVP.
    ChatCompletions,
}

impl JudgeTaskClass {
    /// Whether this task class is sampled in the current build. Only
    /// `ChatCompletions` is in scope for the MVP; everything else is opt-in
    /// later. Keeping the filter here (rather than inline at the call site) is
    /// what makes the scope explicit + extensible.
    #[must_use]
    pub fn is_sampled(self) -> bool {
        matches!(self, JudgeTaskClass::ChatCompletions)
    }
}

/// Configuration for the sampled quality judge. Built from the environment via
/// [`JudgeConfig::from_env`]; defaults keep the judge **off** until a judge
/// model + an enabled flag are configured, so wiring it changes no behavior for
/// operators who don't opt in.
#[derive(Debug, Clone)]
pub struct JudgeConfig {
    /// Master enable flag (`TT_JUDGE_ENABLED`). Off by default.
    pub enabled: bool,
    /// Fraction of eligible requests to judge, in `[0, 1]`
    /// (`TT_JUDGE_SAMPLE_RATE`, default [`DEFAULT_JUDGE_SAMPLE_RATE`]).
    pub sample_rate: f64,
    /// Cheap judge model id (`TT_JUDGE_MODEL`, default [`DEFAULT_JUDGE_MODEL`]).
    pub judge_model: String,
    /// Judge BOTH A/B orders and require agreement (`TT_JUDGE_BOTH_ORDERS`,
    /// default **off**). Doubles the judge tax per sample, so it stays behind
    /// its own opt-in; disagreement between the two orders records `Unclear`
    /// (a position-sensitive judgment is genuinely unclassifiable).
    pub both_orders: bool,
    /// Deadline for the baseline reference dispatch inside the detached judge
    /// task (`TT_JUDGE_BASELINE_TIMEOUT_SECS`, default
    /// [`DEFAULT_JUDGE_BASELINE_TIMEOUT`] = 30s).
    pub baseline_timeout: std::time::Duration,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_rate: DEFAULT_JUDGE_SAMPLE_RATE,
            judge_model: DEFAULT_JUDGE_MODEL.to_string(),
            both_orders: false,
            baseline_timeout: DEFAULT_JUDGE_BASELINE_TIMEOUT,
        }
    }
}

/// Truthy parse shared by the `TT_JUDGE_*` boolean env flags.
fn env_truthy(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    v == "1" || v == "true" || v == "yes" || v == "on"
}

impl JudgeConfig {
    /// Read the judge config from `TT_JUDGE_*` env vars. Malformed values fall
    /// back to the defaults; the rate is clamped to `[0, 1]`.
    #[must_use]
    pub fn from_env() -> Self {
        let enabled = std::env::var("TT_JUDGE_ENABLED")
            .ok()
            .map(|v| env_truthy(&v))
            .unwrap_or(false);
        let sample_rate = std::env::var("TT_JUDGE_SAMPLE_RATE")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .map(|r| r.clamp(0.0, 1.0))
            .unwrap_or(DEFAULT_JUDGE_SAMPLE_RATE);
        let judge_model = std::env::var("TT_JUDGE_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_JUDGE_MODEL.to_string());
        let both_orders = std::env::var("TT_JUDGE_BOTH_ORDERS")
            .ok()
            .map(|v| env_truthy(&v))
            .unwrap_or(false);
        let baseline_timeout = std::env::var("TT_JUDGE_BASELINE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or(DEFAULT_JUDGE_BASELINE_TIMEOUT);
        Self {
            enabled,
            sample_rate,
            judge_model,
            both_orders,
            baseline_timeout,
        }
    }
}

/// The outcome recorded for one judged request: the per-sample score plus the
/// aggregate [`RiskBand`] derived from it. For a single sample the band reflects
/// just that verdict (`Acceptable`/`Unclear` → `Low`, `Degraded` → `High`); a
/// store-backed sink aggregates many of these per route over time.
#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    /// Org the request belonged to (for per-org/route aggregation).
    pub org_id: Uuid,
    /// The matched route's id, when known — the unit a follow-up would pause.
    pub route_id: Option<Uuid>,
    /// The originally-requested (more expensive) model.
    pub requested_model: String,
    /// The served (cheaper, rerouted-to) model whose quality we judged.
    pub served_model: String,
    /// The per-sample judge score (verdict + reason + request id).
    pub score: SampleScore,
    /// Aggregate risk band for this sample.
    pub risk_band: RiskBand,
    /// The judge model that produced the verdict.
    pub judge_model: String,
    /// Judge tax (USD) for this sample — the cost of the judge call(s), summed
    /// over both orders in both-orders mode. **Measurement tax**: recorded here
    /// (and on the `tokentrimmer.quality.judge_cost_usd` span attribute) only,
    /// never folded into `request_logs.cost_usd`, never counted as or against
    /// savings. `0.0` means the judge model had no catalog pricing (unmetered),
    /// never "free". Phase 2 nets this into routing attribution.
    pub judge_cost_usd: f64,
    /// Cost (USD) of the baseline reference dispatch, when the reference was
    /// produced by re-dispatching the baseline model inside the detached task
    /// ([`ReferenceSource::Dispatch`]). `None` when the reference pre-existed
    /// ([`ReferenceSource::Ready`]) — no extra dispatch was billed. Same
    /// measurement-tax rules as `judge_cost_usd`.
    pub baseline_cost_usd: Option<f64>,
    /// The blind slot (`a`/`b`) the OPTIMIZED answer occupied in the paired
    /// prompt — makes the position debiasing auditable.
    pub optimized_position: AbOrder,
    /// How many A/B orders were judged (1, or 2 in both-orders mode).
    pub orders_judged: u8,
    /// Whether the two orders' mapped verdicts agreed; `None` unless both
    /// orders were judged.
    pub orders_agreed: Option<bool>,
}

/// Sink that records a [`JudgeOutcome`]. Production wires a store-backed sink;
/// tests use a recording sink to assert the judge fired and produced a band.
///
/// **Record only** — implementations must not pause routes or mutate live
/// routing. Auto-pause is a deliberate follow-up.
#[async_trait]
pub trait JudgeSink: Send + Sync {
    /// Persist / aggregate one judged sample. Best-effort: errors are the sink's
    /// to log; the caller (a detached task) ignores the result.
    async fn record(&self, outcome: JudgeOutcome);
}

/// Running verdict tally for one `(requested_model → served_model)` swap.
/// Aggregation mirrors `tt_plan_core::score_quality`: `degraded_pct` is computed
/// over *classified* (non-`Unclear`) samples and fed to
/// [`RiskBand::from_degraded_pct`], so a single `Degraded` in a large acceptable
/// set is `Low`, not `High`.
#[derive(Debug, Clone, Copy, Default)]
struct VerdictTally {
    acceptable: u64,
    degraded: u64,
    unclear: u64,
}

impl VerdictTally {
    fn add(&mut self, verdict: JudgeVerdict) {
        match verdict {
            JudgeVerdict::Acceptable => self.acceptable += 1,
            JudgeVerdict::Degraded => self.degraded += 1,
            JudgeVerdict::Unclear => self.unclear += 1,
        }
    }

    /// Aggregate band over the classified samples, or `None` when nothing has
    /// been classified yet (only `Unclear` verdicts → we genuinely don't know).
    fn band(self) -> Option<RiskBand> {
        let classified = self.acceptable + self.degraded;
        if classified == 0 {
            return None;
        }
        let degraded_pct = self.degraded as f64 / classified as f64 * 100.0;
        Some(RiskBand::from_degraded_pct(degraded_pct))
    }
}

/// An in-process, store-backed [`JudgeSink`] that aggregates judged outcomes per
/// `(requested_model → served_model)` swap and exposes the aggregate
/// [`tt_preview::QualityRiskBand`] for suggestion enrichment.
///
/// This is the production join the spec asks for: the live judge records into
/// it (closing the persistence gap), and [`enrich_suggestions`] reads from it
/// through the `tt_preview` enrichment seam (closing the lookup gap) so a live
/// judge outcome flows end-to-end into a populated suggestion band — replacing
/// the hard-coded `Unknown`. It is in-process (per-instance) and bounded by the
/// distinct swap count; a follow-up can back it with Postgres without changing
/// the call sites.
///
/// **Record only** — it never pauses a route or mutates live routing.
#[derive(Debug, Default)]
pub struct InMemoryJudgeBandStore {
    /// `(requested_model, served_model) → running verdict tally`.
    tallies: std::sync::Mutex<std::collections::HashMap<(String, String), VerdictTally>>,
}

impl InMemoryJudgeBandStore {
    /// Fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Aggregate [`tt_preview::QualityRiskBand`] for one swap, or `None` when the
    /// judge has not classified that swap yet (no samples, or only `Unclear`).
    /// `None` keeps an unjudged swap honestly `Unknown` at the call site.
    #[must_use]
    pub fn band_for(
        &self,
        requested_model: &str,
        served_model: &str,
    ) -> Option<tt_preview::QualityRiskBand> {
        let key = (requested_model.to_string(), served_model.to_string());
        let tallies = self.tallies.lock().expect("judge band store poisoned");
        tallies
            .get(&key)
            .copied()
            .and_then(VerdictTally::band)
            .map(risk_band_to_preview)
    }

    /// Enrich `suggestions` (all swaps *from* `requested_model`) in place,
    /// lifting each from `Unknown` to its judged band where one exists. Joins
    /// the recorded judge outcomes to the suggestion pill through the
    /// `tt_preview` enrichment seam.
    pub fn enrich_suggestions(
        &self,
        requested_model: &str,
        suggestions: &mut [tt_preview::RouteSuggestion],
    ) {
        tt_preview::route_suggestions::enrich_with_judged_bands(suggestions, |s| {
            self.band_for(requested_model, &s.model)
        });
    }
}

#[async_trait]
impl JudgeSink for InMemoryJudgeBandStore {
    async fn record(&self, outcome: JudgeOutcome) {
        let key = (outcome.requested_model, outcome.served_model);
        let mut tallies = self.tallies.lock().expect("judge band store poisoned");
        tallies.entry(key).or_default().add(outcome.score.verdict);
    }
}

/// A [`JudgeSink`] that clones each outcome to every wired sink. Production
/// wires `[InMemoryJudgeBandStore, PostgresJudgeSink]` so one recorded verdict
/// both feeds the live `/v1/preview` enrichment AND lands durably in the
/// `quality_verdicts` table (Phase 2 attribution netting). Record-only, like
/// every sink: it never pauses routes.
pub struct FanoutJudgeSink {
    sinks: Vec<Arc<dyn JudgeSink>>,
}

impl FanoutJudgeSink {
    /// Fan out to `sinks`, in order. Each sink is awaited sequentially inside
    /// the detached judge task (no user latency either way).
    #[must_use]
    pub fn new(sinks: Vec<Arc<dyn JudgeSink>>) -> Self {
        Self { sinks }
    }
}

#[async_trait]
impl JudgeSink for FanoutJudgeSink {
    async fn record(&self, outcome: JudgeOutcome) {
        for sink in &self.sinks {
            sink.record(outcome.clone()).await;
        }
    }
}

/// Map a plan-core [`RiskBand`] onto the reserved `tt_preview::QualityRiskBand`
/// hook (currently hard-coded to `Unknown` in `plan_suggest`). This is the
/// adapter that lets a live judge outcome populate the suggestion pill.
#[must_use]
pub fn risk_band_to_preview(band: RiskBand) -> tt_preview::QualityRiskBand {
    match band {
        RiskBand::Low => tt_preview::QualityRiskBand::Low,
        RiskBand::Medium => tt_preview::QualityRiskBand::Medium,
        RiskBand::High => tt_preview::QualityRiskBand::High,
    }
}

/// Aggregate risk band for a single verdict. `Degraded` is the only signal that
/// the optimization hurt quality, so it maps to `High`; `Acceptable` and the
/// non-committal `Unclear` map to `Low` (mirrors `score_quality`, which excludes
/// `Unclear` from the degraded denominator).
#[must_use]
pub fn risk_band_for_verdict(verdict: JudgeVerdict) -> RiskBand {
    match verdict {
        JudgeVerdict::Degraded => RiskBand::High,
        JudgeVerdict::Acceptable | JudgeVerdict::Unclear => RiskBand::Low,
    }
}

/// Map the plan-core [`RiskBand`] onto the cache crate's eviction-decision band
/// [`tt_cache::JudgeBand`]. The cache uses this to decide whether a
/// served-from-L2 entry should be evicted: **only** `High` triggers an eviction.
#[must_use]
pub fn risk_band_to_cache_band(band: RiskBand) -> JudgeBand {
    match band {
        RiskBand::Low => JudgeBand::Low,
        RiskBand::Medium => JudgeBand::Medium,
        RiskBand::High => JudgeBand::High,
    }
}

/// Where a judged-from-L2 verdict is recorded (and possibly evicted). Carried on
/// a [`QualityJudgeJob`] only when the response being judged was **served from
/// the L2 cache** — i.e. there is a specific entry whose quality this verdict
/// reflects. Absent for the ordinary downgraded-dispatch judge path (there is no
/// cache entry to attribute the verdict to).
pub struct L2EvictionTarget {
    /// The L2 cache to record the verdict / evict from.
    pub cache: Arc<dyn L2Cache>,
    /// The id of the specific entry that served the judged response. **Only**
    /// this entry is ever touched — never a bulk operation.
    pub entry_id: Uuid,
}

/// Lowercase wire string for a verdict (`acceptable` / `degraded` / `unclear`),
/// matching the `tt_plan_core::JudgeVerdict` serde representation. Used on the
/// `tokentrimmer.quality.verdict` telemetry attribute.
#[must_use]
pub fn verdict_str(verdict: JudgeVerdict) -> &'static str {
    match verdict {
        JudgeVerdict::Acceptable => "acceptable",
        JudgeVerdict::Degraded => "degraded",
        JudgeVerdict::Unclear => "unclear",
    }
}

/// Lowercase wire string for a risk band (`low` / `medium` / `high`), matching
/// the `tt_plan_core::RiskBand` serde representation. Emitted on the
/// `tokentrimmer.quality.band` telemetry attribute (the live surface; the
/// `x-tokentrimmer-quality-band` header name is reserved but not emitted because
/// the verdict is produced asynchronously after the response is built).
#[must_use]
pub fn band_str(band: RiskBand) -> &'static str {
    match band {
        RiskBand::Low => "low",
        RiskBand::Medium => "medium",
        RiskBand::High => "high",
    }
}

/// Deterministic-but-uniform sampling decision for a request, keyed on its trace
/// id. Returns `true` for a `rate` fraction of trace ids, uniformly.
///
/// Implementation: take the low 53 bits of the trace id's first 8 bytes (a v7
/// UUID is effectively uniform there once the timestamp prefix is mixed by the
/// hash below) and divide into `[0, 1)`. We hash the full 16-byte uuid with
/// FxHash-free `DefaultHasher` to avoid any structure in the timestamp/counter
/// prefix biasing the low bits. `rate <= 0` never samples; `rate >= 1` always.
#[must_use]
pub fn should_sample(trace_id: Uuid, rate: f64) -> bool {
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    trace_id.hash(&mut hasher);
    let h = hasher.finish();
    // Map the 64-bit hash uniformly into [0, 1) using the top 53 bits (f64
    // mantissa width) so the fraction is exact.
    let frac = (h >> 11) as f64 / (1u64 << 53) as f64;
    frac < rate
}

/// Which blind slot the OPTIMIZED (served) answer occupies in the paired
/// judge prompt. The baseline answer takes the other slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbOrder {
    /// Optimized answer is ANSWER A, baseline is ANSWER B.
    OptimizedA,
    /// Optimized answer is ANSWER B, baseline is ANSWER A.
    OptimizedB,
}

/// Deterministic per-sample A/B order, keyed on the trace id — the position
/// debiasing for the paired judge. Same trace → same order (reproducible);
/// across traces the orders are uniform, so position bias averages out.
///
/// Hashes the trace id THEN a dedicated salt (`tt-judge-ab-v1`) with the std
/// `DefaultHasher` — a **different** salt than [`should_sample`]'s plain hash,
/// so the order is independent of the keep/drop decision (the judged subset
/// sees both orders). No `rand` involvement, by design: plan-replay bootstrap
/// CIs are attested against the pinned rand version, and this module must
/// never create pressure to touch it.
#[must_use]
pub fn ab_order_for(trace_id: Uuid) -> AbOrder {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    trace_id.hash(&mut hasher);
    b"tt-judge-ab-v1".hash(&mut hasher);
    if hasher.finish() & 1 == 0 {
        AbOrder::OptimizedA
    } else {
        AbOrder::OptimizedB
    }
}

/// Wire string for the blind slot the OPTIMIZED answer occupied (`"a"`/`"b"`),
/// persisted on `quality_verdicts.optimized_position`.
#[must_use]
pub fn ab_position_str(order: AbOrder) -> &'static str {
    match order {
        AbOrder::OptimizedA => "a",
        AbOrder::OptimizedB => "b",
    }
}

/// Role-neutral verdict a paired judge emits about the two blind slots.
/// Deliberately carries NO optimized/baseline semantics — mapping back to a
/// [`JudgeVerdict`] happens in [`judge_paired`] via the slot the optimized
/// answer occupied, so the judge model never learns which answer is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairVerdict {
    /// Both answers convey the same material information.
    Equivalent,
    /// ANSWER A omits or contradicts material information present in ANSWER B.
    AMissing,
    /// ANSWER B omits or contradicts material information present in ANSWER A.
    BMissing,
    /// The judge could not classify the pair.
    Unclear,
}

/// One paired judge call's result: the role-neutral verdict, the judge's
/// one-line reason, and the call's own metered cost (the judge tax; `0.0`
/// when the judge model has no catalog pricing — unmetered, never "free").
#[derive(Debug, Clone)]
pub struct PairCall {
    /// Role-neutral verdict over the two blind slots.
    pub verdict: PairVerdict,
    /// Best-effort one-line reason from the judge.
    pub reason: String,
    /// Metered cost (USD) of this judge call.
    pub cost_usd: f64,
}

/// A judge that compares two BLIND answers (`answer_a` / `answer_b`) for the
/// same input and reports which (if either) omits material information.
///
/// This is the live gateway's paired-A/B seam (research Phase 0.4). It is a
/// NEW trait in tt-core — `tt_plan_core::JudgeProvider` (and its replay
/// goldens) stay untouched.
#[async_trait]
pub trait PairedJudgeProvider: Send + Sync {
    /// Judge one blind (A, B) pair. Implementations must not be told — and
    /// must not be able to infer from arguments — which slot is the optimized
    /// answer.
    async fn judge_pair(
        &self,
        input: &str,
        answer_a: &str,
        answer_b: &str,
    ) -> Result<PairCall, QualityError>;
}

/// The mapped result of judging one sample (one or two orders).
#[derive(Debug, Clone)]
pub struct PairedJudgeOutcome {
    /// Final mapped verdict for the OPTIMIZED answer.
    pub verdict: JudgeVerdict,
    /// Judge reason(s); both orders' reasons joined in both-orders mode.
    pub reason: String,
    /// Judge tax (USD), summed over all judge calls for this sample.
    pub judge_cost_usd: f64,
    /// Number of A/B orders judged (1 or 2).
    pub orders_judged: u8,
    /// Whether the two orders agreed (`None` in single-order mode).
    pub orders_agreed: Option<bool>,
}

/// Map a role-neutral [`PairVerdict`] onto the optimized answer's
/// [`JudgeVerdict`], given the blind slot the optimized answer occupied.
///
/// The mapping is **recall-of-baseline**: the optimized side failing to
/// preserve the baseline's information (`*Missing` on the optimized slot) is
/// `Degraded`; `Equivalent` — or the BASELINE side missing information (the
/// optimized answer added info) — passes the recall gate as `Acceptable`. A
/// precision/hallucination gate (penalizing fabricated additions) is
/// explicitly out of scope here and noted as future work.
fn map_pair_verdict(verdict: PairVerdict, optimized: AbOrder) -> JudgeVerdict {
    match verdict {
        PairVerdict::Equivalent => JudgeVerdict::Acceptable,
        PairVerdict::AMissing => match optimized {
            AbOrder::OptimizedA => JudgeVerdict::Degraded,
            AbOrder::OptimizedB => JudgeVerdict::Acceptable,
        },
        PairVerdict::BMissing => match optimized {
            AbOrder::OptimizedB => JudgeVerdict::Degraded,
            AbOrder::OptimizedA => JudgeVerdict::Acceptable,
        },
        PairVerdict::Unclear => JudgeVerdict::Unclear,
    }
}

/// Parse a paired judge model's reply into a role-neutral verdict + trimmed
/// first-line reason. `A_MISSING` xor `B_MISSING` wins; a reply naming BOTH
/// sides is `Unclear` (unclassifiable, not a coin flip); otherwise
/// `EQUIVALENT` → `Equivalent`; anything else is `Unclear`.
fn parse_pair_verdict(reply: &str) -> (PairVerdict, String) {
    let upper = reply.to_ascii_uppercase();
    let a_missing = upper.contains("A_MISSING");
    let b_missing = upper.contains("B_MISSING");
    let verdict = match (a_missing, b_missing) {
        (true, true) => PairVerdict::Unclear,
        (true, false) => PairVerdict::AMissing,
        (false, true) => PairVerdict::BMissing,
        (false, false) if upper.contains("EQUIVALENT") => PairVerdict::Equivalent,
        (false, false) => PairVerdict::Unclear,
    };
    let reason = reply.trim().lines().next().unwrap_or("").trim().to_string();
    (verdict, reason)
}

/// Judge one sample as a blind pair, with position debiasing.
///
/// Single-order mode (`both_orders == false`): one [`PairedJudgeProvider`]
/// call with the slots laid out per `order`, mapped back via
/// [`map_pair_verdict`]; `orders_judged == 1`, `orders_agreed == None`.
///
/// Both-orders mode: a second call with the slots swapped (and the mapping
/// flipped accordingly). Agreement keeps the mapped verdict
/// (`orders_agreed == Some(true)`); disagreement records
/// [`JudgeVerdict::Unclear`] (`Some(false)`) — a position-sensitive judgment
/// is genuinely unclassifiable. The judge tax sums over all calls either way.
pub async fn judge_paired(
    judge: &dyn PairedJudgeProvider,
    input: &str,
    baseline_answer: &str,
    optimized_answer: &str,
    order: AbOrder,
    both_orders: bool,
) -> Result<PairedJudgeOutcome, QualityError> {
    let (a, b) = match order {
        AbOrder::OptimizedA => (optimized_answer, baseline_answer),
        AbOrder::OptimizedB => (baseline_answer, optimized_answer),
    };
    let first = judge.judge_pair(input, a, b).await?;
    let first_verdict = map_pair_verdict(first.verdict, order);
    if !both_orders {
        return Ok(PairedJudgeOutcome {
            verdict: first_verdict,
            reason: first.reason,
            judge_cost_usd: first.cost_usd,
            orders_judged: 1,
            orders_agreed: None,
        });
    }

    // Second call with the slots swapped; the optimized answer now occupies
    // the OTHER slot, so the mapping flips with it.
    let flipped = match order {
        AbOrder::OptimizedA => AbOrder::OptimizedB,
        AbOrder::OptimizedB => AbOrder::OptimizedA,
    };
    let second = judge.judge_pair(input, b, a).await?;
    let second_verdict = map_pair_verdict(second.verdict, flipped);
    let judge_cost_usd = first.cost_usd + second.cost_usd;
    if first_verdict == second_verdict {
        Ok(PairedJudgeOutcome {
            verdict: first_verdict,
            reason: format!("fwd: {} | rev: {}", first.reason, second.reason),
            judge_cost_usd,
            orders_judged: 2,
            orders_agreed: Some(true),
        })
    } else {
        Ok(PairedJudgeOutcome {
            verdict: JudgeVerdict::Unclear,
            reason: format!("position-sensitive: {first_verdict:?} vs {second_verdict:?}"),
            judge_cost_usd,
            orders_judged: 2,
            orders_agreed: Some(false),
        })
    }
}

/// Whether a reroute lowered the request's cost — i.e. the served model is
/// cheaper than the originally requested model for this request's realized usage.
///
/// Both pricings must be known; an unknown price on either side returns `false`
/// (we don't judge a reroute we can't prove was a downgrade). Cost is computed on
/// the **realized** `usage` so a tiny token count can't make a nominally-cheaper
/// model look more expensive.
#[must_use]
pub fn is_downgrade(
    requested_pricing: Option<&ModelPricing>,
    served_pricing: Option<&ModelPricing>,
    usage: &Usage,
) -> bool {
    let (Some(req_pr), Some(served_pr)) = (requested_pricing, served_pricing) else {
        return false;
    };
    let cost = |p: &ModelPricing| {
        // `Usage` token counts are u64; cast to f64 for the rate multiply (token
        // counts never approach 2^53, so this is lossless in practice).
        (usage.prompt_tokens as f64 * p.input_per_million
            + usage.completion_tokens as f64 * p.output_per_million)
            / 1_000_000.0
    };
    served_pr_cost_is_lower(cost(req_pr), cost(served_pr))
}

/// Strictly-lower comparison with a tiny epsilon so equal-priced models (e.g. a
/// same-family alias) aren't counted as a downgrade.
fn served_pr_cost_is_lower(requested_cost: f64, served_cost: f64) -> bool {
    served_cost + f64::EPSILON < requested_cost
}

/// Extract the assistant text from a chat completion response. Concatenates the
/// text of every choice's assistant message; tool-call-only responses yield an
/// empty string (and are skipped by the caller before reaching the judge).
fn response_text(resp: &ChatCompletionResponse) -> String {
    resp.choices
        .iter()
        .filter_map(|c| match &c.message {
            Message::Assistant { content, .. } => content.as_ref().map(content_text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                tt_shared::messages::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// A [`JudgeProvider`] backed by a real gateway [`Provider`] + a cheap judge
/// model. Sends the judge rubric as a chat completion and parses the verdict.
///
/// The judge prompt asks the cheap model to compare an original-model reference
/// answer against the served (cheaper-model) answer for the same input and emit
/// one of `ACCEPTABLE` / `DEGRADED` / `UNCLEAR` plus a one-line reason. Any
/// dispatch error surfaces as [`QualityError::Judge`]; an unparseable verdict is
/// recorded as [`JudgeVerdict::Unclear`] (never an error) so a flaky judge can't
/// poison the score.
pub struct GatewayLlmJudge {
    provider: Arc<dyn Provider>,
    judge_model: String,
    ctx: RequestContext,
}

impl GatewayLlmJudge {
    /// Build a judge that dispatches the rubric to `judge_model` on `provider`
    /// using `ctx` (org credentials etc.). The caller is responsible for picking
    /// a *cheap* model and a provider that serves it.
    pub fn new(provider: Arc<dyn Provider>, judge_model: String, ctx: RequestContext) -> Self {
        Self {
            provider,
            judge_model,
            ctx,
        }
    }

    /// Build the BLIND paired-comparison judge request (the live gateway
    /// path). The two answers are presented only as `ANSWER A` / `ANSWER B` —
    /// no role labels, no model ids — and the rubric is recall-oriented with
    /// an explicit anti-length-bias instruction, so neither label leakage nor
    /// verbosity can tilt the verdict. The caller controls which slot the
    /// optimized answer occupies (see [`ab_order_for`]).
    fn build_paired_request(
        &self,
        input: &str,
        answer_a: &str,
        answer_b: &str,
    ) -> ChatCompletionRequest {
        let system = "You are a strict information-preservation judge for an LLM gateway. \
You will see a user INPUT and two candidate answers, ANSWER A and ANSWER B, presented \
in random order. Judge ONLY whether each answer preserves the material information of \
the other: facts, claims, numbers, caveats, and directly actionable content. Verbosity, \
style, formatting, and answer length must NOT influence your verdict — a longer answer \
is NOT better, and extra wording that adds no information is NOT better. Reply with \
EXACTLY one token on the first line: EQUIVALENT (both convey the same material \
information), A_MISSING (ANSWER A omits or contradicts material information present in \
ANSWER B), B_MISSING (ANSWER B omits or contradicts material information present in \
ANSWER A), or UNCLEAR (cannot tell). Then one line naming the missing or contradicted \
information, if any.";
        let user = format!(
            "INPUT:\n{input}\n\nANSWER A:\n{answer_a}\n\nANSWER B:\n{answer_b}\n\nVerdict:"
        );
        ChatCompletionRequest {
            model: self.judge_model.clone(),
            messages: vec![
                Message::System {
                    content: MessageContent::Text(system.to_string()),
                },
                Message::User {
                    content: MessageContent::Text(user),
                    name: None,
                },
            ],
            // Deterministic, short scoring: temperature 0, capped output (one
            // verdict token + one reason line). Other fields default via the
            // serde-seed idiom (same as `build_request`).
            temperature: Some(0.0),
            max_tokens: Some(96),
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#)
                .expect("static minimal request JSON is valid")
        }
    }

    /// Legacy single-answer-labeled rubric builder for the [`JudgeProvider`]
    /// impl below. The live gateway no longer uses it — see
    /// [`Self::build_paired_request`].
    fn build_request(&self, input: &str, original: &str, proposed: &str) -> ChatCompletionRequest {
        let system = "You are a strict quality judge for an LLM cost-optimization gateway. \
A request was served by a CHEAPER model than the one originally requested. \
Given the user input, the ORIGINAL model's reference answer, and the CHEAPER model's \
answer, decide whether the cheaper answer preserved quality. Reply with EXACTLY one \
word on the first line — ACCEPTABLE (interchangeable quality), DEGRADED (materially \
worse), or UNCLEAR (cannot tell) — then a one-line reason.";
        let user = format!(
            "INPUT:\n{input}\n\nORIGINAL ANSWER:\n{original}\n\nCHEAPER ANSWER:\n{proposed}\n\nVerdict:"
        );
        ChatCompletionRequest {
            model: self.judge_model.clone(),
            messages: vec![
                Message::System {
                    content: MessageContent::Text(system.to_string()),
                },
                Message::User {
                    content: MessageContent::Text(user),
                    name: None,
                },
            ],
            // Deterministic, short scoring: temperature 0, capped output. All
            // other fields default (serde fills the skip_serializing_if optionals
            // + flatten/extras maps) — the same idiom the routing tests use.
            temperature: Some(0.0),
            max_tokens: Some(64),
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#)
                .expect("static minimal request JSON is valid")
        }
    }
}

/// Parse a judge model's reply into a verdict + trimmed reason. Case-insensitive
/// keyword match on the reply; anything unrecognized is `Unclear`.
fn parse_verdict(reply: &str) -> (JudgeVerdict, String) {
    let upper = reply.to_ascii_uppercase();
    let verdict = if upper.contains("DEGRADED") {
        JudgeVerdict::Degraded
    } else if upper.contains("ACCEPTABLE") {
        JudgeVerdict::Acceptable
    } else {
        JudgeVerdict::Unclear
    };
    let reason = reply.trim().lines().next().unwrap_or("").trim().to_string();
    (verdict, reason)
}

/// **Legacy single-order path** — kept for API stability (cloud pins public
/// crates by git SHA; do not delete a pub item's impl without checking cloud
/// usage). The live gateway path uses only [`PairedJudgeProvider`], whose
/// blind labels + position randomization supersede this labeled rubric.
#[async_trait]
impl JudgeProvider for GatewayLlmJudge {
    async fn judge(
        &self,
        input_body: &str,
        original_response: &str,
        proposed_response: &str,
    ) -> Result<(JudgeVerdict, String), QualityError> {
        let req = self.build_request(input_body, original_response, proposed_response);
        let resp = self
            .provider
            .chat_completion(req, &self.ctx)
            .await
            .map_err(|e| QualityError::Judge(e.to_string()))?;
        let reply = response_text(&resp);
        Ok(parse_verdict(&reply))
    }
}

#[async_trait]
impl PairedJudgeProvider for GatewayLlmJudge {
    async fn judge_pair(
        &self,
        input: &str,
        answer_a: &str,
        answer_b: &str,
    ) -> Result<PairCall, QualityError> {
        let req = self.build_paired_request(input, answer_a, answer_b);
        let resp = self
            .provider
            .chat_completion(req, &self.ctx)
            .await
            .map_err(|e| QualityError::Judge(e.to_string()))?;
        // Meter the judge tax on the judge model's own pricing — the same
        // computation as `measurement::measured_single_dispatch` (`0.0` when
        // the model has no catalog pricing: unmetered, never "free").
        let pricing = self.provider.pricing(&resp.model);
        let cost_usd = crate::routes::chat::compute_cost(
            &resp.usage,
            pricing.as_ref(),
            pricing.as_ref(),
            self.provider.fee_multiplier(),
        )
        .cost_usd;
        let reply = response_text(&resp);
        let (verdict, reason) = parse_pair_verdict(&reply);
        Ok(PairCall {
            verdict,
            reason,
            cost_usd,
        })
    }
}

/// Everything the detached judge task needs, captured by value so the spawn
/// borrows nothing from the request handler. Built on the response path (cheap:
/// a few clones) and moved into the [`tokio::spawn`] in [`spawn_quality_judge`].
pub struct QualityJudgeJob {
    /// Paired judge backend (a [`GatewayLlmJudge`] in production).
    pub judge: Arc<dyn PairedJudgeProvider>,
    /// Where to record the outcome.
    pub sink: Arc<dyn JudgeSink>,
    /// The org the request belonged to.
    pub org_id: Uuid,
    /// The matched route id, when known.
    pub route_id: Option<Uuid>,
    /// Stable id for the judged sample (the request's trace id).
    pub request_id: Uuid,
    /// Originally requested (expensive) model.
    pub requested_model: String,
    /// Served (cheaper) model.
    pub served_model: String,
    /// The user input text the judge compares answers against.
    pub input_text: String,
    /// The served (cheaper-model) answer.
    pub served_answer: String,
    /// The judge model id, recorded on the persisted verdict.
    pub judge_model: String,
    /// The blind slot the OPTIMIZED (served) answer occupies in the paired
    /// prompt — deterministic per trace via [`ab_order_for`].
    pub ab_order: AbOrder,
    /// Judge both A/B orders and require agreement (doubles the judge tax;
    /// see [`JudgeConfig::both_orders`]).
    pub both_orders: bool,
    /// How to obtain the original-model reference answer the judge compares
    /// against. Resolved INSIDE the detached task so the reference dispatch never
    /// touches the user response path.
    pub reference: ReferenceSource,
    /// When the judged response was **served from L2**, the specific cache entry
    /// to record the verdict against (and evict iff `Degraded`/High). `None` for
    /// the ordinary downgraded-dispatch path, where there is no cache entry to
    /// attribute the verdict to. This is the cloud join the roadmap flagged:
    /// closing it lets a clearly-degraded cached response be removed so it can't
    /// be served again.
    pub l2_eviction: Option<L2EvictionTarget>,
}

/// Where the original-model reference answer comes from.
pub enum ReferenceSource {
    /// A ready reference string (tests supply this directly).
    Ready(String),
    /// Re-dispatch the original model on its source provider to produce a fresh
    /// reference. Runs in the detached judge task — off the user response path —
    /// so it adds zero user latency. The judge only fires for a ~2% sample of
    /// downgraded requests, bounding the extra spend.
    Dispatch {
        /// Source provider that serves the original (expensive) model.
        provider: Arc<dyn Provider>,
        /// The original request (original model, original messages).
        request: Box<ChatCompletionRequest>,
        /// Request context (org credentials) for the source provider.
        ctx: Box<RequestContext>,
        /// Deadline for the reference dispatch
        /// ([`JudgeConfig::baseline_timeout`], default 30s — NOT the 2s canary
        /// shadow timeout: nobody is waiting on the detached task, and 2s
        /// would starve real baseline answers).
        deadline: std::time::Duration,
    },
}

impl ReferenceSource {
    /// Resolve the reference answer plus the cost of obtaining it. `Ready`
    /// returns the string with no cost (`None` — nothing was dispatched);
    /// `Dispatch` runs the shared
    /// [`crate::measurement::measured_single_dispatch`] (single shot,
    /// non-streaming, no failover, deadline-bounded — the same plumbing as the
    /// #146 canary shadow) and returns the assistant text plus the metered
    /// baseline cost. A dispatch error surfaces as [`QualityError::Judge`] so
    /// the job records nothing rather than a misleading verdict.
    async fn resolve(self) -> Result<(String, Option<f64>), QualityError> {
        match self {
            ReferenceSource::Ready(s) => Ok((s, None)),
            ReferenceSource::Dispatch {
                provider,
                request,
                ctx,
                deadline,
            } => {
                // Override the captured deadline with the judge's own baseline
                // deadline (mirrors dispatch_shadow's shadow_ctx construction).
                let dispatch_ctx = RequestContext {
                    deadline: Some(deadline),
                    ..*ctx
                };
                let measured = crate::measurement::measured_single_dispatch(
                    &provider,
                    *request,
                    &dispatch_ctx,
                    deadline,
                )
                .await
                .map_err(|e| QualityError::Judge(format!("reference dispatch: {e}")))?;
                Ok((response_text(&measured.response), Some(measured.cost_usd)))
            }
        }
    }
}

/// Run one judge job to completion: call the judge, build a [`SampleScore`] +
/// [`RiskBand`], and record the [`JudgeOutcome`]. Returns `Ok(())` even when the
/// judge declines (records `Unclear`); only a hard judge dispatch error or a
/// missing reference short-circuits without recording.
async fn run_job(mut job: QualityJudgeJob) -> Result<(), QualityError> {
    // Extract the L2 eviction target up front: `reference.resolve()` below
    // partially moves `job.reference`, after which `job` can't be field-accessed
    // for `l2_eviction`. Taking it here keeps the move analysis simple.
    let l2_eviction = job.l2_eviction.take();
    let (reference, baseline_cost_usd) = job.reference.resolve().await?;
    if reference.trim().is_empty() {
        // Empty reference (e.g. a tool-call-only original response) — nothing to
        // compare against, so record nothing rather than a meaningless verdict.
        return Ok(());
    }
    let paired = judge_paired(
        job.judge.as_ref(),
        &job.input_text,
        &reference,
        &job.served_answer,
        job.ab_order,
        job.both_orders,
    )
    .await?;
    let verdict = paired.verdict;
    let risk_band = risk_band_for_verdict(verdict);

    // Surface the per-request verdict on the telemetry path — but ONLY here,
    // where a judge actually ran and produced a verdict. An unjudged request
    // never reaches `run_job`, so the quality attributes are never fabricated
    // for one. This records onto the detached judge task's span (off the user
    // response path → zero user latency); the hosted side ingests + aggregates
    // these into "Quality preserved: …% (n=…)" via
    // `tt_plan_core::quality_preserved_summary`.
    record_quality_verdict_telemetry(
        job.request_id,
        &job.requested_model,
        &job.served_model,
        verdict,
        risk_band,
        paired.judge_cost_usd,
    );

    // Close the L2 join: if this response was SERVED FROM L2, record the verdict
    // on that specific entry and — only when the band is High (clearly
    // Degraded) — evict exactly that entry so the degraded cached answer can't
    // be served again. Targeted + conservative: a single-entry operation that
    // fires on `High` alone, never on Low/Medium/Unclear, and never a bulk
    // delete. Best-effort: a failure here never blocks the verdict recording.
    if let Some(target) = l2_eviction {
        let cache_band = risk_band_to_cache_band(risk_band);
        match target
            .cache
            .record_judge_verdict(
                target.entry_id,
                verdict.quality_score().map(|s| s as f32),
                verdict_str(verdict),
                cache_band,
            )
            .await
        {
            Ok(JudgeRecordOutcome::Evicted) => {
                tracing::info!(
                    entry_id = %target.entry_id,
                    "evicted L2 entry after degraded judge verdict"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, entry_id = %target.entry_id, "L2 verdict record failed");
            }
        }
    }

    let score = SampleScore {
        request_id: job.request_id,
        verdict,
        reason: paired.reason,
    };
    job.sink
        .record(JudgeOutcome {
            org_id: job.org_id,
            route_id: job.route_id,
            requested_model: job.requested_model,
            served_model: job.served_model,
            score,
            risk_band,
            judge_model: job.judge_model,
            judge_cost_usd: paired.judge_cost_usd,
            baseline_cost_usd,
            optimized_position: job.ab_order,
            orders_judged: paired.orders_judged,
            orders_agreed: paired.orders_agreed,
        })
        .await;
    Ok(())
}

/// Stamp the per-request quality verdict onto the current (judge-task) span via
/// the telemetry crate, using the same per-request values recorded into the
/// sink. The score is the classified verdict's `[0, 1]` value; an `Unclear`
/// verdict has no valence, so `quality_score()` is `None` and the score
/// attribute is omitted (band/verdict still surface) rather than fabricating a
/// "preserved" score — keeping the emitted per-request scores consistent with
/// `quality_preserved_summary`, which drops `Unclear` from its denominator.
fn record_quality_verdict_telemetry(
    request_id: Uuid,
    requested_model: &str,
    served_model: &str,
    verdict: JudgeVerdict,
    risk_band: RiskBand,
    judge_cost_usd: f64,
) {
    tt_telemetry::gen_ai::record_quality_verdict(
        &tracing::Span::current(),
        &tt_telemetry::gen_ai::QualityVerdictAttributes {
            request_id: &request_id.to_string(),
            requested_model,
            served_model,
            score: verdict.quality_score(),
            band: band_str(risk_band),
            verdict: verdict_str(verdict),
            judge_cost_usd,
        },
    );
}

/// Spawn the judge job on the tokio runtime and return immediately.
///
/// **This is the only public way to run the judge from the request path, and it
/// adds zero latency:** it does not await the job — it hands ownership to a
/// detached task and returns. The HTTP response is built and returned by the
/// caller before (and independently of) this task making any progress.
pub fn spawn_quality_judge(job: QualityJudgeJob) {
    use tracing::Instrument as _;
    // Instrument the detached task with its own `quality_judge` span so the
    // per-request quality verdict recorded inside `run_job` has a stable home
    // span (the cloud ingests these), and the judge's own dispatch is traceable
    // separately from the user request span it never touches.
    let request_id = job.request_id;
    let span = tracing::info_span!("quality_judge", request_id = %request_id);
    tokio::spawn(
        async move {
            if let Err(e) = run_job(job).await {
                tracing::warn!(error = %e, "quality judge sample failed");
            }
        }
        .instrument(span),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn pricing(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            prompt_cache_min_tokens: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            effective_at: Utc::now(),
        }
    }

    fn usage() -> Usage {
        Usage {
            prompt_tokens: 100,
            completion_tokens: 100,
            total_tokens: 200,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
        }
    }

    #[test]
    fn downgrade_detected_when_served_cheaper() {
        // gpt-4o (5/15) → gpt-4o-mini (0.15/0.6): clearly cheaper.
        assert!(is_downgrade(
            Some(&pricing(5.0, 15.0)),
            Some(&pricing(0.15, 0.6)),
            &usage()
        ));
    }

    #[test]
    fn not_a_downgrade_when_served_more_expensive_or_equal() {
        // Equal price → not a downgrade.
        assert!(!is_downgrade(
            Some(&pricing(1.0, 2.0)),
            Some(&pricing(1.0, 2.0)),
            &usage()
        ));
        // Served pricier → not a downgrade.
        assert!(!is_downgrade(
            Some(&pricing(0.15, 0.6)),
            Some(&pricing(5.0, 15.0)),
            &usage()
        ));
    }

    #[test]
    fn unknown_pricing_is_never_a_downgrade() {
        assert!(!is_downgrade(None, Some(&pricing(0.1, 0.1)), &usage()));
        assert!(!is_downgrade(Some(&pricing(5.0, 5.0)), None, &usage()));
        assert!(!is_downgrade(None, None, &usage()));
    }

    #[test]
    fn sample_rate_zero_never_samples_one_always() {
        for _ in 0..50 {
            let id = Uuid::now_v7();
            assert!(!should_sample(id, 0.0));
            assert!(should_sample(id, 1.0));
        }
    }

    #[test]
    fn sample_is_deterministic_per_trace() {
        let id = Uuid::now_v7();
        let a = should_sample(id, 0.5);
        let b = should_sample(id, 0.5);
        assert_eq!(a, b, "same trace + rate must give the same decision");
    }

    #[test]
    fn sample_rate_is_approximately_uniform() {
        // Over many distinct trace ids, ~rate fraction should be sampled.
        let n = 20_000;
        let rate = 0.02;
        let mut kept = 0;
        for _ in 0..n {
            if should_sample(Uuid::now_v7(), rate) {
                kept += 1;
            }
        }
        let frac = kept as f64 / n as f64;
        // Generous band around 2% (binomial noise + v7 timestamp structure).
        assert!(
            (0.01..0.035).contains(&frac),
            "kept fraction {frac} should be near {rate}"
        );
    }

    #[test]
    fn verdict_parsing() {
        assert_eq!(
            parse_verdict("ACCEPTABLE\nsame substance").0,
            JudgeVerdict::Acceptable
        );
        assert_eq!(
            parse_verdict("degraded — missed the key point").0,
            JudgeVerdict::Degraded
        );
        assert_eq!(parse_verdict("hmm not sure").0, JudgeVerdict::Unclear);
        assert_eq!(parse_verdict("ACCEPTABLE\nreason").1, "ACCEPTABLE");
    }

    #[test]
    fn risk_band_mapping() {
        assert_eq!(
            risk_band_for_verdict(JudgeVerdict::Degraded),
            RiskBand::High
        );
        assert_eq!(
            risk_band_for_verdict(JudgeVerdict::Acceptable),
            RiskBand::Low
        );
        assert_eq!(risk_band_for_verdict(JudgeVerdict::Unclear), RiskBand::Low);
        assert_eq!(
            risk_band_to_preview(RiskBand::High),
            tt_preview::QualityRiskBand::High
        );
    }

    #[test]
    fn verdict_and_band_wire_strings() {
        assert_eq!(verdict_str(JudgeVerdict::Acceptable), "acceptable");
        assert_eq!(verdict_str(JudgeVerdict::Degraded), "degraded");
        assert_eq!(verdict_str(JudgeVerdict::Unclear), "unclear");
        assert_eq!(band_str(RiskBand::Low), "low");
        assert_eq!(band_str(RiskBand::Medium), "medium");
        assert_eq!(band_str(RiskBand::High), "high");
    }

    #[test]
    fn task_class_only_chat_completions_sampled() {
        assert!(JudgeTaskClass::ChatCompletions.is_sampled());
    }

    #[test]
    fn config_defaults_off_with_2pct_rate() {
        let c = JudgeConfig::default();
        assert!(!c.enabled);
        assert!((c.sample_rate - 0.02).abs() < 1e-12);
        assert_eq!(c.judge_model, DEFAULT_JUDGE_MODEL);
        assert!(!c.both_orders, "both-orders mode must default OFF");
        assert_eq!(
            c.baseline_timeout,
            std::time::Duration::from_secs(30),
            "baseline reference dispatch deadline defaults to 30s"
        );
    }

    /// `TT_JUDGE_BOTH_ORDERS` parses with the same truthy convention as
    /// `TT_JUDGE_ENABLED`, and `from_env` without the var keeps the default.
    #[test]
    fn config_paired_defaults() {
        let unset = JudgeConfig::from_env();
        assert!(!unset.both_orders);
        assert_eq!(unset.baseline_timeout, std::time::Duration::from_secs(30));

        std::env::set_var("TT_JUDGE_BOTH_ORDERS", "1");
        let on = JudgeConfig::from_env();
        std::env::remove_var("TT_JUDGE_BOTH_ORDERS");
        assert!(on.both_orders, "TT_JUDGE_BOTH_ORDERS=1 must parse true");
    }

    // ── Paired A/B judge core (research Phase 0.4) ───────────────────────────

    /// A/B order is deterministic per trace id and roughly balanced across
    /// many ids (debiasing requires both: stable per sample, uniform across
    /// samples).
    #[test]
    fn ab_order_is_deterministic_and_balanced() {
        let id = Uuid::now_v7();
        assert_eq!(
            ab_order_for(id),
            ab_order_for(id),
            "same trace id must always get the same A/B order"
        );

        let n = 20_000;
        let mut optimized_a = 0usize;
        for _ in 0..n {
            if ab_order_for(Uuid::now_v7()) == AbOrder::OptimizedA {
                optimized_a += 1;
            }
        }
        let frac = optimized_a as f64 / n as f64;
        assert!(
            (0.45..=0.55).contains(&frac),
            "OptimizedA fraction {frac} should be near 0.5"
        );
    }

    #[test]
    fn ab_position_strings() {
        assert_eq!(ab_position_str(AbOrder::OptimizedA), "a");
        assert_eq!(ab_position_str(AbOrder::OptimizedB), "b");
    }

    /// A/B order must be INDEPENDENT of the keep/drop sampling decision —
    /// otherwise the judged subset would systematically see one order. Over
    /// many sampled-in ids, both orders appear.
    #[test]
    fn ab_order_is_independent_of_sampling_salt() {
        let mut seen_a = false;
        let mut seen_b = false;
        for _ in 0..1000 {
            let id = Uuid::now_v7();
            if !should_sample(id, 0.5) {
                continue;
            }
            match ab_order_for(id) {
                AbOrder::OptimizedA => seen_a = true,
                AbOrder::OptimizedB => seen_b = true,
            }
            if seen_a && seen_b {
                break;
            }
        }
        assert!(
            seen_a && seen_b,
            "sampled-in traces must still see both A/B orders"
        );
    }

    #[test]
    fn parse_pair_verdict_tokens() {
        assert_eq!(
            parse_pair_verdict("EQUIVALENT\nsame info").0,
            PairVerdict::Equivalent
        );
        assert_eq!(
            parse_pair_verdict("A_MISSING — dropped the dosage caveat").0,
            PairVerdict::AMissing
        );
        assert_eq!(
            parse_pair_verdict("B_MISSING\nomitted the second step").0,
            PairVerdict::BMissing
        );
        assert_eq!(
            parse_pair_verdict("who knows, really").0,
            PairVerdict::Unclear
        );
        // A reply naming BOTH sides is unclassifiable, not a coin flip.
        assert_eq!(
            parse_pair_verdict("A_MISSING and also B_MISSING").0,
            PairVerdict::Unclear
        );
        // Reason = first trimmed line.
        assert_eq!(
            parse_pair_verdict("EQUIVALENT\nsecond line").1,
            "EQUIVALENT"
        );
    }

    /// The paired prompt is BLIND (no ORIGINAL/CHEAPER role labels, no model
    /// ids) and carries the anti-length-bias instruction. This is the
    /// label-bias regression guard: position randomization is useless if the
    /// labels still reveal which answer is the optimized one.
    #[test]
    fn paired_prompt_is_blind_and_length_neutral() {
        let judge = GatewayLlmJudge::new(Arc::new(NoopProvider), "judge-x".to_string(), test_ctx());
        let req = judge.build_paired_request("what is 2+2", "it is 4", "the answer is 4");

        let mut system = String::new();
        let mut user = String::new();
        for m in &req.messages {
            match m {
                Message::System { content } => system = content_text(content),
                Message::User { content, .. } => user = content_text(content),
                _ => {}
            }
        }
        assert!(user.contains("ANSWER A:"), "user msg labels slot A: {user}");
        assert!(user.contains("ANSWER B:"), "user msg labels slot B: {user}");

        let full = format!("{system}\n{user}");
        let upper = full.to_ascii_uppercase();
        assert!(
            !upper.contains("ORIGINAL"),
            "blind prompt must not leak the ORIGINAL role label: {full}"
        );
        assert!(
            !upper.contains("CHEAPER"),
            "blind prompt must not leak the CHEAPER role label: {full}"
        );
        assert!(
            !full.contains("judge-x"),
            "prompt must not name any model id: {full}"
        );
        assert!(
            system.to_ascii_lowercase().contains("length"),
            "rubric must address answer length: {system}"
        );
        assert!(
            system.contains("NOT better"),
            "rubric must state longer/verbose is NOT better: {system}"
        );
    }

    /// AMissing/BMissing map to Degraded ONLY when the missing slot is the one
    /// the optimized answer occupied.
    #[tokio::test]
    async fn paired_mapping_optimized_missing_is_degraded() {
        for (pair, order, expect) in [
            (
                PairVerdict::AMissing,
                AbOrder::OptimizedA,
                JudgeVerdict::Degraded,
            ),
            (
                PairVerdict::AMissing,
                AbOrder::OptimizedB,
                JudgeVerdict::Acceptable,
            ),
            (
                PairVerdict::BMissing,
                AbOrder::OptimizedB,
                JudgeVerdict::Degraded,
            ),
            (
                PairVerdict::BMissing,
                AbOrder::OptimizedA,
                JudgeVerdict::Acceptable,
            ),
        ] {
            let judge = MockPairedJudge {
                verdict: pair,
                reason: "r".to_string(),
                cost_usd: 0.001,
            };
            let out = judge_paired(&judge, "in", "baseline", "optimized", order, false)
                .await
                .expect("judge_paired ok");
            assert_eq!(
                out.verdict, expect,
                "pair verdict {pair:?} with optimized in {order:?}"
            );
        }
    }

    #[tokio::test]
    async fn paired_mapping_equivalent_acceptable_unclear_unclear() {
        let eq = MockPairedJudge {
            verdict: PairVerdict::Equivalent,
            reason: "same".to_string(),
            cost_usd: 0.001,
        };
        let out = judge_paired(&eq, "in", "b", "o", AbOrder::OptimizedA, false)
            .await
            .unwrap();
        assert_eq!(out.verdict, JudgeVerdict::Acceptable);
        assert_eq!(out.orders_judged, 1, "single-order mode judges once");
        assert_eq!(
            out.orders_agreed, None,
            "no agreement signal in single-order mode"
        );
        assert!((out.judge_cost_usd - 0.001).abs() < 1e-12);

        let un = MockPairedJudge {
            verdict: PairVerdict::Unclear,
            reason: "?".to_string(),
            cost_usd: 0.001,
        };
        let out = judge_paired(&un, "in", "b", "o", AbOrder::OptimizedB, false)
            .await
            .unwrap();
        assert_eq!(out.verdict, JudgeVerdict::Unclear);
    }

    /// Both-orders mode: a judge consistent across the swap (AMissing then
    /// BMissing — both naming the optimized answer's slot) keeps the mapped
    /// verdict and sums the judge tax over both calls.
    #[tokio::test]
    async fn both_orders_agreement_keeps_verdict_and_sums_cost() {
        let judge = SequenceJudge::new(vec![
            PairCall {
                verdict: PairVerdict::AMissing,
                reason: "fwd reason".to_string(),
                cost_usd: 0.001,
            },
            PairCall {
                verdict: PairVerdict::BMissing,
                reason: "rev reason".to_string(),
                cost_usd: 0.001,
            },
        ]);
        let out = judge_paired(
            &judge,
            "in",
            "baseline",
            "optimized",
            AbOrder::OptimizedA,
            true,
        )
        .await
        .unwrap();
        assert_eq!(out.verdict, JudgeVerdict::Degraded);
        assert!(
            (out.judge_cost_usd - 0.002).abs() < 1e-12,
            "judge tax sums over orders"
        );
        assert_eq!(out.orders_judged, 2);
        assert_eq!(out.orders_agreed, Some(true));
        assert!(out.reason.contains("fwd reason") && out.reason.contains("rev reason"));
    }

    /// Both-orders mode: a position-biased judge (always blames slot A) maps to
    /// conflicting verdicts across the swap → recorded as Unclear, with the
    /// disagreement flagged and the cost still summed.
    #[tokio::test]
    async fn both_orders_disagreement_records_unclear() {
        let judge = MockPairedJudge {
            verdict: PairVerdict::AMissing, // regardless of order — pure position bias
            reason: "slot A is worse".to_string(),
            cost_usd: 0.001,
        };
        let out = judge_paired(
            &judge,
            "in",
            "baseline",
            "optimized",
            AbOrder::OptimizedA,
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            out.verdict,
            JudgeVerdict::Unclear,
            "a position-sensitive judgment is genuinely unclassifiable"
        );
        assert_eq!(out.orders_judged, 2);
        assert_eq!(out.orders_agreed, Some(false));
        assert!((out.judge_cost_usd - 0.002).abs() < 1e-12);
        assert!(out.reason.contains("position-sensitive"), "{}", out.reason);
    }

    /// run_job persists the measurement tax + debiasing audit trail onto the
    /// recorded JudgeOutcome and stamps the judge tax on the telemetry span.
    #[test]
    fn run_job_records_judge_and_baseline_cost() {
        let sink = Arc::new(RecordingSink::default());
        let mut job = job_with(
            ReferenceSource::Ready("reference".to_string()),
            "acceptable",
        );
        job.sink = sink.clone();
        job.judge = Arc::new(MockPairedJudge {
            verdict: PairVerdict::Equivalent,
            reason: "because".to_string(),
            cost_usd: 0.002,
        });
        let expected_order = job.ab_order;
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });

        let outcomes = sink.0.lock().unwrap();
        assert_eq!(outcomes.len(), 1);
        let o = &outcomes[0];
        assert!((o.judge_cost_usd - 0.002).abs() < 1e-12);
        assert_eq!(
            o.baseline_cost_usd, None,
            "a Ready (pre-existing) reference has no baseline dispatch cost"
        );
        assert_eq!(o.judge_model, "judge-mock");
        assert_eq!(o.optimized_position, expected_order);
        assert_eq!(o.orders_judged, 1);
        assert_eq!(o.orders_agreed, None);

        assert_eq!(
            attrs.get("tokentrimmer.quality.judge_cost_usd"),
            Some(&Value::F64(0.002)),
            "judge tax must surface on the judge-task span"
        );
    }

    /// FanoutJudgeSink clones one outcome to every wired sink (band store +
    /// durable Postgres sink in production).
    #[tokio::test]
    async fn fanout_sink_records_to_all_sinks() {
        let a = Arc::new(RecordingSink::default());
        let b = Arc::new(RecordingSink::default());
        let fanout = FanoutJudgeSink::new(vec![
            a.clone() as Arc<dyn JudgeSink>,
            b.clone() as Arc<dyn JudgeSink>,
        ]);
        fanout
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Acceptable))
            .await;
        assert_eq!(a.0.lock().unwrap().len(), 1, "first sink got the outcome");
        assert_eq!(b.0.lock().unwrap().len(), 1, "second sink got the outcome");
    }

    fn outcome(requested: &str, served: &str, verdict: JudgeVerdict) -> JudgeOutcome {
        JudgeOutcome {
            org_id: Uuid::now_v7(),
            route_id: Some(Uuid::now_v7()),
            requested_model: requested.to_string(),
            served_model: served.to_string(),
            score: SampleScore {
                request_id: Uuid::now_v7(),
                verdict,
                reason: "r".to_string(),
            },
            risk_band: risk_band_for_verdict(verdict),
            judge_model: "judge-mock".to_string(),
            judge_cost_usd: 0.0,
            baseline_cost_usd: None,
            optimized_position: AbOrder::OptimizedA,
            orders_judged: 1,
            orders_agreed: None,
        }
    }

    /// Fixed-verdict paired judge: returns the same [`PairVerdict`] regardless
    /// of slot contents (so it doubles as the "position-biased" judge in the
    /// both-orders disagreement test).
    struct MockPairedJudge {
        verdict: PairVerdict,
        reason: String,
        cost_usd: f64,
    }

    #[async_trait]
    impl PairedJudgeProvider for MockPairedJudge {
        async fn judge_pair(
            &self,
            _input: &str,
            _answer_a: &str,
            _answer_b: &str,
        ) -> Result<PairCall, QualityError> {
            Ok(PairCall {
                verdict: self.verdict,
                reason: self.reason.clone(),
                cost_usd: self.cost_usd,
            })
        }
    }

    /// Scripted paired judge: pops one pre-queued [`PairCall`] per call.
    struct SequenceJudge {
        calls: std::sync::Mutex<std::collections::VecDeque<PairCall>>,
    }

    impl SequenceJudge {
        fn new(calls: Vec<PairCall>) -> Self {
            Self {
                calls: std::sync::Mutex::new(calls.into()),
            }
        }
    }

    #[async_trait]
    impl PairedJudgeProvider for SequenceJudge {
        async fn judge_pair(
            &self,
            _input: &str,
            _answer_a: &str,
            _answer_b: &str,
        ) -> Result<PairCall, QualityError> {
            Ok(self
                .calls
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra judge call"))
        }
    }

    /// Sink that records every outcome (unit-test mirror of the integration
    /// RecordingSink).
    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<JudgeOutcome>>);

    #[async_trait]
    impl JudgeSink for RecordingSink {
        async fn record(&self, outcome: JudgeOutcome) {
            self.0.lock().unwrap().push(outcome);
        }
    }

    /// Provider that never dispatches — only used to construct a
    /// [`GatewayLlmJudge`] whose prompt builder is under test.
    struct NoopProvider;

    #[async_trait]
    impl Provider for NoopProvider {
        fn id(&self) -> &'static str {
            "noop"
        }
        fn models(&self) -> Vec<tt_shared::ModelInfo> {
            Vec::new()
        }
        fn pricing(&self, _model: &str) -> Option<ModelPricing> {
            None
        }
        async fn chat_completion(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<ChatCompletionResponse, tt_shared::ProviderError> {
            Err(tt_shared::ProviderError::Unsupported("noop".into()))
        }
        async fn chat_completion_stream(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<tt_shared::ChatCompletionChunk, tt_shared::ProviderError>,
            >,
            tt_shared::ProviderError,
        > {
            Err(tt_shared::ProviderError::Unsupported("noop".into()))
        }
        async fn embeddings(
            &self,
            _req: tt_shared::EmbeddingsRequest,
            _ctx: &RequestContext,
        ) -> Result<tt_shared::EmbeddingsResponse, tt_shared::ProviderError> {
            Err(tt_shared::ProviderError::Unsupported("noop".into()))
        }
    }

    fn test_ctx() -> RequestContext {
        RequestContext {
            trace_id: Uuid::now_v7(),
            org_id: Uuid::now_v7(),
            api_key_id: Uuid::now_v7(),
            credentials: tt_shared::context::ProviderCredentials {
                api_key: tt_shared::context::SecretString::new("k"),
                base_url: None,
                extra_headers: Vec::new(),
            },
            tag: None,
            deadline: None,
        }
    }

    /// Unjudged swap → `None` (caller keeps it `Unknown`).
    #[tokio::test]
    async fn band_store_returns_none_for_unjudged_swap() {
        let store = InMemoryJudgeBandStore::new();
        assert_eq!(store.band_for("gpt-4o", "gpt-4o-mini"), None);
    }

    /// A single `Degraded` sample aggregates to `High` for that swap.
    #[tokio::test]
    async fn band_store_single_degraded_is_high() {
        let store = InMemoryJudgeBandStore::new();
        store
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Degraded))
            .await;
        assert_eq!(
            store.band_for("gpt-4o", "gpt-4o-mini"),
            Some(tt_preview::QualityRiskBand::High)
        );
    }

    /// Aggregation matches `score_quality`: one `Degraded` in a large acceptable
    /// set is `Low` (degraded share ≤ 5%), not `High`.
    #[tokio::test]
    async fn band_store_aggregates_degraded_pct_over_classified() {
        let store = InMemoryJudgeBandStore::new();
        for _ in 0..30 {
            store
                .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Acceptable))
                .await;
        }
        store
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Degraded))
            .await;
        // 1 / 31 ≈ 3.2% degraded → Low.
        assert_eq!(
            store.band_for("gpt-4o", "gpt-4o-mini"),
            Some(tt_preview::QualityRiskBand::Low)
        );
    }

    /// Only-`Unclear` samples stay `None` (genuinely unknown), and `Unclear` is
    /// excluded from the degraded denominator.
    #[tokio::test]
    async fn band_store_only_unclear_is_none() {
        let store = InMemoryJudgeBandStore::new();
        store
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Unclear))
            .await;
        assert_eq!(store.band_for("gpt-4o", "gpt-4o-mini"), None);
    }

    /// End-to-end join: a recorded outcome flows through the enrichment seam to
    /// populate the suggestion band, replacing the hard-coded `Unknown`. An
    /// unjudged swap in the same batch stays `Unknown`.
    #[tokio::test]
    async fn band_store_enriches_suggestions_through_seam() {
        let store = InMemoryJudgeBandStore::new();
        store
            .record(outcome("gpt-4o", "gpt-4o-mini", JudgeVerdict::Degraded))
            .await;

        let mut suggestions = vec![
            tt_preview::RouteSuggestion {
                route: "swap-to-gpt-4o-mini".into(),
                model: "gpt-4o-mini".into(),
                cost_usd: 0.001,
                savings_usd: 0.01,
                quality_risk_band: tt_preview::QualityRiskBand::Unknown,
                rationale: "x".into(),
                applicable: true,
            },
            tt_preview::RouteSuggestion {
                route: "swap-to-haiku".into(),
                model: "claude-haiku-4-5".into(),
                cost_usd: 0.001,
                savings_usd: 0.01,
                quality_risk_band: tt_preview::QualityRiskBand::Unknown,
                rationale: "x".into(),
                applicable: true,
            },
        ];

        store.enrich_suggestions("gpt-4o", &mut suggestions);

        // Judged swap → lifted to High.
        assert_eq!(
            suggestions[0].quality_risk_band,
            tt_preview::QualityRiskBand::High
        );
        // Unjudged swap → stays honestly Unknown.
        assert_eq!(
            suggestions[1].quality_risk_band,
            tt_preview::QualityRiskBand::Unknown
        );
    }

    // ── Telemetry surfacing of the per-request verdict (JUDGE-SURFACE) ──────
    //
    // The judge runs detached, AFTER the user response, so the per-request
    // verdict is surfaced on the JUDGE TASK's own span (zero user latency). The
    // hosted side ingests these `tokentrimmer.quality.*` attributes and
    // aggregates them into "Quality preserved: …% (n=…)". A request the judge
    // never scored carries no quality attributes at all (no fabricated verdict).

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::Value;
    use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::TracerProvider;
    use std::collections::HashMap;
    use tracing_subscriber::prelude::*;

    /// Sink that swallows outcomes — the telemetry tests assert on the span, not
    /// the sink.
    struct NullSink;
    #[async_trait]
    impl JudgeSink for NullSink {
        async fn record(&self, _outcome: JudgeOutcome) {}
    }

    /// Drive `f` under a scoped OTel subscriber on a current-thread runtime and
    /// return the attributes recorded on the single `quality_judge` span as a
    /// name→Value map. `run_job` is async; a current-thread runtime keeps the
    /// future on the subscriber's thread so the span reaches the exporter.
    fn capture_judge_span_attributes<F, Fut>(f: F) -> HashMap<String, Value>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("judge-test");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(otel_layer);

        tracing::subscriber::with_default(subscriber, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");
            rt.block_on(async {
                use tracing::Instrument as _;
                let span = tracing::info_span!("quality_judge");
                f().instrument(span).await;
            });
        });

        provider.force_flush();
        let spans = exporter.get_finished_spans().expect("finished spans");
        spans
            .into_iter()
            .find(|s| s.name == "quality_judge")
            .map(|s| {
                s.attributes
                    .into_iter()
                    .map(|kv| (kv.key.to_string(), kv.value))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn job_with(reference: ReferenceSource, verdict_word: &'static str) -> QualityJudgeJob {
        // The job pins `ab_order: OptimizedA`, so a fixed AMissing pair verdict
        // deterministically maps to Degraded (the optimized slot is missing).
        QualityJudgeJob {
            judge: Arc::new(MockPairedJudge {
                verdict: match verdict_word {
                    "degraded" => PairVerdict::AMissing,
                    "unclear" => PairVerdict::Unclear,
                    _ => PairVerdict::Equivalent,
                },
                reason: "because".to_string(),
                cost_usd: 0.0,
            }),
            sink: Arc::new(NullSink),
            org_id: Uuid::now_v7(),
            route_id: Some(Uuid::now_v7()),
            request_id: Uuid::now_v7(),
            requested_model: "gpt-4o".to_string(),
            served_model: "gpt-4o-mini".to_string(),
            input_text: "in".to_string(),
            served_answer: "served".to_string(),
            judge_model: "judge-mock".to_string(),
            ab_order: AbOrder::OptimizedA,
            both_orders: false,
            reference,
            l2_eviction: None,
        }
    }

    /// A judged (Acceptable) request stamps `quality.score = 1.0`, `band = low`,
    /// `verdict = acceptable`, plus the request id + the swapped models.
    #[test]
    fn judged_request_records_quality_verdict_on_span() {
        let job = job_with(
            ReferenceSource::Ready("reference".to_string()),
            "acceptable",
        );
        let request_id = job.request_id.to_string();
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });

        assert_eq!(
            attrs.get("tokentrimmer.quality.score"),
            Some(&Value::F64(1.0)),
            "acceptable verdict → preserved score 1.0"
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.band"),
            Some(&Value::String("low".into()))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.verdict"),
            Some(&Value::String("acceptable".into()))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.request_id"),
            Some(&Value::String(request_id.into()))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.requested_model"),
            Some(&Value::String("gpt-4o".into()))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.served_model"),
            Some(&Value::String("gpt-4o-mini".into()))
        );
    }

    /// A Degraded verdict surfaces `score = 0.0` / `band = high`.
    #[test]
    fn degraded_request_records_zero_score_high_band() {
        let job = job_with(ReferenceSource::Ready("reference".to_string()), "degraded");
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });
        assert_eq!(
            attrs.get("tokentrimmer.quality.score"),
            Some(&Value::F64(0.0))
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.band"),
            Some(&Value::String("high".into()))
        );
    }

    /// An Unclear verdict was judged, so it surfaces `band` + `verdict`, but it
    /// has no valence — the `score` attribute is OMITTED rather than defaulted to
    /// a "preserved" `1.0`. This keeps the emitted per-request scores consistent
    /// with `quality_preserved_summary`, which drops `Unclear` from its
    /// denominator (an emitted `1.0` would inflate an averaged headline).
    #[test]
    fn unclear_request_omits_score_but_records_band_and_verdict() {
        let job = job_with(ReferenceSource::Ready("reference".to_string()), "unclear");
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });
        assert!(
            !attrs.contains_key("tokentrimmer.quality.score"),
            "unclear verdict has no valence → score attribute must be omitted"
        );
        assert_eq!(
            attrs.get("tokentrimmer.quality.verdict"),
            Some(&Value::String("unclear".into()))
        );
        // Band still surfaces (the judge ran), so the sample is visible.
        assert!(
            attrs.contains_key("tokentrimmer.quality.band"),
            "a judged unclear request still records its band"
        );
    }

    /// An UNJUDGED request (empty reference → `run_job` records nothing) carries
    /// NO quality attributes — the verdict is never fabricated.
    #[test]
    fn unjudged_request_records_no_quality_attributes() {
        // Empty reference short-circuits run_job before any judge/verdict.
        let job = job_with(ReferenceSource::Ready("   ".to_string()), "acceptable");
        let attrs = capture_judge_span_attributes(|| async move {
            run_job(job).await.expect("run_job ok");
        });
        assert!(
            !attrs.contains_key("tokentrimmer.quality.score"),
            "unjudged request must not carry a quality score"
        );
        assert!(
            !attrs.contains_key("tokentrimmer.quality.band"),
            "unjudged request must not carry a quality band"
        );
        assert!(
            !attrs.contains_key("tokentrimmer.quality.verdict"),
            "unjudged request must not carry a quality verdict"
        );
    }

    // ── L2 judge join: eviction on degraded served-from-L2 responses ─────────

    use tt_cache::{CacheEntry, InMemoryL2Cache};

    /// Build an L2 cache holding one entry and return `(cache, entry_id)`.
    async fn l2_with_one_entry() -> (Arc<InMemoryL2Cache>, Uuid) {
        let cache = Arc::new(InMemoryL2Cache::new());
        let id = Uuid::now_v7();
        let now = chrono::Utc::now();
        cache
            .insert(CacheEntry {
                id,
                org_id: Uuid::now_v7(),
                embedding: vec![1.0, 0.0],
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
            })
            .await
            .unwrap();
        (cache, id)
    }

    fn job_targeting_l2(
        verdict_word: &'static str,
        cache: Arc<InMemoryL2Cache>,
        entry_id: Uuid,
    ) -> QualityJudgeJob {
        let mut job = job_with(
            ReferenceSource::Ready("reference".to_string()),
            verdict_word,
        );
        job.l2_eviction = Some(L2EvictionTarget { cache, entry_id });
        job
    }

    /// A Degraded verdict on a SERVED-FROM-L2 response evicts exactly that entry.
    #[tokio::test]
    async fn degraded_l2_verdict_evicts_the_served_entry() {
        let (cache, id) = l2_with_one_entry().await;
        run_job(job_targeting_l2("degraded", cache.clone(), id))
            .await
            .expect("run_job ok");
        // The entry is gone (lookup with threshold 0 can't find it).
        let hit = cache
            .lookup(
                Uuid::nil(), // org doesn't matter — entry deleted regardless
                &[1.0, 0.0],
                0.0,
                "gpt-4o",
                "mock-v1",
            )
            .await
            .unwrap();
        assert!(hit.is_none(), "degraded served-from-L2 response must evict");
    }

    /// An Acceptable verdict records the score but KEEPS the entry.
    #[tokio::test]
    async fn acceptable_l2_verdict_keeps_the_entry() {
        let (cache, id) = l2_with_one_entry().await;
        run_job(job_targeting_l2("acceptable", cache.clone(), id))
            .await
            .expect("run_job ok");
        // Probe existence: a record against the same id finds the entry
        // (Recorded, not NotFound), proving it survived the Acceptable verdict.
        let outcome = cache
            .record_judge_verdict(id, Some(1.0), "acceptable", JudgeBand::Low)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            JudgeRecordOutcome::Recorded,
            "acceptable served-from-L2 response must keep the entry"
        );
    }

    /// An Unclear verdict KEEPS the entry (records only — no eviction on Unclear).
    #[tokio::test]
    async fn unclear_l2_verdict_keeps_the_entry() {
        let (cache, id) = l2_with_one_entry().await;
        run_job(job_targeting_l2("unclear", cache.clone(), id))
            .await
            .expect("run_job ok");
        let outcome = cache
            .record_judge_verdict(id, None, "unclear", JudgeBand::Low)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            JudgeRecordOutcome::Recorded,
            "unclear served-from-L2 response must keep the entry"
        );
    }

    #[test]
    fn risk_band_to_cache_band_maps_high_to_high() {
        assert_eq!(risk_band_to_cache_band(RiskBand::High), JudgeBand::High);
        assert_eq!(risk_band_to_cache_band(RiskBand::Medium), JudgeBand::Medium);
        assert_eq!(risk_band_to_cache_band(RiskBand::Low), JudgeBand::Low);
    }
}
