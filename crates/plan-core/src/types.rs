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
    /// The L2 task class this request belongs to — used by the L2 projection to
    /// break down per-class effectiveness. Defaults to
    /// [`L2TaskClass::ChatCompletions`] for the v1 chat-completions corpus and
    /// for older serialized rows that predate this field.
    #[serde(default)]
    pub task_class: L2TaskClass,
    /// Measured output-diff savings, USD, that the gateway REALIZED on this
    /// request at serve time (research Phase 3.4 — reconstructed-artifact minus
    /// billed-patch tokens, the `request_logs.diff_saved_usd` column). Replay
    /// cannot forward-project diff for traffic that didn't use it (the row
    /// carries no prior artifact to re-derive a patch from); this carries the
    /// ALREADY-realized number so the Plan can attribute per-lever ROI for a
    /// `diff` route. It is already reflected in this row's
    /// `baseline_cost_usd - cost_usd`, so the Plan surfaces it as a caveat and
    /// never folds it on top (double-count guard). `None`/absent for rows that
    /// predate the column or were served without diff. Skipped from JSON when
    /// `None` so existing snapshots / persisted inputs stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_saved_usd: Option<f64>,
    /// Estimated minified-JSON whitespace savings, USD (research Phase 3.1 —
    /// the gateway's pretty-vs-emitted re-tokenize estimate, the
    /// `request_logs.minify_saved_est_usd` column). A LABELED ESTIMATE, never
    /// invoice-reconciled: the Plan surfaces it as an estimated caveat for a
    /// `minify_json` route and NEVER folds it into the projected-savings
    /// headline (mirroring the runtime contract). `None`/absent when not
    /// estimated. Skipped from JSON when `None` (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minify_saved_est_usd: Option<f64>,
}

/// The L2 task class of a replayed request — the plan-core mirror of the
/// gateway's `JudgeTaskClass` / `tt_cache::TaskClass`, kept here so the Plan can
/// report L2 effectiveness per class without depending on those crates.
///
/// `#[non_exhaustive]` so additional classes can be added without breaking
/// downstream match arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum L2TaskClass {
    /// `POST /v1/chat/completions` — the only class in the v1 corpus.
    ChatCompletions,
}

impl Default for L2TaskClass {
    fn default() -> Self {
        Self::ChatCompletions
    }
}

impl L2TaskClass {
    /// Stable lowercase wire string (`chat_completions`), matching the serde
    /// representation — handy for keying report rows / telemetry.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            L2TaskClass::ChatCompletions => "chat_completions",
        }
    }
}

/// Per-task-class L2 effectiveness, aggregated across the whole threshold
/// sweep. Lets the Plan answer "how well does the L2 cache work for this class
/// of request?" — hits vs. considered, plus the poisoning-candidate count.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PerClassL2Metrics {
    /// The task class these metrics are for.
    pub task_class: L2TaskClass,
    /// Requests of this class that carried an embedding (the L2-eligible
    /// denominator), summed across every threshold in the sweep.
    pub considered: u32,
    /// Projected L2 hits for this class, summed across every threshold.
    pub hits: u32,
    /// Projected L2 misses for this class (`considered - hits`), summed across
    /// every threshold.
    pub misses: u32,
    /// Count of **distinct** requests of this class that the poisoning
    /// heuristic flagged at any threshold (deduped across the sweep, mirroring
    /// the top-level aggregate).
    pub poisoning_candidates: u32,
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
    /// Mirror of `tt_routing::RouteConditions::has_images`. Not evaluable in
    /// replay (RequestLog records no modality) — see `routing::matches_conditions`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_images: Option<bool>,
    /// Mirror of `tt_routing::RouteConditions::has_audio`. See `has_images`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_audio: Option<bool>,
    /// Mirror of `tt_routing::RouteConditions::prompt_contains_any_of`. Replay
    /// matches against `RequestLog.body` when present, else conservative no-match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_contains_any_of: Vec<String>,
    /// Mirror of `tt_routing::RouteConditions::estimated_cost_gt`. Evaluated
    /// against `RequestLog.baseline_cost_usd` (the request's logged cost on its
    /// original model) — accurately projectable, unlike modality/topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_gt: Option<f64>,
    /// Mirror of `tt_routing::RouteConditions::estimated_cost_lt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_lt: Option<f64>,
    /// Mirror of `tt_routing::RouteConditions::upstream_latency_ms_p95_gt`.
    /// Carried for lossless wire round-trip only — NOT projectable in replay:
    /// the gateway's live in-process p95 window does not exist for historical
    /// `RequestLog` rows. Like the modality mirrors, a route carrying this
    /// condition is treated as a conservative non-match in
    /// `routing::matches_conditions`, so Plan never over-projects savings on a
    /// latency route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_latency_ms_p95_gt: Option<u32>,
}

/// What a matching [`ProposedRoute`] does.
///
/// Field order matches `tt_routing::RouteAction` so the two types produce
/// identical JSON for the same logical value — a requirement for the Plan
/// apply round-trip (a `ProposedRoute` serialized by the Plan engine and then
/// deserialized by the Gateway must carry all fields without loss or
/// reordering). The `flex` flag IS mirrored (COST-7) — it is a real ~50% rate
/// cut the replay PROJECTS (see [`crate::cost::project_flex_cost`]), so it must
/// round-trip. KNOWN EXCEPTION: the gateway's `compress` flag is still not
/// mirrored here (pre-existing drift; it is false-omitted on the wire, so
/// identity holds whenever it is unset). The
/// `route_action_cross_type_lockstep_guard` test below fails to compile when
/// `tt_routing::RouteAction` grows a field, so any future addition must
/// explicitly decide the mirror question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteAction {
    /// Rewrite to this model. May target a different provider than the request
    /// (V3d-1). Cost projection resolves the target's own provider from the
    /// pricing table; a target absent from the table is counted unchanged.
    pub target_model: String,
    /// Ordered fallback model ids, mirroring `tt_routing::RouteAction::fallbacks`.
    /// The replay engine does not yet simulate fallback firing (follow-up:
    /// extend the replay matcher to account for fallback paths); the field
    /// is present so a `tt_routing::RouteAction` round-trips losslessly to a
    /// `tt_plan_core::RouteAction` without dropping fallbacks on read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
    /// Mirror of `tt_routing::RouteAction::disable_cache`. The replay engine does
    /// not yet model cache opt-out (follow-up); present for lossless round-trip.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_cache: bool,
    /// Mirror of `tt_routing::RouteAction::max_cost_usd`. A matched request whose
    /// projected cost exceeds this would be rejected at runtime — replay counts
    /// it unchanged (no savings) and surfaces a caveat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// Mirror of `tt_routing::RouteAction::flex` — opt the matched request into
    /// OpenAI's Flex service tier (`service_tier="flex"`), billed at ~50% of
    /// standard. UNLIKE the advisory Batch Lane, the gateway applies Flex
    /// in-band and the discount is REALIZED, so replay PROJECTS it: a flex
    /// route whose served model carries a catalog Flex rate is priced at that
    /// rate via [`crate::cost::project_flex_cost`] (the exact shape/precision of
    /// `project_batch_cost`), the delta folds into projected savings, and a
    /// per-run caveat counts the affected requests. A served model with no Flex
    /// rate projects NO discount (never a fabricated 0.5×) and is caveated as
    /// inapplicable — mirroring the gateway's `flex_not_applied:<model>`
    /// behavior. Same serde gating as the gateway side: omitted when false
    /// (back-compat — existing plan JSON/snapshots stay byte-identical).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flex: bool,
    /// Mirror of `tt_routing::RouteAction::batch`. Replay PROJECTS the
    /// discount the Batch Lane would deliver (target priced at its catalog
    /// batch rate — see [`crate::cost::project_batch_cost`]) and surfaces a
    /// per-run caveat counting the affected requests — the live gateway is
    /// synchronous today and does NOT realize this discount (advisory marker;
    /// the async Batch Lane is a later phase). Same serde gating as the
    /// gateway side (omitted when false).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub batch: bool,
    /// Mirror of `tt_routing::RouteAction::redact`. The request-redaction
    /// guardrail is a runtime-only SAFETY transform (it strips PII/secrets from
    /// the outbound prompt); the replay engine does not model it (no cost/savings
    /// effect to project). Present so a `tt_routing::RouteAction` carrying
    /// `redact` round-trips losslessly to a `tt_plan_core::RouteAction` without
    /// dropping the flag on read. Same serde gating as the gateway side: omitted
    /// from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redact: bool,
    /// Mirror of `tt_routing::RouteAction::format_switch` (research Phase
    /// 3.3 — opt-in CSV/bare emission instead of verbose JSON). Carried for
    /// lossless wire round-trip; replay does not model output shaping (no
    /// projection — savings for this lever are ESTIMATED at runtime only and
    /// never fold into invoice-reconciled figures). Same serde gating as the
    /// gateway side: omitted from JSON when `None` (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_switch: Option<String>,
    /// Mirror of `tt_routing::RouteAction::diff` (research Phase 3.4 —
    /// opt-in anchored search/replace patch responses with fail-closed full
    /// re-emit). Carried for lossless wire round-trip; replay does not model
    /// output shaping (no projection — savings for this lever are REALIZED
    /// at runtime from real tokenizer counts, but a historical `RequestLog`
    /// row carries no prior artifact to re-derive them from). Same serde
    /// gating as the gateway side: omitted from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub diff: bool,
    /// Mirror of `tt_routing::RouteAction::traffic_pct`. A canary split — when
    /// `Some(pct)`, only a deterministic `pct`% of matched requests take the
    /// rewrite at runtime (chosen by `tt_routing::sticky_traffic_split`); the
    /// rest pass through unchanged. The replay engine does not yet model the
    /// split (a follow-up would re-derive the per-request arm via the same sticky
    /// hash from the logged idempotency key, so projected savings match the
    /// fraction that actually routed); the field is present so a
    /// `tt_routing::RouteAction` carrying `traffic_pct` round-trips losslessly to
    /// a `tt_plan_core::RouteAction` without dropping the split on read. Omitted
    /// from JSON when `None` (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_pct: Option<u32>,
    /// Mirror of `tt_routing::RouteAction::shadow_model`. Shadow mode is a
    /// runtime-only side-channel (the gateway dispatches the candidate, discards
    /// its response, and records its cost separately); it never changes the
    /// served response, so the replay engine does not model it. Present so a
    /// `tt_routing::RouteAction` carrying `shadow_model` round-trips losslessly to
    /// a `tt_plan_core::RouteAction` without dropping it on read. Omitted from
    /// JSON when `None` (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_model: Option<String>,
    /// Mirror of `tt_routing::RouteAction::auto_pause`. The quality circuit
    /// breaker is a runtime-only guardrail (the gateway sticky-pauses the
    /// route when its live paired-judge pass-rate regresses below the floor);
    /// replay has no live verdict stream, so the engine does not simulate
    /// pauses — it treats the route as always active and projects the route's
    /// FULL savings (the same not-yet-modeled assumption it already makes for
    /// `traffic_pct`; a real pause can only shrink realized savings, never
    /// change quality). Present so a `tt_routing::RouteAction` carrying
    /// `auto_pause` round-trips losslessly to a `tt_plan_core::RouteAction`
    /// without dropping it on read. Omitted from JSON when false
    /// (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_pause: bool,
    /// Mirror of `tt_routing::RouteAction::pause_floor_pass_rate`. See
    /// `auto_pause`. Omitted from JSON when `None` (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_floor_pass_rate: Option<f64>,
    /// Mirror of `tt_routing::RouteAction::pause_min_verdicts`. See
    /// `auto_pause`. Omitted from JSON when `None` (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_min_verdicts: Option<u32>,
    /// Mirror of `tt_routing::RouteAction::minify_json`. Carried, NOT modeled:
    /// the per-request minify saving is a runtime ESTIMATE grounded in the
    /// actual emitted JSON (pretty re-render re-tokenized with the served
    /// model's tokenizer), which is not derivable from a historical
    /// `RequestLog` row — so replay projects nothing for it (like
    /// `redact`/`shadow_model`). Present so a `tt_routing::RouteAction`
    /// carrying `minify_json` round-trips losslessly. Omitted from JSON when
    /// false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub minify_json: bool,
    /// Mirror of `tt_routing::RouteAction::reasoning_max_effort`. Carried, NOT
    /// modeled: the reasoning-token cap books $0 at runtime (unspent thinking
    /// tokens are only statistically visible) and `RequestLog` carries no
    /// reasoning-spend signal to replay against. Omitted when `None`
    /// (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_max_effort: Option<String>,
    /// Mirror of `tt_routing::RouteAction::reasoning_budget_tokens`. Carried,
    /// NOT modeled — see `reasoning_max_effort`. Omitted when `None`
    /// (back-compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget_tokens: Option<u32>,
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
    /// USD per 1M batch (async Batch API) input tokens. `None` = no batch
    /// tier; a batch-eligibility route targeting such a model projects NO
    /// discount (never a fabricated 0.5×). Skipped from JSON when `None` so
    /// existing snapshots / persisted plan inputs stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_input_per_million: Option<f64>,
    /// USD per 1M batch (async Batch API) output tokens. See
    /// [`Self::batch_input_per_million`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_output_per_million: Option<f64>,
    /// USD per 1M input tokens under OpenAI's **Flex** service tier
    /// (`service_tier="flex"`), mirroring `tt_shared::pricing::ModelPricing`.
    /// `None` = no Flex tier; a `flex` route targeting such a model projects NO
    /// discount (never a fabricated 0.5×). PRESENCE is the eligibility gate.
    /// Skipped from JSON when `None` so existing snapshots / persisted plan
    /// inputs stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_input_per_million: Option<f64>,
    /// USD per 1M output tokens under the Flex service tier. See
    /// [`Self::flex_input_per_million`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flex_output_per_million: Option<f64>,
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
    /// Per-[`L2TaskClass`] L2 effectiveness breakdown. Empty when L2 projection
    /// was skipped (no embeddings). `#[serde(default)]` keeps older plan_runs
    /// rows / snapshots byte-identical (they decode to an empty Vec).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub l2_per_class: Vec<PerClassL2Metrics>,
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
    /// Count of L2 hits the cache-poisoning heuristic flagged as suspicious
    /// **at this threshold** — high similarity but historical outcomes
    /// diverged. Reported per-threshold so the risk is shown against the
    /// threshold that produced it (a higher threshold typically yields
    /// fewer, tighter matches and so fewer candidates). See
    /// [`crate::l2_projection`] for the heuristic.
    #[serde(default)]
    pub poisoning_candidates: u32,
}

/// The complete output of one L2 sweep pass — the per-threshold rows plus
/// the deduplicated poisoning-candidate count across the whole sweep.
#[derive(Debug, Clone, Default)]
pub struct L2SweepResult {
    /// One [`L2Projection`] per requested threshold, in input order. Each
    /// row carries its own [`L2Projection::poisoning_candidates`].
    pub per_threshold: Vec<L2Projection>,
    /// Count of **distinct** requests the poisoning heuristic flagged at
    /// **any** threshold in the sweep. A request that poisons at several
    /// thresholds is counted once — this is deliberately NOT the sum of the
    /// per-threshold counts (which would inflate the metric up to N× for an
    /// N-threshold sweep). See [`crate::l2_projection`] for the rules.
    pub poisoning_candidates: u32,
    /// Per-[`L2TaskClass`] effectiveness breakdown, sorted by class for
    /// determinism. Empty when no request carried an embedding. Lets the Plan
    /// surface L2 hit/miss/poisoning metrics by request class.
    pub per_class: Vec<PerClassL2Metrics>,
}

/// `(provider, model)` tuple — handy for the few places that produce a
/// pricing key. Free function lives next to the type used by callers.
#[must_use]
pub fn pricing_key(provider: &str, model: &str) -> String {
    format!("{provider}:{model}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- rv-routeaction-shared-type: field-parity serde tests ---

    /// (c) Serializing a `RouteAction` with empty fallbacks produces the
    /// minimal JSON `{"target_model":"x"}` — confirming skip_serializing_if.
    #[test]
    fn route_action_minimal_serializes_without_new_fields() {
        let a = RouteAction {
            format_switch: None,
            diff: false,
            target_model: "x".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
        };
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(
            json, r#"{"target_model":"x"}"#,
            "empty fallbacks must be omitted from JSON"
        );
    }

    /// (b) Old JSON that has only `target_model` still deserializes — serde
    /// default fills in empty fallbacks.
    #[test]
    fn route_action_backward_compat_deserialize() {
        let json = r#"{"target_model":"claude-3-5-haiku"}"#;
        let a: RouteAction = serde_json::from_str(json).unwrap();
        assert_eq!(a.target_model, "claude-3-5-haiku");
        assert!(a.fallbacks.is_empty(), "fallbacks must default to empty");
    }

    /// (a) Full round-trip: a `RouteAction` with fallbacks serializes to JSON
    /// carrying them, and deserializes back with all values preserved.
    #[test]
    fn route_action_full_round_trip() {
        let original = RouteAction {
            format_switch: None,
            diff: false,
            target_model: "claude-3-5-haiku".into(),
            fallbacks: vec!["gpt-4o-mini".into(), "gemini-flash".into()],
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(
            json.contains("\"fallbacks\""),
            "fallbacks must appear in JSON: {json}"
        );
        let roundtripped: RouteAction = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.target_model, original.target_model);
        assert_eq!(roundtripped.fallbacks, original.fallbacks);
    }

    #[test]
    fn route_action_disable_cache_round_trips() {
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.disable_cache, "defaults false");
        let a = RouteAction {
            format_switch: None,
            diff: false,
            target_model: "m".into(),
            fallbacks: Vec::new(),
            disable_cache: true,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
        };
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"disable_cache\":true"));
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.disable_cache);
    }

    /// Cross-crate lossless round-trip: JSON produced by
    /// `tt_routing::RouteAction` (target_model + fallbacks) is accepted by
    /// `tt_plan_core::RouteAction` without dropping any field. The wire shapes
    /// are structurally identical.
    #[test]
    fn route_action_cross_type_wire_compat() {
        let gateway_json = r#"{"target_model":"gpt-4o-mini","fallbacks":["claude-haiku"]}"#;
        let plan_action: RouteAction = serde_json::from_str(gateway_json).unwrap();
        assert_eq!(plan_action.target_model, "gpt-4o-mini");
        assert_eq!(plan_action.fallbacks, vec!["claude-haiku"]);
        // Re-serialize: must re-produce the same JSON (declaration order).
        let reemitted = serde_json::to_string(&plan_action).unwrap();
        assert_eq!(reemitted, gateway_json);
    }

    #[test]
    fn route_action_redact_round_trips() {
        // Defaults false + omitted when absent.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.redact, "redact must default false");
        let a = RouteAction {
            format_switch: None,
            diff: false,
            target_model: "m".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            redact: true,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
        };
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            j.contains("\"redact\":true"),
            "redact=true must serialize: {j}"
        );
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.redact);
    }

    /// Cross-crate wire-compat for the `redact` guardrail flag: gateway-written
    /// JSON carrying `"redact":true` deserializes into the plan-core type with
    /// the flag set and re-emits the same wire form. Mirrors the gateway-side
    /// `route_action_redact_cross_type_wire_compat` so the two `RouteAction`
    /// shapes stay in lockstep on `redact`.
    #[test]
    fn route_action_redact_cross_type_wire_compat() {
        let gateway_json = r#"{"target_model":"gpt-4o","redact":true}"#;
        let plan_action: RouteAction = serde_json::from_str(gateway_json).unwrap();
        assert_eq!(plan_action.target_model, "gpt-4o");
        assert!(
            plan_action.redact,
            "redact must round-trip from gateway JSON"
        );
        let reemitted = serde_json::to_string(&plan_action).unwrap();
        assert_eq!(reemitted, gateway_json);
    }

    /// Cross-crate wire-compat for the advisory batch-eligibility marker:
    /// gateway-written JSON carrying `"batch":true` deserializes into the
    /// plan-core type with the flag set and re-emits the same wire form (same
    /// relative field order + skip_serializing_if gating), and the flag is
    /// omitted when false. Mirrors the gateway-side
    /// `route_action_batch_cross_type_wire_compat` so the two `RouteAction`
    /// shapes stay in lockstep on `batch`.
    #[test]
    fn route_action_batch_wire_compat_with_gateway() {
        let gateway_json = r#"{"target_model":"gpt-5.5","batch":true}"#;
        let plan_action: RouteAction = serde_json::from_str(gateway_json).unwrap();
        assert_eq!(plan_action.target_model, "gpt-5.5");
        assert!(plan_action.batch, "batch must round-trip from gateway JSON");
        let reemitted = serde_json::to_string(&plan_action).unwrap();
        assert_eq!(reemitted, gateway_json);

        // Defaults false + omitted when false (back-compat for persisted plans).
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.batch, "batch must default false");
        let j = serde_json::to_string(&parsed).unwrap();
        assert!(!j.contains("batch"), "batch=false must be omitted: {j}");
    }

    /// Cross-crate wire-compat for the Flex service-tier flag (COST-7):
    /// gateway-written JSON carrying `"flex":true` deserializes into the
    /// plan-core type with the flag set and re-emits the same wire form (same
    /// relative field order + skip_serializing_if gating), and the flag is
    /// omitted when false. Mirrors `route_action_batch_wire_compat_with_gateway`
    /// so the two `RouteAction` shapes stay in lockstep on `flex`.
    #[test]
    fn route_action_flex_wire_compat_with_gateway() {
        let gateway_json = r#"{"target_model":"gpt-5.5","flex":true}"#;
        let plan_action: RouteAction = serde_json::from_str(gateway_json).unwrap();
        assert_eq!(plan_action.target_model, "gpt-5.5");
        assert!(plan_action.flex, "flex must round-trip from gateway JSON");
        let reemitted = serde_json::to_string(&plan_action).unwrap();
        assert_eq!(reemitted, gateway_json);

        // Defaults false + omitted when false (back-compat for persisted plans).
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.flex, "flex must default false");
        let j = serde_json::to_string(&parsed).unwrap();
        assert!(!j.contains("flex"), "flex=false must be omitted: {j}");
    }

    /// Cross-crate wire-compat for the canary fields (`traffic_pct` +
    /// `shadow_model`): gateway-written JSON carrying both deserializes into the
    /// plan-core type with the values set and re-emits the same wire form (same
    /// field order + skip_serializing_if gating). Mirrors the gateway-side
    /// `route_action_canary_cross_type_wire_compat` so the two `RouteAction`
    /// shapes stay in lockstep on the canary fields.
    #[test]
    fn route_action_canary_cross_type_wire_compat() {
        let gateway_json =
            r#"{"target_model":"gpt-4o","traffic_pct":30,"shadow_model":"claude-haiku-4-5"}"#;
        let plan_action: RouteAction = serde_json::from_str(gateway_json).unwrap();
        assert_eq!(plan_action.target_model, "gpt-4o");
        assert_eq!(plan_action.traffic_pct, Some(30));
        assert_eq!(
            plan_action.shadow_model.as_deref(),
            Some("claude-haiku-4-5")
        );
        let reemitted = serde_json::to_string(&plan_action).unwrap();
        assert_eq!(reemitted, gateway_json);
    }

    /// Canary fields default to `None` and are omitted from JSON when `None`.
    #[test]
    fn route_action_canary_fields_default_none_and_omit() {
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert_eq!(parsed.traffic_pct, None);
        assert_eq!(parsed.shadow_model, None);
        let j = serde_json::to_string(&parsed).unwrap();
        assert!(!j.contains("traffic_pct"), "{j}");
        assert!(!j.contains("shadow_model"), "{j}");
    }

    /// Cross-crate wire-compat for the auto-pause config (`auto_pause` +
    /// `pause_floor_pass_rate` + `pause_min_verdicts`): gateway-written JSON
    /// carrying all three deserializes into the plan-core type with the
    /// values set and re-emits the same wire form (same field order +
    /// skip_serializing_if gating), so a gateway `RouteAction` carrying
    /// auto-pause config round-trips through plan-core without loss. The
    /// fields default false/None and are omitted when default, so every
    /// existing plan JSON/snapshot stays byte-identical.
    #[test]
    fn route_action_auto_pause_cross_type_wire_compat() {
        let gateway_json = r#"{"target_model":"gpt-4o-mini","auto_pause":true,"pause_floor_pass_rate":0.85,"pause_min_verdicts":30}"#;
        let plan_action: RouteAction = serde_json::from_str(gateway_json).unwrap();
        assert!(plan_action.auto_pause);
        assert_eq!(plan_action.pause_floor_pass_rate, Some(0.85));
        assert_eq!(plan_action.pause_min_verdicts, Some(30));
        let reemitted = serde_json::to_string(&plan_action).unwrap();
        assert_eq!(reemitted, gateway_json);

        // Defaults: absent on read → false/None; default values omitted on write.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.auto_pause);
        assert_eq!(parsed.pause_floor_pass_rate, None);
        assert_eq!(parsed.pause_min_verdicts, None);
        let j = serde_json::to_string(&parsed).unwrap();
        assert!(!j.contains("pause"), "{j}");
    }

    /// Cross-crate wire-compat for the output-shaping levers (research Phase
    /// 3.3 + 3.4): gateway-written JSON carrying `format_switch` or `diff`
    /// deserializes into the plan-core type with the value set and re-emits
    /// the same wire form byte-for-byte (same relative field order + same
    /// skip_serializing_if gating). The fields default `None`/false and are
    /// omitted when default, so every existing plan JSON/snapshot stays
    /// byte-identical (zero replay drift).
    #[test]
    fn route_action_output_shaping_cross_type_wire_compat() {
        let csv_json = r#"{"target_model":"m","format_switch":"csv"}"#;
        let a: RouteAction = serde_json::from_str(csv_json).unwrap();
        assert_eq!(a.format_switch.as_deref(), Some("csv"));
        assert_eq!(serde_json::to_string(&a).unwrap(), csv_json);

        let diff_json = r#"{"target_model":"m","diff":true}"#;
        let d: RouteAction = serde_json::from_str(diff_json).unwrap();
        assert!(d.diff);
        assert_eq!(serde_json::to_string(&d).unwrap(), diff_json);

        // Defaults: absent on read → None/false; defaults omitted on write.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert_eq!(parsed.format_switch, None);
        assert!(!parsed.diff);
        let j = serde_json::to_string(&parsed).unwrap();
        assert!(!j.contains("format_switch") && !j.contains("diff"), "{j}");
    }

    /// Cross-crate lockstep guard for `RouteAction`, the companion to
    /// `route_conditions_cross_type_wire_compat`. The
    /// `tt_routing::RouteAction` literal below sets EVERY field with NO
    /// `..Default::default()` on purpose: adding a field to
    /// `tt_routing::RouteAction` makes this test fail to compile, forcing
    /// whoever adds it to decide the mirror question here instead of letting
    /// `tt_plan_core::RouteAction` silently drop it on read (the drift this
    /// branch had to repair for the auto-pause fields).
    ///
    /// `flex` IS now mirrored (COST-7) and set TRUE here so the round-trip
    /// exercises it. KNOWN REMAINING DRIFT: `compress` is NOT mirrored in
    /// `tt_plan_core::RouteAction` (pre-existing); it is set `false` here, where
    /// its skip_serializing_if gating omits it from the wire form, keeping the
    /// byte-identity assertion honest for every mirrored field. The
    /// output-shaping levers (`format_switch` / `diff`, research Phase 3.3 +
    /// 3.4) ARE mirrored — round-trip-only, not projected by replay.
    #[test]
    fn route_action_cross_type_lockstep_guard() {
        let gateway = tt_routing::RouteAction {
            batch: false,
            target_model: "gpt-4o-mini".to_string(),
            fallbacks: vec!["claude-haiku-4-5".to_string()],
            disable_cache: true,
            max_cost_usd: Some(0.25),
            flex: true,      // mirrored (COST-7) — round-trips to plan-core
            compress: false, // not mirrored (pre-existing) — omitted when false
            redact: true,
            format_switch: Some("csv".to_string()),
            diff: false, // mutually exclusive with format_switch on a real route
            traffic_pct: Some(30),
            shadow_model: Some("gemini-flash".to_string()),
            auto_pause: true,
            pause_floor_pass_rate: Some(0.9),
            pause_min_verdicts: Some(25),
            minify_json: true,
            reasoning_max_effort: Some("low".to_string()),
            reasoning_budget_tokens: Some(8192),
            // not mirrored (route-grained shaping mode, not a replay-projected
            // effect) — `None` is omitted from the wire, keeping byte-identity.
            agentic_budget: None,
        };
        let gateway_json = serde_json::to_string(&gateway).unwrap();

        // The gateway JSON deserializes into the plan-core type without
        // dropping any mirrored field…
        let plan_action: RouteAction = serde_json::from_str(&gateway_json).unwrap();
        assert_eq!(plan_action.target_model, "gpt-4o-mini");
        assert_eq!(plan_action.fallbacks, vec!["claude-haiku-4-5"]);
        assert!(plan_action.disable_cache);
        assert_eq!(plan_action.max_cost_usd, Some(0.25));
        assert!(plan_action.flex, "flex must round-trip from gateway JSON");
        assert!(plan_action.redact);
        assert_eq!(plan_action.format_switch.as_deref(), Some("csv"));
        assert!(!plan_action.diff);
        assert_eq!(plan_action.traffic_pct, Some(30));
        assert_eq!(plan_action.shadow_model.as_deref(), Some("gemini-flash"));
        assert!(plan_action.auto_pause);
        assert_eq!(plan_action.pause_floor_pass_rate, Some(0.9));
        assert_eq!(plan_action.pause_min_verdicts, Some(25));
        assert!(plan_action.minify_json);
        assert_eq!(plan_action.reasoning_max_effort.as_deref(), Some("low"));
        assert_eq!(plan_action.reasoning_budget_tokens, Some(8192));

        // …and re-emits the gateway wire form byte-for-byte (same declaration
        // order + same skip_serializing_if gating).
        let reemitted = serde_json::to_string(&plan_action).unwrap();
        assert_eq!(reemitted, gateway_json);
    }

    /// Cross-crate lockstep guard for `RouteConditions`, the companion to
    /// `route_action_cross_type_wire_compat`. The two structs are field-identical
    /// by hand; this test fails if they ever drift.
    ///
    /// The `tt_routing::RouteConditions { .. }` literal below sets EVERY field
    /// with NO `..Default::default()` on purpose: adding a condition predicate to
    /// `tt_routing::RouteConditions` makes this test fail to compile (missing
    /// field), forcing whoever adds it to extend the round-trip here and notice
    /// if `tt_plan_core::RouteConditions` would silently drop it. The runtime
    /// assertions then prove the wire shapes (declaration order +
    /// `skip_serializing_if` gating) are byte-for-byte identical on both sides.
    #[test]
    fn route_conditions_cross_type_wire_compat() {
        let gateway = tt_routing::RouteConditions {
            model_in: vec!["gpt-4o".to_string()],
            input_tokens_lt: Some(1000),
            input_tokens_gt: Some(10),
            tag_equals: Some("batch".to_string()),
            has_images: Some(true),
            has_audio: Some(false),
            prompt_contains_any_of: vec!["summarize".to_string()],
            estimated_cost_gt: Some(0.01),
            estimated_cost_lt: Some(5.0),
            upstream_latency_ms_p95_gt: Some(1500),
        };
        let gateway_json = serde_json::to_string(&gateway).unwrap();

        // The gateway JSON deserializes into the plan-core type without dropping
        // a field.
        let plan_conditions: RouteConditions = serde_json::from_str(&gateway_json).unwrap();
        assert_eq!(plan_conditions.model_in, vec!["gpt-4o".to_string()]);
        assert_eq!(plan_conditions.input_tokens_lt, Some(1000));
        assert_eq!(plan_conditions.input_tokens_gt, Some(10));
        assert_eq!(plan_conditions.tag_equals, Some("batch".to_string()));
        assert_eq!(plan_conditions.has_images, Some(true));
        assert_eq!(plan_conditions.has_audio, Some(false));
        assert_eq!(
            plan_conditions.prompt_contains_any_of,
            vec!["summarize".to_string()]
        );
        assert_eq!(plan_conditions.estimated_cost_gt, Some(0.01));
        assert_eq!(plan_conditions.estimated_cost_lt, Some(5.0));
        assert_eq!(plan_conditions.upstream_latency_ms_p95_gt, Some(1500));

        // Re-serialize the plan-core value: must reproduce the gateway JSON
        // byte-for-byte (same field order + same skip_serializing_if gating).
        let reemitted = serde_json::to_string(&plan_conditions).unwrap();
        assert_eq!(reemitted, gateway_json);
    }

    /// Back-compat: legacy JSON still carrying the removed `force_cache_layer`
    /// key deserializes fine (serde ignores the unknown field) and re-serializes
    /// without it. Guards persisted plans/routes written before the removal.
    #[test]
    fn route_action_legacy_force_cache_layer_is_ignored() {
        let legacy = r#"{"target_model":"gpt-4o-mini","fallbacks":["x"],"force_cache_layer":"l2"}"#;
        let a: RouteAction = serde_json::from_str(legacy).unwrap();
        assert_eq!(a.target_model, "gpt-4o-mini");
        assert_eq!(a.fallbacks, vec!["x"]);
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            !j.contains("force_cache_layer"),
            "obsolete key must not be re-emitted: {j}"
        );
    }
}
