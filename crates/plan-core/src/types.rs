//! Plan input + output types. All Serde-serializable so the determinism
//! contract (`same input -> bit-identical JSON output`) can be asserted in
//! tests and snapshotted via `insta`.
//!
//! These types are intentionally decoupled from `tt-shared::ModelPricing`
//! and the on-disk telemetry row shape: the replay engine works against a
//! condensed in-memory view assembled by the caller (CLI / API handler),
//! which lets us evolve those storage shapes without bumping the Plan
//! result schema.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One historical `request_logs` row, condensed to what the replay needs.
///
/// Mirrors `crates/core/migrations/0001_request_logs.up.sql` for the fields
/// the replay reads. Embedding / vector fields are intentionally absent in
/// v1; semantic-cache (L2) projection lands in a follow-up backlog item
/// (ADR-008 covers the embedding choice).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLog {
    /// Stable primary key for ordering during replay.
    pub id: Uuid,
    /// Tenant the request belongs to.
    pub org_id: Uuid,
    /// Wall-clock time the request was served at.
    pub ts: DateTime<Utc>,
    /// Upstream provider name (e.g. `"anthropic"`).
    pub provider: String,
    /// Provider-side model id (e.g. `"claude-3-5-sonnet"`).
    pub model: String,
    /// Input tokens charged on the baseline run.
    pub input_tokens: u32,
    /// Output tokens charged on the baseline run.
    pub output_tokens: u32,
    /// Of `input_tokens`, how many were cached (provider-native prompt cache).
    pub cached_tokens: u32,
    /// Cost the org actually paid for this request, USD.
    pub cost_usd: f64,
    /// Cost the org would have paid without any TokenTrimmer routing/caching
    /// — used as the savings denominator.
    pub baseline_cost_usd: f64,
    /// Whether TokenTrimmer's L1/L2 cache served the response.
    pub cached: bool,
    /// `"l1"` | `"l2"` | `None`. Constrained to the same set as the DB CHECK.
    pub cache_layer: Option<String>,
    /// The route id that matched in the baseline config, if any.
    pub matched_route_id: Option<Uuid>,
    /// End-to-end latency observed by the client, ms.
    pub latency_ms: u32,
    /// Upstream-only latency, ms — `None` if not recorded.
    pub upstream_latency_ms: Option<u32>,
    /// HTTP status the gateway returned.
    pub status: u16,
    /// Free-form tag for per-feature attribution.
    pub tag: Option<String>,
    /// Embedding of the request (typically the last user message), as
    /// produced by the embedding model named in ADR-008
    /// (`text-embedding-3-small`, 1536-dim L2-normalized). `None` when the
    /// historical row predates the embedding pipeline or the tenant opted
    /// out — such requests are skipped by L2 projection.
    ///
    /// Appended at the end of the struct with `#[serde(default)]` so older
    /// serialized payloads continue to deserialize unchanged.
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    /// Provider-reported finish reason (`"stop"`, `"length"`, `"tool_use"`,
    /// …). Used by the L2 cache-poisoning heuristic — when absent on either
    /// side of a candidate pair the heuristic degrades gracefully and the
    /// "divergent finish reason" signal is skipped for that pair.
    ///
    /// Appended at the end of the struct with `#[serde(default)]` so older
    /// serialized payloads continue to deserialize unchanged.
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Raw request prompt text. Only populated when the org has opted into
    /// body logging (ADR-009 / spec §11). Required for Tier 3 quality
    /// scoring — [`crate::quality::score_quality`] skips any sampled row
    /// that lacks it. `#[serde(default, skip_serializing_if = ...)]` keeps
    /// older snapshots / persisted rows byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Raw response text from the original baseline run. Pairs with `body`
    /// for Tier 3 quality scoring. Same opt-in + back-compat constraints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

/// A route in the proposed config: when conditions match, route to
/// `target_model`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedRoute {
    /// Stable identifier — used as the bucket key in per-route breakdown.
    pub id: Uuid,
    /// Human-readable name, surfaced in the report.
    pub name: String,
    /// Higher numbers win — evaluated descending; first match takes effect.
    pub priority: u32,
    /// Disabled routes never match.
    pub enabled: bool,
    /// AND-ed conditions. See [`RouteConditions`].
    pub when: RouteConditions,
    /// What to do when matched.
    pub then: RouteAction,
}

/// Match conditions for a [`ProposedRoute`]. v1 supports: `model_in`,
/// `input_tokens_lt`, `input_tokens_gt`, `tag_equals`. Empty / `None`
/// fields match anything; all non-empty fields are AND-ed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteConditions {
    /// Match only if `req.model` is in this list. Empty list matches any model.
    #[serde(default)]
    pub model_in: Vec<String>,
    /// Match only if `req.input_tokens < this`.
    #[serde(default)]
    pub input_tokens_lt: Option<u32>,
    /// Match only if `req.input_tokens > this`.
    #[serde(default)]
    pub input_tokens_gt: Option<u32>,
    /// Match only if `req.tag == Some(this)`.
    #[serde(default)]
    pub tag_equals: Option<String>,
}

/// What a matching [`ProposedRoute`] does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteAction {
    /// Rewrite to this model on the same provider as the request. v1 is
    /// same-provider only — cross-provider routing lands in a follow-up
    /// because cost projection needs each provider's pricing table and we
    /// want one source of truth here.
    pub target_model: String,
    /// Override the projected cache layer (e.g. force `"l1"` projection
    /// even when the request didn't hit cache in the baseline).
    #[serde(default)]
    pub force_cache_layer: Option<String>,
}

/// Per-model pricing keyed by `"provider:model"`.
pub type PricingTable = HashMap<String, ModelPricing>;

/// USD-per-million pricing for one model. Mirrors
/// `tt_shared::pricing::ModelPricing` but without the `effective_at` field
/// — the replay engine assumes the caller has already picked the right
/// historical rate for each request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPricing {
    /// USD per 1M input tokens (non-cached).
    pub input_per_million: f64,
    /// USD per 1M output tokens.
    pub output_per_million: f64,
    /// USD per 1M cached input tokens. `None` means "no cache discount on
    /// this model"; the replay then charges cached input at the full rate.
    pub cached_input_per_million: Option<f64>,
}

/// Configuration knobs that affect projection but aren't per-route. Today
/// this carries the L1 cache TTL the [`crate::cache_projection`] module
/// uses plus the L2 (semantic) projection knobs consumed by
/// [`crate::l2_projection`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanConfig {
    /// L1 cache TTL, seconds. `None` disables L1 projection entirely.
    #[serde(default)]
    pub l1_ttl_seconds: Option<u32>,
    /// L2 cosine-similarity thresholds to evaluate in a single replay pass.
    /// Each entry produces an independent [`L2Projection`] row in the
    /// [`Aggregates::l2_projections`] sweep. Default is the
    /// `0.85 / 0.90 / 0.92 / 0.95` sensitivity sweep described in
    /// `docs/03-plan-replay-design.md` §6.2.
    #[serde(default = "default_l2_threshold_sweep")]
    pub l2_threshold_sweep: Vec<f32>,
    /// L2 cache TTL, seconds. `None` disables L2 projection entirely (the
    /// replay loop also short-circuits when no request carries an
    /// embedding).
    #[serde(default)]
    pub l2_ttl_seconds: Option<u32>,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self {
            l1_ttl_seconds: None,
            l2_threshold_sweep: default_l2_threshold_sweep(),
            l2_ttl_seconds: None,
        }
    }
}

/// The default `0.85 / 0.90 / 0.92 / 0.95` sensitivity sweep — kept as a
/// free function so it can be used as the `#[serde(default = ...)]` hook
/// for [`PlanConfig::l2_threshold_sweep`].
fn default_l2_threshold_sweep() -> Vec<f32> {
    vec![0.85, 0.90, 0.92, 0.95]
}

/// The full input to a Plan replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanInput {
    /// Stable identifier for the plan run — flows through to the result so
    /// the audit row can reference it.
    pub plan_id: Uuid,
    /// Tenant the plan is being computed for.
    pub org_id: Uuid,
    /// Inclusive lower bound of the replay window (closed interval).
    pub window_start: DateTime<Utc>,
    /// Exclusive upper bound of the replay window (open interval).
    pub window_end: DateTime<Utc>,
    /// The condensed historical telemetry rows to replay.
    pub requests: Vec<RequestLog>,
    /// The proposed routes to evaluate. Evaluated in priority-descending
    /// order; first match wins.
    pub proposed_routes: Vec<ProposedRoute>,
    /// Pricing for every model the proposed routes might rewrite to. A
    /// missing entry causes the request to be counted as "unchanged"
    /// (conservative projection — see invariant #3 in the task spec).
    pub pricing: PricingTable,
    /// Optional non-route config (cache settings, etc.).
    #[serde(default)]
    pub config: PlanConfig,
    /// Deterministic seed for the bootstrap CI computation.
    pub seed: u64,
    /// Bootstrap iterations. Default 10,000 per `docs/03-plan-replay-design.md`
    /// §8.1; tests may lower for speed.
    pub bootstrap_iterations: u32,
}

/// The Plan replay result — what gets serialized to `plan_runs` and
/// surfaced to the user in the CLI / dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    /// Echoed from the input.
    pub plan_id: Uuid,
    /// Echoed from the input.
    pub org_id: Uuid,
    /// Echoed from the input.
    pub window_start: DateTime<Utc>,
    /// Echoed from the input.
    pub window_end: DateTime<Utc>,
    /// Number of requests the replay actually evaluated.
    pub sample_size: u32,
    /// Point-estimate aggregates over the whole sample.
    pub aggregates: Aggregates,
    /// 95% confidence intervals from bootstrap resampling.
    pub confidence_intervals: ConfidenceIntervals,
    /// Per-route attribution. Sorted by `route_id` for determinism.
    pub per_route_breakdown: Vec<PerRouteBreakdown>,
    /// Human-readable warnings (small sample size, missing pricing, etc.).
    pub caveats: Vec<String>,
    /// Tier 3 LLM-judge quality score. `None` when quality scoring was not
    /// requested (the common case — replays default to projection-only).
    /// Populated by [`crate::quality::score_quality`] when the caller opts
    /// in via body logging + budget. Skipped from serialized output when
    /// `None` so existing snapshots / persisted rows stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<crate::quality::QualityResult>,
    /// The proposed routes that produced this projection, echoed from
    /// [`PlanInput::proposed_routes`]. Carried on the result so the apply
    /// path ([`crate::apply::apply_plan`]) can persist them to the Gateway
    /// routing config in the same transaction as the status flip — without
    /// this the result has no record of *what* to apply.
    ///
    /// `#[serde(default)]` so plan_runs rows persisted before this field
    /// existed (which stored only the projection output) still deserialize;
    /// they decode to an empty Vec, and applying such a row writes no routes
    /// (matching today's no-op behavior rather than crashing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed_routes: Vec<ProposedRoute>,
}

/// Point-estimate aggregates the replay produces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Aggregates {
    /// Sum of `baseline_cost_usd` across all replayed requests.
    pub total_baseline_cost_usd: f64,
    /// Sum of projected costs across all replayed requests.
    pub total_projected_cost_usd: f64,
    /// `max(0, baseline - projected)` — never negative.
    pub projected_savings_usd: f64,
    /// `projected_savings_usd / total_baseline_cost_usd * 100` (0–100).
    pub projected_savings_pct: f64,
    /// Fraction of requests projected to be cache hits, 0–1.
    pub cache_hit_rate_projected: f64,
    /// p50 latency, ms — preserved from baseline in v1 (model swap latency
    /// projection lands in a follow-up).
    pub p50_latency_ms_projected: f64,
    /// p95 latency, ms — preserved from baseline in v1.
    pub p95_latency_ms_projected: f64,
    /// Requests that matched a proposed route and were re-projected.
    pub requests_rerouted: u32,
    /// Requests that did not match any proposed route.
    pub requests_unchanged: u32,
    /// Requests that matched a route but the target model lacked a pricing
    /// entry — counted as unchanged for cost (conservative).
    pub requests_unprice_able: u32,
    /// One [`L2Projection`] per threshold in
    /// [`PlanConfig::l2_threshold_sweep`], in input order. Empty when no
    /// request in the window carries an embedding (L2 projection skipped).
    #[serde(default)]
    pub l2_projections: Vec<L2Projection>,
    /// Count of L2 hits the cache-poisoning heuristic flagged as
    /// suspicious — high similarity but historical outcomes diverged
    /// substantially (different `finish_reason` and/or `output_tokens`
    /// outside the per-request tolerance). Aggregated across the entire
    /// threshold sweep — see [`crate::l2_projection`] for the heuristic.
    #[serde(default)]
    pub l2_poisoning_candidates: u32,
}

/// 95% bootstrap CIs for the headline metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceIntervals {
    /// `(lo, hi)` for projected total savings, USD.
    pub savings_usd_95: (f64, f64),
    /// `(lo, hi)` for projected savings percentage, 0–100.
    pub savings_pct_95: (f64, f64),
    /// `(lo, hi)` for projected cache hit rate, 0–1.
    pub cache_hit_rate_95: (f64, f64),
    /// `(lo, hi)` for projected p50 request latency, milliseconds.
    /// Zero when the input has no latency observations.
    #[serde(default)]
    pub p50_latency_ms_95: (f64, f64),
    /// `(lo, hi)` for projected p95 request latency, milliseconds.
    /// Zero when the input has no latency observations.
    #[serde(default)]
    pub p95_latency_ms_95: (f64, f64),
}

/// One row of the per-route attribution table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerRouteBreakdown {
    /// Route the bucket belongs to.
    pub route_id: Uuid,
    /// Echoed `name` for human-readable rendering.
    pub route_name: String,
    /// Count of requests matched by this route.
    pub matched: u32,
    /// Sum of baseline costs for matched requests.
    pub baseline_cost_usd: f64,
    /// Sum of projected costs for matched requests.
    pub projected_cost_usd: f64,
    /// `max(0, baseline - projected)`.
    pub savings_usd: f64,
}

/// Result of projecting L1 cache hits over a request window. See
/// [`crate::cache_projection::project_l1_hits`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheProjection {
    /// Total requests considered.
    pub total: u32,
    /// Projected L1 hits within the configured TTL window.
    pub projected_l1_hits: u32,
    /// `projected_l1_hits / total` (0–1, 0 when `total == 0`).
    pub projected_l1_hit_rate: f64,
}

/// One row of the L2 (semantic) cache sensitivity sweep. Produced by
/// [`crate::l2_projection::project_l2_hits`] — one per threshold in
/// [`PlanConfig::l2_threshold_sweep`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct L2Projection {
    /// Cosine-similarity threshold this row was computed at.
    pub threshold: f32,
    /// Total requests considered (only requests carrying an `embedding`
    /// participate — others are skipped before counting).
    pub total: u32,
    /// Number of requests projected to hit the L2 cache at this threshold.
    pub projected_l2_hits: u32,
    /// `projected_l2_hits / total` (0–1, 0 when `total == 0`).
    pub projected_l2_hit_rate: f64,
}

/// The complete output of one L2 sweep pass — the per-threshold rows plus
/// the poisoning-candidate count aggregated across all thresholds.
#[derive(Debug, Clone, Default)]
pub struct L2SweepResult {
    /// One [`L2Projection`] per requested threshold, in input order.
    pub per_threshold: Vec<L2Projection>,
    /// Aggregate count of L2 hits the poisoning heuristic flagged across
    /// the entire sweep — see [`crate::l2_projection`] for the rules.
    pub poisoning_candidates: u32,
}

/// `(provider, model)` tuple — handy for the few places that produce a
/// pricing key. Free function lives next to the type used by callers.
#[must_use]
pub fn pricing_key(provider: &str, model: &str) -> String {
    format!("{provider}:{model}")
}
