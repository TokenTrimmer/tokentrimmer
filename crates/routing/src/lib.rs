//! Routing engine — match incoming requests against per-org rules to pick a
//! target model (or pass through unchanged).
//!
//! Mirrors the shape used by `tt-plan-core`'s replay-time matcher so a Plan
//! projection and the live Gateway agree on which route would fire for a
//! given request. Differences from plan-core:
//!
//! - Input is the canonical [`ChatCompletionRequest`] + [`RequestContext`]
//!   (live runtime), not a historical `RequestLog`.
//! - Token-count conditions use `input_tokens` estimated from the request
//!   (caller supplies; the engine never tokenizes itself — that's a hot-path
//!   responsibility owned by the caller's tokenizer cache).
//!
//! Rules are stored sorted descending by priority. First match wins.

pub mod cache;
pub mod catalog;
pub mod contract;
pub mod latency;
mod matcher;
pub mod store;
pub mod validate;

pub use cache::CachingRoutingStore;
pub use contract::{
    canonicalize_route_parts, canonicalize_route_value, CanonicalRoute, RouteValidationIssue,
    RouteWriteRequest, ROUTE_SCHEMA_ID, ROUTE_SCHEMA_VERSION,
};
pub use latency::{LatencyTracker, MIN_SAMPLES as LATENCY_MIN_SAMPLES};
pub use matcher::{
    evaluate_route_conditions, route_conditions_match, RouteConditionDecision,
    RouteConditionEvaluation, RouteConditionField, RouteConditionOutcome, RouteFeatureEvidence,
    RouteFeatureSnapshot,
};
#[cfg(feature = "postgres")]
pub use store::PostgresRoutingStore;
pub use store::{
    InMemoryRoutingStore, NewRoute, NewRoutePause, PausedBy, RouteManagementActivation,
    RouteManagementView, RoutingStore, RoutingStoreError,
};
pub use validate::{
    validate_agentic_budget, validate_auto_pause, validate_capability, validate_output_shaping,
    validate_panel, validate_route_has_effect, validate_shadow_model, validate_workflow,
    ValidationError, PANEL_STRATEGY_VALUES, PAUSE_MIN_VERDICTS_MAX,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use tt_shared::{ChatCompletionRequest, RequestContext};

/// A single routing rule. When [`Route::when`] matches the request, the
/// caller rewrites `request.model` to [`Route::then::target_model`] (and may
/// observe the [`Route::id`] for telemetry attribution).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Stable id — used in `request_logs.matched_route_id` for attribution.
    pub id: Uuid,
    /// Human-readable name, surfaced in dashboards.
    pub name: String,
    /// Higher value wins on tie-breaker; engine evaluates descending.
    pub priority: u32,
    /// Disabled routes never match.
    pub enabled: bool,
    /// AND-ed match conditions. Empty / `None` fields match anything.
    pub when: RouteConditions,
    /// What to do when matched.
    pub then: RouteAction,
    /// Whether a sticky pause currently suppresses this route's rewrite. A
    /// paused route still MATCHES (attribution + telemetry marker) but every
    /// cost lever is disabled — requests flow to the originally-requested
    /// model (the EXPENSIVE, quality-safe direction) until an explicit
    /// resume (`POST /v1/routes/:id/resume?expected_revision=N`). Populated by the store
    /// (`route_pauses` LEFT JOIN / in-memory pause map), never written by
    /// callers; false-omitted keeps `/v1/routes` JSON + any fixtures
    /// byte-stable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
}

/// A route together with the immutable version-ledger record that described
/// this exact runtime definition when it was loaded.
///
/// `route_version_id` is deliberately separate from [`Route`]: the latter is
/// the stable public routing contract, while this is gateway-only execution
/// provenance. It is the `BIGINT` primary key of the cloud-owned
/// `public.route_versions` append-only ledger, **never** the mutable
/// `routes.revision` concurrency token. `None` truthfully represents a
/// legacy/unavailable ledger rather than fabricating a version from revision.
#[derive(Debug, Clone)]
pub struct RuntimeRoute {
    pub route: Route,
    pub route_version_id: Option<i64>,
}

impl RuntimeRoute {
    /// Wrap a route whose immutable ledger provenance is unavailable.
    #[must_use]
    pub fn unversioned(route: Route) -> Self {
        Self {
            route,
            route_version_id: None,
        }
    }
}

/// Match conditions for a [`Route`]. Keep these fields in lockstep with
/// [`RouteConditionField`] and the published route-preview coverage corpus.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RouteConditions {
    /// Match only if `req.model` is in this list. Empty list matches any model.
    #[serde(default)]
    pub model_in: Vec<String>,
    /// Match only if estimated `input_tokens < this`.
    #[serde(default)]
    pub input_tokens_lt: Option<u32>,
    /// Match only if estimated `input_tokens > this`.
    #[serde(default)]
    pub input_tokens_gt: Option<u32>,
    /// Match only if `ctx.tag == Some(this)`.
    #[serde(default)]
    pub tag_equals: Option<String>,
    /// Match only if the request carries at least one image input part
    /// (`ContentPart::ImageUrl`). `Some(false)` requires no image; `None` ignores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_images: Option<bool>,
    /// Match only if the request carries at least one audio input part
    /// (`ContentPart::InputAudio`). `Some(false)` requires no audio; `None` ignores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_audio: Option<bool>,
    /// Match only if the request carries at least one document input part
    /// (`ContentPart::Document`) — the Document Lane signal (D4a). `Some(false)`
    /// requires no document; `None` ignores. UNLIKE `has_images`/`has_audio`,
    /// this does NOT require a Vision-capable target: a document route targets a
    /// TEXT model (the pre-routing seam distills the document to text before
    /// dispatch), so it is deliberately absent from the `needs_vision` gate in
    /// `validate::validate_capability`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_documents: Option<bool>,
    /// Match only if the request's DOMINANT content kind (the classified kind of
    /// its largest text block — see
    /// [`tt_shared::capability_check::request_dominant_content_kind`]) equals this
    /// lowercase [`ContentKind`](tt_shared::content_kind::ContentKind) string
    /// (`"json"`, `"csv"`, `"log"`, `"code"`, `"diff"`, `"prose"`) — the
    /// content-aware compression routing signal (P1a). Lets a route target, e.g.,
    /// `content_type=code` to opt code-dominant traffic into a compressor. `None`
    /// ignores. A request with no block large enough to classify never matches a
    /// `content_type` condition (an unclassifiable request is not "of" any kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Match if the request's user+system text contains ANY of these keywords
    /// (case-insensitive substring). Empty = ignore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_contains_any_of: Vec<String>,
    /// Match only if the request's estimated cost (USD) is greater than this.
    /// Unknown cost (caller passed `None`) never matches a cost condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_gt: Option<f64>,
    /// Match only if the request's estimated cost (USD) is less than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_lt: Option<f64>,
    /// Match only when the *live, gateway-observed* p95 upstream latency (in
    /// milliseconds) for the originally-requested `(provider, model)` exceeds
    /// this threshold — letting a route shift traffic off a primary that is
    /// currently slow.
    ///
    /// # In-process-window + cold-start behavior (READ THIS)
    ///
    /// The signal is the gateway's OWN bounded rolling latency window
    /// ([`crate::LatencyTracker`]), NOT any cloud/`request_logs` data — those are
    /// unavailable at decision time. The window is per-instance and reflects only
    /// recent upstream behavior this replica observed. Consequently:
    ///
    /// - The condition is TRUE only when there are **enough samples**
    ///   (`>= LATENCY_MIN_SAMPLES`) AND the observed p95 strictly exceeds the
    ///   threshold — i.e. a genuinely slow primary triggers the alternate route.
    /// - The condition is **FALSE on cold start / insufficient data**: with no
    ///   tracker supplied, or fewer than the minimum samples for the key, an
    ///   unknown primary does NOT match. This is deliberate — the feature must
    ///   never gate on a fabricated signal. See `matches` / `LatencyTracker::p95`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_latency_ms_p95_gt: Option<u32>,
    /// Match only when the request is NOT classified as reasoning-is-the-work
    /// (Math/Code/Legal/Medical, via tt-core's reasoning_class). Used by the
    /// down-route catalog. The classification is computed in the gateway and
    /// supplied to the engine as the `is_reasoning_class` signal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_reasoning_class: bool,
}

/// What a matching [`Route`] does to the request before dispatch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RouteAction {
    /// Rewrite to this model. May target a different provider than the request
    /// (V3d-1 cross-provider routing); the target is capability-checked and
    /// dispatch/savings use the target's own provider.
    ///
    /// `None` = **modifier-only route**: keep the caller's chosen model and
    /// apply only this action's other then-effects (e.g. `agentic_budget`,
    /// `compress`). A modifier-only route MUST carry at least one effect — a
    /// route with neither a `target_model` nor any effect is a no-op mistake
    /// and is rejected at creation (`validate_route_has_effect`). Existing route
    /// JSON always carries `target_model`, so it deserializes to `Some`;
    /// omitting it yields `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_model: Option<String>,
    /// Ordered fallback model ids, tried in order when the primary dispatch
    /// fails with a fallback-eligible error (provider down / 5xx / timeout).
    /// Empty = no failover. The gateway resolves each via the registry, so a
    /// fallback may cross providers. Populated by the cloud routes schema;
    /// `#[serde(default)]` keeps older rows / payloads compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
    /// When true, a request this route matches skips L1+L2 entirely (no lookup,
    /// no insert) — for privacy/sensitive traffic that must not persist in the
    /// shared cache. Default false; omitted from JSON when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_cache: bool,
    /// Pre-dispatch estimated-cost admission limit (USD). After this route's
    /// rewrite, if the rerouted model's static estimate still exceeds this, the
    /// gateway rejects the request (402) instead of dispatching. It does not
    /// reserve or settle provider usage, so it is not a runtime or invoice
    /// ceiling. `None` = no admission limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// Opt the matched request into OpenAI's **Flex** service tier
    /// (`service_tier: "flex"`) — a synchronous-but-slower processing tier
    /// billed at ~50% of standard. The gateway sets `service_tier="flex"` on the
    /// upstream request only when the served model is flex-eligible (carries a
    /// Flex rate in the catalog); an ineligible model is left untouched and a
    /// `flex_not_applied:<model>` warning is surfaced. Savings are attributed as
    /// a distinct `flex` source (standard baseline − flex cost). Default false;
    /// omitted from JSON when false (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flex: bool,
    /// Mark matched traffic **batch-eligible** (advisory, research Phase 2.1).
    /// The provider Batch APIs price async (≤24h) traffic at ~50%; the gateway
    /// is synchronous today, so this action does NOT detour dispatch and does
    /// NOT change the bill. It: (1) tags the request_logs row batch-eligible,
    /// (2) records the FORGONE batch discount (priced from the served model's
    /// real catalog batch rate — never a hardcoded 0.5) in
    /// `batch_forgone_usd` / the `X-TokenTrimmer-Batch-Forgone-Usd` header,
    /// and (3) emits a `batch_deferred_unavailable` warning. NEVER applied to
    /// streaming or interactive (`X-TokenTrimmer-Interactive`) requests — the
    /// gateway clears the marker and warns `batch_ineligible:<reason>`.
    /// Default false; omitted from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub batch: bool,
    /// Opt the matched request into the **conservative compression pass**
    /// (request-pass pipeline, compression pass #1): a content-lossless trim of
    /// *non-prose* blocks (collapse redundant whitespace/blank lines in
    /// system/tool-result payloads, de-duplicate exactly-repeated adjacent
    /// tool-result blocks, canonicalize `tool_calls` arguments JSON). User prose
    /// and the actual instruction content are never altered. The pass runs only
    /// when this is true; the removed input tokens lower the prompt-token bill
    /// and are attributed as a distinct `compression` savings source (standard
    /// baseline − compressed cost). **Off by default** — no behavior change
    /// unless a route enables it; omitted from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compress: bool,
    /// Opt the matched request into the **lossless document-compaction pass**
    /// (Document Lane D2, request-pass pipeline): a content-preserving trim of
    /// LARGE non-prose text documents (system/tool-result blocks ≥ a size
    /// threshold) — markdown-normalize (collapse ≥3 blank lines, strip trailing
    /// whitespace), strip repeated pure separator/header/footer boilerplate
    /// lines, and de-duplicate exactly-repeated multi-line blocks (keeping the
    /// first). User prose and the actual instruction content are never altered
    /// and small blocks are left untouched. Text-only, so it rides the
    /// token-true gate cleanly; the removed input tokens lower the prompt-token
    /// bill and are attributed as a distinct `doc_compaction` savings source
    /// that folds into the baseline exactly like `compress`. **Off by default**
    /// — no behavior change unless a route enables it; omitted from JSON when
    /// false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub doc_compaction: bool,
    /// Opt the matched request into the **Document Lane** (D4): the pre-routing
    /// image/document → text distillation seam. When true (and the seam is
    /// live — D4c), image/document input parts are distilled to text BEFORE
    /// routing, so `request_has_images`/`request_has_documents` flip false and a
    /// route may downgrade the request to a cheaper TEXT model; the
    /// vision-avoided saving books to the ISOLATED `doc_vision_saved_est_usd`
    /// (never the invoice-reconciled headline). Lossy substitution stays
    /// judge-gated (the `DocDistillGate` + 0.90 auto-pause floor) and fails open
    /// to the verbatim request. **Off by default** — in D4a this flag only
    /// carries opt-in intent (the seam is not wired yet); omitted from JSON when
    /// false (back-compat, mirrors `compress`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub document_lane: bool,
    /// Opt the matched request into the **content-aware compression pass**
    /// (content_compress, request-pass pipeline; Phase 1). For each LARGE
    /// non-prose text block the dispatcher classifies its
    /// [`ContentKind`](tt_shared::content_kind::ContentKind) and applies a
    /// specialized backend: in P1a, JSON / CSV / log blocks get a
    /// CONTENT-PRESERVING structural compaction (collapse insignificant JSON
    /// whitespace, collapse repeated identical log lines, trim CSV padding);
    /// Code / Prose blocks are classified but left UNTOUCHED (their backends land
    /// in P1c / P1b). It rides the pipeline's token-true gate (a token-growing
    /// result → verbatim); the measured token reduction is booked into the
    /// ISOLATED `content_compress_saved_est_usd` estimate (never the
    /// invoice-reconciled headline) + surfaced on
    /// `X-TokenTrimmer-Content-Compress-Saved-Est-Usd`. **Off by default** — no
    /// behavior change unless a route enables it; omitted from JSON when false
    /// (back-compat, mirrors `compress`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub content_compress: bool,
    /// Opt the matched request into the **request-redaction guardrail**
    /// (request-pass pipeline): a conservative SAFETY transform that replaces
    /// PII/secrets in the OUTBOUND request (user prose, system blocks,
    /// tool-result content) with a `[REDACTED]` placeholder BEFORE the request
    /// is dispatched upstream. Reuses the Tier-1 secret patterns plus email/SSN
    /// matchers and prefers over-redacting to leaking. This is **not** a savings
    /// feature: no cost/savings is attributed to redaction; the
    /// `x-tokentrimmer-warnings` header (`redacted:<class>`) is the signal that
    /// redaction fired. It redacts the upstream request, not the gateway's logs.
    /// **Off by default** — no behavior change unless a route enables it;
    /// omitted from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redact: bool,
    /// OPT-IN format switch (research Phase 3.3): instruct the model to emit
    /// `"csv"` (flat-uniform records) or `"bare"` (single value) instead of
    /// verbose JSON. **This changes the parse contract** — the caller opted in
    /// via the route, and every switched response is advertised with a
    /// `format_switch:<label>` warnings token. Wire values `"csv"` | `"bare"`
    /// are validated at route creation (`validate_output_shaping`); an unknown
    /// value at dispatch time NO-OPs with a warning (fail-open
    /// forward-compat). Eligibility is enforced in code at dispatch
    /// (schema-shape detection; strict structured output, streaming, tools,
    /// n>1 all no-op). Mutually exclusive with `diff` (rejected at creation).
    /// Default `None` — no behavior change unless a route sets it; omitted
    /// from JSON when `None` (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_switch: Option<String>,
    /// OPT-IN delta/diff responses (research Phase 3.4) for edit/iteration
    /// routes: when the prior artifact is identifiable FROM THE REQUEST
    /// (`tt_extras["diff_prior"]` echo, else the last assistant message), the
    /// gateway instructs the model to emit an anchored search/replace patch,
    /// applies + validates it, and returns the FULL reconstructed artifact
    /// (the caller's contract is preserved; the short patch is what the
    /// provider bills). ANY validation failure fails CLOSED to a full
    /// re-emit. Mutually exclusive with `format_switch` (rejected at
    /// creation). Default false; omitted from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub diff: bool,
    /// Canary traffic split (0-100). When `Some(pct)`, only a deterministic
    /// `pct`% of the matched requests are routed to `target_model` (the canary
    /// arm); the remaining `(100 - pct)%` pass through on their
    /// originally-requested model (the control arm). The arm is chosen by the
    /// replica-independent [`sticky_traffic_split`] hash so the same logical
    /// request always lands in the same arm across replicas and retries.
    ///
    /// `None` (the default) = 100% of matched requests take the route
    /// (unconditional rewrite, today's behavior). `Some(0)` = 0% canary (the
    /// route never rewrites, only its `shadow_model`/side-effects could fire).
    /// Route validation rejects values above 100; the split function still
    /// clamps them defensively for direct library callers. Omitted from JSON
    /// when `None` (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-schema", schemars(range(min = 0, max = 100)))]
    pub traffic_pct: Option<u32>,
    /// Shadow-mode candidate model. When `Some(model)`, the gateway ALSO
    /// dispatches the matched request to `model` as a **shadow** (non-streaming,
    /// single candidate, NO failover), DISCARDS the shadow response, and records
    /// the shadow's cost/usage SEPARATELY (never folded into the primary cost).
    /// Only the primary (control/canary) response is ever returned to the client.
    ///
    /// Shadow mode DOUBLES upstream spend for matched requests, so it is opt-in
    /// (default `None`) and the shadow cost is logged in its own column /
    /// span attribute for reconciliation. A `shadow_model` with no `traffic_pct`
    /// means 100% of matched requests are shadowed (the primary still serves
    /// normally). The configured `shadow_model` MUST resolve to a registered
    /// provider — config validation rejects an unresolvable shadow at route
    /// creation time (fail at config time, not silently at dispatch). Omitted
    /// from JSON when `None` (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_model: Option<String>,
    /// OPT-IN auto-pause: when true AND the paired-judge pass-rate over the
    /// recent verdict window drops below the floor (with at least
    /// `pause_min_verdicts` classified verdicts), the gateway pauses this
    /// route's rewrite (sticky; resume via
    /// `POST /v1/routes/:id/resume?expected_revision=N`).
    /// Default false — no behavior change unless a route enables it; omitted
    /// from JSON when false (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_pause: bool,
    /// Pass-rate floor as a fraction in (0, 1]. The route auto-pauses when
    /// its windowed paired pass rate drops STRICTLY below this. `None` =
    /// the gateway default (`DEFAULT_PAUSE_FLOOR_PASS_RATE` = 0.90 in
    /// tt-core). Omitted from JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_floor_pass_rate: Option<f64>,
    /// Minimum classified verdicts (acceptable + degraded) in-window before
    /// the floor can trigger. `None` = the gateway default
    /// (`DEFAULT_PAUSE_MIN_VERDICTS` = 20 in tt-core). Explicit values are
    /// bounded to the evaluator's finite 100-verdict window. Omitted from JSON
    /// when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-schema", schemars(range(min = 1, max = 100)))]
    pub pause_min_verdicts: Option<u32>,
    /// Opt matched traffic into **minified-JSON output steering** (research
    /// Phase 3.1): the gateway appends a deterministic, conditionally-phrased
    /// system-suffix instruction telling the model to emit JSON with no
    /// indentation/newlines. Lossless by construction (whitespace between JSON
    /// tokens carries no meaning); inert for non-JSON answers (the instruction
    /// is phrased "when responding with JSON"). NEVER injected when the served
    /// provider honors `response_format: json_schema` natively (grammar-locked
    /// structured output already controls whitespace — no-op + no claim,
    /// `minify_skipped:structured_output`). Savings are an ESTIMATE (own
    /// column/header, never in the invoice-reconciled headline). Default false;
    /// omitted from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub minify_json: bool,
    /// Cap OpenAI-style `reasoning_effort` for matched traffic at this level
    /// (`"low"` | `"medium"`). Lower-only: a request already at or below the
    /// cap is untouched; an absent effort on a catalog-Reasoning-capable model
    /// is treated as the provider default ("medium") and lowered when the cap
    /// is "low". HARD class gate: requests classified math/code/legal/medical
    /// are never capped (`reasoning_cap_skipped:class:*`). Books $0 — the
    /// unspent thinking tokens are only statistically visible; the event is
    /// metered and the #163 netted route savings tell the truth over the
    /// window. `None` = off (default); omitted from JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_max_effort: Option<String>,
    /// Cap Anthropic-style extended-thinking spend: when the request carries a
    /// `thinking` config (`extra["thinking"].budget_tokens`) above this, the
    /// budget is lowered to this value. NEVER expressed via `max_tokens`
    /// (Anthropic's max_tokens INCLUDES thinking — bounding it would truncate
    /// the answer). Never ENABLES thinking on a request that didn't ask for
    /// it. Minimum 1024 (Anthropic's floor). Same class gate / $0 booking as
    /// `reasoning_max_effort`. `None` = off (default); omitted when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-schema", schemars(range(min = 1024)))]
    pub reasoning_budget_tokens: Option<u32>,
    /// Opt the matched (loop) traffic into the agentic context budget — the
    /// route-grained mode that brings the CLI's loop-aware levers server-side.
    /// `None` = off (no-op; never alters semantics for non-opted traffic).
    /// Composes with `target_model`/`fallbacks` + `auto_pause`. Levers do NOT
    /// stack at face value — the planner nets them per request (see
    /// `tt_core::passes::agentic_budget`). Default `None`; omitted from JSON
    /// when `None` (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentic_budget: Option<AgenticBudget>,
    /// Trigger + configure the Fusion panel for matched requests. None
    /// (default) ⇒ no panel. A panel route is typically modifier-only
    /// (target_model None); if target_model is also set, the panel governs
    /// dispatch and the rewrite is inert (complete_panel branches first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub panel: Option<RoutePanel>,
    /// CO-1: Trigger + configure a governed workflow detour for matched
    /// requests. None (default) ⇒ no workflow detour. When present, this route
    /// dispatches the matched request through the named workflow (running it
    /// through the workflow engine — with per-node budgets, signed receipts,
    /// etc.) instead of a direct provider call. Mutually exclusive with `panel`
    /// and `target_model` (validated at route creation). Workflows are non-
    /// deterministic (same reason panel bypasses cache), so a workflow route
    /// skips L1+L2. The `mode` controls whether the workflow result REPLACES
    /// the upstream call (`detour`) or runs alongside it for comparison
    /// (`shadow` — the workflow result is compared + logged but not returned).
    /// Excludes the request from the savings headline unless `detour` mode
    /// produces a cheaper answer than the direct call would have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<RouteWorkflow>,
}

/// Workflow-route config (CO-1) — an exact structural clone of the `RoutePanel`
/// seam: a self-contained config that detours dispatch. No competitor gateway
/// has "routing rules that detour into governed, receipted multi-step workflows."
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RouteWorkflow {
    /// The workflow definition id to trigger (a UUID string in the workflow
    /// definitions table). Validated at route creation for existence + org scope.
    pub workflow_id: String,
    /// Pre-dispatch workflow admission budget (USD). The workflow engine uses
    /// it to admit a bounded static plan before dispatch; it is not a runtime
    /// spending reservation, settlement, or invoice ceiling. `None` falls back
    /// to the workflow definition's own budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// Optional release environment resolved when the matched request is
    /// accepted. The gateway executes that environment's exact current
    /// immutable workflow version and non-secret variable snapshot. `None`
    /// preserves the legacy latest-saved-definition behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<RouteWorkflowEnvironment>,
    /// `"detour"` (default) = the workflow result REPLACES the upstream call.
    /// `"shadow"` = the workflow runs alongside; its result is compared + logged
    /// but the direct upstream call's answer is returned. Shadow mode is the
    /// safe rollout path: verify the workflow produces equivalent answers before
    /// flipping to detour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// Closed release selector for a route-triggered workflow invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RouteWorkflowEnvironment {
    Development,
    Staging,
    Production,
}

/// Fusion panel config for a route-triggered panel (the same panel
/// engine as the X-TokenTrimmer-Panel header). Self-contained (not a re-export
/// of tt_shared PanelExtras) to keep the routing wire contract explicit and
/// avoid a tt_shared coupling in this crate — mirrors the AgenticBudget pattern.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RoutePanel {
    /// "synthesize" | "best-of-n" (or "best_of_n") | "majority". Validated at
    /// route creation against PANEL_STRATEGY_VALUES; parsed authoritatively at
    /// request time by tt_core's ArbiterStrategyKind::parse.
    pub strategy: String,
    /// Panel member model ids; empty ⇒ gateway env TT_PANEL_DEFAULT_MEMBERS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    /// Arbiter model id; None ⇒ env TT_PANEL_DEFAULT_ARBITER.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arbiter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "contract-schema", schemars(range(min = 1)))]
    pub quorum: Option<usize>,
    /// Fusion pre-dispatch admission budget (USD) for the static fan-out and
    /// arbitration estimate. This is not a runtime spending or invoice cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
}

/// Route-grained agentic context budget — the opt-in shaping mode that brings
/// the CLI's loop-aware levers server-side. Every lever is OFF by default; the
/// whole struct is `Option<AgenticBudget>` on [`RouteAction`], serde-omitted
/// when `None`, so the default request path is byte-identical (no new tokens,
/// no new headers, no behavior change). The levers do NOT stack at face value —
/// the planner (`tt_core::passes::agentic_budget`) nets them per request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AgenticBudget {
    /// Sub-lever 1 (lossless): annotate cache_control breakpoints the caller
    /// forgot + restructure static-first. Default true when the mode is set.
    #[serde(default)]
    pub cache_prefix: bool,
    /// Sub-lever 2: field-drop (lossless, token-true-gated) + summarize
    /// (lossy, judge-gated) stale tool results.
    #[serde(default)]
    pub elide_stale_tools: bool,
    /// Keep the last N tool-result pairs VERBATIM (caveat C1 blast-radius
    /// bound). Default 3 (mirrors Anthropic `keep=3`).
    #[serde(default = "default_keep_recent_pairs")]
    pub keep_recent_pairs: u32,
    /// Each elision must free at least this many tokens to justify the
    /// re-cache it forces (R1 cache-thrash guard). Default 0 = off.
    #[serde(default)]
    pub clear_at_least_tokens: u32,
    /// Sub-lever 3: down-route mechanical sub-steps to this model in a
    /// CACHE-ISOLATED subagent lane. `None` = no routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_mechanical_to: Option<String>,
    /// Sub-lever 4 (expectation-value, caveat C2): semantic sub-step cache
    /// for READ-ONLY/idempotent sub-steps only.
    #[serde(default)]
    pub semantic_substep_cache: bool,
}

fn default_keep_recent_pairs() -> u32 {
    3
}

impl Default for AgenticBudget {
    fn default() -> Self {
        Self {
            cache_prefix: false,
            elide_stale_tools: false,
            keep_recent_pairs: default_keep_recent_pairs(),
            clear_at_least_tokens: 0,
            route_mechanical_to: None,
            semantic_substep_cache: false,
        }
    }
}

/// Deterministic, replica-independent canary arm selection.
///
/// Returns `true` when this request falls in the **canary** arm (route to the
/// new `target_model`) and `false` when it falls in the **control** arm (pass
/// through unchanged), such that — over many distinct idempotency keys — about
/// `traffic_pct`% land in the canary arm.
///
/// # Determinism + replica independence (READ THIS)
///
/// This is a PURE function of its three arguments. It uses the std-library
/// [`DefaultHasher`](std::collections::hash_map::DefaultHasher) — exactly the
/// deterministic pattern in `quality_sample::should_sample` — and contains NO
/// RNG, NO clock, NO per-instance/process state. Therefore every gateway
/// replica computes the SAME arm for the SAME `(org_id, idempotency_key,
/// traffic_pct)`, and a client retry with the same idempotency key is sticky to
/// the same arm. This is required so a canary split is consistent fleet-wide
/// rather than re-rolling the dice per replica or per retry.
///
/// # Hash input
///
/// The hash absorbs, in order: the 16 raw bytes of `org_id`, a `0xff` domain
/// separator byte (so an org id and an idempotency key that happen to share a
/// byte boundary cannot collide), and the UTF-8 bytes of `idempotency_key`. The
/// 64-bit hash is mapped to a bucket in `[0, 100)` via the top-of-range modulo;
/// the request is in the canary arm iff `bucket < traffic_pct`. Keying on
/// `org_id` as well as the idempotency key means two different orgs that reuse
/// the same idempotency-key string get independent (uncorrelated) splits.
///
/// # Edge cases
///
/// - `traffic_pct == 0` → always `false` (no request is canaried).
/// - `traffic_pct >= 100` → always `true` (every request is canaried); values
///   above 100 are clamped to 100.
/// - An EMPTY `idempotency_key` still hashes deterministically (it just means
///   every request with an empty key for a given org lands in the same arm) —
///   callers that cannot supply a stable key should pass a fresh uuid string so
///   the request is treated as a one-off (its arm is then effectively random
///   but still self-consistent for that single request).
#[must_use]
pub fn sticky_traffic_split(org_id: Uuid, idempotency_key: &str, traffic_pct: u32) -> bool {
    if traffic_pct == 0 {
        return false;
    }
    if traffic_pct >= 100 {
        return true;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Absorb the org id's raw bytes, a domain separator, then the key bytes.
    // Hashing the bytes directly (rather than `Uuid::hash`) keeps the input
    // wire-stable regardless of any future change to `Uuid`'s `Hash` impl.
    org_id.as_bytes().hash(&mut hasher);
    0xffu8.hash(&mut hasher);
    idempotency_key.as_bytes().hash(&mut hasher);
    let h = hasher.finish();
    // Map uniformly into [0, 100). `% 100` is uniform enough for a canary split
    // given a well-mixed 64-bit hash (the modulo bias against 100 across 2^64 is
    // ~6e-18, far below any traffic-shaping precision we need).
    let bucket = (h % 100) as u32;
    bucket < traffic_pct
}

/// Rule engine. Hold routes sorted by descending priority; iterate to find
/// the first match.
#[derive(Debug, Clone, Default)]
pub struct RoutingEngine {
    routes: Vec<Route>,
    /// Immutable ledger identity captured with each runtime route refresh.
    /// Kept out of [`Route`] so route JSON / callers retain their established
    /// public shape; only the gateway execution path consumes it.
    route_version_ids: std::collections::HashMap<Uuid, Option<i64>>,
}

/// Why one route candidate was or was not selected during priority evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteCandidateOutcome {
    /// Disabled routes cannot be selected.
    Disabled,
    /// At least one active condition did not match or lacked evidence.
    ConditionsDidNotMatch,
    /// This is the first enabled match in canonical priority order.
    Selected,
    /// The conditions matched, but a higher-priority candidate already won.
    ShadowedByHigherPriority,
    /// This enabled candidate was not named by a forced-route override.
    /// Conditions were deliberately not evaluated because forced routing
    /// bypasses them.
    NotNamedByForcedRoute,
    /// This enabled candidate was selected by an exact forced-route name.
    /// Conditions were deliberately not evaluated because forced routing
    /// bypasses them.
    SelectedByForcedRoute,
}

/// Selection mechanism used for one routing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteSelectionMode {
    /// Normal enabled-route condition evaluation in canonical priority order.
    ConditionPriority,
    /// Exact enabled-route name selected by the caller's forced-route override.
    ForcedRouteName,
}

/// Value-free condition and priority decision for one route candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteCandidateDecision {
    /// Stable route identifier.
    pub route_id: Uuid,
    /// Immutable route-version ledger identifier captured with the definition.
    pub route_version_id: Option<i64>,
    /// Priority value used for winner order.
    pub priority: u32,
    /// Candidate disposition under this trace's selection mode.
    pub outcome: RouteCandidateOutcome,
    /// Every canonical condition decision, without observed feature values.
    /// Absent only when [`RouteSelectionMode::ForcedRouteName`] bypassed
    /// condition evaluation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<RouteConditionEvaluation>,
}

/// Canonical condition/priority trace for one route selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteDecisionTrace {
    /// Whether conditions/priority or an exact forced name selected the route.
    pub selection_mode: RouteSelectionMode,
    /// Candidates in the exact order evaluated by the live engine.
    pub candidates: Vec<RouteCandidateDecision>,
    /// Winning route, or `None` when no enabled candidate matched.
    pub selected_route_id: Option<Uuid>,
    /// Winning immutable version, or `None` when unavailable/no route matched.
    pub selected_route_version_id: Option<i64>,
}

/// Canonical winner plus its value-free decision trace.
pub struct RoutingEvaluation<'a> {
    /// First enabled matching route in canonical priority order.
    pub matched_route: Option<&'a Route>,
    /// Condition and priority decisions for every candidate.
    pub trace: RouteDecisionTrace,
}

impl RoutingEngine {
    /// Construct an empty engine. Use [`RoutingEngine::with_routes`] for the
    /// common case of building from a stored config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct from a collection of routes. Internally sorted descending by
    /// priority; equal-priority routes preserve the caller's order. The
    /// production store supplies creation order, so that tie order is part of
    /// the live selection contract.
    pub fn with_routes(routes: impl IntoIterator<Item = Route>) -> Self {
        Self::with_runtime_routes(routes.into_iter().map(RuntimeRoute::unversioned))
    }

    /// Construct from runtime routes plus their immutable ledger identities.
    ///
    /// The routing decision still operates on [`Route`], while
    /// [`Self::route_version_id`] exposes the matching snapshot for request-log
    /// provenance. The IDs are captured together by the backing store; this
    /// method merely preserves that association through the cache/engine.
    pub fn with_runtime_routes(routes: impl IntoIterator<Item = RuntimeRoute>) -> Self {
        let mut route_version_ids = std::collections::HashMap::new();
        let mut v = Vec::new();
        for RuntimeRoute {
            route,
            route_version_id,
        } in routes
        {
            route_version_ids.insert(route.id, route_version_id);
            v.push(route);
        }
        v.sort_by_key(|r| std::cmp::Reverse(r.priority));
        Self {
            routes: v,
            route_version_ids,
        }
    }

    /// Add a route in-place and re-sort. Hot-path callers should prefer
    /// [`RoutingEngine::with_routes`] to amortize the sort.
    pub fn add(&mut self, route: Route) {
        self.route_version_ids.insert(route.id, None);
        self.routes.push(route);
        self.routes.sort_by_key(|r| std::cmp::Reverse(r.priority));
    }

    /// All routes, descending priority order.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Immutable ledger ID captured with `route_id` during the same runtime
    /// refresh. `None` means the ledger was unavailable or had no exact
    /// snapshot; callers must persist NULL, never substitute `revision`.
    #[must_use]
    pub fn route_version_id(&self, route_id: Uuid) -> Option<i64> {
        self.route_version_ids.get(&route_id).copied().flatten()
    }

    /// Clone the engine's routes with their captured immutable ledger IDs.
    /// This is used only when a caching store itself is used as another
    /// store's backing source; hot-path route evaluation reads the engine
    /// directly and performs no allocation.
    #[must_use]
    pub fn runtime_routes(&self) -> Vec<RuntimeRoute> {
        self.routes
            .iter()
            .cloned()
            .map(|route| RuntimeRoute {
                route_version_id: self.route_version_id(route.id),
                route,
            })
            .collect()
    }

    /// Find the first matching route for `(req, ctx)`. Returns `None` when no
    /// enabled route matches — caller dispatches the request unchanged.
    ///
    /// `input_tokens_estimate` is supplied by the caller — typically a cheap
    /// length-over-4 heuristic, or the result of a tokenizer call cached at
    /// the request boundary. The engine never tokenizes itself.
    pub fn evaluate(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
    ) -> Option<&Route> {
        // No cost signal — cost conditions never fire (see `evaluate_with_cost`).
        self.evaluate_with_cost(req, ctx, input_tokens_estimate, None)
    }

    /// Like [`RoutingEngine::evaluate`] but with a pre-flight cost estimate (USD)
    /// for `estimated_cost_gt` / `estimated_cost_lt` conditions. `None` means the
    /// cost is unknown (e.g. the requested model has no pricing) — cost
    /// conditions then never match, mirroring the engine's other "unknown data →
    /// don't match" stances.
    pub fn evaluate_with_cost(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
        estimated_cost_usd: Option<f64>,
    ) -> Option<&Route> {
        // No latency signal or reasoning-class classification on this path:
        // upstream_latency_ms_p95_gt conditions never fire, and not_reasoning_class
        // routes match all traffic (treated as non-reasoning).
        self.evaluate_with_signals(
            req,
            ctx,
            input_tokens_estimate,
            estimated_cost_usd,
            None,
            false,
        )
    }

    /// Like [`RoutingEngine::evaluate_with_cost`] but also threads the live,
    /// gateway-observed p95 upstream latency (milliseconds) for the
    /// originally-requested `(provider, model)`, for the
    /// `upstream_latency_ms_p95_gt` condition, and the reasoning-class signal
    /// for the `not_reasoning_class` condition.
    ///
    /// `observed_p95_ms` is `None` when the gateway's
    /// [`crate::LatencyTracker`] has insufficient samples for that key (cold
    /// start) or no tracker is wired. A `None` latency makes the latency
    /// condition FALSE — never a fabricated match. The caller computes this once
    /// (all routes evaluate the same originally-requested model) and passes it
    /// in, mirroring `estimated_cost_usd`.
    ///
    /// `is_reasoning_class` is `true` when the request has been classified as
    /// Math/Code/Legal/Medical reasoning-is-the-work traffic (computed by
    /// `tt-core`'s `reasoning_class`). A route with `not_reasoning_class: true`
    /// will NOT match when this is `true`. Pass `false` when the signal is
    /// unknown or inapplicable (the condition evaluates as if not set).
    pub fn evaluate_with_signals(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
        estimated_cost_usd: Option<f64>,
        observed_p95_ms: Option<u32>,
        is_reasoning_class: bool,
    ) -> Option<&Route> {
        let snapshot = RouteFeatureSnapshot::for_engine(
            &self.routes,
            req,
            ctx,
            input_tokens_estimate,
            estimated_cost_usd,
            observed_p95_ms,
            is_reasoning_class,
        );
        self.evaluate_snapshot(&snapshot)
    }

    /// Like [`Self::evaluate_with_signals`], but retains the value-free
    /// condition/priority trace from the exact same feature snapshot.
    #[must_use]
    pub fn evaluate_with_signals_and_trace(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
        estimated_cost_usd: Option<f64>,
        observed_p95_ms: Option<u32>,
        is_reasoning_class: bool,
    ) -> RoutingEvaluation<'_> {
        let snapshot = RouteFeatureSnapshot::for_engine(
            &self.routes,
            req,
            ctx,
            input_tokens_estimate,
            estimated_cost_usd,
            observed_p95_ms,
            is_reasoning_class,
        );
        self.evaluate_snapshot_with_trace(&snapshot)
    }

    /// Select the first enabled route matching one canonical feature snapshot.
    ///
    /// Retained/partial callers can construct a conservative snapshot with
    /// [`RouteFeatureSnapshot::from_retained_features`]; any active condition
    /// whose required feature is unavailable blocks that candidate.
    #[must_use]
    pub fn evaluate_snapshot(&self, snapshot: &RouteFeatureSnapshot) -> Option<&Route> {
        self.routes
            .iter()
            .find(|route| route.enabled && matcher::route_conditions_match(&route.when, snapshot))
    }

    /// Evaluate all route candidates using the same condition routine as
    /// [`Self::evaluate_snapshot`] and retain a value-free priority trace.
    #[must_use]
    pub fn evaluate_snapshot_with_trace(
        &self,
        snapshot: &RouteFeatureSnapshot,
    ) -> RoutingEvaluation<'_> {
        let mut matched_route = None;
        let mut candidates = Vec::with_capacity(self.routes.len());

        for route in &self.routes {
            let conditions = evaluate_route_conditions(&route.when, snapshot);
            let outcome = if !route.enabled {
                RouteCandidateOutcome::Disabled
            } else if !conditions.matches() {
                RouteCandidateOutcome::ConditionsDidNotMatch
            } else if matched_route.is_none() {
                matched_route = Some(route);
                RouteCandidateOutcome::Selected
            } else {
                RouteCandidateOutcome::ShadowedByHigherPriority
            };
            candidates.push(RouteCandidateDecision {
                route_id: route.id,
                route_version_id: self.route_version_id(route.id),
                priority: route.priority,
                outcome,
                conditions: Some(conditions),
            });
        }

        let selected_route_id = matched_route.map(|route| route.id);
        let selected_route_version_id =
            selected_route_id.and_then(|route_id| self.route_version_id(route_id));
        RoutingEvaluation {
            matched_route,
            trace: RouteDecisionTrace {
                selection_mode: RouteSelectionMode::ConditionPriority,
                candidates,
                selected_route_id,
                selected_route_version_id,
            },
        }
    }

    /// Select an enabled route by exact name and retain an explicit trace of
    /// the forced-route bypass. Candidate conditions are not evaluated and are
    /// omitted from the trace so the payload cannot imply they affected this
    /// decision.
    #[must_use]
    pub fn evaluate_forced_route_with_trace(&self, name: &str) -> RoutingEvaluation<'_> {
        let selected_index = self
            .routes
            .iter()
            .position(|route| route.enabled && route.name == name);
        let matched_route = selected_index.map(|index| &self.routes[index]);
        let selected_route_id = matched_route.map(|route| route.id);
        let selected_route_version_id =
            selected_route_id.and_then(|route_id| self.route_version_id(route_id));
        let candidates = self
            .routes
            .iter()
            .enumerate()
            .map(|(index, route)| RouteCandidateDecision {
                route_id: route.id,
                route_version_id: self.route_version_id(route.id),
                priority: route.priority,
                outcome: if !route.enabled {
                    RouteCandidateOutcome::Disabled
                } else if Some(index) == selected_index {
                    RouteCandidateOutcome::SelectedByForcedRoute
                } else {
                    RouteCandidateOutcome::NotNamedByForcedRoute
                },
                conditions: None,
            })
            .collect();

        RoutingEvaluation {
            matched_route,
            trace: RouteDecisionTrace {
                selection_mode: RouteSelectionMode::ForcedRouteName,
                candidates,
                selected_route_id,
                selected_route_version_id,
            },
        }
    }

    /// Returns `true` when at least one **enabled** route in this engine carries
    /// the `not_reasoning_class` condition — meaning the caller MUST classify
    /// the request and supply the `is_reasoning_class` signal to
    /// [`evaluate_with_signals`] for correct route evaluation.
    ///
    /// Callers can use this as a cheap pre-flight check to avoid running the
    /// (more expensive) classifier when no route needs it.
    #[must_use]
    pub fn uses_reasoning_class(&self) -> bool {
        self.routes
            .iter()
            .any(|r| r.enabled && r.when.not_reasoning_class)
    }

    /// Find an enabled route by exact name (case-sensitive), bypassing condition
    /// evaluation — used to honor a forced-route request header.
    pub fn find_by_name(&self, name: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.enabled && r.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::{
        context::{ProviderCredentials, SecretString},
        messages::{ContentPart, DocumentPart, DocumentSource, ImageUrl, InputAudio},
        ChatCompletionRequest, Message, MessageContent,
    };

    fn make_route(name: &str, priority: u32, model_in: Vec<&str>, target: &str) -> Route {
        Route {
            id: Uuid::now_v7(),
            name: name.into(),
            priority,
            enabled: true,
            when: RouteConditions {
                model_in: model_in.into_iter().map(String::from).collect(),
                ..Default::default()
            },
            then: RouteAction {
                format_switch: None,
                diff: false,
                target_model: Some(target.into()),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                compress: false,
                doc_compaction: false,
                document_lane: false,
                content_compress: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                agentic_budget: None,
                panel: None,
                workflow: None,
            },
            paused: false,
        }
    }

    fn make_req(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Text("hi".into()),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    fn make_req_with_part(model: &str, part: ContentPart) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![part]),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    fn image_part() -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: "data:image/png;base64,abc".into(),
                detail: None,
                media_type: None,
            },
        }
    }

    fn audio_part() -> ContentPart {
        ContentPart::InputAudio {
            input_audio: InputAudio {
                data: "abc".into(),
                format: "wav".into(),
            },
        }
    }

    fn document_part() -> ContentPart {
        ContentPart::Document {
            document: DocumentPart {
                source: DocumentSource::Base64 {
                    media_type: "application/pdf".into(),
                    data: "JVBERi0=".into(),
                },
                filename: Some("a.pdf".into()),
            },
        }
    }

    #[test]
    fn find_by_name_matches_enabled_route_by_exact_name() {
        let enabled = make_route("alpha", 10, vec!["gpt-4o"], "gpt-4o-mini");
        let mut disabled = make_route("beta", 10, vec!["gpt-4o"], "gpt-4o-mini");
        disabled.enabled = false;
        let eng = RoutingEngine::with_routes(vec![enabled, disabled]);
        assert!(eng.find_by_name("alpha").is_some());
        assert_eq!(eng.find_by_name("alpha").unwrap().name, "alpha");
        assert!(
            eng.find_by_name("beta").is_none(),
            "disabled route not found"
        );
        assert!(eng.find_by_name("missing").is_none());
    }

    #[test]
    fn has_images_true_matches_only_image_requests() {
        let route = Route {
            when: RouteConditions {
                has_images: Some(true),
                ..Default::default()
            },
            ..make_route("vision", 10, vec![], "vision-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn has_images_false_matches_only_non_image_requests() {
        let route = Route {
            when: RouteConditions {
                has_images: Some(false),
                ..Default::default()
            },
            ..make_route("text", 10, vec![], "cheap")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_none());
    }

    #[test]
    fn has_audio_true_matches_only_audio_requests() {
        let route = Route {
            when: RouteConditions {
                has_audio: Some(true),
                ..Default::default()
            },
            ..make_route("audio", 10, vec![], "audio-model")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", audio_part()),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_none());
    }

    #[test]
    fn has_documents_true_matches_only_document_requests() {
        // A document route targets a TEXT model ("cheap") — the Document Lane
        // point. It matches only requests carrying a Document part, and NOT
        // image/audio/text-only requests.
        let route = Route {
            when: RouteConditions {
                has_documents: Some(true),
                ..Default::default()
            },
            ..make_route("docs", 10, vec![], "cheap")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", document_part()),
                &make_ctx(None),
                100
            )
            .is_some());
        // Image and text-only requests must NOT match a document route.
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_none());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn has_documents_false_matches_only_non_document_requests() {
        let route = Route {
            when: RouteConditions {
                has_documents: Some(false),
                ..Default::default()
            },
            ..make_route("text", 10, vec![], "cheap")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", document_part()),
                &make_ctx(None),
                100
            )
            .is_none());
    }

    #[test]
    fn modality_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                has_images: Some(true),
                ..Default::default()
            },
            ..make_route("both", 10, vec!["gpt-4o"], "vision-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
        assert!(eng
            .evaluate(
                &make_req_with_part("other", image_part()),
                &make_ctx(None),
                100
            )
            .is_none());
    }

    fn make_ctx(tag: Option<&str>) -> RequestContext {
        RequestContext {
            trace_id: Uuid::now_v7(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new(""),
                base_url: None,
                extra_headers: Vec::new(),
            },
            tag: tag.map(String::from),
            deadline: None,
            run_id: None,
            node_id: None,
        }
    }

    #[test]
    fn empty_engine_matches_nothing() {
        let eng = RoutingEngine::new();
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn model_in_matches() {
        let eng = RoutingEngine::with_routes(vec![make_route(
            "to-mini",
            10,
            vec!["gpt-4o"],
            "gpt-4o-mini",
        )]);
        let m = eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .expect("should match");
        assert_eq!(m.then.target_model.as_deref(), Some("gpt-4o-mini"));
    }

    #[test]
    fn priority_descending_first_match_wins() {
        let eng = RoutingEngine::with_routes(vec![
            make_route("low", 1, vec!["gpt-4o"], "low-target"),
            make_route("high", 100, vec!["gpt-4o"], "high-target"),
            make_route("mid", 50, vec!["gpt-4o"], "mid-target"),
        ]);
        let m = eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .unwrap();
        assert_eq!(m.then.target_model.as_deref(), Some("high-target"));
    }

    #[test]
    fn decision_trace_uses_the_live_winner_and_exposes_no_feature_values() {
        let mut disabled = make_route(
            "private-disabled-route",
            100,
            vec!["sensitive-model"],
            "never",
        );
        disabled.enabled = false;
        disabled.when.tag_equals = Some("sensitive-tag".into());

        let nonmatching = make_route(
            "private-nonmatching-route",
            90,
            vec!["different-model"],
            "never",
        );

        let mut selected = make_route(
            "private-selected-route",
            80,
            vec!["sensitive-model"],
            "winner",
        );
        selected.when.prompt_contains_any_of = vec!["private-prompt".into()];

        let shadowed = make_route(
            "private-shadowed-route",
            70,
            vec!["sensitive-model"],
            "shadowed",
        );

        let ids = [disabled.id, nonmatching.id, selected.id, shadowed.id];
        let engine = RoutingEngine::with_runtime_routes([
            RuntimeRoute {
                route: shadowed,
                route_version_id: Some(704),
            },
            RuntimeRoute {
                route: selected,
                route_version_id: Some(803),
            },
            RuntimeRoute {
                route: nonmatching,
                route_version_id: Some(902),
            },
            RuntimeRoute {
                route: disabled,
                route_version_id: Some(1_001),
            },
        ]);
        let request = ChatCompletionRequest {
            model: "sensitive-model".into(),
            messages: vec![Message::User {
                content: MessageContent::Text("PRIVATE-PROMPT customer material".into()),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        };
        let snapshot = RouteFeatureSnapshot::from_request(
            &request,
            &make_ctx(Some("sensitive-tag")),
            100,
            None,
            None,
            false,
        );

        let direct = engine.evaluate_snapshot(&snapshot).map(|route| route.id);
        let evaluated = engine.evaluate_snapshot_with_trace(&snapshot);

        assert_eq!(direct, Some(ids[2]));
        assert_eq!(evaluated.matched_route.map(|route| route.id), direct);
        assert_eq!(
            evaluated.trace.selection_mode,
            RouteSelectionMode::ConditionPriority
        );
        assert_eq!(evaluated.trace.selected_route_id, direct);
        assert_eq!(evaluated.trace.selected_route_version_id, Some(803));
        assert_eq!(
            evaluated
                .trace
                .candidates
                .iter()
                .map(|candidate| (
                    candidate.route_id,
                    candidate.route_version_id,
                    candidate.outcome
                ))
                .collect::<Vec<_>>(),
            vec![
                (ids[0], Some(1_001), RouteCandidateOutcome::Disabled),
                (
                    ids[1],
                    Some(902),
                    RouteCandidateOutcome::ConditionsDidNotMatch
                ),
                (ids[2], Some(803), RouteCandidateOutcome::Selected),
                (
                    ids[3],
                    Some(704),
                    RouteCandidateOutcome::ShadowedByHigherPriority
                ),
            ]
        );
        assert!(evaluated.trace.candidates.iter().all(|candidate| {
            candidate.conditions.as_ref().is_some_and(|conditions| {
                conditions.decisions.len() == RouteConditionField::ALL.len()
            })
        }));

        let serialized = serde_json::to_string(&evaluated.trace).unwrap();
        for private_value in [
            "private-disabled-route",
            "private-nonmatching-route",
            "private-selected-route",
            "private-shadowed-route",
            "sensitive-model",
            "sensitive-tag",
            "private-prompt",
            "customer material",
        ] {
            assert!(!serialized.contains(private_value), "{serialized}");
        }

        let missed = engine.evaluate_forced_route_with_trace("private-missing-route");
        assert!(missed.matched_route.is_none());
        assert_eq!(missed.trace.selected_route_id, None);
        assert_eq!(missed.trace.selected_route_version_id, None);
        assert!(missed.trace.candidates.iter().all(|candidate| {
            candidate.outcome == RouteCandidateOutcome::Disabled
                || candidate.outcome == RouteCandidateOutcome::NotNamedByForcedRoute
        }));
        assert!(!serde_json::to_string(&missed.trace)
            .unwrap()
            .contains("private-missing-route"));
    }

    #[test]
    fn forced_route_trace_names_the_bypass_without_conditions_or_private_values() {
        let disabled = Route {
            enabled: false,
            ..make_route(
                "private-disabled-route",
                100,
                vec!["private-disabled-model"],
                "never",
            )
        };
        let ordinary = make_route(
            "private-ordinary-route",
            90,
            vec!["private-ordinary-model"],
            "ordinary",
        );
        let forced = make_route(
            "private-forced-route",
            10,
            vec!["private-forced-condition-model"],
            "forced",
        );
        let ids = [disabled.id, ordinary.id, forced.id];
        let engine = RoutingEngine::with_runtime_routes([
            RuntimeRoute {
                route: forced,
                route_version_id: Some(103),
            },
            RuntimeRoute {
                route: ordinary,
                route_version_id: Some(102),
            },
            RuntimeRoute {
                route: disabled,
                route_version_id: Some(101),
            },
        ]);

        let evaluated = engine.evaluate_forced_route_with_trace("private-forced-route");

        assert_eq!(
            evaluated.trace.selection_mode,
            RouteSelectionMode::ForcedRouteName
        );
        assert_eq!(evaluated.matched_route.map(|route| route.id), Some(ids[2]));
        assert_eq!(evaluated.trace.selected_route_id, Some(ids[2]));
        assert_eq!(evaluated.trace.selected_route_version_id, Some(103));
        assert_eq!(
            evaluated
                .trace
                .candidates
                .iter()
                .map(|candidate| (candidate.route_id, candidate.outcome))
                .collect::<Vec<_>>(),
            vec![
                (ids[0], RouteCandidateOutcome::Disabled),
                (ids[1], RouteCandidateOutcome::NotNamedByForcedRoute),
                (ids[2], RouteCandidateOutcome::SelectedByForcedRoute),
            ]
        );
        assert!(evaluated
            .trace
            .candidates
            .iter()
            .all(|candidate| candidate.conditions.is_none()));

        let serialized = serde_json::to_string(&evaluated.trace).unwrap();
        assert!(!serialized.contains("conditions"), "{serialized}");
        for private_value in [
            "private-disabled-route",
            "private-ordinary-route",
            "private-forced-route",
            "private-disabled-model",
            "private-ordinary-model",
            "private-forced-condition-model",
        ] {
            assert!(!serialized.contains(private_value), "{serialized}");
        }
    }

    #[test]
    fn traced_live_signal_entrypoint_has_the_same_winner_as_normal_evaluation() {
        let mut route = make_route("live", 10, vec!["gpt-4o"], "gpt-4o-mini");
        route.when.not_reasoning_class = true;
        let engine = RoutingEngine::with_routes([route]);
        let request = make_req("gpt-4o");
        let context = make_ctx(None);

        let direct = engine
            .evaluate_with_signals(&request, &context, 100, None, None, false)
            .map(|route| route.id);
        let traced =
            engine.evaluate_with_signals_and_trace(&request, &context, 100, None, None, false);

        assert_eq!(traced.matched_route.map(|route| route.id), direct);
        assert_eq!(traced.trace.selected_route_id, direct);
    }

    #[test]
    fn equal_priority_trace_preserves_store_order() {
        let first = make_route("created-first", 10, vec!["gpt-4o"], "first");
        let second = make_route("created-second", 10, vec!["gpt-4o"], "second");
        let ids = [first.id, second.id];
        let engine = RoutingEngine::with_runtime_routes([
            RuntimeRoute {
                route: first,
                route_version_id: Some(11),
            },
            RuntimeRoute {
                route: second,
                route_version_id: Some(12),
            },
        ]);
        let snapshot = RouteFeatureSnapshot::from_retained_features("gpt-4o".into(), 10, None);

        let evaluated = engine.evaluate_snapshot_with_trace(&snapshot);

        assert_eq!(evaluated.trace.selected_route_id, Some(ids[0]));
        assert_eq!(
            evaluated
                .trace
                .candidates
                .iter()
                .map(|candidate| candidate.route_id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(
            evaluated
                .trace
                .candidates
                .iter()
                .map(|candidate| candidate.outcome)
                .collect::<Vec<_>>(),
            vec![
                RouteCandidateOutcome::Selected,
                RouteCandidateOutcome::ShadowedByHigherPriority,
            ]
        );
    }

    #[test]
    fn disabled_route_skipped() {
        let mut route = make_route("disabled", 100, vec!["gpt-4o"], "never");
        route.enabled = false;
        let eng = RoutingEngine::with_routes(vec![
            route,
            make_route("enabled", 10, vec!["gpt-4o"], "winner"),
        ]);
        let m = eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .unwrap();
        assert_eq!(m.then.target_model.as_deref(), Some("winner"));
    }

    #[test]
    fn token_lt_filters() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                input_tokens_lt: Some(500),
                ..Default::default()
            },
            ..make_route("short-only", 10, vec!["gpt-4o"], "gpt-4o-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 600)
            .is_none());
    }

    #[test]
    fn token_gt_filters() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                input_tokens_gt: Some(1000),
                ..Default::default()
            },
            ..make_route("long-only", 10, vec!["gpt-4o"], "claude-opus-4-7")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 500)
            .is_none());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 1500)
            .is_some());
    }

    #[test]
    fn tag_equals_filters() {
        let route = Route {
            when: RouteConditions {
                tag_equals: Some("background".into()),
                ..Default::default()
            },
            ..make_route("bg-only", 10, vec![], "cheap-model")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(Some("background")), 100)
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(Some("foreground")), 100)
            .is_none());
    }

    #[test]
    fn empty_model_in_matches_any_model() {
        let route = make_route("any", 10, vec![], "target");
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("claude-sonnet-4-6"), &make_ctx(None), 100)
            .is_some());
    }

    // --- rv-routeaction-shared-type: field-parity serde tests ---

    /// (c) Serializing a `RouteAction` with empty fallbacks produces the
    /// minimal JSON `{"target_model":"x"}` — confirming skip_serializing_if.
    #[test]
    fn route_action_minimal_serializes_without_new_fields() {
        let a = RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("x".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: false,
            doc_compaction: false,
            document_lane: false,
            content_compress: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            agentic_budget: None,
            panel: None,
            workflow: None,
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
        let json = r#"{"target_model":"gpt-4o-mini"}"#;
        let a: RouteAction = serde_json::from_str(json).unwrap();
        assert_eq!(a.target_model.as_deref(), Some("gpt-4o-mini"));
        assert!(a.fallbacks.is_empty(), "fallbacks must default to empty");
    }

    /// (a) Full round-trip: a `RouteAction` with fallbacks serializes to JSON
    /// carrying them, and deserializes back with all values preserved.
    #[test]
    fn route_action_full_round_trip() {
        let original = RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("claude-haiku-4-5".into()),
            fallbacks: vec!["gpt-4o-mini".into(), "gemini-flash".into()],
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: false,
            doc_compaction: false,
            document_lane: false,
            content_compress: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            agentic_budget: None,
            panel: None,
            workflow: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(
            json.contains("\"fallbacks\""),
            "fallbacks must be present: {json}"
        );
        let roundtripped: RouteAction = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.target_model, original.target_model);
        assert_eq!(roundtripped.fallbacks, original.fallbacks);
    }

    #[test]
    fn route_action_disable_cache_defaults_false_and_omits() {
        // Omitted from JSON when false (back-compat: existing rows unchanged).
        let a = RouteAction {
            format_switch: None,
            diff: false,
            target_model: Some("x".into()),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: false,
            doc_compaction: false,
            document_lane: false,
            content_compress: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            agentic_budget: None,
            panel: None,
            workflow: None,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"target_model":"x"}"#
        );
        // Defaults false when absent.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.disable_cache);
        // Present when true.
        let b = RouteAction {
            workflow: None,
            disable_cache: true,
            ..a
        };
        assert!(serde_json::to_string(&b)
            .unwrap()
            .contains("\"disable_cache\":true"));
    }

    /// `doc_compaction` (Document Lane D2) defaults to false, is omitted from
    /// JSON when false (back-compat: existing rows/payloads unchanged), and
    /// `{"doc_compaction":true}` deserializes to the flag being set.
    #[test]
    fn route_action_doc_compaction_defaults_false_omits_and_round_trips() {
        // Default: absent on read → false.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.doc_compaction, "doc_compaction must default false");

        // {"doc_compaction":true} deserializes with the flag set.
        let on: RouteAction = serde_json::from_str(r#"{"doc_compaction":true}"#).unwrap();
        assert!(on.doc_compaction, "explicit true must deserialize as set");

        // Omitted from JSON when false (existing route JSON stays byte-identical).
        let off = RouteAction {
            target_model: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&off).unwrap(),
            r#"{"target_model":"x"}"#,
            "doc_compaction:false must be omitted from the wire form"
        );

        // Present when true.
        let b = RouteAction {
            workflow: None,
            doc_compaction: true,
            ..off
        };
        assert!(serde_json::to_string(&b)
            .unwrap()
            .contains("\"doc_compaction\":true"));
    }

    /// `document_lane` (Document Lane D4) defaults to false, is omitted from JSON
    /// when false (back-compat), `{"document_lane":true}` deserializes set, and
    /// a `document_lane`-only modifier route (no target_model) is a valid effect.
    #[test]
    fn route_action_document_lane_defaults_false_omits_and_is_an_effect() {
        // Default: absent on read → false.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.document_lane, "document_lane must default false");

        // {"document_lane":true} deserializes with the flag set.
        let on: RouteAction = serde_json::from_str(r#"{"document_lane":true}"#).unwrap();
        assert!(on.document_lane, "explicit true must deserialize as set");

        // Omitted from JSON when false (existing route JSON stays byte-identical).
        let off = RouteAction {
            target_model: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&off).unwrap(),
            r#"{"target_model":"x"}"#,
            "document_lane:false must be omitted from the wire form"
        );

        // Present when true.
        let b = RouteAction {
            workflow: None,
            document_lane: true,
            content_compress: false,
            ..off
        };
        assert!(serde_json::to_string(&b)
            .unwrap()
            .contains("\"document_lane\":true"));

        // A modifier-only route (no target_model) carrying ONLY document_lane is
        // a valid effect (mirrors compress/doc_compaction).
        let modifier_only = RouteAction {
            document_lane: true,
            content_compress: false,
            ..Default::default()
        };
        assert!(crate::validate::validate_route_has_effect(&modifier_only).is_ok());
    }

    /// `content_compress` (content-aware compression, P1a) defaults to false, is
    /// omitted from JSON when false (back-compat), `{"content_compress":true}`
    /// deserializes set, and a `content_compress`-only modifier route (no
    /// target_model) is a valid effect (mirrors compress/doc_compaction).
    #[test]
    fn route_action_content_compress_defaults_false_omits_and_is_an_effect() {
        // Default: absent on read → false.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(
            !parsed.content_compress,
            "content_compress must default false"
        );

        // {"content_compress":true} deserializes with the flag set.
        let on: RouteAction = serde_json::from_str(r#"{"content_compress":true}"#).unwrap();
        assert!(on.content_compress, "explicit true must deserialize as set");

        // Omitted from JSON when false (existing route JSON stays byte-identical).
        let off = RouteAction {
            target_model: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&off).unwrap(),
            r#"{"target_model":"x"}"#,
            "content_compress:false must be omitted from the wire form"
        );

        // Present when true.
        let b = RouteAction {
            workflow: None,
            content_compress: true,
            ..off
        };
        assert!(serde_json::to_string(&b)
            .unwrap()
            .contains("\"content_compress\":true"));

        // A modifier-only route carrying ONLY content_compress is a valid effect.
        let modifier_only = RouteAction {
            content_compress: true,
            ..Default::default()
        };
        assert!(crate::validate::validate_route_has_effect(&modifier_only).is_ok());
    }

    /// A `content_type` condition matches only requests whose DOMINANT content
    /// kind equals the targeted kind: `content_type=code` matches a code-dominant
    /// request and NOT a prose-dominant one nor a tiny/unclassifiable request.
    #[test]
    fn content_type_matches_dominant_content_kind() {
        let route = Route {
            when: RouteConditions {
                content_type: Some("code".into()),
                ..Default::default()
            },
            ..make_route("code-route", 10, vec![], "cheap")
        };
        let eng = RoutingEngine::with_routes(vec![route]);

        let code = "fn a() {\n  let x = 1;\n}\n".repeat(20);
        let code_req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::User {
                content: MessageContent::Text(code),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        };
        assert!(
            eng.evaluate(&code_req, &make_ctx(None), 100).is_some(),
            "code-dominant request must match content_type=code"
        );

        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(40);
        let prose_req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::User {
                content: MessageContent::Text(prose),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        };
        assert!(
            eng.evaluate(&prose_req, &make_ctx(None), 100).is_none(),
            "prose-dominant request must NOT match content_type=code"
        );

        // Tiny / unclassifiable request → never matches a content_type route.
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
    }

    /// `content_type` defaults to `None`, is omitted from JSON when `None`
    /// (legacy rows / payloads unchanged), and `Some("code")` round-trips.
    #[test]
    fn route_conditions_content_type_defaults_none_omits_and_round_trips() {
        // Default: absent on read → None; omitted on write (skip_serializing_if).
        let parsed: RouteConditions = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(parsed.content_type, None);
        let j = serde_json::to_string(&parsed).unwrap();
        assert!(!j.contains("content_type"), "{j}");

        // Some("code") deserializes and re-emits the key (RouteConditions does
        // not skip its leading always-present fields, so we assert on the key,
        // not full byte-identity).
        let c: RouteConditions = serde_json::from_str(r#"{"content_type":"code"}"#).unwrap();
        assert_eq!(c.content_type.as_deref(), Some("code"));
        assert!(serde_json::to_string(&c)
            .unwrap()
            .contains("\"content_type\":\"code\""));
    }

    /// `format_switch` defaults to `None`, is omitted from JSON when `None`
    /// (legacy rows / payloads unchanged), and `Some("csv")` round-trips.
    #[test]
    fn route_action_format_switch_defaults_none_omits_and_round_trips() {
        // Default: absent on read → None; None omitted on write.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert_eq!(parsed.format_switch, None);
        let j = serde_json::to_string(&parsed).unwrap();
        assert!(!j.contains("format_switch"), "{j}");

        // Some("csv") round-trips byte-stably.
        let json = r#"{"target_model":"m","format_switch":"csv"}"#;
        let a: RouteAction = serde_json::from_str(json).unwrap();
        assert_eq!(a.format_switch.as_deref(), Some("csv"));
        assert_eq!(serde_json::to_string(&a).unwrap(), json);
    }

    /// `diff` defaults to false, is omitted from JSON when false (legacy rows
    /// / payloads unchanged), and `true` round-trips.
    #[test]
    fn route_action_diff_defaults_false_omits_and_round_trips() {
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.diff);
        let j = serde_json::to_string(&parsed).unwrap();
        assert!(!j.contains("diff"), "{j}");

        let json = r#"{"target_model":"m","diff":true}"#;
        let a: RouteAction = serde_json::from_str(json).unwrap();
        assert!(a.diff);
        assert_eq!(serde_json::to_string(&a).unwrap(), json);
    }

    /// `agentic_budget` is OMITTED from JSON when `None` — the back-compat
    /// invariant: the default request path is byte-identical (no new field on
    /// the wire), mirroring how `format_switch`/`shadow_model` are tested.
    #[test]
    fn agentic_budget_omitted_from_json_when_none() {
        let a = RouteAction::default();
        assert_eq!(a.agentic_budget, None);
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            !j.contains("agentic_budget"),
            "agentic_budget must be omitted when None: {j}"
        );
    }

    /// A `RouteAction` carrying `agentic_budget: Some(..)` serializes and
    /// deserializes byte-stably (round-trip), and the nested `AgenticBudget`
    /// preserves its non-default field values.
    #[test]
    fn agentic_budget_round_trips() {
        let original = RouteAction {
            target_model: Some("m".into()),
            agentic_budget: Some(AgenticBudget {
                cache_prefix: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: RouteAction = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.agentic_budget,
            Some(AgenticBudget {
                cache_prefix: true,
                ..Default::default()
            })
        );
        // Byte-stable re-emit.
        assert_eq!(serde_json::to_string(&parsed).unwrap(), json);
    }

    // --- modifier-only route: optional target_model serde ---

    /// (a) A modifier-only `RouteAction` (`target_model: None` +
    /// `agentic_budget: Some(..)`) serializes WITHOUT a `target_model` key and
    /// round-trips back to `None`.
    #[test]
    fn modifier_only_target_model_none_omits_key_and_round_trips() {
        let a = RouteAction {
            target_model: None,
            agentic_budget: Some(AgenticBudget {
                cache_prefix: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(
            !json.contains("target_model"),
            "target_model must be omitted when None: {json}"
        );
        assert!(json.contains("agentic_budget"), "{json}");
        let back: RouteAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back.target_model, None);
        assert_eq!(
            back.agentic_budget,
            Some(AgenticBudget {
                cache_prefix: true,
                ..Default::default()
            })
        );
    }

    /// (b) JSON omitting `target_model` deserializes to `None`.
    #[test]
    fn target_model_absent_deserializes_to_none() {
        let a: RouteAction = serde_json::from_str(r#"{"agentic_budget":{"cache_prefix":true}}"#)
            .expect("modifier-only JSON must parse");
        assert_eq!(a.target_model, None);
    }

    /// (c) Back-compat: JSON carrying `target_model` deserializes to `Some`.
    #[test]
    fn target_model_present_deserializes_to_some() {
        let a: RouteAction = serde_json::from_str(r#"{"target_model":"gpt-4o-mini"}"#).unwrap();
        assert_eq!(a.target_model.as_deref(), Some("gpt-4o-mini"));
    }

    /// `AgenticBudget::default()` is OFF: every lever disabled, the
    /// blast-radius bound `keep_recent_pairs` defaults to 3 (Anthropic
    /// `keep=3`), no forced re-cache threshold, no routing.
    #[test]
    fn agentic_budget_default_is_off() {
        let ab = AgenticBudget::default();
        assert!(!ab.cache_prefix);
        assert!(!ab.elide_stale_tools);
        assert_eq!(ab.keep_recent_pairs, 3);
        assert_eq!(ab.clear_at_least_tokens, 0);
        assert_eq!(ab.route_mechanical_to, None);
        assert!(!ab.semantic_substep_cache);
    }

    /// Cross-crate lossless round-trip: JSON produced by `tt_routing::RouteAction`
    /// (target_model + fallbacks) deserializes into a structurally identical
    /// representation. Because both types are field-identical, the JSON is the
    /// shared wire format — a plan apply can serialize a `tt_plan_core::RouteAction`
    /// and the gateway reads it as a `tt_routing::RouteAction` without loss.
    #[test]
    fn route_action_cross_type_wire_compat() {
        let plan_side_json = r#"{"target_model":"claude-3-5-haiku","fallbacks":["gpt-4o-mini"]}"#;
        let gateway_action: RouteAction = serde_json::from_str(plan_side_json).unwrap();
        assert_eq!(
            gateway_action.target_model.as_deref(),
            Some("claude-3-5-haiku")
        );
        assert_eq!(gateway_action.fallbacks, vec!["gpt-4o-mini"]);
        let reemitted = serde_json::to_string(&gateway_action).unwrap();
        assert_eq!(reemitted, plan_side_json);
    }

    /// Cross-type wire-compat for the `redact` guardrail flag: JSON written by
    /// the plan side carrying `"redact":true` deserializes into a
    /// `tt_routing::RouteAction` with the flag set and re-emits the same wire
    /// form (same field order + skip_serializing_if gating). Locks `redact` into
    /// the shared wire format alongside `flex`/`compress`.
    #[test]
    fn route_action_redact_cross_type_wire_compat() {
        let plan_side_json = r#"{"target_model":"gpt-4o","redact":true}"#;
        let gateway_action: RouteAction = serde_json::from_str(plan_side_json).unwrap();
        assert_eq!(gateway_action.target_model.as_deref(), Some("gpt-4o"));
        assert!(
            gateway_action.redact,
            "redact must round-trip from plan JSON"
        );
        let reemitted = serde_json::to_string(&gateway_action).unwrap();
        assert_eq!(reemitted, plan_side_json);
    }

    /// The v1 route contract is strict: legacy JSON still carrying the removed
    /// `force_cache_layer` key must be rejected rather than silently changing
    /// meaning when it is canonicalized and persisted.
    #[test]
    fn route_action_legacy_force_cache_layer_is_rejected() {
        let legacy =
            r#"{"target_model":"claude-3-5-haiku","fallbacks":["x"],"force_cache_layer":"l1"}"#;
        let error = serde_json::from_str::<RouteAction>(legacy)
            .expect_err("removed route-action fields must not be silently ignored");
        let message = error.to_string();
        assert!(
            message.contains("unknown field `force_cache_layer`"),
            "unexpected deserialization error: {message}"
        );
    }

    fn make_req_text(model: &str, text: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Text(text.into()),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    #[test]
    fn prompt_contains_matches_case_insensitive_any() {
        let route = Route {
            when: RouteConditions {
                prompt_contains_any_of: vec!["confidential".into(), "salary".into()],
                ..Default::default()
            },
            ..make_route("topic", 10, vec![], "local")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_text("gpt-4o", "This is a Confidential memo"),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_text("gpt-4o", "my SALARY is"),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_text("gpt-4o", "the weather today"),
                &make_ctx(None),
                100
            )
            .is_none());
    }

    #[test]
    fn prompt_contains_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                prompt_contains_any_of: vec!["confidential".into()],
                ..Default::default()
            },
            ..make_route("both", 10, vec!["gpt-4o"], "local")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_text("gpt-4o", "confidential"),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(&make_req_text("gpt-4o", "hello"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn route_action_flex_defaults_false_omits_and_round_trips() {
        // Omitted from JSON when false (back-compat: existing rows unchanged).
        let mut a = make_route("x", 10, vec![], "gpt-5.4").then;
        assert!(
            !serde_json::to_string(&a).unwrap().contains("flex"),
            "flex must be omitted when false"
        );
        // Defaults false when absent from legacy JSON.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.flex);
        // Present + round-trips when true.
        a.flex = true;
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"flex\":true"), "flex=true must serialize: {j}");
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.flex);
    }

    /// The advisory batch-eligibility marker defaults false, is omitted from
    /// JSON when false (back-compat: existing rows unchanged), and round-trips
    /// when true — the same serde gating as `flex`/`compress`/`redact`.
    #[test]
    fn route_action_batch_defaults_false_and_omitted_when_false() {
        // Omitted from JSON when false.
        let mut a = make_route("x", 10, vec![], "gpt-5.5").then;
        assert!(
            !serde_json::to_string(&a).unwrap().contains("batch"),
            "batch must be omitted when false"
        );
        // Defaults false when absent from legacy JSON.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.batch, "batch must default false");
        // Present + round-trips when true.
        a.batch = true;
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            j.contains("\"batch\":true"),
            "batch=true must serialize: {j}"
        );
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.batch);
    }

    /// Cross-type wire-compat for the batch-eligibility marker: JSON written by
    /// the plan side carrying `"batch":true` deserializes into a
    /// `tt_routing::RouteAction` with the flag set and re-emits the same wire
    /// form (same field order + skip_serializing_if gating). Locks `batch` into
    /// the shared wire format alongside `flex`/`compress`/`redact`.
    #[test]
    fn route_action_batch_cross_type_wire_compat() {
        let plan_side_json = r#"{"target_model":"m","batch":true}"#;
        let gateway_action: RouteAction = serde_json::from_str(plan_side_json).unwrap();
        assert_eq!(gateway_action.target_model.as_deref(), Some("m"));
        assert!(gateway_action.batch, "batch must round-trip from plan JSON");
        let reemitted = serde_json::to_string(&gateway_action).unwrap();
        assert_eq!(reemitted, plan_side_json);
    }

    #[test]
    fn route_action_redact_defaults_false_omits_and_round_trips() {
        // Omitted from JSON when false (back-compat: existing rows unchanged).
        let mut a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        assert!(
            !serde_json::to_string(&a).unwrap().contains("redact"),
            "redact must be omitted when false"
        );
        // Defaults false when absent from legacy JSON.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.redact, "redact must default false");
        // Present + round-trips when true.
        a.redact = true;
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            j.contains("\"redact\":true"),
            "redact=true must serialize: {j}"
        );
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.redact);
    }

    #[test]
    fn max_cost_usd_round_trips_and_omits_when_none() {
        let mut a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        assert!(!serde_json::to_string(&a).unwrap().contains("max_cost_usd"));
        a.max_cost_usd = Some(0.1);
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"max_cost_usd\":0.1"));
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert_eq!(back.max_cost_usd, Some(0.1));
    }

    #[test]
    fn cost_gt_matches_above_threshold_only() {
        let route = Route {
            when: RouteConditions {
                estimated_cost_gt: Some(0.02),
                ..Default::default()
            },
            ..make_route("expensive", 10, vec![], "cheaper")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        // est 0.03 > 0.02 → match; 0.01 !> 0.02 → no match; unknown cost → no match.
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, Some(0.03))
            .is_some());
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, Some(0.01))
            .is_none());
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, None)
            .is_none());
    }

    // --- upstream_latency_ms_p95_gt: live-signal condition tests ---

    fn latency_route(threshold_ms: u32) -> Route {
        Route {
            when: RouteConditions {
                upstream_latency_ms_p95_gt: Some(threshold_ms),
                ..Default::default()
            },
            ..make_route("slow-primary", 10, vec![], "faster-alt")
        }
    }

    #[test]
    fn p95_condition_false_without_tracker_signal() {
        // No latency signal (None) → cold start → never matches, even though the
        // route would otherwise match any model. This is the anti-masquerading
        // guarantee: with no real data, the latency route does NOT fire.
        let eng = RoutingEngine::with_routes(vec![latency_route(1000)]);
        assert!(
            eng.evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
                .is_none(),
            "no signal → no match"
        );
        assert!(eng
            .evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, None, false)
            .is_none());
    }

    #[test]
    fn p95_condition_false_when_p95_at_or_below_threshold() {
        let eng = RoutingEngine::with_routes(vec![latency_route(1000)]);
        // p95 == threshold → strictly-greater requirement not met → no match.
        assert!(eng
            .evaluate_with_signals(
                &make_req("gpt-4o"),
                &make_ctx(None),
                100,
                None,
                Some(1000),
                false
            )
            .is_none());
        // p95 below threshold → no match.
        assert!(eng
            .evaluate_with_signals(
                &make_req("gpt-4o"),
                &make_ctx(None),
                100,
                None,
                Some(800),
                false
            )
            .is_none());
    }

    #[test]
    fn p95_condition_true_when_p95_exceeds_threshold() {
        let eng = RoutingEngine::with_routes(vec![latency_route(1000)]);
        let m = eng
            .evaluate_with_signals(
                &make_req("gpt-4o"),
                &make_ctx(None),
                100,
                None,
                Some(1500),
                false,
            )
            .expect("slow primary → alternate route fires");
        assert_eq!(m.then.target_model.as_deref(), Some("faster-alt"));
    }

    #[test]
    fn p95_condition_fed_by_tracker_min_samples_gate() {
        use crate::LatencyTracker;
        // End-to-end with the real tracker: below MIN_SAMPLES p95 is None →
        // condition FALSE; once enough slow samples exist, p95 > threshold → TRUE.
        let tracker = LatencyTracker::new();
        let eng = RoutingEngine::with_routes(vec![latency_route(1000)]);

        // Feed a handful of slow samples — not yet enough to report a p95.
        for _ in 0..(crate::LATENCY_MIN_SAMPLES - 1) {
            tracker.record("openai", "gpt-4o", 3000);
        }
        let p95_cold = tracker.p95("openai", "gpt-4o");
        assert!(p95_cold.is_none(), "still cold");
        assert!(
            eng.evaluate_with_signals(
                &make_req("gpt-4o"),
                &make_ctx(None),
                100,
                None,
                p95_cold,
                false
            )
            .is_none(),
            "cold start (insufficient samples) → no match"
        );

        // One more slow sample crosses MIN_SAMPLES → p95 is reported and high.
        tracker.record("openai", "gpt-4o", 3000);
        let p95_warm = tracker.p95("openai", "gpt-4o");
        assert_eq!(p95_warm, Some(3000));
        assert!(
            eng.evaluate_with_signals(
                &make_req("gpt-4o"),
                &make_ctx(None),
                100,
                None,
                p95_warm,
                false
            )
            .is_some(),
            "warm + slow → alternate route fires"
        );
    }

    #[test]
    fn p95_condition_serde_round_trip_and_omitted_when_none() {
        // Omitted from JSON when None (back-compat with existing route rows).
        let when = RouteConditions::default();
        let j = serde_json::to_string(&when).unwrap();
        assert!(
            !j.contains("upstream_latency_ms_p95_gt"),
            "must be omitted when None: {j}"
        );
        // Present + round-trips when set.
        let when2 = RouteConditions {
            upstream_latency_ms_p95_gt: Some(1500),
            ..Default::default()
        };
        let j2 = serde_json::to_string(&when2).unwrap();
        assert!(j2.contains("\"upstream_latency_ms_p95_gt\":1500"), "{j2}");
        let back: RouteConditions = serde_json::from_str(&j2).unwrap();
        assert_eq!(back.upstream_latency_ms_p95_gt, Some(1500));
        // Legacy JSON without the field still deserializes (serde default None).
        let legacy: RouteConditions = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.upstream_latency_ms_p95_gt, None);
    }

    #[test]
    fn p95_condition_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                upstream_latency_ms_p95_gt: Some(1000),
                ..Default::default()
            },
            ..make_route("both", 10, vec!["gpt-4o"], "faster-alt")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        // Right model + slow p95 → match.
        assert!(eng
            .evaluate_with_signals(
                &make_req("gpt-4o"),
                &make_ctx(None),
                100,
                None,
                Some(2000),
                false
            )
            .is_some());
        // Wrong model → no match even though p95 is high.
        assert!(eng
            .evaluate_with_signals(
                &make_req("claude-x"),
                &make_ctx(None),
                100,
                None,
                Some(2000),
                false
            )
            .is_none());
        // Right model but fast → no match.
        assert!(eng
            .evaluate_with_signals(
                &make_req("gpt-4o"),
                &make_ctx(None),
                100,
                None,
                Some(500),
                false
            )
            .is_none());
    }

    #[test]
    fn cost_lt_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                estimated_cost_lt: Some(0.05),
                ..Default::default()
            },
            ..make_route("cheap-small", 10, vec![], "target")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, Some(0.01))
            .is_some());
        // cost not below threshold → no match
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, Some(0.09))
            .is_none());
        // wrong model → no match
        assert!(eng
            .evaluate_with_cost(&make_req("claude-x"), &make_ctx(None), 100, Some(0.01))
            .is_none());
    }

    // --- canary: traffic_pct + shadow_model serde + sticky split tests ---

    /// `traffic_pct` / `shadow_model` default to `None` from legacy JSON and are
    /// omitted from the wire form when `None` (back-compat with existing rows).
    #[test]
    fn canary_fields_default_none_and_omit() {
        let a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            !j.contains("traffic_pct"),
            "traffic_pct must be omitted when None: {j}"
        );
        assert!(
            !j.contains("shadow_model"),
            "shadow_model must be omitted when None: {j}"
        );
        // Legacy JSON without the fields still deserializes with None defaults.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert_eq!(parsed.traffic_pct, None);
        assert_eq!(parsed.shadow_model, None);
    }

    /// `traffic_pct` + `shadow_model` round-trip and serialize when set.
    #[test]
    fn canary_fields_round_trip_when_set() {
        let mut a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        a.traffic_pct = Some(25);
        a.shadow_model = Some("claude-haiku-4-5".into());
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"traffic_pct\":25"), "{j}");
        assert!(j.contains("\"shadow_model\":\"claude-haiku-4-5\""), "{j}");
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert_eq!(back.traffic_pct, Some(25));
        assert_eq!(back.shadow_model.as_deref(), Some("claude-haiku-4-5"));
    }

    /// Cross-type wire-compat: JSON written by the plan side carrying the canary
    /// fields deserializes into a `tt_routing::RouteAction` with both set and
    /// re-emits the same wire form (same field order + skip_serializing_if
    /// gating). Locks `traffic_pct`/`shadow_model` into the shared wire format
    /// alongside `flex`/`compress`/`redact`. Mirrored by
    /// `tt_plan_core::types::tests::route_action_canary_cross_type_wire_compat`.
    #[test]
    fn route_action_canary_cross_type_wire_compat() {
        let plan_side_json =
            r#"{"target_model":"gpt-4o","traffic_pct":30,"shadow_model":"claude-haiku-4-5"}"#;
        let gateway_action: RouteAction = serde_json::from_str(plan_side_json).unwrap();
        assert_eq!(gateway_action.target_model.as_deref(), Some("gpt-4o"));
        assert_eq!(gateway_action.traffic_pct, Some(30));
        assert_eq!(
            gateway_action.shadow_model.as_deref(),
            Some("claude-haiku-4-5")
        );
        let reemitted = serde_json::to_string(&gateway_action).unwrap();
        assert_eq!(reemitted, plan_side_json);
    }

    /// `sticky_traffic_split` is deterministic: the SAME `(org, key, pct)` always
    /// returns the SAME arm — the property a multi-replica fleet and client
    /// retries depend on. (Replica-independence is implied by purity: no RNG, no
    /// clock, no captured state — the same inputs always produce the same output
    /// in any process.)
    #[test]
    fn sticky_split_is_deterministic_for_same_key() {
        let org = Uuid::now_v7();
        for key in ["req-1", "req-2", "abc", ""] {
            let first = sticky_traffic_split(org, key, 50);
            for _ in 0..1000 {
                assert_eq!(
                    sticky_traffic_split(org, key, 50),
                    first,
                    "same (org,key,pct) must always pick the same arm — key={key:?}"
                );
            }
        }
    }

    /// `pct == 0` never canaries; `pct >= 100` always canaries (and values above
    /// 100 are clamped, not arithmetic-overflowed).
    #[test]
    fn sticky_split_boundary_pcts() {
        let org = Uuid::now_v7();
        for key in ["a", "b", "c", "really-long-idempotency-key-value", ""] {
            assert!(
                !sticky_traffic_split(org, key, 0),
                "pct=0 must never canary"
            );
            assert!(
                sticky_traffic_split(org, key, 100),
                "pct=100 must always canary"
            );
            assert!(
                sticky_traffic_split(org, key, 250),
                "pct>100 clamps to always-canary"
            );
        }
    }

    /// Over many distinct idempotency keys the canary fraction is close to
    /// `traffic_pct`% — i.e. the split actually shapes traffic by the configured
    /// percentage rather than being all-or-nothing.
    #[test]
    fn sticky_split_distribution_approximates_pct() {
        let org = Uuid::now_v7();
        const N: usize = 20_000;
        for pct in [10u32, 30, 50, 70, 90] {
            let canaried = (0..N)
                .filter(|i| sticky_traffic_split(org, &format!("key-{i}"), pct))
                .count();
            let observed = canaried as f64 / N as f64 * 100.0;
            // 20k samples → the std error on the proportion is well under 0.5pp;
            // a 3pp tolerance is comfortably loose yet still proves shaping.
            assert!(
                (observed - pct as f64).abs() < 3.0,
                "pct={pct}: observed canary share {observed:.2}% off target by >3pp"
            );
        }
    }

    /// `Route.paused` is false-omitted on serialize (fixture/snapshot
    /// stability), absent-defaults-false on deserialize, and the
    /// `RouteAction` auto-pause fields default false/None and are omitted
    /// when default — existing JSON rows/fixtures stay byte-stable.
    #[test]
    fn route_paused_false_omitted_on_serialize() {
        let route = make_route("r", 10, vec!["gpt-4o"], "gpt-4o-mini");
        let j = serde_json::to_string(&route).unwrap();
        assert!(
            !j.contains("\"paused\""),
            "paused=false must be omitted: {j}"
        );
        let back: Route = serde_json::from_str(&j).unwrap();
        assert!(!back.paused, "absent key must deserialize to false");

        // RouteAction auto-pause fields: default + omitted when default.
        let a = route.then.clone();
        let ja = serde_json::to_string(&a).unwrap();
        assert!(!ja.contains("auto_pause"), "{ja}");
        assert!(!ja.contains("pause_floor_pass_rate"), "{ja}");
        assert!(!ja.contains("pause_min_verdicts"), "{ja}");
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.auto_pause, "auto_pause must default false (OFF)");
        assert_eq!(parsed.pause_floor_pass_rate, None);
        assert_eq!(parsed.pause_min_verdicts, None);

        // Round-trip when set.
        let mut b = a;
        b.auto_pause = true;
        b.pause_floor_pass_rate = Some(0.85);
        b.pause_min_verdicts = Some(10);
        let jb = serde_json::to_string(&b).unwrap();
        assert!(jb.contains("\"auto_pause\":true"), "{jb}");
        assert!(jb.contains("\"pause_floor_pass_rate\":0.85"), "{jb}");
        assert!(jb.contains("\"pause_min_verdicts\":10"), "{jb}");
        let back: RouteAction = serde_json::from_str(&jb).unwrap();
        assert!(back.auto_pause);
        assert_eq!(back.pause_floor_pass_rate, Some(0.85));
        assert_eq!(back.pause_min_verdicts, Some(10));
    }

    // --- output shaping (research Phase 3.1 + 3.2): minify_json + reasoning caps ---

    /// The output-shaping fields default OFF (`minify_json=false`, both caps
    /// `None`) when absent from legacy JSON, and a default-valued action
    /// re-serializes WITHOUT the new keys — byte back-compat with existing
    /// persisted rows / fixtures.
    #[test]
    fn output_shaping_fields_default_and_omitted() {
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.minify_json, "minify_json must default false (OFF)");
        assert_eq!(parsed.reasoning_max_effort, None);
        assert_eq!(parsed.reasoning_budget_tokens, None);

        let a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        let j = serde_json::to_string(&a).unwrap();
        assert!(!j.contains("minify_json"), "{j}");
        assert!(!j.contains("reasoning_max_effort"), "{j}");
        assert!(!j.contains("reasoning_budget_tokens"), "{j}");
        assert_eq!(
            j, r#"{"target_model":"gpt-4o-mini"}"#,
            "default action must stay byte-identical to the pre-3.x wire form"
        );
    }

    /// All three output-shaping fields serialize → parse → byte-identical
    /// re-emit (the cross-type wire-compat property the plan mirror relies on).
    #[test]
    fn output_shaping_fields_round_trip() {
        let mut a = make_route("x", 10, vec![], "o3-mini").then;
        a.minify_json = true;
        a.reasoning_max_effort = Some("low".into());
        a.reasoning_budget_tokens = Some(8192);
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"minify_json\":true"), "{j}");
        assert!(j.contains("\"reasoning_max_effort\":\"low\""), "{j}");
        assert!(j.contains("\"reasoning_budget_tokens\":8192"), "{j}");
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.minify_json);
        assert_eq!(back.reasoning_max_effort.as_deref(), Some("low"));
        assert_eq!(back.reasoning_budget_tokens, Some(8192));
        let reemitted = serde_json::to_string(&back).unwrap();
        assert_eq!(reemitted, j, "round-trip must be byte-identical");
    }

    // --- not_reasoning_class condition (COST-1U) ---

    #[test]
    fn not_reasoning_class_condition_uses_signal() {
        // Route: when { model_in:["gpt-4o"], not_reasoning_class:true }, then { target_model: Some("gpt-4o-mini") }
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                not_reasoning_class: true,
                ..Default::default()
            },
            ..make_route("down-route", 10, vec!["gpt-4o"], "gpt-4o-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        let req = make_req("gpt-4o");
        let ctx = make_ctx(None);
        // is_reasoning_class=true (Math/Code/Legal/Medical) → must NOT match
        assert!(
            eng.evaluate_with_signals(&req, &ctx, 100, None, None, true)
                .is_none(),
            "reasoning traffic must not match not_reasoning_class route"
        );
        // is_reasoning_class=false (non-reasoning) → must match
        assert!(
            eng.evaluate_with_signals(&req, &ctx, 100, None, None, false)
                .is_some(),
            "non-reasoning traffic must match not_reasoning_class route"
        );
    }

    #[test]
    fn not_reasoning_class_falls_through_to_plain_route() {
        // Route A (higher priority): when { model_in:["gpt-4o"], not_reasoning_class:true }
        let route_a = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                not_reasoning_class: true,
                ..Default::default()
            },
            ..make_route("route-a", 20, vec!["gpt-4o"], "a")
        };
        // Route B (lower priority): when { model_in:["gpt-4o"] } — no not_reasoning_class
        let route_b = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                ..Default::default()
            },
            ..make_route("route-b", 10, vec!["gpt-4o"], "b")
        };
        let eng = RoutingEngine::with_routes(vec![route_a, route_b]);
        let req = make_req("gpt-4o");
        let ctx = make_ctx(None);
        // is_reasoning_class=true → A is blocked, engine falls through to B
        let result_reasoning = eng.evaluate_with_signals(&req, &ctx, 100, None, None, true);
        assert_eq!(
            result_reasoning.and_then(|r| r.then.target_model.as_deref()),
            Some("b"),
            "reasoning traffic must skip not_reasoning_class route and match plain route"
        );
        // is_reasoning_class=false → A matches (higher priority)
        let result_non_reasoning = eng.evaluate_with_signals(&req, &ctx, 100, None, None, false);
        assert_eq!(
            result_non_reasoning.and_then(|r| r.then.target_model.as_deref()),
            Some("a"),
            "non-reasoning traffic must match the higher-priority not_reasoning_class route"
        );
    }

    #[test]
    fn uses_reasoning_class_reports_presence() {
        // Route with not_reasoning_class==true → uses_reasoning_class() returns true.
        let route_on = Route {
            when: RouteConditions {
                not_reasoning_class: true,
                ..Default::default()
            },
            ..make_route("a", 10, vec![], "mini")
        };
        let eng_on = RoutingEngine::with_routes(vec![route_on]);
        assert!(
            eng_on.uses_reasoning_class(),
            "engine with not_reasoning_class route must report uses_reasoning_class=true"
        );

        // Same route with not_reasoning_class==false → false.
        let route_off = Route {
            when: RouteConditions {
                not_reasoning_class: false,
                ..Default::default()
            },
            ..make_route("b", 10, vec![], "mini")
        };
        let eng_off = RoutingEngine::with_routes(vec![route_off]);
        assert!(
            !eng_off.uses_reasoning_class(),
            "engine with no not_reasoning_class route must report false"
        );

        // Disabled route with not_reasoning_class==true → false (disabled doesn't count).
        let mut route_disabled = Route {
            when: RouteConditions {
                not_reasoning_class: true,
                ..Default::default()
            },
            ..make_route("c", 10, vec![], "mini")
        };
        route_disabled.enabled = false;
        let eng_disabled = RoutingEngine::with_routes(vec![route_disabled]);
        assert!(
            !eng_disabled.uses_reasoning_class(),
            "disabled route must not count toward uses_reasoning_class"
        );
    }

    #[test]
    fn not_reasoning_class_defaults_false_and_round_trips() {
        let c = RouteConditions::default();
        assert!(!c.not_reasoning_class);
        let json = serde_json::to_string(&c).unwrap();
        assert!(
            !json.contains("not_reasoning_class"),
            "absent when false: {json}"
        );
        let parsed: RouteConditions = serde_json::from_str(r#"{"model_in":["gpt-4o"]}"#).unwrap();
        assert!(!parsed.not_reasoning_class);
        let on = RouteConditions {
            not_reasoning_class: true,
            ..Default::default()
        };
        let j = serde_json::to_string(&on).unwrap();
        assert!(j.contains(r#""not_reasoning_class":true"#));
        let back: RouteConditions = serde_json::from_str(&j).unwrap();
        assert!(back.not_reasoning_class);
    }

    // --- RoutePanel serde tests ---

    /// A `RouteAction { panel: None, .. }` serializes WITHOUT a "panel" key
    /// (serde skip_serializing_if = "Option::is_none").
    #[test]
    fn route_action_panel_none_omitted_from_json() {
        let a = RouteAction {
            target_model: Some("x".into()),
            panel: None,
            workflow: None,
            ..Default::default()
        };
        let j = serde_json::to_string(&a).unwrap();
        assert!(!j.contains("panel"), "panel must be omitted when None: {j}");
    }

    /// JSON lacking a "panel" key deserializes to `panel: None` (back-compat).
    #[test]
    fn route_action_panel_absent_deserializes_to_none() {
        let a: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert_eq!(a.panel, None);
    }

    /// A `RoutePanel` round-trips: serialize → deserialize produces an equal value.
    #[test]
    fn route_panel_round_trips() {
        let original = RoutePanel {
            strategy: "synthesize".into(),
            members: vec!["gpt-4o".into(), "claude-3-5-haiku".into()],
            arbiter: Some("gpt-4o".into()),
            quorum: Some(2),
            max_cost_usd: Some(0.05),
        };
        let j = serde_json::to_string(&original).unwrap();
        let parsed: RoutePanel = serde_json::from_str(&j).unwrap();
        assert_eq!(parsed, original);
    }

    /// Two different orgs reusing the SAME idempotency-key string get
    /// independent (uncorrelated) splits — keying on `org_id` prevents one org's
    /// key-space from dictating another's arms.
    #[test]
    fn sticky_split_is_org_scoped() {
        // Find a key that lands in different arms for two orgs (exists with high
        // probability since the splits are independent at pct=50).
        let org_a = Uuid::from_u128(1);
        let org_b = Uuid::from_u128(2);
        let differs = (0..1000).any(|i| {
            let k = format!("shared-{i}");
            sticky_traffic_split(org_a, &k, 50) != sticky_traffic_split(org_b, &k, 50)
        });
        assert!(
            differs,
            "two orgs must not be forced into identical arms for every shared key"
        );
    }
}
