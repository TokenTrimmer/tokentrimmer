//! `POST /v1/chat/completions` — OpenAI-compatible chat completion.
//!
//! Request pipeline (see the numbered steps in `handler`):
//!   1. Resolve the provider from `request.model`; 404 on an unknown model.
//!   2. Authenticate (bearer key → `ApiKeyContext`), resolve the org's upstream
//!      credentials, apply the routing engine (may rewrite `req.model`), honor
//!      an explicit provider pin, and compute the per-request cache behavior.
//!   3. Non-streaming: try the negative cache, then L1 exact-match, then the L2
//!      semantic cache; on a miss, single-flight-coalesce and dispatch to the
//!      provider (with cross-provider failover), then best-effort insert into
//!      L1 + L2 and write a `request_logs` row.
//!      Streaming: dispatch directly (failover only on initial establishment).
//!   4. Stamp the `X-TokenTrimmer-*` response headers (cost, cache state,
//!      provider, model, route-matched, warnings).
//!
//! `tt_test_*` keys short-circuit to a deterministic sandbox response (step 2a).

use std::time::{Duration, Instant};

use axum::{
    extract::{Extension, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use tt_cache::{key::cache_key_with, l2_context_text, AliasMapCanonicalizer, CacheEntry, L1Entry};
use tt_telemetry::{
    body_capture::{BodyCaptureRecord, BodyCaptureWriter},
    request_logs::{RequestLogRow, RequestLogWriter},
};
use uuid::Uuid;

use tt_auth::ApiKeyContext;
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::Choice,
    parse_cache_control, CacheControlConfig, CacheMode, ChatCompletionRequest,
    ChatCompletionResponse, Message, MessageContent, ModelPricing, RequestContext,
    RequestDeltaEvidenceState, Usage,
};

use crate::{
    middleware::retrieval::RetrievalTelemetry,
    middleware::trace::TraceId,
    passes::PassEffects,
    retry::{with_retry, RetryPolicy},
    routes::panel,
    routes::sse::{self, CacheInsertContext, StreamLogContext, StreamSpanContext},
    routes::workflows,
    single_flight::wait_for_leader,
    state::{L1Config, L2Config, L2VerifyConfig},
    ApiError, ApiResult, AppState,
};

mod accounting;
mod admission;
mod cache;
mod dispatch;
mod preparation;
mod response;
mod selection;
use dispatch::handle_streaming;
pub(crate) use dispatch::{complete_once, complete_once_budgeted_workflow};

#[cfg(test)]
use accounting::compute_cost_with_flex;
pub(crate) use accounting::{compute_cost, compute_cost_full, CostBreakdown};
use cache::*;
pub(crate) use preparation::prepare;
use response::*;
pub(crate) use selection::apply_routing;

pub(crate) use admission::{
    apply_provider_override, cost_limit_from_header, enforce_cost_limit, estimate_cost_usd,
    provider_override_from_header, route_override_from_header, timeout_ms_from_header,
};
use admission::{fallback_override_from_header, map_failover_error};

#[cfg(test)]
use tt_shared::CacheWriteTier;

/// The fully-prepared per-request setup the branch (streaming | non-streaming)
/// consumes after the shared pipeline (routing → route-action capture →
/// redaction → compression → agentic-budget → cache-behavior → body-capture
/// gating). [`prepare`] runs that shared pipeline and returns this bundle; the
/// chat [`handler`] calls it once before the `if req.stream` branch, then the
/// streaming arm reads these fields directly and the non-streaming arm hands
/// the bundle to [`complete_once`]. The server-side agent loop (slice 1a)
/// rebuilds the same bundle per turn via [`prepare`] so it can re-route/redact
/// each turn.
///
/// All fields are owned (moved out of the shared setup in [`prepare`]), so the
/// field set + types mirror the handler's post-setup locals exactly. This keeps
/// both the carved [`complete_once`] pipeline body and the streaming arm
/// byte-for-byte the handler's, which is what makes the refactor verifiably
/// behavior-preserving.
pub(crate) struct Prepared {
    /// Final served provider (post routing / pin / failover-primary).
    pub provider: std::sync::Arc<dyn tt_shared::Provider>,
    /// The (possibly routed/shaped/redacted/compressed) request to dispatch.
    pub req: ChatCompletionRequest,
    /// Per-request cache behaviour (lookup/insert/ttl), already resolved.
    pub cache_behavior: CacheBehavior,
    /// L2 entitlement (paid-tier only).
    pub l2_allowed: bool,
    /// Format-switch disables L2 for the request (lookup + insert).
    pub skip_l2: bool,
    /// Matched route name for the `X-TokenTrimmer-Route-Matched` header.
    pub route_matched_name: Option<String>,
    /// Matched route id for the `request_logs` row.
    pub matched_route_id: Option<Uuid>,
    /// Immutable `route_versions.id` captured with the matched route during
    /// the cached runtime refresh. NULL is honest for unrouted / legacy-ledger
    /// requests; this is never derived from a mutable route revision.
    pub matched_route_version_id: Option<i64>,
    /// Paused-route passthrough marker.
    pub route_paused: bool,
    /// Originally-requested model (pre-routing) — `gen_ai.request.model`.
    pub requested_model: String,
    /// Pricing for the originally-requested model (baseline when rewritten).
    pub requested_pricing: Option<ModelPricing>,
    /// Whether routing actually rewrote `req.model` (a matched, non-paused route
    /// on the canary/unconditional arm — NOT a control-arm revert). The
    /// streaming arm reads this to price its `request_logs` baseline against the
    /// originally-requested model only on a real rewrite. (`complete_once`
    /// derives its own baseline from `matched_route_id.is_some()` — the
    /// pre-existing per-arm difference — so it ignores this field.)
    pub model_was_rewritten: bool,
    /// Format-switch plan (response-side shaping), if any.
    pub format_switch_plan: Option<crate::shaping::format_switch::FormatSwitchPlan>,
    /// Diff plan (response-side shaping), if any.
    pub diff_plan: Option<crate::shaping::diff::DiffPlan>,
    /// Aggregated request-pass effects threaded into the cost computation.
    pub pass_effects: PassEffects,
    /// Document Lane D4c-v2: the post-route-match, pre-provider-rebind
    /// distillation seam's bookkeeping (raw image tokens the distilled-away image
    /// parts would have spent vs the distilled text tokens they now spend).
    /// All-zero when the route did not opt in / the sidecar is disabled / an
    /// incomplete transaction preserved raw media → `complete_once` prices it via
    /// D0's `document_projection::project` (Gemini guard + fail-open to $0) into
    /// the isolated `doc_vision_saved_est_usd` counterfactual.
    pub doc_distill_booking: crate::document_lane::seam::DistillBookkeeping,
    /// Content-aware compression flywheel label (P1a): the dominant content kind
    /// the content_compress backend compacted (`json`/`csv`/`log`), or `None`
    /// when the route did not opt in / nothing compacted. Recorded on the
    /// `request_logs` row (`content_compress_kind`) only when the pass removed
    /// tokens. Metrics-only (no request content).
    pub content_compress_kind: Option<String>,
    /// Whether the minify-JSON instruction was injected (drives the estimate).
    pub minify_applied: bool,
    /// Whether a reasoning cap fired (judge eligibility).
    pub reasoning_capped: bool,
    /// Whether OpenAI Flex was applied (cost attribution).
    pub flex_applied: bool,
    /// Whether the request was marked batch-eligible (advisory).
    pub batch_marked: bool,
    /// Caller tier (per-tier cache TTL).
    pub caller_tier: Option<tt_shared::CallerTier>,
    /// CO-4: `true` when the auth pre-flight allowed an at-breach PauseShadow
    /// org's request (spend_remaining == Some(0.0)). The panel branch + the
    /// shadow dispatch in `complete_once` read this to SKIP the doubled-spend
    /// routes — a breach no longer 2×-es spend via the shadow. `false` (the
    /// default) for every non-at-breach / non-PauseShadow / dev path.
    pub skip_shadow: bool,
    /// Canary traffic-split arm (`canary`/`control`/None). Bound to the
    /// `traffic_split_arm_owned` local the pipeline reads.
    pub traffic_split_arm_owned: Option<String>,
    /// Canary traffic-split percentage (span attr).
    pub route_traffic_pct: Option<u32>,
    /// Canary shadow model to dispatch concurrently (discarded), if any.
    pub route_shadow_model: Option<String>,
    /// Failover candidate model ids (empty = single-provider dispatch).
    pub failover_candidates: Vec<String>,
    /// Per-provider credentials for the failover candidate set.
    pub failover_creds: std::collections::HashMap<String, ProviderCredentials>,
    /// Applicable route/header cost ceilings for each failover candidate. The
    /// route keeps its historical match-time input estimate; the header
    /// re-estimates the full final prompt with each resolved candidate's
    /// provider tokenizer. `None` means neither ceiling applies; with a ceiling,
    /// unknown candidate pricing fails closed.
    pub failover_cost_check: Option<crate::failover::CandidateCostCheck>,
    /// Route-derived fallback chain (its `is_empty()` selects single vs failover
    /// dispatch — the pipeline reads `route_fallbacks.is_empty()` verbatim).
    pub route_fallbacks: Vec<String>,
    /// Pre-dispatch warning tokens (route_paused / redacted / shaping skips),
    /// extended in-pipeline with response-shaping + dispatch tokens.
    pub warnings: Vec<String>,
    /// Per-request upstream deadline.
    pub request_timeout: Option<Duration>,
    /// Raw bearer (source provider's key; shadow + re-emit credential resolve).
    pub raw_bearer: String,
    /// Retrieval-middleware telemetry (tokens saved upstream).
    pub retrieval_telemetry: RetrievalTelemetry,
    /// Wall-clock request start for `request_logs.latency_ms`.
    pub request_started: Instant,
    /// Serialized request body for capture (`Some` only when capture is armed,
    /// the org is non-anonymous, and the org opted in). Consumed by value.
    pub capture_request_json: Option<Vec<u8>>,
    /// TR-3: the request body serialized BEFORE the conservative `compress`
    /// pass ran (the "before" side of the prompt diff). Populated in `prepare`
    /// only when `route_compress` + a body-capture writer is armed; consumed
    /// in `complete_once` only when `pass_effects.compression_tokens_removed > 0`
    /// (the pass committed) AND capture is on — so an off path or a pass that
    /// removed nothing pays nothing + persists nothing.
    pub pre_compression_request_json: Option<Vec<u8>>,
    /// L2 quality-judge captures (PRE-redaction): the source provider/ctx/req
    /// re-dispatched for the reference answer. Consumed by value (passed by-ref
    /// into the L2-hit gate, moved into `maybe_spawn_quality_judge`).
    pub judge_source_provider: Option<std::sync::Arc<dyn tt_shared::Provider>>,
    pub judge_source_ctx: Option<RequestContext>,
    pub judge_original_req: Option<ChatCompletionRequest>,
    /// Resolved Fusion panel config when the request opted in via the
    /// `X-TokenTrimmer-Panel` header (Phase 1). `None` for every default-path
    /// request — the off-by-default invariant: an absent panel header leaves the
    /// single-model path wire-identical (the only added work is parsing one
    /// absent header + one `None` check at the top of [`complete_once`]). When
    /// `Some`, [`complete_once`] branches to [`panel::complete_panel`] BEFORE any
    /// cache / single-flight check (panels are non-deterministic and bypass both).
    pub panel: Option<panel::PanelConfig>,
    /// Opaque proof that `panel` passed Fusion's static admission gate. This
    /// travels with the prepared request so both buffered and streaming fan-out
    /// revalidate the exact work immediately before any upstream dispatch.
    /// `None` whenever `panel` is `None`.
    pub panel_admission: Option<panel::PanelAdmission>,
    /// Per-provider credentials for the panel member set, keyed by **provider
    /// id** (spec §6.4 step 4). Resolved in [`prepare`] alongside `panel` using
    /// the same store-then-bearer-fallback pattern as the failover pre-resolution
    /// — `run_panel` records a member whose provider id is absent here as
    /// `skipped_no_cred`. Empty (and unused) when `panel` is `None`.
    pub panel_creds: std::collections::HashMap<String, ProviderCredentials>,
    /// The matched route's opt-in **workflow detour** trigger
    /// (`RouteAction.workflow`, CO-1). `Some(_)` makes a matched request run
    /// the referenced workflow instead of (detour) or alongside (shadow) the
    /// upstream call — resolved in [`complete_once`] BEFORE any cache lookup
    /// (workflows are non-deterministic, the same reason `panel` bypasses
    /// cache/single-flight). `None` for the overwhelming majority of routes
    /// (no workflow), so this is a single cheap `Option::take` + `None` check
    /// on the hot path; the un-opted single-model path is byte-identical (off
    /// by default, load-bearing — same invariant as `panel`).
    pub workflow: Option<tt_routing::RouteWorkflow>,
}

/// Cost/route/cache/warning metadata the chat [`handler`] turns into
/// `x-tokentrimmer-*` response headers after a dispatched (non-cache-hit)
/// non-streaming completion. The field set mirrors exactly what the current
/// non-streaming tail reads when assembling the response: the
/// [`attach_cost_headers`] inputs, the `x-tokentrimmer-cache` /
/// `x-tokentrimmer-route-matched` / `x-tokentrimmer-captured` headers, and the
/// [`attach_warnings`] inputs. (The OTel request-span attributes are recorded
/// inside [`complete_once`] alongside the other per-request side effects —
/// `tracing::Span::current()` is identical there and in the handler tail.)
pub(crate) struct CompletionHeaders {
    /// `attach_cost_headers` inputs.
    pub trace_id: Uuid,
    pub provider_id: String,
    pub model_used: String,
    pub cost_breakdown: CostBreakdown,
    /// `x-tokentrimmer-cache` value (`miss` / `none`).
    pub cache_state: &'static str,
    /// `x-tokentrimmer-route-matched` value, if a route matched.
    pub route_matched_name: Option<String>,
    /// Whether the request+response bodies were persisted (`x-tokentrimmer-captured`).
    pub body_captured: bool,
    /// The dispatched request — `attach_warnings` evaluates `dropped_params`
    /// against it (and the served model).
    pub req: ChatCompletionRequest,
    /// The served provider — `attach_warnings` calls `dropped_params` on it.
    pub provider: std::sync::Arc<dyn tt_shared::Provider>,
    /// Pre-dispatch + dispatch warning tokens (comma-joined into the header).
    pub warnings: Vec<String>,
    /// Fusion panel attribution object to merge into the serialized
    /// response body as `tokentrimmer.panel` (Phase 1). `None` on every
    /// non-panel dispatch — the handler then serializes the typed response
    /// byte-identically (off-by-default). `Some(value)` ONLY on the
    /// [`panel::complete_panel`] path; the handler tail merges it into the
    /// top-level JSON object before responding.
    pub panel_body: Option<serde_json::Value>,
}

/// The result of one non-streaming completion through [`complete_once`].
///
/// A dispatched completion returns the typed [`ChatCompletionResponse`] plus the
/// [`CompletionHeaders`] the wrapper turns into the HTTP response — this is the
/// path the agent loop (slice 1a) consumes per turn. A cache hit (L1 / L2 /
/// negative cache / single-flight follower) already built the exact client
/// `Response` it returns today, so it is carried verbatim to keep behavior
/// byte-for-byte identical (the negative-cache hit in particular is an error
/// status body, not a `ChatCompletionResponse`, so it cannot be re-typed
/// losslessly). The loop will consume cache hits via the typed response in a
/// later slice; 1a-0 only carves the pipeline behavior-preservingly.
pub(crate) enum CompletionOutcome {
    /// A fresh provider dispatch: typed response + header metadata.
    Dispatched {
        response: ChatCompletionResponse,
        headers: Box<CompletionHeaders>,
    },
    /// A cache hit (or negative-cache hit): the fully-built client response,
    /// returned verbatim (already carries its own headers).
    CacheHit(Response),
}

/// Stamp run/node attribution from the request context onto a log row.
///
/// Called from [`complete_once`] after the [`RequestLogRow`] is constructed so
/// the attribution is applied in one place that is independently unit-testable.
/// If the call is ever accidentally removed the `attribute_run_copies_run_and_node_id`
/// test in `telemetry_drain_tests` catches it.
pub(crate) fn attribute_run(row: &mut RequestLogRow, ctx: &tt_shared::RequestContext) {
    row.run_id = ctx.run_id;
    row.node_id = ctx.node_id;
}

/// Handler for `POST /v1/chat/completions`.
///
/// Resolves the provider for `req.model`, builds a [`RequestContext`] from the
/// auth-middleware-supplied [`ApiKeyContext`] (when present) and the
/// credential store, then dispatches to either the streaming or non-streaming
/// provider path. Falls back to a synthetic context when auth is not wired
/// (tests, dev) — preserving the legacy "forward the Bearer token verbatim"
/// behaviour so existing integrations keep working.
pub async fn handler(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    retrieval: Option<Extension<RetrievalTelemetry>>,
    headers: HeaderMap,
    Json(mut req): Json<ChatCompletionRequest>,
) -> ApiResult<Response> {
    // Wall-clock start — fed into `request_logs.latency_ms`.
    let request_started = Instant::now();
    let retrieval_telemetry = retrieval.map(|Extension(v)| v).unwrap_or_default();

    // 1. Resolve provider — 404 for unknown models. (May be re-resolved inside
    //    `prepare` after routing rewrites req.model.) `resolve` falls back to
    //    provider inference for valid-but-unlisted model ids so they dispatch
    //    instead of 404ing.
    let provider = state
        .registry
        .resolve(&req.model)
        .ok_or_else(|| ApiError::ModelNotFound {
            model: req.model.clone(),
        })?;

    // 2. Pull the api_key credential (Authorization: Bearer, with x-api-key as
    //    the Anthropic-SDK alias). This is the customer's TokenTrimmer key — the
    //    auth middleware already verified it (when configured); we re-read it
    //    here only to detect the sandbox `tt_test_*` short-circuit. Using the
    //    shared `extract_bearer` guarantees the x-api-key alias is honored here
    //    too (so a Claude Code user with ANTHROPIC_API_KEY + a tt_test_* key
    //    still hits the sandbox path).
    let raw_bearer = crate::middleware::auth::extract_bearer(&headers).unwrap_or_default();

    // Explicit provider pin (X-TokenTrimmer-Provider), applied after routing below.
    let provider_pin = provider_override_from_header(&headers);
    // Forced route (X-TokenTrimmer-Route) — passed into apply_routing below.
    let forced_route = route_override_from_header(&headers);
    // Per-request upstream deadline (X-TokenTrimmer-Timeout-Ms).
    let request_timeout = timeout_ms_from_header(&headers).map(std::time::Duration::from_millis);

    // 2a. Sandbox short-circuit: `tt_test_*` keys return a deterministic
    //     synthetic response without contacting any real provider — for
    //     E2E test suites and customer integration testing without spend.
    if raw_bearer.starts_with("tt_test_") {
        return Ok(sandbox_response(&req, trace.0.as_str()));
    }

    // Prefer the TraceId extension set by trace middleware; fall back to header
    // (for callers that pre-supply it) or a fresh UUID.
    let trace_id = if !trace.0.is_empty() {
        Uuid::parse_str(&trace.0).unwrap_or_else(|_| Uuid::now_v7())
    } else {
        headers
            .get("x-tokentrimmer-trace-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_else(Uuid::now_v7)
    };

    // Standard logical-request key shared by sticky routing and durable
    // per-provider-attempt admission. Without one, the trace id keeps this
    // request internally stable but deliberately does not deduplicate a later
    // client retry.
    let idempotency_key = super::idempotency_key_from_headers(&headers)?.unwrap_or_else(|| {
        if trace_id != Uuid::nil() {
            trace_id.to_string()
        } else {
            Uuid::now_v7().to_string()
        }
    });

    // 2b. Identity + credentials.
    //
    // If the auth middleware attached an `ApiKeyContext`, use the real
    // org_id/key_id and look up the customer's stored upstream credentials
    // from the credential store. If anything is missing, fall back to the
    // legacy behavior of forwarding the raw Bearer to the upstream provider.
    // That fallback is what keeps `tt_test_*` E2E tests and unauthenticated
    // dev calls working through this handler.
    let (org_id, api_key_id, caller_tier, skip_shadow) = match auth_ctx.as_deref() {
        Some(c) => (c.org_id, c.key_id, c.tier, c.skip_shadow),
        None => (Uuid::nil(), Uuid::nil(), None, false),
    };
    // L2 semantic cache is a paid-tier entitlement (BudgetLimits.l2_cache:
    // false for Free, true for Pro/Team/Scale; internal orgs resolve to Scale).
    // Unauthenticated/dev (tier None) is treated as Free → no L2.
    let l2_allowed = matches!(
        caller_tier,
        Some(
            tt_shared::CallerTier::Pro | tt_shared::CallerTier::Team | tt_shared::CallerTier::Scale
        )
    );
    let source_provider_id = provider.id().to_string();
    // BYO-only (P0 #9): `None` means a VERIFIED org has no stored credential
    // for the source provider. The error is deferred rather than raised here —
    // routing below may rewrite the request to a provider the org HAS
    // onboarded (the cross-provider re-resolve and the per-candidate failover
    // map fail closed on their own) — so the guard after pin/failover
    // resolution returns `missing_provider_credential` only when the serving
    // provider still needs the missing source credential. Until then
    // `ctx.credentials` holds the raw bearer as an inert placeholder (the old
    // legacy value); it is never dispatched on the guarded paths.
    let resolved_source_creds =
        resolve_credentials(&state, org_id, provider.id(), &raw_bearer).await;
    let source_creds_missing = resolved_source_creds.is_none();
    let credentials = resolved_source_creds.unwrap_or_else(|| ProviderCredentials {
        api_key: SecretString::new(raw_bearer.clone()),
        base_url: None,
        extra_headers: Vec::new(),
    });

    let mut ctx = RequestContext {
        budget_dispatch: crate::budget_reservation::dispatch_state_for_idempotency(
            org_id,
            api_key_id,
            &idempotency_key,
        ),
        trace_id,
        org_id,
        api_key_id,
        credentials,
        tag: headers
            .get("x-tokentrimmer-tag")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        deadline: request_timeout,
        run_id: None,
        node_id: None,
    };

    // 2c+. Shared per-request setup (routing → route-action capture → redaction
    //      → compression → agentic-budget → cache-behavior → body-capture
    //      gating). Factored into `prepare` so the server-side agent loop
    //      (slice 1a) can rebuild the same `Prepared` bundle per turn. `&mut ctx`
    //      / `&mut req` are mutated in place exactly as the former inline setup
    //      did (cross-provider credential rebinding, model rewrite, request
    //      shaping); the final `req`/`provider` are moved into the returned
    //      `Prepared`.
    let mut prep = prepare(
        &state,
        &mut ctx,
        &mut req,
        &headers,
        provider,
        provider_pin,
        forced_route,
        request_timeout,
        idempotency_key,
        raw_bearer,
        org_id,
        source_provider_id,
        source_creds_missing,
        caller_tier,
        l2_allowed,
        retrieval_telemetry,
        request_started,
        // The chat path is never a mechanical agent-loop sub-step: pass `false`
        // so the `prepare` mechanical down-route block is inert (behavior-
        // preserving — `/v1/chat/completions` stays byte-identical).
        false,
        skip_shadow,
    )
    .await?;

    // 3. Branch: streaming vs non-streaming. Both arms consume `prep` (the
    //    streaming arm reads the fields it needs; the non-streaming arm hands
    //    the whole bundle to `complete_once`).
    // A `stream: true` panel request streams the ARBITER answer over SSE
    // (Phase 5): take the panel config out of `prep` and hand the bundle to
    // `complete_panel_streaming`, which fans out the member legs, establishes
    // the arbiter as a chunk stream, and defers the ONE aggregate
    // `provider='panel'` row to the stream-end DropGuard. A `stream: true`
    // request with NO panel header (the overwhelming majority) goes down
    // `handle_streaming` unchanged — off-by-default.
    if prep.req.stream {
        // Streaming workflow detour is unsupported in v1 (the workflow runs to
        // a synthesized aggregate answer, not a chunk stream). A `stream:true`
        // request on a workflow route warns + falls through to single-model
        // streaming dispatch, leaving the config dropped. Best-effort + off by
        // default (`prep.workflow` is `None` for the overwhelming majority).
        if let Some(cfg) = prep.workflow.take() {
            prep.warnings.push(format!(
                "workflow-{}: streaming detour unsupported, single-model fallback",
                cfg.workflow_id
            ));
        }
        // CO-4: an at-breach PauseShadow org skips the panel fan-out (no
        // doubled spend via the streaming arbiter dispatch). Same gate as the
        // non-streaming complete_once block — drop the panel so the request
        // flows through as a single-model stream instead.
        if prep.skip_shadow {
            prep.panel = None;
            prep.panel_admission = None;
        }
        if let Some(cfg) = prep.panel.take() {
            let admission = prep.panel_admission.take().ok_or_else(|| {
                ApiError::Internal("panel configuration missing its admission proof".to_string())
            })?;
            // `complete_panel_streaming` returns `Result<Response, ApiError>`;
            // `ApiError` is `IntoResponse`, so a fail-closed error (quorum-unmet
            // 502, arbiter-establishment failure) becomes a proper non-200
            // response — and, critically, returns BEFORE any stream is opened
            // (no 200, no request_logs row).
            return crate::routes::panel::complete_panel_streaming(
                &state, &ctx, prep, cfg, admission,
            )
            .await;
        }
        return handle_streaming(&state, &ctx, prep).await;
    }
    // Non-streaming: hand the prepared per-request setup to `complete_once`
    // (the carved, reusable completion pipeline the server-side agent loop also
    // calls per turn) and assemble the HTTP response from its typed outcome. A
    // cache hit already built its client `Response`; a dispatched completion
    // returns the typed body + header metadata.
    match complete_once(&state, &ctx, prep).await? {
        // Cache hit (L1 / L2 / negative cache / single-flight follower): the
        // fully-built client response is returned verbatim.
        CompletionOutcome::CacheHit(resp) => {
            // P0-1/P0-3: settle the served request as a cache hit. Advances the
            // served counter (COGS guard) but NOT the billed monthly counter —
            // cache hits do not consume an included request. The dispatched arm
            // already settled `cached=false` inside `complete_once`.
            state
                .spend_sink()
                .settle(ctx.org_id, ctx.api_key_id, true, Utc::now());
            // P2: synchronous served-counter bump, in-band, once per served
            // cache hit — the cheap sync truth to diff against the async-written
            // `request_logs` row (the L1/L2-hit row is spawned fire-and-forget
            // inside `complete_once`).
            crate::metrics::record_request_served("chat", "cache_hit");
            Ok(resp)
        }
        // Dispatched completion: build the HTTP response from the typed body +
        // the cost/route/cache/warning metadata, via the same
        // `attach_cost_headers` / `attach_warnings` the tail used inline.
        CompletionOutcome::Dispatched { response, headers } => {
            // P2: synchronous served-counter bump, in-band, once per served
            // dispatched completion — the cheap sync truth to diff against the
            // async-written `request_logs` row (spawned fire-and-forget inside
            // `complete_once`). A divergence ⇒ lost billing writes.
            crate::metrics::record_request_served("chat", "dispatch");
            let CompletionHeaders {
                trace_id,
                provider_id,
                model_used,
                cost_breakdown,
                cache_state,
                route_matched_name,
                body_captured,
                req,
                provider,
                warnings,
                panel_body,
            } = *headers;

            // 5. Serialize body and attach TokenTrimmer extension headers.
            //
            // Panel path (Phase 1): when `panel_body` is `Some`, merge the
            // `tokentrimmer.panel` attribution object into the top-level
            // response JSON object before responding. The typed
            // `ChatCompletionResponse` is a closed struct (no extension field),
            // so the per-leg breakdown is injected here at the serialization
            // boundary. `None` on every non-panel dispatch ⇒ the typed response
            // is serialized byte-identically (off-by-default invariant).
            let mut http_response = match panel_body {
                Some(panel_value) => {
                    // Serialize the typed response, then graft the panel object
                    // onto the top-level JSON map. A serialization failure falls
                    // back to the plain typed body (never drops the answer).
                    match serde_json::to_value(&response) {
                        Ok(serde_json::Value::Object(mut map)) => {
                            map.insert(
                                "tokentrimmer".to_string(),
                                serde_json::json!({ "panel": panel_value }),
                            );
                            Json(serde_json::Value::Object(map)).into_response()
                        }
                        _ => Json(response).into_response(),
                    }
                }
                None => Json(response).into_response(),
            };
            attach_cost_headers(
                http_response.headers_mut(),
                trace_id,
                &provider_id,
                &model_used,
                &cost_breakdown,
            );
            if let Ok(v) = cache_state.parse() {
                http_response
                    .headers_mut()
                    .insert("x-tokentrimmer-cache", v);
            }
            if let Some(name) = route_matched_name.as_deref() {
                if let Ok(v) = name.parse() {
                    http_response
                        .headers_mut()
                        .insert("x-tokentrimmer-route-matched", v);
                }
            }
            // Present ONLY when the org opted in and the request+response bodies
            // were persisted to the encrypted capture sink — absent on the
            // default (capture-off) path AND for armed-but-not-opted-in orgs,
            // which both stay byte-identical.
            if body_captured {
                http_response.headers_mut().insert(
                    "x-tokentrimmer-captured",
                    axum::http::HeaderValue::from_static("true"),
                );
            }
            attach_warnings(
                http_response.headers_mut(),
                provider.as_ref(),
                &req,
                &model_used,
                &warnings,
            );
            Ok(http_response)
        }
    }
}

/// Run `fut` under an optional per-request deadline; on expiry return 408.
pub(crate) async fn with_request_timeout<T>(
    timeout: Option<std::time::Duration>,
    fut: impl std::future::Future<Output = ApiResult<T>>,
) -> ApiResult<T> {
    match timeout {
        Some(d) => match tokio::time::timeout(d, fut).await {
            Ok(r) => r,
            Err(_) => Err(ApiError::RequestTimeout {
                ms: d.as_millis().min(u128::from(u64::MAX)) as u64,
            }),
        },
        None => fut.await,
    }
}

/// Stamp `X-TokenTrimmer-Route-Matched` with the applied route's name (no-op when
/// `name` is `None` or not header-safe).
fn with_route_matched(mut resp: Response, name: Option<&str>) -> Response {
    if let Some(name) = name {
        if let Ok(v) = name.parse() {
            resp.headers_mut().insert("x-tokentrimmer-route-matched", v);
        }
    }
    resp
}

/// Insert the `X-TokenTrimmer-*` cost/savings response headers (trace-id,
/// provider, model-used, cost, baseline, headline saved, provider-cache saved,
/// flex saved, and compression saved).
pub(crate) fn attach_cost_headers(
    headers: &mut axum::http::HeaderMap,
    trace_id: Uuid,
    provider_id: &str,
    model_used: &str,
    cost: &CostBreakdown,
) {
    let pairs: &[(&str, String)] = &[
        ("x-tokentrimmer-trace-id", trace_id.to_string()),
        ("x-tokentrimmer-provider", provider_id.to_string()),
        ("x-tokentrimmer-model-used", model_used.to_string()),
        ("x-tokentrimmer-cost-usd", format!("{:.6}", cost.cost_usd)),
        (
            "x-tokentrimmer-baseline-cost-usd",
            format!("{:.6}", cost.baseline_cost_usd),
        ),
        // Strictly TokenTrimmer-attributed (routing / TT cache / failover) —
        // excludes the provider's automatic prompt-cache discount, which is
        // reported separately below so the headline survives invoice
        // reconciliation.
        (
            "x-tokentrimmer-saved-usd",
            format!("{:.6}", cost.tt_saved_usd()),
        ),
        (
            "x-tokentrimmer-provider-cache-saved-usd",
            format!("{:.6}", cost.provider_cache_saved_usd),
        ),
        // Flex-tier-attributed savings (standard baseline − flex cost) — a
        // distinct source from routing/cache, included in `saved_usd`. Zero
        // when the request was not served via OpenAI Flex.
        (
            "x-tokentrimmer-flex-saved-usd",
            format!("{:.6}", cost.flex_saved_usd),
        ),
        // Compression-pass-attributed savings (removed input tokens × served
        // input rate) — a distinct source from routing/cache/flex, included in
        // `saved_usd`. Zero when the request was not compressed.
        (
            "x-tokentrimmer-compression-saved-usd",
            format!("{:.6}", cost.compression_saved_usd),
        ),
        // Document-compaction-attributed savings (Document Lane D2: removed
        // input tokens from LARGE documents × served input rate) — a distinct
        // source from compression, included in `saved_usd` via the same
        // baseline fold. 0.000000 unless the route opted into doc_compaction.
        (
            "x-tokentrimmer-doc-compaction-saved-usd",
            format!("{:.6}", cost.doc_compaction_saved_usd),
        ),
        // NEGATIVE savings entry: estimated cost of a deliberate
        // NON-deterministic stable-prefix mutation — already subtracted from
        // `saved_usd` pre-clamp. 0.000000 on every request whose stable
        // prefix was untouched (and for all redaction traffic: an
        // ingress-deterministic mutation dispatches byte-identical prefixes
        // every turn, so it busts nothing). Never folded into cost/baseline
        // (those reconcile against the realized invoice).
        (
            "x-tokentrimmer-cache-bust-usd",
            format!("{:.6}", cost.cache_bust_penalty_usd),
        ),
        // NEGATIVE savings entry: REAL summarizer-LLM aux spend (Sub-lever 2b),
        // already subtracted from `saved_usd` pre-clamp. Aux spend is taxed,
        // never free (spec §4.4 item 3). 0.000000 on every request that ran no
        // summarizer. Never folded into cost/baseline (the summarizer call bills
        // the org on its own credentials, not this request's served dispatch).
        (
            "x-tokentrimmer-summarizer-tax-usd",
            format!("{:.6}", cost.summarizer_tax_usd),
        ),
        // ADVISORY forgone Batch-API discount for batch-eligible requests —
        // what the future async Batch Lane would have saved, priced from the
        // served model's real catalog batch rate. NEVER included in
        // `saved-usd` (the request was dispatched synchronously and billed
        // `cost-usd` in full). 0.000000 for all unmarked traffic.
        (
            "x-tokentrimmer-batch-forgone-usd",
            format!("{:.6}", cost.batch_forgone_usd),
        ),
        // ESTIMATED minify saving (pretty re-render of the emitted JSON minus
        // the tokens actually emitted, priced at the billed output rate) —
        // NEVER included in `saved-usd` (an estimate of an unmeasurable
        // counterfactual must not enter the invoice-reconciled headline).
        // 0.000000 for all un-minified traffic, non-JSON responses, and
        // streaming (v1 meters but does not estimate).
        (
            "x-tokentrimmer-minify-saved-est-usd",
            format!("{:.6}", cost.minify_saved_est_usd),
        ),
        // MEASURED diff-lane saving (reconstructed artifact tokens − billed
        // patch tokens, output-rate-priced) — included in `saved-usd` via the
        // baseline fold, isolated here for the methodology breakdown.
        // 0.000000 for all undiffed traffic.
        (
            "x-tokentrimmer-diff-saved-usd",
            format!("{:.6}", cost.diff_saved_usd),
        ),
        // ESTIMATED format-switch saving ("Est" = the estimated label): a
        // JSON-equivalent reconstruction, NEVER included in `saved-usd` /
        // baseline (those reconcile against the provider invoice).
        // 0.000000 for all unswitched traffic.
        (
            "x-tokentrimmer-format-switch-saved-est-usd",
            format!("{:.6}", cost.format_switch_saved_est_usd),
        ),
        // Realized cost of a FAILED diff patch attempt (fail-closed double
        // dispatch) — already folded into `cost-usd` (real invoice spend),
        // duplicated here so the retry tax is unpickable. 0.000000 unless a
        // patch failed on this trace.
        (
            "x-tokentrimmer-diff-failed-cost-usd",
            format!("{:.6}", cost.diff_failed_cost_usd),
        ),
        // ESTIMATED Document-Lane vision-avoided saving (D4): a COUNTERFACTUAL
        // (the dispatched request never contained the image) — NEVER included in
        // `saved-usd` / baseline (isolated, like the minify estimate). 0.000000
        // for all traffic in D4a; the seam that books a non-zero value is D4c.
        (
            "x-tokentrimmer-doc-vision-saved-est-usd",
            format!("{:.6}", cost.doc_vision_saved_est_usd),
        ),
        // ESTIMATED content-aware compression saving (P1a): the input tokens the
        // content_compress backend removed, input-rate-priced — a conservative
        // estimate, NEVER included in `saved-usd` / baseline (isolated, like the
        // minify + doc-vision estimates). 0.000000 unless the route opted in.
        (
            "x-tokentrimmer-content-compress-saved-est-usd",
            format!("{:.6}", cost.content_compress_saved_est_usd),
        ),
    ];

    for (name, value) in pairs {
        if let Ok(v) = value.parse() {
            headers.insert(*name, v);
        }
    }
}

/// Bridge a [`CostBreakdown`] + token counts to the telemetry crate's
/// [`tt_telemetry::gen_ai::RequestSpanCost`] (pulling `saved_usd` from
/// [`CostBreakdown::tt_saved_usd`] — the same headline the header carries).
fn span_cost(
    cost: &CostBreakdown,
    input_tokens: u64,
    output_tokens: u64,
) -> tt_telemetry::gen_ai::RequestSpanCost {
    tt_telemetry::gen_ai::RequestSpanCost {
        input_tokens,
        output_tokens,
        cost_usd: cost.cost_usd,
        baseline_cost_usd: cost.baseline_cost_usd,
        saved_usd: cost.tt_saved_usd(),
        provider_cache_saved_usd: cost.provider_cache_saved_usd,
    }
}

/// Record the OpenTelemetry GenAI semantic-convention attributes plus the
/// TokenTrimmer cost attributes onto the current request span (the gateway's
/// `http_request` span from [`crate::middleware::trace`]).
///
/// Pulls from the same per-request values stamped onto the `x-tokentrimmer-*`
/// response headers (token usage + [`CostBreakdown`]) — nothing is recomputed.
/// `request_model` is the model the caller asked for; `response_model` is the
/// model that actually served the request (they differ after routing /
/// cross-model failover). On a span with no OpenTelemetry layer (dev `fmt`
/// subscriber) this is a cheap no-op. This module handles only
/// `POST /v1/chat/completions`, so the operation is always `chat`.
#[allow(clippy::too_many_arguments)]
fn record_request_span_attributes(
    request_model: &str,
    response_model: &str,
    provider_id: &str,
    cost: tt_telemetry::gen_ai::RequestSpanCost,
    cache_outcome: &str,
    route: Option<&str>,
    traffic_split_pct: Option<u32>,
    shadow_model: Option<&str>,
    shadow_cost_usd: Option<f64>,
) {
    tt_telemetry::gen_ai::record_request_attributes(
        &tracing::Span::current(),
        &tt_telemetry::gen_ai::RequestSpanAttributes {
            provider_id,
            request_model,
            response_model,
            operation: "chat",
            cost,
            cache_outcome: Some(cache_outcome),
            route,
            traffic_split_pct,
            shadow_model,
            shadow_cost_usd,
            // Single-model dispatch never involves a panel.
            panel_strategy: None,
            panel_leg_count: None,
            panel_quorum_required: None,
            panel_quorum_met: None,
        },
    );
}

/// Decide which credentials to send to the upstream provider.
///
/// Precedence:
///
/// 1. The credential store (if configured) — production path: per-org
///    upstream key, possibly with `base_url` / `extra_headers`.
/// 2. The raw Bearer token as a fallback — ONLY for anonymous callers (no
///    verified org) or when no store is configured: preserves the legacy
///    BYO-key passthrough where customers pointed their OpenAI SDK at the
///    gateway with their own upstream key in the `Authorization` header.
///
/// Returns `None` exactly when a credential store is configured and a
/// VERIFIED org has no stored credential for `provider_id` (BYO-only,
/// P0 #9). The caller must surface that as
/// [`ApiError::MissingProviderCredential`] — never forward the org's
/// `tt_live_*` bearer upstream and never substitute an operator key.
pub(crate) async fn resolve_credentials(
    state: &AppState,
    org_id: Uuid,
    provider_id: &str,
    raw_bearer: &str,
) -> Option<ProviderCredentials> {
    resolve_credentials_for(state, org_id, provider_id, raw_bearer, true).await
}

/// Resolve upstream credentials for `provider_id`.
///
/// With a credential store configured (the hosted per-org model), a store hit
/// wins; on a miss the raw-Bearer fallback applies only when
/// `allow_bearer_fallback` is true (the source provider's key is its own) AND
/// the caller is anonymous (`org_id` nil — no verified `ApiKeyContext`). A
/// verified org's bearer is its TokenTrimmer key, never a valid upstream key,
/// so a store miss returns `None` and the handler answers with an actionable
/// `missing_provider_credential` error instead of a confusing upstream 401
/// (BYO-only, P0 #9 — the operator's env keys are not even in the store
/// composition unless explicitly opted in at boot). A cross-provider target
/// with no stored key likewise returns `None` (fail closed — we must not
/// forward the source key to a different provider).
///
/// With **no** store configured (dev / dogfood / BYO-key passthrough) there is
/// no per-provider credential model to enforce, so the raw Bearer is forwarded
/// to every provider — never fail-closed.
pub(crate) async fn resolve_credentials_for(
    state: &AppState,
    org_id: Uuid,
    provider_id: &str,
    raw_bearer: &str,
    allow_bearer_fallback: bool,
) -> Option<ProviderCredentials> {
    let bearer = || ProviderCredentials {
        api_key: SecretString::new(raw_bearer.to_string()),
        base_url: None,
        extra_headers: Vec::new(),
    };
    let Some(store) = state.credential_store.as_ref() else {
        // Legacy passthrough: no per-org credential store.
        return Some(bearer());
    };
    match store.get(org_id, provider_id).await {
        Ok(Some(c)) => Some(c),
        // Anonymous BYO passthrough only — a verified org's store miss fails
        // closed (see doc comment).
        Ok(None) if allow_bearer_fallback && org_id.is_nil() => Some(bearer()),
        Ok(None) => None,
        Err(e) => {
            // Store ERRORS (DB blip) stay best-effort: log and keep the
            // legacy bearer fallback. This never serves an operator env key —
            // only the caller's own bearer.
            tracing::warn!(error = %e, "credential store lookup failed");
            allow_bearer_fallback.then(bearer)
        }
    }
}

/// Fire-and-forget `request_logs` insert. The handler MUST NOT block on
/// this — telemetry rows are best-effort, and a slow DB write would
/// dominate p50.
///
/// Durability (P2): the spawned write goes through
/// [`tt_telemetry::request_logs::write_with_retry`], which retries a TRANSIENT
/// DB blip a bounded number of times (idempotent-safe — the row's `id` is the
/// `request_logs` PK, so a retry cannot double-insert). On PERMANENT failure
/// the loud, alertable `tt_request_log_write_failed_total` counter is bumped
/// (alongside the existing `tracing` log) so lost billing rows surface instead
/// of vanishing silently.
///
/// When `tracker` is `Some` (REL-3), the spawned future is tracked so a
/// graceful shutdown can drain all in-flight writes via `close()` + `wait()`.
/// When `None`, falls back to bare `tokio::spawn` (existing behavior).
///
/// NOTE: the synchronous `tt_requests_served_total` served-counter bump is the
/// CALLER's responsibility — it must run IN-BAND (on the request thread),
/// never inside this spawn, so it is the cheap sync truth to diff against the
/// async row count.
pub(crate) fn spawn_request_log(
    tracker: Option<&tokio_util::task::TaskTracker>,
    writer: Option<&std::sync::Arc<dyn RequestLogWriter>>,
    row: RequestLogRow,
) {
    let Some(writer) = writer else { return };
    let writer = writer.clone();
    let fut = async move {
        if let Err(e) = tt_telemetry::request_logs::write_with_retry(
            writer.as_ref(),
            row,
            tt_telemetry::request_logs::DEFAULT_WRITE_ATTEMPTS,
            tt_telemetry::request_logs::DEFAULT_WRITE_BACKOFF,
        )
        .await
        {
            // Permanent failure after the bounded retry: a billing row was
            // served (counted) but never persisted. Loud + alertable.
            tracing::error!(error = %e, "request_logs write failed permanently after retries");
            crate::metrics::record_request_log_write_failed("chat");
        }
    };
    match tracker {
        Some(t) => {
            t.spawn(fut);
        }
        None => {
            tokio::spawn(fut);
        }
    }
}

/// Fire-and-forget encrypted body capture. The writer itself enforces per-org
/// opt-in and retention; handler latency should not depend on storage.
///
/// When `tracker` is `Some` (REL-3), the spawned future is tracked so a
/// graceful shutdown can drain all in-flight writes via `close()` + `wait()`.
/// When `None`, falls back to bare `tokio::spawn` (existing behavior).
fn spawn_body_capture(
    tracker: Option<&tokio_util::task::TaskTracker>,
    writer: Option<&std::sync::Arc<dyn BodyCaptureWriter>>,
    record: BodyCaptureRecord,
) {
    let Some(writer) = writer else { return };
    let writer = writer.clone();
    let fut = async move {
        if let Err(e) = writer.record(record).await {
            tracing::warn!(error = %e, "request body capture write failed");
        }
    };
    match tracker {
        Some(t) => {
            t.spawn(fut);
        }
        None => {
            tokio::spawn(fut);
        }
    }
}

/// Outcome of a canary **shadow** dispatch (`RouteAction::shadow_model`).
///
/// The shadow response itself is DISCARDED — only its cost/usage are kept, in
/// their own fields, so the doubled spend is attributed to the experiment and
/// never folded into the primary cost.
pub(crate) struct ShadowOutcome {
    /// The shadow candidate model that was dispatched (recorded for the row /
    /// span even on error, so a failed shadow is still auditable).
    pub(crate) model: String,
    /// Cost (USD) the shadow dispatch billed. `0.0` when the shadow errored or
    /// the shadow model has no pricing.
    pub(crate) cost_usd: f64,
    /// Whether the shadow upstream call actually completed (`true`) or errored /
    /// timed out (`false`). A `true` value is the proof the candidate provider
    /// was really called — instrumented in tests via a mock provider.
    pub(crate) succeeded: bool,
}

/// Dispatch a canary shadow candidate (`RouteAction::shadow_model`) ONCE,
/// non-streaming, with NO failover, discarding the response and returning only
/// its cost/usage. SEPARATE from the primary dispatch in every way:
///
/// * single candidate — the shadow never fails over to another model/provider;
/// * its own short `shadow_timeout` (the result is discarded, so it must never
///   delay the primary response);
/// * its cost is computed independently and returned for SEPARATE recording
///   (never added to the primary `cost_usd`).
///
/// Fails closed on a missing shadow credential (returns `succeeded=false`,
/// `cost=0`) — it never forwards the caller's bearer to an unintended provider.
/// `base_req` is the request as the gateway would dispatch it (post
/// redaction/compression); only `model` is swapped to the shadow candidate so
/// the shadow exercises the SAME prompt as the primary.
async fn dispatch_shadow(
    state: &AppState,
    ctx: &RequestContext,
    base_req: &ChatCompletionRequest,
    shadow_model: &str,
    raw_bearer: &str,
) -> ShadowOutcome {
    let mut outcome = ShadowOutcome {
        model: shadow_model.to_string(),
        cost_usd: 0.0,
        succeeded: false,
    };
    let Some(shadow_provider) = state.registry.resolve(shadow_model) else {
        // Should be unreachable: config validation rejects an unresolvable
        // shadow model at route-creation time. Guard anyway (fail closed).
        tracing::warn!(
            shadow_model = %shadow_model,
            "shadow model did not resolve to a provider — skipping shadow dispatch"
        );
        return outcome;
    };
    // Resolve the shadow provider's credential. `allow_bearer_fallback=true`
    // here is gated INSIDE `resolve_credentials_for` to anonymous orgs only
    // (`org_id.is_nil()`), so a VERIFIED org with no stored shadow credential
    // fails closed (returns None → no shadow, no bearer leak). With no
    // credential store wired (dev/dogfood) the bearer is forwarded, matching the
    // primary path.
    let Some(shadow_creds) =
        resolve_credentials_for(state, ctx.org_id, shadow_provider.id(), raw_bearer, true).await
    else {
        tracing::warn!(
            shadow_model = %shadow_model,
            provider = shadow_provider.id(),
            "no credential for shadow provider — skipping shadow dispatch (fail closed)"
        );
        return outcome;
    };

    // Build the shadow request: same prompt, shadow model, never streaming.
    let mut shadow_req = base_req.clone();
    shadow_req.model = shadow_model.to_string();
    shadow_req.stream = false;
    let shadow_ctx = RequestContext {
        credentials: shadow_creds,
        deadline: Some(state.shadow_timeout),
        ..ctx.clone()
    };

    // SINGLE candidate, NO failover, NO retry storm — one shot under the short
    // shadow deadline. The dispatch + costing core lives in the shared
    // `measurement::measured_single_dispatch` helper (also used by the quality
    // judge's baseline reference dispatch); the cost is computed on the shadow's
    // OWN pricing — never the primary's. No flex, no compression delta: the
    // shadow is a plain measurement dispatch.
    match crate::measurement::measured_single_dispatch(
        &shadow_provider,
        shadow_req,
        &shadow_ctx,
        state.shadow_timeout,
    )
    .await
    {
        Ok(measured) => {
            // Flatten unmetered (`None`) to the #146 shadow convention: `0.0`
            // means "no catalog pricing", never "free".
            outcome.cost_usd = measured.cost_usd.unwrap_or(0.0);
            outcome.succeeded = true;
            tracing::debug!(
                shadow_model = %shadow_model,
                shadow_cost_usd = outcome.cost_usd,
                "shadow dispatch completed (response discarded)"
            );
            // measured.response is intentionally dropped here — the shadow
            // output is discarded.
        }
        Err(e) => {
            tracing::debug!(
                shadow_model = %shadow_model,
                error = %e,
                "shadow dispatch failed — recording shadow_model with zero cost"
            );
        }
    }
    outcome
}

/// Extract the assistant text of a non-streaming response — the served
/// (cheaper-model) answer the quality judge scores. Empty when the response
/// carries only tool calls.
fn response_assistant_text(resp: &ChatCompletionResponse) -> String {
    resp.choices
        .iter()
        .filter_map(|c| match &c.message {
            Message::Assistant {
                content: Some(content),
                ..
            } => Some(match content {
                MessageContent::Text(s) => s.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        tt_shared::messages::ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            }),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when the first choice's `finish_reason` reports anything other than
/// a clean `"stop"` — `"length"` (token-limit truncation) and
/// `"content_filter"` being the realistic cases for a text emission. Both
/// shaping arms check this BEFORE accepting an emission: a truncated patch
/// or CSV body can pass every structural check while being silently
/// incomplete. A MISSING `finish_reason` is treated as clean — truncation
/// cannot be proven (some providers/mocks omit it), and the gate must fail
/// in the accept direction or it would disable shaping wholesale on those
/// providers. The served response keeps the provider's `finish_reason`
/// either way, so callers retain the standard truncation signal.
fn response_emission_truncated(resp: &ChatCompletionResponse) -> bool {
    resp.choices
        .first()
        .and_then(|c| c.finish_reason.as_deref())
        .is_some_and(|r| r != "stop")
}

/// Gate + spawn the sampled async quality judge for a rerouted-DOWN chat
/// completion. Returns immediately in all cases; when it does fire, the actual
/// judging (including the original-model reference re-dispatch) runs in a
/// detached task via [`crate::quality_sample::spawn_quality_judge`], AFTER the
/// caller has returned the user response — so it adds zero user latency.
///
/// Spawns only when ALL hold:
/// - the judge is enabled and a sink is wired (`judge_source_*` are `Some`),
/// - the matched route did NOT opt into redaction (`redact` routes clear the
///   judge captures at the handler — the pre-redaction request must never
///   ride the measurement path to any vendor),
/// - a route matched (`matched_route_id.is_some()`),
/// - the served model is cheaper than the originally-requested one (a true
///   downgrade priced on realized usage — [`crate::quality_sample::is_downgrade`])
///   OR the request was output-shaped (`output_shaped`: minify-JSON steering
///   or a reasoning cap actually applied). The pre-routing capture
///   (`judge_original_req`, taken BEFORE all pre-dispatch shaping) means the
///   baseline reference re-dispatch is the UN-shaped request on the original
///   model — exactly the paired counterfactual for an action-only shaped
///   route whose `target_model` equals the requested model (where
///   `is_downgrade` is false because pricing is identical). Verdicts are
///   keyed by `route_id`, so `RouteAction::auto_pause` (#163) composes with
///   zero new code: a capped/minified route whose paired pass-rate regresses
///   below its floor sticky-pauses itself, and the paused arm suppresses
///   both new actions — fail-safe in the expensive direction. The judge tax
///   on a same-model shaped route can make #163's netted saving negative —
///   that is the honest answer, itemized as `judge_tax_usd`,
///   OR the response was SHAPED (`response_shaped` — a validated
///   format-switch or an applied diff, research Phase 3.3 + 3.4): shaping
///   changes what the model emits, which is exactly what this gate exists to
///   police. `judge_original_req` is the pre-routing, PRE-INSTRUCTION
///   request, so the reference re-dispatch produces the verbose/full answer
///   and the served (stripped/reconstructed) answer is scored against it.
///   Note the judge may penalize the CSV/bare format itself — conservative
///   in the quality-safe direction. With `auto_pause: true` on the route,
///   shaped regressions trip the sticky circuit breaker for free,
/// - the served answer is non-empty (tool-call-only responses are skipped),
/// - the trace falls in the deterministic ~2% sample
///   ([`crate::quality_sample::should_sample`]),
/// - the judge model resolves to a provider,
/// - a credential for the judge model's provider resolves.
///
/// Credential scope (matches [`resolve_credentials_for`] / the #146 shadow
/// precedent exactly): with a per-org credential store configured (the
/// hosted/verified-org model) a cross-provider judge FAILS CLOSED on a store
/// miss — the source provider's key is never forwarded to a different vendor.
/// With NO store configured (dev / dogfood / BYO-key passthrough) there is no
/// per-provider credential model to enforce, and the caller's raw bearer is
/// forwarded to every provider — judge included — like every other dispatch.
///
/// Budget scope: the baseline reference dispatch and judge call(s) bill the
/// org on its own provider credentials but are NOT counted toward
/// `monthly_cap_usd` and never appear in `request_logs` — they are ledgered
/// only in `quality_verdicts` (see `quality_sample` module docs, invariant 6).
#[allow(clippy::too_many_arguments)]
fn maybe_spawn_quality_judge(
    state: &AppState,
    matched_route_id: Option<Uuid>,
    requested_model: &str,
    response: &ChatCompletionResponse,
    requested_pricing: Option<&ModelPricing>,
    served_pricing: Option<&ModelPricing>,
    output_shaped: bool,
    trace_id: Uuid,
    org_id: Uuid,
    raw_bearer: &str,
    judge_source_provider: Option<std::sync::Arc<dyn tt_shared::Provider>>,
    judge_source_ctx: Option<RequestContext>,
    judge_original_req: Option<ChatCompletionRequest>,
    response_shaped: bool,
    content_compressed: bool,
) {
    use crate::quality_sample as qs;

    // Enable gate + the pre-routing captures (all-or-nothing).
    let (Some(sink), Some(source_provider), Some(source_ctx), Some(original_req)) = (
        state.judge_sink.as_ref(),
        judge_source_provider,
        judge_source_ctx,
        judge_original_req,
    ) else {
        return;
    };
    // Agent-run evidence is finalized synchronously at the run boundary.
    // Never start a provider-billed detached judge that could settle after that
    // evidence or after a paused segment has persisted its reservation ledger.
    if source_ctx.run_id.is_some() {
        return;
    }
    if !state.judge_config.enabled {
        return;
    }
    // MVP task-class filter: chat-completions only (explicit + extensible).
    if !qs::JudgeTaskClass::ChatCompletions.is_sampled() {
        return;
    }
    // A route fired AND (the served model is cheaper — reroute-DOWN — OR the
    // request was output-shaped). See the doc comment: the pre-routing
    // capture already provides the un-shaped baseline counterfactual, so an
    // action-only shaped route (same target model, identical pricing) is
    // judge-gateable too.
    //
    // P2a (label-gap closure): a content_compress-only same-model request (where
    // `is_downgrade` is false + not shaped) is ALSO judge-eligible when the
    // pass actually removed tokens — the pre-routing `judge_original_req` capture
    // is the uncompressed counterfactual, so re-dispatching it produces the
    // recall-of-baseline verdict the P1d capture pair needs to become RUNG 3
    // gold. `content_compressed = pass_effects.content_compress_tokens_removed > 0`
    // — a route that opted in but compressed nothing is NOT eligible (no
    // compression event to label).
    if matched_route_id.is_none() {
        return;
    }
    if !(qs::is_downgrade(requested_pricing, served_pricing, &response.usage)
        || output_shaped
        || response_shaped
        || content_compressed)
    {
        return;
    }
    // Deterministic ~2% sample keyed on the trace id.
    if !qs::should_sample(trace_id, state.judge_config.sample_rate) {
        return;
    }
    // P2a (label-gap budget): the per-org / per-UTC-day cap on dispatch-path
    // judge re-dispatches. Bounds the RUNG 3 measurement spend on
    // content_compress-only traffic (judge cost is ALREADY excluded from
    // monthly_cap_usd + savings — this is an additional throttle that protects
    // the org's provider bill from the re-dispatch tax when the sample rate is
    // raised for capture traffic). The cap fires BEFORE any upstream call, so
    // NO 'unjudged:cap' row is recorded (migration 0014's "EVERY failure AFTER
    // an upstream call was attempted records a row" rule is about attempted
    // dispatches — a capped request never attempted one, so no row is the
    // honest ledger posture). A metrics log carries the throttle signal.
    if !state.judge_daily_cap.try_acquire(org_id) {
        tracing::debug!(
            org_id = %org_id,
            trace_id = %trace_id,
            "quality judge throttled by per-org/day cap (TT_JUDGE_DAILY_CAP_PER_ORG)"
        );
        return;
    }
    let served_answer = response_assistant_text(response);
    if served_answer.trim().is_empty() {
        return; // tool-call-only response — nothing textual to judge.
    }
    // Resolve a provider that serves the (cheap) judge model.
    let Some(judge_provider) = state.registry.resolve(&state.judge_config.judge_model) else {
        tracing::warn!(
            judge_model = %state.judge_config.judge_model,
            "quality judge model not resolvable — skipping sample"
        );
        return;
    };
    let input_text = tt_shared::message_text_for_estimation(&original_req);
    let served_model = response.model.clone();
    let requested_model = requested_model.to_string();

    // The judge model may live on a DIFFERENT provider than the source request
    // (e.g. an Anthropic-sourced org with an OpenAI judge model). Credentials
    // are per-provider: resolve the judge provider's OWN credential rather than
    // forwarding the source provider's key to the wrong vendor (the
    // `dispatch_shadow` pattern; fail closed on a verified-org store miss).
    // Resolution can hit the credential store (a DB lookup), so the whole job
    // assembly runs inside a detached task — zero user latency, like the judge
    // itself.
    let state = state.clone();
    let raw_bearer = raw_bearer.to_string();
    let sink = sink.clone();
    tokio::spawn(async move {
        let judge_ctx = if judge_provider.id() == source_provider.id() {
            // Same provider — the captured source credentials are the right ones.
            source_ctx.clone()
        } else {
            match resolve_credentials_for(&state, org_id, judge_provider.id(), &raw_bearer, true)
                .await
            {
                Some(credentials) => RequestContext {
                    credentials,
                    ..source_ctx.clone()
                },
                None => {
                    tracing::warn!(
                        judge_model = %state.judge_config.judge_model,
                        provider = judge_provider.id(),
                        "no credential for the judge provider — skipping quality sample (fail closed)"
                    );
                    return;
                }
            }
        };
        let judge = std::sync::Arc::new(
            qs::GatewayLlmJudge::new(
                judge_provider,
                state.judge_config.judge_model.clone(),
                judge_ctx,
            )
            // Bound each judge call like the baseline dispatch (a hung judge
            // upstream must not pin the detached task indefinitely).
            .with_call_timeout(state.judge_config.baseline_timeout),
        );

        qs::spawn_quality_judge(qs::QualityJudgeJob {
            judge,
            sink,
            org_id,
            route_id: matched_route_id,
            request_id: trace_id,
            requested_model,
            served_model,
            input_text,
            served_answer,
            judge_model: state.judge_config.judge_model.clone(),
            // Deterministic per-trace blind slot for the optimized answer
            // (position debiasing), independent of the keep/drop sampling
            // decision.
            ab_order: qs::ab_order_for(trace_id),
            both_orders: state.judge_config.both_orders,
            // Reference = the ORIGINAL model re-dispatched off-path inside the
            // task, metered + bounded by the judge's own baseline deadline (not
            // the 2s shadow timeout — nobody is waiting on the detached task).
            reference: qs::ReferenceSource::Dispatch {
                provider: source_provider,
                request: Box::new(original_req),
                ctx: Box::new(source_ctx),
                deadline: state.judge_config.baseline_timeout,
            },
            // This judge fires on the rerouted-down DISPATCH path — an L2 hit
            // short-circuits before dispatch, so there is no served-from-L2
            // entry to attribute the verdict to here. The L2 judge join
            // (`L2EvictionTarget`) is exercised by `maybe_spawn_l2_hit_judge`
            // on the served-from-L2 path; this dispatch path carries no
            // eviction target, no hit similarity, and no FP feed.
            l2_eviction: None,
            hit_similarity: None,
            l2_fp_feed: None,
        });
    });
}

/// Replace the FIRST choice's assistant text with `text` — the single seam
/// the output-shaping arms use to hand the caller the stripped (format
/// switch) or fully reconstructed (diff) body. Shaping is n==1-gated at the
/// planners, so the first choice is the only choice; a non-assistant first
/// choice is left untouched (the shaping arms never reach here for
/// tool-call-only responses — their empty assistant text fails validation
/// first).
fn set_assistant_text(resp: &mut ChatCompletionResponse, text: String) {
    if let Some(choice) = resp.choices.first_mut() {
        if let Message::Assistant { content, .. } = &mut choice.message {
            *content = Some(MessageContent::Text(text));
        }
    }
}

/// Gate + spawn the sampled async quality judge for a response **served from
/// the L2 semantic cache** — the production code path that closes the
/// QualityRiskBand → L2 eviction join.
///
/// Unlike [`maybe_spawn_quality_judge`] (which fires on a rerouted-DOWN model
/// DOWNGRADE), this fires on a cache HIT: the served answer is the cached
/// response, and the reference is the ORIGINAL request re-dispatched to its
/// source provider (an L2 hit never re-runs routing, so the served model equals
/// the requested model). The judge therefore measures whether the cached
/// near-duplicate answer is still faithful to *this* query — exactly the
/// cache-poisoning signal the per-class thresholds aim to keep out. A clearly
/// degraded verdict (`High` band) evicts EXACTLY this entry via the
/// [`qs::L2EvictionTarget`] carried on the job; Low/Medium/Unclear only record
/// the score. Detached + deterministically sampled → zero user latency and
/// bounded extra spend.
///
/// Spawns only when ALL hold:
/// - the judge is enabled and a sink is wired,
/// - the pre-routing source captures are present (provider/ctx/original req —
///   cleared at the handler for `redact` routes, so a redact route's
///   pre-redaction request never rides the measurement path),
/// - this task class (chat-completions) is in scope,
/// - the trace falls in the deterministic sample ([`qs::should_sample`]),
/// - the cached answer is non-empty (tool-call-only responses are skipped),
/// - the cached body deserializes and the judge model resolves to a provider,
/// - a credential for the judge model's provider resolves (fail closed on a
///   verified-org store miss; with NO credential store configured the raw
///   bearer is forwarded to every provider — see
///   [`maybe_spawn_quality_judge`]'s credential-scope note).
#[allow(clippy::too_many_arguments)]
fn maybe_spawn_l2_hit_judge(
    state: &AppState,
    l2: &L2Config,
    entry: &CacheEntry,
    similarity: f32,
    in_band: bool,
    task_class: Option<tt_cache::TaskClass>,
    trace_id: Uuid,
    org_id: Uuid,
    raw_bearer: &str,
    judge_source_provider: Option<&std::sync::Arc<dyn tt_shared::Provider>>,
    judge_source_ctx: Option<&RequestContext>,
    judge_original_req: Option<&ChatCompletionRequest>,
) {
    use crate::quality_sample as qs;

    // Enable gate + the pre-routing captures (all-or-nothing).
    let (Some(sink), Some(source_provider), Some(source_ctx), Some(original_req)) = (
        state.judge_sink.as_ref(),
        judge_source_provider,
        judge_source_ctx,
        judge_original_req,
    ) else {
        return;
    };
    if !state.judge_config.enabled {
        return;
    }
    // MVP task-class filter: chat-completions only (explicit + extensible).
    if !qs::JudgeTaskClass::ChatCompletions.is_sampled() {
        return;
    }
    // Deterministic sample keyed on the trace id. Rate precedence: an
    // ambiguous-band hit prefers the dedicated band rate (so the FP estimator
    // converges without judging every confident hit), then the dedicated
    // L2-hit rate, then the shared dispatch-path rate (today's behavior).
    let cfg = &state.judge_config;
    let rate = if in_band {
        cfg.l2_band_sample_rate
            .or(cfg.l2_hit_sample_rate)
            .unwrap_or(cfg.sample_rate)
    } else {
        cfg.l2_hit_sample_rate.unwrap_or(cfg.sample_rate)
    };
    if !qs::should_sample(trace_id, rate) {
        return;
    }
    // The served answer is the cached response body. A body that fails to
    // deserialize (or a tool-call-only response with no assistant text) carries
    // nothing textual to judge — skip rather than record a meaningless verdict.
    let Ok(cached_response) = serde_json::from_slice::<ChatCompletionResponse>(&entry.response)
    else {
        return;
    };
    let served_answer = response_assistant_text(&cached_response);
    if served_answer.trim().is_empty() {
        return;
    }
    // Resolve a provider that serves the (cheap) judge model.
    let Some(judge_provider) = state.registry.resolve(&state.judge_config.judge_model) else {
        tracing::warn!(
            judge_model = %state.judge_config.judge_model,
            "quality judge model not resolvable — skipping L2-hit sample"
        );
        return;
    };
    // Hourly per-instance spend cap — consumed LAST, after every cheap
    // skip (deserialize / empty-answer / judge-model resolution), so each
    // consumed token corresponds to a real would-be judge dispatch and a
    // skipped sample never burns judge budget. Dispatch-path judging is
    // unaffected.
    if !state.l2_hit_judge_limiter.try_acquire() {
        metrics::counter!("cache_l2_judge_capped_total").increment(1);
        return;
    }
    let input_text = tt_shared::message_text_for_estimation(original_req);

    // Per-provider judge credentials, resolved off-path inside a detached task
    // (the `dispatch_shadow` pattern; fail closed on a verified-org store
    // miss) — see `maybe_spawn_quality_judge` for the full rationale.
    let state = state.clone();
    let raw_bearer = raw_bearer.to_string();
    let sink = sink.clone();
    let source_provider = source_provider.clone();
    let source_ctx = source_ctx.clone();
    let original_req = original_req.clone();
    let entry_model = entry.model.clone();
    let entry_id = entry.id;
    let l2_cache = l2.cache.clone();
    // FP-estimator feed: only an AMBIGUOUS-BAND hit's verdict measures the
    // gate's false-positive rate (a confident hit says nothing about the
    // band). Carries the shared adaptive gate + the effective tolerance.
    let l2_fp_feed = if in_band {
        l2.verify.as_ref().map(|v| qs::L2FpFeed {
            gate: v.gate.clone(),
            task_class,
            tolerance_pct: v.tolerance_pct,
        })
    } else {
        None
    };
    tokio::spawn(async move {
        let judge_ctx = if judge_provider.id() == source_provider.id() {
            // Same provider — the captured source credentials are the right ones.
            source_ctx.clone()
        } else {
            match resolve_credentials_for(&state, org_id, judge_provider.id(), &raw_bearer, true)
                .await
            {
                Some(credentials) => RequestContext {
                    credentials,
                    ..source_ctx.clone()
                },
                None => {
                    tracing::warn!(
                        judge_model = %state.judge_config.judge_model,
                        provider = judge_provider.id(),
                        "no credential for the judge provider — skipping L2-hit sample (fail closed)"
                    );
                    return;
                }
            }
        };
        let judge = std::sync::Arc::new(
            qs::GatewayLlmJudge::new(
                judge_provider,
                state.judge_config.judge_model.clone(),
                judge_ctx,
            )
            // Bound each judge call like the baseline dispatch (a hung judge
            // upstream must not pin the detached task indefinitely).
            .with_call_timeout(state.judge_config.baseline_timeout),
        );

        qs::spawn_quality_judge(qs::QualityJudgeJob {
            judge,
            sink,
            org_id,
            // No route fired on a cache hit — the verdict attributes to the L2
            // entry.
            route_id: None,
            request_id: trace_id,
            // An L2 hit never re-runs routing: the served (cached) model equals
            // the requested model.
            requested_model: entry_model.clone(),
            served_model: entry_model,
            input_text,
            served_answer,
            judge_model: state.judge_config.judge_model.clone(),
            // Deterministic per-trace blind slot for the optimized (cached)
            // answer.
            ab_order: qs::ab_order_for(trace_id),
            both_orders: state.judge_config.both_orders,
            // Reference = the ORIGINAL request re-dispatched off-path inside
            // the task, so the judge scores the cached answer against a fresh
            // answer to THIS query. Metered + bounded by the judge's baseline
            // deadline.
            reference: qs::ReferenceSource::Dispatch {
                provider: source_provider,
                request: Box::new(original_req),
                ctx: Box::new(source_ctx),
                deadline: state.judge_config.baseline_timeout,
            },
            // The join the roadmap flagged: a High-band verdict evicts EXACTLY
            // this served-from-L2 entry (single-row, never bulk);
            // Low/Medium/Unclear only record the score.
            l2_eviction: Some(qs::L2EvictionTarget {
                cache: l2_cache,
                entry_id,
            }),
            // Durable attribution (migration 0018) + the FP-estimator feed for
            // ambiguous-band hits.
            hit_similarity: Some(similarity),
            l2_fp_feed,
        });
    });
}

/// Build the `request_logs` row for an L1 cache hit. The provider id
/// stored in the envelope (e.g. `"openai"`) is preserved so the
/// dashboard's per-provider breakdowns include cache hits; the cache
/// label only surfaces via the `cache_layer` column.
#[derive(Debug, Clone, Copy)]
struct RouteLogAttribution {
    route_id: Option<Uuid>,
    route_version_id: Option<i64>,
    paused: bool,
}

fn request_log_for_l1_hit(
    entry: &L1Entry,
    ctx: &RequestContext,
    requested_model: &str,
    trace_id: Uuid,
    request_started: Instant,
    route: RouteLogAttribution,
    retrieval_tokens_saved: i64,
) -> RequestLogRow {
    let baseline = if entry.is_legacy_format() {
        // Pre-envelope row — fall back to the conservative synthetic
        // baseline so saved_usd is still non-zero in the aggregate cards.
        synthetic_baseline_from_usage(&entry.response.usage)
    } else {
        entry.baseline_cost_usd
    };
    let provider_id = if entry.provider_id.is_empty() {
        "cache".to_string()
    } else {
        entry.provider_id.clone()
    };
    RequestLogRow {
        id: Uuid::now_v7(),
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        ts: Utc::now(),
        provider: provider_id,
        requested_model: Some(requested_model.to_owned()),
        model: entry.response.model.clone(),
        input_tokens: entry.response.usage.prompt_tokens as i32,
        output_tokens: entry.response.usage.completion_tokens as i32,
        cached_tokens: entry.response.usage.cached_tokens as i32,
        cost_usd: 0.0,
        baseline_cost_usd: baseline,
        // TT cache hit — no provider call, no provider-side discount, and no
        // upstream prompt cache exists to bust.
        provider_cache_saved_usd: 0.0,
        cache_bust_penalty_usd: 0.0,
        flex_saved_usd: 0.0,
        doc_compaction_saved_usd: 0.0,
        summarizer_tax_usd: 0.0,
        request_delta_evidence_state: entry.request_delta_evidence_state,
        cached: true,
        cache_layer: Some("l1".into()),
        route_id: route.route_id,
        route_version_id: route.route_version_id,
        latency_ms: clamp_latency_ms(request_started),
        upstream_latency_ms: None,
        status: 200,
        tag: ctx.tag.clone(),
        error_class: None,
        trace_id: Some(trace_id.to_string()),
        truncated: false,
        // A cache hit performs no live dispatch, so no shadow fires and the
        // canary arm is not re-derived here (the response is served from cache
        // regardless of arm). Columns stay NULL.
        shadow_model: None,
        shadow_cost_usd: None,
        traffic_split_arm: None,
        // TT cache hit — no provider call at serve time; the original miss row
        // carries the provider-cache telemetry. NULL (not the entry's stored
        // counts) so per-route aggregates never double-count provider cache
        // reads. (`cached_tokens` above deliberately keeps its legacy
        // echo-the-miss behavior for back-compat.)
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        // Cache hit = no dispatch cost, nothing forgone — the Batch Lane
        // could not have saved anything on a request that never billed.
        batch_eligible: false,
        batch_forgone_usd: 0.0,
        route_paused: route.paused,
        // TT cache hit — nothing dispatched, nothing minify-estimated.
        minify_saved_est_usd: 0.0,
        // TT cache hit — the serve performed no shaping dispatch; the
        // original miss row carries any shaping markers/figures.
        format_switched: None,
        format_switch_saved_est_usd: 0.0,
        diff_applied: false,
        diff_saved_usd: 0.0,
        diff_failed: false,
        diff_failed_cost_usd: 0.0,
        retrieval_tokens_saved,
        // Cache hits never run the doc-compaction pass (nothing dispatched).
        doc_compaction_tokens_removed: 0,
        // Cache hits never run the compress pass → 0 (TR-2).
        compression_saved_usd: 0.0,
        compression_tokens_removed: 0,
        // Document Lane D4: cache hits never run the seam → 0.
        doc_vision_saved_est_usd: 0.0,
        // run_id/node_id stamped in Task 4 (agentic loop context).
        run_id: None,
        node_id: None,
        // Cache hit → nothing dispatched → no content_compress.
        content_compress_saved_est_usd: 0.0,
        content_compress_kind: None,
        // L1 hit — no L2 provenance.
        l2_matched_entry_id: None,
        l2_similarity: None,
        l2_verdict: None,
    }
}

/// Build the `request_logs` row for an L2 cache hit. `baseline_cost_usd` is
/// the catalog-derived baseline resolved by [`l2_entry_baseline`].
// Row builder takes the L2-hit row's independent inputs directly; bundling
// them into a param struct would add indirection without improving clarity.
#[allow(clippy::too_many_arguments)]
fn request_log_for_l2_hit(
    entry: &CacheEntry,
    ctx: &RequestContext,
    requested_model: &str,
    trace_id: Uuid,
    request_started: Instant,
    route_id: Option<Uuid>,
    route_version_id: Option<i64>,
    route_paused: bool,
    baseline_cost_usd: f64,
    request_delta_evidence_state: RequestDeltaEvidenceState,
    retrieval_tokens_saved: i64,
    similarity: f32,
    verdict: L2VerifyDecision,
) -> RequestLogRow {
    RequestLogRow {
        id: Uuid::now_v7(),
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        ts: Utc::now(),
        provider: "cache".into(),
        requested_model: Some(requested_model.to_owned()),
        model: entry.model.clone(),
        input_tokens: entry.input_tokens as i32,
        output_tokens: entry.output_tokens as i32,
        cached_tokens: 0,
        cost_usd: 0.0,
        baseline_cost_usd,
        // TT cache hit — no provider call, no provider-side discount, and no
        // upstream prompt cache exists to bust.
        provider_cache_saved_usd: 0.0,
        cache_bust_penalty_usd: 0.0,
        flex_saved_usd: 0.0,
        doc_compaction_saved_usd: 0.0,
        summarizer_tax_usd: 0.0,
        request_delta_evidence_state,
        cached: true,
        cache_layer: Some("l2".into()),
        route_id,
        route_version_id,
        latency_ms: clamp_latency_ms(request_started),
        upstream_latency_ms: None,
        status: 200,
        tag: ctx.tag.clone(),
        error_class: None,
        trace_id: Some(trace_id.to_string()),
        truncated: false,
        // L2 hit — no live dispatch, no shadow, arm not re-derived. NULL.
        shadow_model: None,
        shadow_cost_usd: None,
        traffic_split_arm: None,
        // TT cache hit — no provider call at serve time; the original miss row
        // carries the provider-cache telemetry. NULL so per-route aggregates
        // never double-count provider cache reads.
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        // Cache hit = no dispatch cost, nothing forgone (see the L1 builder).
        batch_eligible: false,
        batch_forgone_usd: 0.0,
        route_paused,
        // TT cache hit — nothing dispatched, nothing minify-estimated.
        minify_saved_est_usd: 0.0,
        // TT cache hit — the serve performed no shaping dispatch; the
        // original miss row carries any shaping markers/figures.
        format_switched: None,
        format_switch_saved_est_usd: 0.0,
        diff_applied: false,
        diff_saved_usd: 0.0,
        diff_failed: false,
        diff_failed_cost_usd: 0.0,
        retrieval_tokens_saved,
        // Cache hits never run the doc-compaction pass (nothing dispatched).
        doc_compaction_tokens_removed: 0,
        // Cache hits never run the compress pass → 0 (TR-2).
        compression_saved_usd: 0.0,
        compression_tokens_removed: 0,
        // Document Lane D4: cache hits never run the seam → 0.
        doc_vision_saved_est_usd: 0.0,
        // run_id/node_id stamped in Task 4 (agentic loop context).
        run_id: None,
        node_id: None,
        // Cache hit → nothing dispatched → no content_compress.
        content_compress_saved_est_usd: 0.0,
        content_compress_kind: None,
        // L2-hit provenance (migration 0035) — the fields a signed L2 receipt
        // (tt_telemetry::l2_receipt) attests. The matched entry id +
        // similarity + the verify-gate verdict. Read by the cloud mint endpoint
        // (POST /v1/admin/requests/{trace_id}/l2-receipt/sign).
        l2_matched_entry_id: Some(entry.id),
        l2_similarity: Some(similarity),
        l2_verdict: Some(l2_verdict_code(verdict).to_string()),
    }
}

/// Map an `L2VerifyDecision` to the stable string code a signed L2 receipt
/// carries in its canonical payload (see `tt_telemetry::l2_receipt` —
/// `confident` / `verified` / `unverifiable` / `rejected`). The code is part
/// of the SIGNED bytes, so it must be byte-stable across versions (change the
/// version, never the code).
fn l2_verdict_code(d: L2VerifyDecision) -> &'static str {
    match d {
        L2VerifyDecision::Confident => "confident",
        // The agreement f32 is telemetry, not part of the receipt contract.
        L2VerifyDecision::Verified(_) => "verified",
        L2VerifyDecision::Unverifiable => "unverifiable",
        L2VerifyDecision::Rejected(_) => "rejected",
    }
}

fn clamp_latency_ms(started: Instant) -> i32 {
    started.elapsed().as_millis().min(i32::MAX as u128) as i32
}

/// Clamp an optional raw provider token count into the `request_logs` INT
/// columns, preserving the Option-ness (`None` -> SQL NULL = "provider did
/// not report"; `Some(0)` = "provider explicitly reported zero").
pub(crate) fn opt_tokens_i32(v: Option<u64>) -> Option<i32> {
    v.map(|t| t.min(i32::MAX as u64) as i32)
}

#[cfg(test)]
mod cache_header_tests {
    use super::*;
    use axum::http::HeaderMap;

    fn l1_entry(version: u32, baseline_cost_usd: f64) -> L1Entry {
        L1Entry {
            response: ChatCompletionResponse {
                id: "chatcmpl-cache-test".into(),
                object: "chat.completion".into(),
                created: 1,
                model: "gpt-4o-mini".into(),
                choices: vec![],
                usage: Usage {
                    prompt_tokens: 5,
                    completion_tokens: 4,
                    total_tokens: 9,
                    cached_tokens: 1,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                },
            },
            baseline_cost_usd,
            cost_usd: 0.003,
            provider_id: "openai".into(),
            request_delta_evidence_state: if version >= 2 {
                RequestDeltaEvidenceState::Measured
            } else {
                RequestDeltaEvidenceState::MissingEvidence
            },
            version,
        }
    }

    fn hv(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-tokentrimmer-cache", v.parse().unwrap());
        h
    }

    fn request_context() -> RequestContext {
        RequestContext {
            budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
            trace_id: Uuid::nil(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new("test"),
                base_url: None,
                extra_headers: vec![],
            },
            tag: None,
            deadline: None,
            run_id: None,
            node_id: None,
        }
    }

    #[test]
    fn l1_hit_log_preserves_matched_immutable_route_version() {
        let ctx = request_context();
        let route_id = Uuid::now_v7();
        let row = request_log_for_l1_hit(
            &l1_entry(1, 0.0045),
            &ctx,
            "caller-model",
            Uuid::now_v7(),
            Instant::now(),
            RouteLogAttribution {
                route_id: Some(route_id),
                route_version_id: Some(9_876_543_210),
                paused: false,
            },
            0,
        );
        assert_eq!(row.route_id, Some(route_id));
        assert_eq!(row.route_version_id, Some(9_876_543_210));
        assert_eq!(row.requested_model.as_deref(), Some("caller-model"));
    }

    #[test]
    fn cache_override_parsing() {
        assert_eq!(cache_override_from_header(&HeaderMap::new()).unwrap(), None);
        assert_eq!(
            cache_override_from_header(&hv("disabled")).unwrap(),
            Some((false, false))
        );
        assert_eq!(
            cache_override_from_header(&hv("read-only")).unwrap(),
            Some((true, false))
        );
        assert_eq!(
            cache_override_from_header(&hv("bypass")).unwrap(),
            Some((false, true))
        );
        assert_eq!(
            cache_override_from_header(&hv(" Force-Write ")).unwrap(),
            Some((true, true))
        );
        assert_eq!(cache_override_from_header(&hv("   ")).unwrap(), None);
        assert!(cache_override_from_header(&hv("nope")).is_err());
    }

    #[test]
    fn streaming_l1_receipt_uses_only_a_priced_envelope_baseline() {
        let priced = l1_entry(1, 0.0045);
        let attribution = l1_cache_stream_attribution(&priced).expect("priced envelope");
        assert_eq!(attribution.baseline_cost_usd(), 0.0045);

        // Pre-envelope values have no stored counterfactual. Their historical
        // synthetic baseline remains telemetry-only and must not become a
        // terminal savings receipt.
        assert!(l1_cache_stream_attribution(&l1_entry(0, 0.0045)).is_none());
        assert!(l1_cache_stream_attribution(&l1_entry(1, -0.0045)).is_none());
    }

    #[test]
    fn streaming_l1_headers_match_the_verified_cache_receipt() {
        let entry = l1_entry(1, 0.0045);
        let receipt = l1_cache_stream_attribution(&entry);
        let mut headers = HeaderMap::new();
        attach_l1_cache_stream_headers(&mut headers, Uuid::nil(), &entry.response.model, receipt);

        assert_eq!(
            headers
                .get("x-tokentrimmer-cache")
                .and_then(|v| v.to_str().ok()),
            Some("hit-l1")
        );
        assert_eq!(
            headers
                .get("x-tokentrimmer-provider")
                .and_then(|v| v.to_str().ok()),
            Some("cache")
        );
        assert_eq!(
            headers
                .get("x-tokentrimmer-cost-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.000000")
        );
        assert_eq!(
            headers
                .get("x-tokentrimmer-baseline-cost-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.004500")
        );
        assert_eq!(
            headers
                .get("x-tokentrimmer-saved-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.004500")
        );

        let legacy = l1_entry(0, 0.0045);
        let mut legacy_headers = HeaderMap::new();
        attach_l1_cache_stream_headers(
            &mut legacy_headers,
            Uuid::nil(),
            &legacy.response.model,
            l1_cache_stream_attribution(&legacy),
        );
        assert_eq!(
            legacy_headers
                .get("x-tokentrimmer-cache")
                .and_then(|v| v.to_str().ok()),
            Some("hit-l1")
        );
        assert!(
            legacy_headers.get("x-tokentrimmer-saved-usd").is_none(),
            "legacy synthetic baseline must not be surfaced as confirmed savings"
        );
    }
}

#[cfg(test)]
mod cost_limit_header_tests {
    use super::*;
    use axum::http::HeaderMap;

    fn with_cost_limit(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-tokentrimmer-cost-limit-usd", value.parse().unwrap());
        headers
    }

    #[test]
    fn cost_limit_requires_a_finite_positive_number() {
        assert_eq!(
            cost_limit_from_header(&HeaderMap::new()).unwrap(),
            None,
            "an absent header leaves the request uncapped"
        );
        assert_eq!(
            cost_limit_from_header(&with_cost_limit("0.25")).unwrap(),
            Some(0.25)
        );
        for invalid in ["0", "-1", "NaN", "inf", "-inf", "not-a-number"] {
            assert!(
                matches!(
                    cost_limit_from_header(&with_cost_limit(invalid)),
                    Err(ApiError::InvalidRequest(_))
                ),
                "{invalid:?} must be rejected rather than disabling the budget"
            );
        }
    }

    #[test]
    fn active_cost_limit_fails_closed_when_pricing_is_unknown() {
        assert!(enforce_cost_limit(None, None, "unpriced-model", 1, Some(1)).is_ok());
        assert!(matches!(
            enforce_cost_limit(Some(1.0), None, "unpriced-model", 1, Some(1)),
            Err(ApiError::PriceUnknown { model }) if model == "unpriced-model"
        ));
    }
}

#[cfg(test)]
mod provider_override_tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn timeout_ms_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(timeout_ms_from_header(&h), None);
        h.insert("x-tokentrimmer-timeout-ms", " 30000 ".parse().unwrap());
        assert_eq!(timeout_ms_from_header(&h), Some(30_000));
        for bad in ["0", "700000", "abc", "-5"] {
            let mut b = HeaderMap::new();
            b.insert("x-tokentrimmer-timeout-ms", bad.parse().unwrap());
            assert_eq!(timeout_ms_from_header(&b), None, "{bad} must be rejected");
        }
    }

    #[test]
    fn fallback_override_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(fallback_override_from_header(&h), None);
        h.insert("x-tokentrimmer-fallback", "a, b ,c".parse().unwrap());
        assert_eq!(
            fallback_override_from_header(&h),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        let mut blank = HeaderMap::new();
        blank.insert("x-tokentrimmer-fallback", " , ".parse().unwrap());
        assert_eq!(fallback_override_from_header(&blank), None);
    }

    #[test]
    fn route_override_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(route_override_from_header(&h), None);
        // case-preserved (route names are case-sensitive labels), trimmed.
        h.insert(
            "x-tokentrimmer-route",
            "  Cheap-For-Short ".parse().unwrap(),
        );
        assert_eq!(
            route_override_from_header(&h).as_deref(),
            Some("Cheap-For-Short")
        );
        let mut empty = HeaderMap::new();
        empty.insert("x-tokentrimmer-route", "   ".parse().unwrap());
        assert_eq!(route_override_from_header(&empty), None);
    }

    #[test]
    fn provider_override_header_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(provider_override_from_header(&h), None);
        h.insert("x-tokentrimmer-provider", "  Anthropic ".parse().unwrap());
        assert_eq!(
            provider_override_from_header(&h).as_deref(),
            Some("anthropic")
        );
        let mut empty = HeaderMap::new();
        empty.insert("x-tokentrimmer-provider", "   ".parse().unwrap());
        assert_eq!(provider_override_from_header(&empty), None);
    }
}

#[cfg(test)]
mod cache_eligibility_tests {
    use super::*;
    use std::collections::HashMap;

    fn base_req() -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![],
            temperature: None,
            top_p: None,
            max_tokens: None,
            stream: false,
            tools: vec![],
            tool_choice: None,
            response_format: None,
            stop: vec![],
            presence_penalty: None,
            frequency_penalty: None,
            n: None,
            seed: None,
            user: None,
            tt_extras: HashMap::new(),
            ..Default::default()
        }
    }

    fn cache_context(org_id: Uuid, api_key: &str) -> RequestContext {
        RequestContext {
            budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
            trace_id: Uuid::nil(),
            org_id,
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new(api_key),
                base_url: None,
                extra_headers: vec![],
            },
            tag: None,
            deadline: None,
            run_id: None,
            node_id: None,
        }
    }

    #[test]
    fn anonymous_cache_namespace_is_bearer_scoped_and_secret_free() {
        let req = base_req();
        let first = namespaced_l1_key(&cache_context(Uuid::nil(), "provider-secret-first"), &req);
        let second = namespaced_l1_key(&cache_context(Uuid::nil(), "provider-secret-second"), &req);
        assert_ne!(first, second);
        assert!(!first.contains("provider-secret-first"));
        assert!(!second.contains("provider-secret-second"));

        let org = Uuid::new_v4();
        assert_eq!(
            namespaced_l1_key(&cache_context(org, "old-provider-key"), &req),
            namespaced_l1_key(&cache_context(org, "rotated-provider-key"), &req),
            "verified callers remain scoped by stable organization identity"
        );
    }

    fn assistant_resp(tool_calls: Vec<tt_shared::ToolCall>) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "id".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "gpt-4o".into(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: None,
                    tool_calls,
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        }
    }

    // --- Fix A: is_cache_eligible ---

    #[test]
    fn deterministic_request_is_eligible() {
        assert!(is_cache_eligible(&base_req()));
    }

    #[test]
    fn include_usage_true_is_honored() {
        let mut req = base_req();
        req.stream_options = Some(serde_json::json!({ "include_usage": true }));
        assert!(client_requested_include_usage(&req));
    }

    #[test]
    fn include_usage_absent_or_false_is_not_honored() {
        // No stream_options at all.
        assert!(!client_requested_include_usage(&base_req()));
        // stream_options present but include_usage false.
        let mut req = base_req();
        req.stream_options = Some(serde_json::json!({ "include_usage": false }));
        assert!(!client_requested_include_usage(&req));
        // stream_options present but include_usage absent.
        let mut req = base_req();
        req.stream_options = Some(serde_json::json!({ "other": 1 }));
        assert!(!client_requested_include_usage(&req));
        // Non-bool include_usage degrades to false rather than panicking.
        let mut req = base_req();
        req.stream_options = Some(serde_json::json!({ "include_usage": "yes" }));
        assert!(!client_requested_include_usage(&req));
    }

    #[test]
    fn temperature_above_zero_is_not_eligible() {
        let mut req = base_req();
        req.temperature = Some(0.7);
        assert!(!is_cache_eligible(&req));
    }

    #[test]
    fn temperature_zero_is_eligible() {
        let mut req = base_req();
        req.temperature = Some(0.0);
        assert!(is_cache_eligible(&req));
    }

    #[test]
    fn top_p_below_one_is_not_eligible() {
        let mut req = base_req();
        req.top_p = Some(0.9);
        assert!(!is_cache_eligible(&req));
    }

    #[test]
    fn top_p_one_is_eligible() {
        let mut req = base_req();
        req.top_p = Some(1.0);
        assert!(is_cache_eligible(&req));
    }

    #[test]
    fn n_greater_than_one_is_not_eligible() {
        let mut req = base_req();
        req.n = Some(2);
        assert!(!is_cache_eligible(&req));
    }

    #[test]
    fn n_equal_one_is_eligible() {
        let mut req = base_req();
        req.n = Some(1);
        assert!(is_cache_eligible(&req));
    }

    #[test]
    fn seed_set_is_not_eligible() {
        let mut req = base_req();
        req.seed = Some(42);
        assert!(!is_cache_eligible(&req));
    }

    #[test]
    fn response_with_tool_calls_not_inserted() {
        let tool_call = tt_shared::ToolCall {
            id: "call_1".into(),
            r#type: "function".into(),
            function: tt_shared::messages::ToolCallFunction {
                name: "get_weather".into(),
                arguments: "{}".into(),
            },
        };
        let resp = assistant_resp(vec![tool_call]);
        assert!(response_has_tool_calls(&resp));
    }

    #[test]
    fn response_without_tool_calls_is_fine() {
        let resp = assistant_resp(vec![]);
        assert!(!response_has_tool_calls(&resp));
    }

    // --- Fix B: CacheBehavior::resolve ---

    #[test]
    fn normal_extras_gives_lookup_and_insert() {
        let req = base_req();
        let b = CacheBehavior::resolve(&req);
        assert!(b.do_lookup);
        assert!(b.do_insert);
        assert!(b.ttl_secs.is_none());
    }

    #[test]
    fn bypass_skips_both() {
        let mut req = base_req();
        req.tt_extras
            .insert("cache".into(), serde_json::json!({"mode": "bypass"}));
        let b = CacheBehavior::resolve(&req);
        assert!(!b.do_lookup);
        assert!(!b.do_insert);
    }

    #[test]
    fn refresh_skips_lookup_but_inserts() {
        let mut req = base_req();
        req.tt_extras.insert(
            "cache".into(),
            serde_json::json!({"mode": "refresh", "ttl_secs": 7200}),
        );
        let b = CacheBehavior::resolve(&req);
        assert!(!b.do_lookup);
        assert!(b.do_insert);
        assert_eq!(b.ttl_secs, Some(7200));
    }

    #[test]
    fn read_only_looks_up_never_inserts() {
        let mut req = base_req();
        req.tt_extras
            .insert("cache".into(), serde_json::json!({"mode": "read-only"}));
        let b = CacheBehavior::resolve(&req);
        assert!(b.do_lookup);
        assert!(!b.do_insert);
    }

    #[test]
    fn nondeterministic_request_overrides_refresh() {
        // Even if caller says "refresh", temperature>0 means we skip both.
        let mut req = base_req();
        req.temperature = Some(1.0);
        req.tt_extras
            .insert("cache".into(), serde_json::json!({"mode": "refresh"}));
        let b = CacheBehavior::resolve(&req);
        assert!(!b.do_lookup);
        assert!(!b.do_insert);
    }

    // --- Per-org semantic_cache_disabled compliance control ---

    #[test]
    fn org_cache_disabled_forces_lookup_and_insert_off() {
        // An org that opted OUT of caching → both lookup and insert forced off,
        // even for an otherwise fully cache-eligible request.
        let req = base_req();
        let mut b = CacheBehavior::resolve(&req);
        assert!(b.do_lookup, "precondition: eligible request looks up");
        assert!(b.do_insert, "precondition: eligible request inserts");
        b.apply_org_cache_disabled(true);
        assert!(!b.do_lookup, "disabled org must not look up");
        assert!(!b.do_insert, "disabled org must not insert");
    }

    #[test]
    fn org_cache_not_disabled_is_noop() {
        // An org WITHOUT the flag (the default) sees zero behaviour change.
        let req = base_req();
        let mut b = CacheBehavior::resolve(&req);
        let (lookup_before, insert_before, ttl_before) = (b.do_lookup, b.do_insert, b.ttl_secs);
        b.apply_org_cache_disabled(false);
        assert_eq!(b.do_lookup, lookup_before);
        assert_eq!(b.do_insert, insert_before);
        assert_eq!(b.ttl_secs, ttl_before);
    }

    #[test]
    fn org_cache_disabled_stays_off_when_already_off() {
        // Idempotent with the other force-off short-circuits (route/diff/bypass):
        // applying it to an already-off behavior leaves both off.
        let mut req = base_req();
        req.tt_extras
            .insert("cache".into(), serde_json::json!({"mode": "bypass"}));
        let mut b = CacheBehavior::resolve(&req);
        assert!(!b.do_lookup);
        assert!(!b.do_insert);
        b.apply_org_cache_disabled(true);
        assert!(!b.do_lookup);
        assert!(!b.do_insert);
    }
}

#[cfg(test)]
mod credential_resolution_tests {
    // The test below holds `ENV_LOCK` (a std Mutex serializing env-var
    // access) across the awaited resolutions, so the env var stays stable
    // for the duration of the calls. Only other test threads ever contend
    // the lock, so there is no deadlock risk — the await-holding-lock lint
    // does not apply. (Same pattern as the `middleware::retrieval` tests.)
    #![allow(clippy::await_holding_lock)]

    use std::sync::Mutex;

    use super::*;

    /// Process-wide lock that serializes tests which read/write
    /// `OPENAI_API_KEY` so they cannot race each other in the multi-threaded
    /// test runner.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// P0 #21 (fail-closed gateway): a gateway running WITHOUT a key store
    /// wires NO credential store at all, so an unverified caller (`org_id` is
    /// nil — no `ApiKeyContext` was attached) must resolve to their own raw
    /// Bearer key, never the operator's env-sourced provider keys. The env
    /// store being unreachable is wiring (tested in the `tt` binary); this
    /// guards the handler half: no store → bearer passthrough only.
    #[tokio::test]
    async fn unverified_caller_without_store_gets_bearer_passthrough_only() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        // Operator env key present in the process — must NOT be served.
        std::env::set_var("OPENAI_API_KEY", "sk-operator-do-not-serve");
        let state = AppState::with_default_providers();

        let got = resolve_credentials(&state, Uuid::nil(), "openai", "sk-caller-own-key").await;
        // Cross-provider resolution (routing rewrite) without a store also
        // never reaches env keys — bearer or nothing.
        let got_for =
            resolve_credentials_for(&state, Uuid::nil(), "openai", "sk-caller-own-key", true).await;

        // Clean up BEFORE asserting so a failed assert cannot leak the env
        // var into the rest of this test binary.
        std::env::remove_var("OPENAI_API_KEY");

        assert!(state.credential_store.is_none(), "no store wired");
        let got = got.expect("no-store dev mode keeps the bearer passthrough");
        assert_eq!(got.api_key.expose(), "sk-caller-own-key");
        let got_for = got_for.expect("bearer fallback");
        assert_eq!(got_for.api_key.expose(), "sk-caller-own-key");
    }

    /// No-store dev mode is untouched by BYO-only even for a VERIFIED org
    /// (e.g. the dogfood org id): without a credential store there is no
    /// per-provider credential model to enforce, so the bearer passthrough
    /// keeps working (#106's boot guard already keeps that mode loopback /
    /// explicitly-opted-in only).
    #[tokio::test]
    async fn verified_org_without_store_keeps_bearer_passthrough() {
        let state = AppState::with_default_providers();
        let got = resolve_credentials(&state, Uuid::now_v7(), "openai", "sk-own-key")
            .await
            .expect("dev-mode passthrough");
        assert_eq!(got.api_key.expose(), "sk-own-key");
    }

    /// BYO-only (P0 #9): with a credential store CONFIGURED, a VERIFIED org
    /// (non-nil `org_id`) that has no stored credential for the provider must
    /// resolve to `None` — never its own raw bearer (the org's TokenTrimmer
    /// key, useless upstream) and never an operator env key (which is not in
    /// the store composition unless the operator opted in at boot).
    #[tokio::test]
    async fn verified_org_with_store_but_no_credential_fails_closed() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        // Operator env key present in the process — must NOT be served.
        std::env::set_var("OPENAI_API_KEY", "sk-operator-do-not-serve");
        let store = tt_auth::credentials::InMemoryProviderCredentialStore::new();
        let state =
            AppState::with_default_providers().with_credential_store(std::sync::Arc::new(store));
        let org = Uuid::now_v7();

        let got_for = resolve_credentials_for(&state, org, "openai", "tt_live_abc123", true).await;

        std::env::remove_var("OPENAI_API_KEY");

        assert!(
            got_for.is_none(),
            "verified org + store miss must fail closed (BYO-only), got {:?}",
            got_for.map(|c| c.api_key.expose().to_string())
        );
    }

    /// BYO-only leaves the ANONYMOUS legacy passthrough intact: with a store
    /// configured, a caller with no verified org (nil `org_id`, e.g. an
    /// `sk-…` bearer) still forwards their own key upstream.
    #[tokio::test]
    async fn anonymous_caller_with_store_keeps_bearer_passthrough() {
        let store = tt_auth::credentials::InMemoryProviderCredentialStore::new();
        let state =
            AppState::with_default_providers().with_credential_store(std::sync::Arc::new(store));

        let got = resolve_credentials_for(&state, Uuid::nil(), "openai", "sk-caller-own-key", true)
            .await
            .expect("anonymous BYO passthrough must keep working");
        assert_eq!(got.api_key.expose(), "sk-caller-own-key");
    }

    /// A verified org WITH a stored credential resolves it — the BYO-only
    /// guard only fires on a miss.
    #[tokio::test]
    async fn verified_org_with_stored_credential_resolves_it() {
        let store = tt_auth::credentials::InMemoryProviderCredentialStore::new();
        let org = Uuid::now_v7();
        store.insert(
            org,
            "openai",
            ProviderCredentials {
                api_key: SecretString::new("sk-org-own"),
                base_url: None,
                extra_headers: Vec::new(),
            },
        );
        let state =
            AppState::with_default_providers().with_credential_store(std::sync::Arc::new(store));

        let got = resolve_credentials_for(&state, org, "openai", "tt_live_abc123", true)
            .await
            .expect("stored credential");
        assert_eq!(got.api_key.expose(), "sk-org-own");
    }
}

#[cfg(test)]
mod l2_baseline_tests {
    use super::*;

    fn entry(baseline_cost_usd: Option<f64>) -> CacheEntry {
        CacheEntry {
            id: Uuid::now_v7(),
            org_id: Uuid::nil(),
            embedding: vec![1.0, 0.0],
            response: b"{}".to_vec(),
            model: "gpt-4o-mini".into(),
            embedding_model: "text-embedding-3-small".into(),
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            baseline_cost_usd,
            request_delta_evidence_state: if baseline_cost_usd.is_some() {
                RequestDeltaEvidenceState::Measured
            } else {
                RequestDeltaEvidenceState::MissingEvidence
            },
            hit_count: 0,
            quality_score: None,
            judge_verdict: None,
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            lexical_sig: None,
        }
    }

    fn pricing(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    #[test]
    fn l2_hit_log_preserves_matched_immutable_route_version() {
        let ctx = RequestContext {
            budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
            trace_id: Uuid::nil(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new("test"),
                base_url: None,
                extra_headers: vec![],
            },
            tag: None,
            deadline: None,
            run_id: None,
            node_id: None,
        };
        let route_id = Uuid::now_v7();
        let row = request_log_for_l2_hit(
            &entry(Some(0.0123)),
            &ctx,
            "caller-model",
            Uuid::now_v7(),
            Instant::now(),
            Some(route_id),
            Some(9_876_543_210),
            false,
            0.0123,
            RequestDeltaEvidenceState::Measured,
            0,
            0.97,
            L2VerifyDecision::Confident,
        );
        assert_eq!(row.route_id, Some(route_id));
        assert_eq!(row.route_version_id, Some(9_876_543_210));
        assert_eq!(row.requested_model.as_deref(), Some("caller-model"));
    }

    /// The row's stored catalog baseline is authoritative — current pricing
    /// must not override it.
    #[test]
    fn stored_baseline_wins_over_current_catalog() {
        let e = entry(Some(0.0123));
        let p = pricing(3.0, 6.0);
        assert_eq!(l2_entry_baseline(&e, Some(&p)), 0.0123);
        assert_eq!(l2_entry_baseline(&e, None), 0.0123);
    }

    /// A pre-migration row (NULL baseline) is re-priced against the current
    /// catalog for its stored model/token counts.
    #[test]
    fn null_baseline_falls_back_to_current_catalog() {
        let e = entry(None);
        // gpt-4o-mini-class rates: $0.15/M input, $0.60/M output.
        let p = pricing(0.15, 0.60);
        let got = l2_entry_baseline(&e, Some(&p));
        // 1M input × $0.15/M + 0.5M output × $0.60/M = 0.15 + 0.30 = 0.45.
        assert!((got - 0.45).abs() < 1e-12, "expected 0.45, got {got}");
        // Sanity: the old hardcoded $1/M·$2/M placeholder would have claimed
        // $2.00 here — a ~4.4x overstatement for this cheap model.
        assert!(
            (got - 2.0).abs() > 1.0,
            "must not be the placeholder figure"
        );
    }

    /// NULL baseline AND no catalog entry for the model: report 0 saved —
    /// never fabricate a rate.
    #[test]
    fn null_baseline_without_catalog_pricing_reports_zero() {
        let e = entry(None);
        assert_eq!(l2_entry_baseline(&e, None), 0.0);
    }
}

#[cfg(test)]
mod cache_bust_tests {
    use super::*;

    fn pricing(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    fn usage_1m_input() -> Usage {
        Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        }
    }

    /// The booked cache-bust penalty lands fee-applied in
    /// `cache_bust_penalty_usd`, reduces `tt_saved_usd` PRE-clamp (and clamps
    /// at 0 when it exceeds the savings), and never contaminates
    /// `cost_usd` / `baseline_cost_usd` (invoice-reconcilable fields).
    #[test]
    fn cache_bust_penalty_reduces_tt_saved_pre_clamp() {
        let served = pricing(1.0, 2.0);
        let requested = pricing(5.0, 10.0); // routed-down: baseline 5x cost
        let usage = usage_1m_input();

        // No bust: headline = baseline − cost = 5.0 − 1.0 = 4.0.
        let no_bust = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            PassEffects::default(),
            0,
            crate::shaping::ShapeEffects::default(),
        );
        assert!((no_bust.tt_saved_usd() - 4.0).abs() < 1e-9);
        assert_eq!(no_bust.cache_bust_penalty_usd, 0.0);

        // A $1.50 bust reduces the headline to 2.5 — cost/baseline unchanged.
        let effects = PassEffects {
            compression_tokens_removed: 0,
            cache_bust_penalty_usd: 1.5,
            ..Default::default()
        };
        let bd = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            effects,
            0,
            crate::shaping::ShapeEffects::default(),
        );
        assert!((bd.cache_bust_penalty_usd - 1.5).abs() < 1e-9);
        assert!((bd.tt_saved_usd() - 2.5).abs() < 1e-9);
        assert!(
            (bd.cost_usd - no_bust.cost_usd).abs() < 1e-12,
            "the penalty must never be folded into cost_usd"
        );
        assert!(
            (bd.baseline_cost_usd - no_bust.baseline_cost_usd).abs() < 1e-12,
            "the penalty must never be folded into baseline_cost_usd"
        );

        // The fee multiplier scales the penalty like every other figure.
        let bd_fee = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.05,
            false,
            false,
            effects,
            0,
            crate::shaping::ShapeEffects::default(),
        );
        assert!((bd_fee.cache_bust_penalty_usd - 1.5 * 1.05).abs() < 1e-9);

        // A penalty larger than the savings clamps the headline at 0 — never
        // a negative saving.
        let big = PassEffects {
            compression_tokens_removed: 0,
            cache_bust_penalty_usd: 100.0,
            ..Default::default()
        };
        let clamped = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            big,
            0,
            crate::shaping::ShapeEffects::default(),
        );
        assert_eq!(clamped.tt_saved_usd(), 0.0);
        assert!(
            (clamped.cost_usd - 1.0).abs() < 1e-9,
            "clamping happens in the headline only"
        );
    }

    /// `compute_cost` / `compute_cost_with_flex` (the legacy entry points)
    /// carry zero pass effects — no phantom penalty on any existing path.
    #[test]
    fn legacy_entry_points_have_zero_bust_penalty() {
        let p = pricing(1.0, 2.0);
        let usage = usage_1m_input();
        assert_eq!(
            compute_cost(&usage, Some(&p), Some(&p), 1.0).cache_bust_penalty_usd,
            0.0
        );
        assert_eq!(
            compute_cost_with_flex(&usage, Some(&p), Some(&p), 1.0, false).cache_bust_penalty_usd,
            0.0
        );
    }

    /// Sub-lever 2 field-drop + judge-passed summary input-token removals ride
    /// `tt_saved_usd` EXACTLY like the compression pass: they are valued at the
    /// served input rate (lossless / judge-gated reductions in billed input
    /// tokens) and the SAME token count raises `baseline_cost_usd` at the
    /// baseline input rate, so the `baseline − cost` headline picks them up.
    /// (The realized `cost_usd` already excludes them — the upstream metered the
    /// reduced prompt.) This is the agentic-budget extension of the compression
    /// precedent (`chat.rs:4147-4151`).
    #[test]
    fn field_drop_and_summary_tokens_raise_baseline_like_compression() {
        // Routed-down: served 1×, baseline 5× — so the baseline fold is at a
        // different rate than the served valuation (each side priced honestly).
        let served = pricing(1.0, 2.0);
        let requested = pricing(5.0, 10.0);
        let usage = usage_1m_input();

        let control = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            PassEffects::default(),
            0,
            crate::shaping::ShapeEffects::default(),
        );

        // 100k field-dropped + 50k summary-removed = 150k input tokens removed.
        let effects = PassEffects {
            elide_field_drop_tokens_removed: 100_000,
            elide_summary_tokens_removed: 50_000,
            ..Default::default()
        };
        let bd = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            effects,
            0,
            crate::shaping::ShapeEffects::default(),
        );

        // Served-rate valuation rides `compression_saved_usd` (the same lossless
        // billed-input-token reduction bucket): 150k × $1/M.
        let served_saving = 150_000.0 * 1.0 / 1e6;
        assert!(
            (bd.compression_saved_usd - (control.compression_saved_usd + served_saving)).abs()
                < 1e-12,
            "elide tokens must be valued at the served input rate like compression"
        );

        // Baseline raised by the SAME token count at the BASELINE input rate:
        // 150k × $5/M. Cost unchanged (the upstream already metered the reduced
        // prompt — these tokens were never billed).
        let baseline_fold = 150_000.0 * 5.0 / 1e6;
        assert!(
            (bd.baseline_cost_usd - (control.baseline_cost_usd + baseline_fold)).abs() < 1e-12,
            "elide tokens must raise baseline at the baseline input rate"
        );
        assert!(
            (bd.cost_usd - control.cost_usd).abs() < 1e-12,
            "elide removals never touch the realized cost"
        );

        // The headline picks the elide saving up via `baseline − cost`.
        assert!(
            (bd.tt_saved_usd() - (control.tt_saved_usd() + baseline_fold)).abs() < 1e-9,
            "elide saving must ride tt_saved_usd like compression"
        );
    }

    /// Document-compaction (Document Lane D2) input-token removals ride
    /// `tt_saved_usd` EXACTLY like compression (served-rate valuation + baseline
    /// fold) but attribute to their OWN `doc_compaction_saved_usd` bucket —
    /// never double-counted against `compression_saved_usd`.
    #[test]
    fn doc_compaction_tokens_raise_baseline_and_isolate_own_bucket() {
        // Routed-down: served 1×, baseline 5× (baseline fold at a different rate
        // than the served valuation, so each side is priced honestly).
        let served = pricing(1.0, 2.0);
        let requested = pricing(5.0, 10.0);
        let usage = usage_1m_input();

        let control = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            PassEffects::default(),
            0,
            crate::shaping::ShapeEffects::default(),
        );

        // 200k input tokens removed by doc-compaction (compression bucket empty).
        let effects = PassEffects {
            doc_compaction_tokens_removed: 200_000,
            ..Default::default()
        };
        let bd = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            effects,
            0,
            crate::shaping::ShapeEffects::default(),
        );

        // Valued into the OWN bucket at the served input rate: 200k × $1/M.
        let served_saving = 200_000.0 * 1.0 / 1e6;
        assert!(
            (bd.doc_compaction_saved_usd - served_saving).abs() < 1e-12,
            "doc-compaction tokens must be valued at the served input rate in the own bucket"
        );
        // NOT double-counted into the compression bucket.
        assert!(
            (bd.compression_saved_usd - control.compression_saved_usd).abs() < 1e-12,
            "doc-compaction must not bleed into compression_saved_usd"
        );

        // Baseline raised by the SAME token count at the BASELINE input rate:
        // 200k × $5/M. Realized cost unchanged (never billed).
        let baseline_fold = 200_000.0 * 5.0 / 1e6;
        assert!(
            (bd.baseline_cost_usd - (control.baseline_cost_usd + baseline_fold)).abs() < 1e-12,
            "doc-compaction tokens must raise baseline at the baseline input rate"
        );
        assert!(
            (bd.cost_usd - control.cost_usd).abs() < 1e-12,
            "doc-compaction removals never touch the realized cost"
        );

        // The headline picks the doc-compaction saving up via `baseline − cost`.
        assert!(
            (bd.tt_saved_usd() - (control.tt_saved_usd() + baseline_fold)).abs() < 1e-9,
            "doc-compaction saving must ride tt_saved_usd like compression"
        );
    }

    /// The `doc_compaction_saved_usd` figure is surfaced on its own
    /// `x-tokentrimmer-doc-compaction-saved-usd` response header, and a default
    /// (un-opted) breakdown emits `0.000000`.
    #[test]
    fn doc_compaction_header_emitted() {
        let mut headers = axum::http::HeaderMap::new();
        attach_cost_headers(
            &mut headers,
            Uuid::nil(),
            "openai",
            "gpt-4o",
            &CostBreakdown {
                doc_compaction_saved_usd: 0.001234,
                ..Default::default()
            },
        );
        assert_eq!(
            headers
                .get("x-tokentrimmer-doc-compaction-saved-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.001234")
        );

        // Default breakdown → 0.000000 on the header.
        let mut default_headers = axum::http::HeaderMap::new();
        attach_cost_headers(
            &mut default_headers,
            Uuid::nil(),
            "openai",
            "gpt-4o",
            &CostBreakdown::default(),
        );
        assert_eq!(
            default_headers
                .get("x-tokentrimmer-doc-compaction-saved-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.000000")
        );
    }

    /// The summarizer-LLM "tax" is REAL aux spend: it reduces `tt_saved_usd`
    /// pre-clamp (the win is honestly net-of-tax) and surfaces in its OWN
    /// field/header, but is NEVER folded into `cost_usd` / `baseline_cost_usd`
    /// (caveat C3's sibling: aux-spend / estimate channels stay out of the
    /// invoice-reconciled figures). Mirrors the cache-bust precedent.
    #[test]
    fn summarizer_tax_stays_out_of_invoice_fields() {
        let served = pricing(1.0, 2.0);
        let requested = pricing(5.0, 10.0); // baseline 5×: headline = 5 − 1 = 4.0
        let usage = usage_1m_input();

        let no_tax = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            PassEffects::default(),
            0,
            crate::shaping::ShapeEffects::default(),
        );
        assert!((no_tax.tt_saved_usd() - 4.0).abs() < 1e-9);
        assert_eq!(no_tax.summarizer_tax_usd, 0.0);

        // A $0.30 summarizer tax reduces the headline to 3.7 — cost/baseline
        // unchanged.
        let effects = PassEffects {
            summarizer_tax_usd: 0.30,
            ..Default::default()
        };
        let bd = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            effects,
            0,
            crate::shaping::ShapeEffects::default(),
        );
        assert!((bd.summarizer_tax_usd - 0.30).abs() < 1e-9);
        assert!((bd.tt_saved_usd() - 3.7).abs() < 1e-9);
        assert!(
            (bd.cost_usd - no_tax.cost_usd).abs() < 1e-12,
            "the summarizer tax must never be folded into cost_usd"
        );
        assert!(
            (bd.baseline_cost_usd - no_tax.baseline_cost_usd).abs() < 1e-12,
            "the summarizer tax must never be folded into baseline_cost_usd"
        );

        // The fee multiplier scales the tax like every other figure.
        let bd_fee = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.05,
            false,
            false,
            effects,
            0,
            crate::shaping::ShapeEffects::default(),
        );
        assert!((bd_fee.summarizer_tax_usd - 0.30 * 1.05).abs() < 1e-9);
    }

    /// Caveat C3: the Anthropic cl100k proxy undercounts by ~15–20%, so any
    /// `CacheBustEstimate` priced from it is systematically LOW. Under-booking a
    /// NEGATIVE entry favors TT — acceptable ONLY because the bust is an
    /// estimate channel (its own field/header) and is NEVER copied into the
    /// invoice-reconciled `cost_usd` / `baseline_cost_usd`. This extends the
    /// `cache_bust_penalty_reduces_tt_saved_pre_clamp` precedent with the
    /// explicit C3 contract.
    #[test]
    fn cache_bust_estimate_stays_out_of_invoice_fields() {
        let served = pricing(1.0, 2.0);
        let requested = pricing(5.0, 10.0); // headline = 5 − 1 = 4.0
        let usage = usage_1m_input();

        let no_bust = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            PassEffects::default(),
            0,
            crate::shaping::ShapeEffects::default(),
        );

        // A $0.80 bust (priced from the systematically-LOW cl100k proxy — so the
        // TRUE induced cost is HIGHER, but under-booking a negative favors TT and
        // is acceptable here because the bust never reaches the invoice fields).
        let effects = PassEffects {
            cache_bust_penalty_usd: 0.80,
            ..Default::default()
        };
        let bd = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            effects,
            0,
            crate::shaping::ShapeEffects::default(),
        );
        // Surfaces in its own field and reduces the headline pre-clamp.
        assert!((bd.cache_bust_penalty_usd - 0.80).abs() < 1e-9);
        assert!((bd.tt_saved_usd() - (4.0 - 0.80)).abs() < 1e-9);
        // INVOICE fields are byte-identical to the no-bust path — the estimate
        // never contaminates the realized-cost-reconcilable figures.
        assert!(
            (bd.cost_usd - no_bust.cost_usd).abs() < 1e-12,
            "the cl100k-priced bust estimate must never enter cost_usd"
        );
        assert!(
            (bd.baseline_cost_usd - no_bust.baseline_cost_usd).abs() < 1e-12,
            "the cl100k-priced bust estimate must never enter baseline_cost_usd"
        );

        // The header carries the estimate so a CFO can unpick it.
        let mut headers = axum::http::HeaderMap::new();
        attach_cost_headers(&mut headers, Uuid::nil(), "anthropic", "claude", &bd);
        assert_eq!(
            headers
                .get("x-tokentrimmer-cache-bust-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.800000")
        );
    }
}

#[cfg(test)]
mod shape_cost_tests {
    use super::*;
    use crate::shaping::ShapeEffects;

    fn pricing(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    fn usage(prompt: u64, completion: u64) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cached_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        }
    }

    /// The MEASURED diff saving raises the baseline at the baseline output
    /// rate and lands fee-applied in `diff_saved_usd`, so `tt_saved_usd()`
    /// includes it (the compression precedent).
    #[test]
    fn diff_saving_folds_into_baseline_and_headline() {
        let p = pricing(1.0, 2.0);
        // Billed: 1000-token patch. Avoided: 99_000 more output tokens.
        let u = usage(10_000, 1_000);
        let shape = ShapeEffects {
            diff_output_tokens_saved: 99_000,
            ..Default::default()
        };
        let bd = compute_cost_full(
            &u,
            Some(&p),
            Some(&p),
            1.05,
            false,
            false,
            PassEffects::default(),
            0,
            shape,
        );
        let no_shape = compute_cost_full(
            &u,
            Some(&p),
            Some(&p),
            1.05,
            false,
            false,
            PassEffects::default(),
            0,
            ShapeEffects::default(),
        );
        // diff_saved = 99k × $2/M × 1.05 fee.
        let expected_saved = 99_000.0 * 2.0 / 1e6 * 1.05;
        assert!((bd.diff_saved_usd - expected_saved).abs() < 1e-9);
        // Baseline raised by exactly the saved-token value; cost unchanged.
        assert!(
            (bd.baseline_cost_usd - (no_shape.baseline_cost_usd + expected_saved)).abs() < 1e-9
        );
        assert!((bd.cost_usd - no_shape.cost_usd).abs() < 1e-12);
        // The headline picks the diff saving up via baseline − cost.
        assert!((bd.tt_saved_usd() - (no_shape.tt_saved_usd() + expected_saved)).abs() < 1e-9);
    }

    /// The format-switch ESTIMATE lands fee-applied in its OWN field and is
    /// EXCLUDED from baseline / `tt_saved_usd()` (the batch_forgone
    /// precedent: estimates never contaminate invoice-reconciled figures).
    #[test]
    fn format_switch_estimate_excluded_from_headline() {
        let p = pricing(1.0, 2.0);
        let u = usage(10_000, 500);
        let shape = ShapeEffects {
            format_switch_saved_est_usd: 0.5,
            ..Default::default()
        };
        let bd = compute_cost_full(
            &u,
            Some(&p),
            Some(&p),
            1.05,
            false,
            false,
            PassEffects::default(),
            0,
            shape,
        );
        let control = compute_cost_full(
            &u,
            Some(&p),
            Some(&p),
            1.05,
            false,
            false,
            PassEffects::default(),
            0,
            ShapeEffects::default(),
        );
        assert!((bd.format_switch_saved_est_usd - 0.5 * 1.05).abs() < 1e-12);
        assert!((bd.cost_usd - control.cost_usd).abs() < 1e-12);
        assert!((bd.baseline_cost_usd - control.baseline_cost_usd).abs() < 1e-12);
        assert!(
            (bd.tt_saved_usd() - control.tt_saved_usd()).abs() < 1e-12,
            "the estimate must never ride the headline"
        );
    }

    /// The failed-patch cost FOLDS into `cost_usd` (real invoice spend for
    /// the trace) AND its own field; on a pure-failure trace (no model
    /// downgrade — baseline == re-emit cost) the headline clamps to 0: the
    /// double dispatch can never fabricate a saving.
    #[test]
    fn diff_failed_cost_folds_into_cost_and_headline_clamps() {
        let p = pricing(1.0, 2.0);
        // The re-emit's usage (what the caller's row meters).
        let u = usage(10_000, 50_000);
        let shape = ShapeEffects {
            diff_failed_cost_usd: 0.02, // pre-fee patch-attempt spend
            ..Default::default()
        };
        let bd = compute_cost_full(
            &u,
            Some(&p),
            Some(&p),
            1.05,
            false,
            false,
            PassEffects::default(),
            0,
            shape,
        );
        let control = compute_cost_full(
            &u,
            Some(&p),
            Some(&p),
            1.05,
            false,
            false,
            PassEffects::default(),
            0,
            ShapeEffects::default(),
        );
        assert!((bd.diff_failed_cost_usd - 0.02 * 1.05).abs() < 1e-12);
        assert!(
            (bd.cost_usd - (control.cost_usd + 0.02 * 1.05)).abs() < 1e-12,
            "the failed attempt is real invoice spend — cost_usd must carry it"
        );
        // Baseline is re-emit-only ⇒ baseline < cost ⇒ headline clamps to 0.
        assert!((bd.baseline_cost_usd - control.baseline_cost_usd).abs() < 1e-12);
        assert_eq!(
            bd.tt_saved_usd(),
            0.0,
            "never a fabricated saving on failure"
        );
    }

    /// attach_cost_headers emits the three new always-present headers with
    /// 0.000000 defaults on unshaped traffic and the breakdown figures
    /// otherwise.
    #[test]
    fn cost_headers_carry_shaping_figures() {
        let mut headers = axum::http::HeaderMap::new();
        attach_cost_headers(
            &mut headers,
            Uuid::nil(),
            "openai",
            "gpt-4o",
            &CostBreakdown::default(),
        );
        for name in [
            "x-tokentrimmer-diff-saved-usd",
            "x-tokentrimmer-format-switch-saved-est-usd",
            "x-tokentrimmer-diff-failed-cost-usd",
        ] {
            assert_eq!(
                headers.get(name).and_then(|v| v.to_str().ok()),
                Some("0.000000"),
                "{name} must be always-present with a zero default"
            );
        }

        let mut headers = axum::http::HeaderMap::new();
        attach_cost_headers(
            &mut headers,
            Uuid::nil(),
            "openai",
            "gpt-4o",
            &CostBreakdown {
                diff_saved_usd: 0.123456,
                format_switch_saved_est_usd: 0.000042,
                diff_failed_cost_usd: 0.0021,
                ..Default::default()
            },
        );
        assert_eq!(
            headers
                .get("x-tokentrimmer-diff-saved-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.123456")
        );
        assert_eq!(
            headers
                .get("x-tokentrimmer-format-switch-saved-est-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.000042")
        );
        assert_eq!(
            headers
                .get("x-tokentrimmer-diff-failed-cost-usd")
                .and_then(|v| v.to_str().ok()),
            Some("0.002100")
        );
    }

    /// `set_assistant_text` swaps the first choice's assistant text in place.
    #[test]
    fn set_assistant_text_replaces_first_choice() {
        let mut resp = ChatCompletionResponse {
            id: "x".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "m".into(),
            choices: vec![Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("patch".into())),
                    tool_calls: vec![],
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        set_assistant_text(&mut resp, "full artifact".into());
        assert_eq!(response_assistant_text(&resp), "full artifact");
    }
}

#[cfg(test)]
mod fee_tests {
    use super::*;

    fn flat_pricing() -> ModelPricing {
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    #[test]
    fn fee_multiplier_scales_cost_and_baseline() {
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let p = flat_pricing();
        let bd = compute_cost(&usage, Some(&p), Some(&p), 1.0);
        let bd_fee = compute_cost(&usage, Some(&p), Some(&p), 1.05);
        // 1M input @ $1/M = $1.00 with no fee.
        assert!((bd.cost_usd - 1.0).abs() < 1e-9, "cost = {}", bd.cost_usd);
        // OpenRouter's 5% BYOK fee scales cost and baseline by 1.05.
        assert!(
            (bd_fee.cost_usd - 1.05).abs() < 1e-9,
            "cost_fee = {}",
            bd_fee.cost_usd
        );
        assert!(
            (bd_fee.baseline_cost_usd - bd.baseline_cost_usd * 1.05).abs() < 1e-12,
            "base_fee = {}",
            bd_fee.baseline_cost_usd
        );
    }

    fn flex_pricing() -> ModelPricing {
        ModelPricing {
            // Standard $10/$30, flex $5/$15 (exactly 50% — verified flex==batch).
            input_per_million: 10.0,
            output_per_million: 30.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: Some(5.0),
            flex_output_per_million: Some(15.0),
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    /// Flex served: cost is metered at flex rates and the flex saving equals
    /// the standard baseline minus the flex cost for the usage (the `flex`
    /// source). Headline `tt_saved_usd` equals that saving (no routing/cache).
    #[test]
    fn flex_attributes_standard_minus_flex_saving() {
        let usage = Usage {
            prompt_tokens: 1_000,
            completion_tokens: 500,
            total_tokens: 1_500,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let p = flex_pricing();
        let bd = compute_cost_with_flex(&usage, Some(&p), Some(&p), 1.0, true);

        // standard = 1000×$10/M + 500×$30/M = 0.01 + 0.015 = 0.025
        // flex     = 1000×$5/M  + 500×$15/M = 0.005 + 0.0075 = 0.0125
        let standard = 1000.0 * 10.0 / 1e6 + 500.0 * 30.0 / 1e6;
        let flex = 1000.0 * 5.0 / 1e6 + 500.0 * 15.0 / 1e6;
        assert!((bd.cost_usd - flex).abs() < 1e-12, "cost = {}", bd.cost_usd);
        assert!(
            (bd.flex_saved_usd - (standard - flex)).abs() < 1e-12,
            "flex_saved = {}",
            bd.flex_saved_usd
        );
        // baseline == standard served cost (no routing) → tt_saved == flex_saved.
        assert!((bd.baseline_cost_usd - standard).abs() < 1e-12);
        assert!(
            (bd.tt_saved_usd() - (standard - flex)).abs() < 1e-12,
            "tt_saved = {}",
            bd.tt_saved_usd()
        );
    }

    /// `flex_applied=false` on a flex-eligible model leaves cost at standard and
    /// claims zero flex saving (the flag, not the rate, gates the discount).
    #[test]
    fn flex_not_applied_keeps_standard_cost_and_zero_flex_saving() {
        let usage = Usage {
            prompt_tokens: 1_000,
            completion_tokens: 500,
            total_tokens: 1_500,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let p = flex_pricing();
        let bd = compute_cost_with_flex(&usage, Some(&p), Some(&p), 1.0, false);
        let standard = 1000.0 * 10.0 / 1e6 + 500.0 * 30.0 / 1e6;
        assert!(
            (bd.cost_usd - standard).abs() < 1e-12,
            "cost = {}",
            bd.cost_usd
        );
        assert_eq!(bd.flex_saved_usd, 0.0);
    }

    /// Flex composes with a downgrade route: baseline is the (expensive)
    /// originally-requested model, cost is the flex-rate served model, and the
    /// flex saving isolates only the standard→flex delta at the served model.
    #[test]
    fn flex_composes_with_routing_baseline() {
        let usage = Usage {
            prompt_tokens: 1_000,
            completion_tokens: 500,
            total_tokens: 1_500,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let served = flex_pricing(); // $10/$30 std, $5/$15 flex
        let requested = ModelPricing {
            input_per_million: 40.0,
            output_per_million: 120.0,
            ..flex_pricing()
        };
        let bd = compute_cost_with_flex(&usage, Some(&served), Some(&requested), 1.0, true);

        let flex = 1000.0 * 5.0 / 1e6 + 500.0 * 15.0 / 1e6; // 0.0125
        let served_standard = 1000.0 * 10.0 / 1e6 + 500.0 * 30.0 / 1e6; // 0.025
        let requested_baseline = 1000.0 * 40.0 / 1e6 + 500.0 * 120.0 / 1e6; // 0.10
        assert!((bd.cost_usd - flex).abs() < 1e-12);
        assert!((bd.baseline_cost_usd - requested_baseline).abs() < 1e-12);
        // flex_saved isolates ONLY the served standard→flex delta.
        assert!((bd.flex_saved_usd - (served_standard - flex)).abs() < 1e-12);
        // headline = routing + flex combined = baseline − flex cost.
        assert!((bd.tt_saved_usd() - (requested_baseline - flex)).abs() < 1e-12);
    }
}

/// Tests for the Anthropic cache-write-rate fix (rv-anthropic-cache-write-rate).
///
/// Token-budget breakdown enforced by compute_cost:
///   cache_read    → cached_input_per_million  (or base if absent)
///   cache_write   → cache_write_per_million   (or base if absent; non-Anthropic unchanged)
///   fresh_input   → input_per_million
///   (cache_read + cache_write + fresh_input == prompt_tokens — no double counting)
///   output        → output_per_million
///
/// Covers:
/// (a) Model WITH cache_write rate prices cache_creation at the write premium.
/// (b) Model WITHOUT cache_write rate is unchanged — cache_creation gets base input rate.
/// (c) cache_read tokens are priced at the cached (discounted) rate.
/// (d) No double counting: sum of priced buckets == all prompt_tokens.
#[cfg(test)]
mod cache_write_rate_tests {
    use super::*;

    /// Anthropic-like pricing: $3/$15 input/output, $0.30 cache-read, $3.75 cache-write.
    fn anthropic_pricing() -> ModelPricing {
        ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.30),
            cache_write_per_million: Some(3.75),
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    /// Non-Anthropic pricing: base input rate only, no cache-write premium.
    fn no_write_rate_pricing() -> ModelPricing {
        ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.30),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    // (a) cache_creation tokens priced at write-premium rate (strictly > base input rate).
    #[test]
    fn cache_creation_with_write_rate_costs_more_than_base_input() {
        // 0 fresh input, 0 cache_read, 1M cache_write, 0 output.
        // At write rate ($3.75/M): $3.75.
        // At base input rate ($3.00/M): $3.00.
        let p = anthropic_pricing();
        let usage_write = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_input_tokens: Some(1_000_000),
            cache_read_input_tokens: None,
        };
        let usage_base = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_input_tokens: None, // same tokens, no write bucket
            cache_read_input_tokens: None,
        };
        let cost_write = compute_cost(&usage_write, Some(&p), Some(&p), 1.0).cost_usd;
        let cost_base = compute_cost(&usage_base, Some(&p), Some(&p), 1.0).cost_usd;
        assert!(
            cost_write > cost_base,
            "write-premium cost ({cost_write}) must exceed base-input cost ({cost_base})"
        );
        assert!(
            (cost_write - 3.75).abs() < 1e-9,
            "1M cache_write @ $3.75/M = $3.75, got {cost_write}"
        );
    }

    // (b) Without a write rate, cache_creation tokens fall back to base input rate (unchanged behavior).
    #[test]
    fn cache_creation_without_write_rate_uses_base_input_rate() {
        let p = no_write_rate_pricing();
        // 1M cache_write, 0 cache_read, 0 fresh — should price at $3.00/M.
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_input_tokens: Some(1_000_000),
            cache_read_input_tokens: None,
        };
        let cost = compute_cost(&usage, Some(&p), Some(&p), 1.0).cost_usd;
        assert!(
            (cost - 3.0).abs() < 1e-9,
            "1M cache_write with no write-rate @ $3.00/M = $3.00, got {cost}"
        );
    }

    // (c) cache_read tokens use the discounted cached rate, not base input rate.
    #[test]
    fn cache_read_uses_cached_rate() {
        let p = anthropic_pricing();
        // 1M cache_read, 0 cache_write, 0 fresh — should price at $0.30/M.
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 1_000_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let cost = compute_cost(&usage, Some(&p), Some(&p), 1.0).cost_usd;
        assert!(
            (cost - 0.30).abs() < 1e-9,
            "1M cache_read @ $0.30/M = $0.30, got {cost}"
        );
    }

    // (d) No double counting: fresh + cache_read + cache_write cover all prompt_tokens exactly.
    #[test]
    fn no_double_counting_three_buckets_sum_to_prompt_tokens() {
        // 400K fresh, 300K cache_read, 300K cache_write = 1M prompt_tokens.
        // Expected cost: 400K @ $3.00/M + 300K @ $0.30/M + 300K @ $3.75/M
        //   = $1.20 + $0.09 + $1.125 = $2.415
        let p = anthropic_pricing();
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 300_000,
            cache_creation_input_tokens: Some(300_000),
            cache_read_input_tokens: None,
        };
        let cost = compute_cost(&usage, Some(&p), Some(&p), 1.0).cost_usd;
        let expected = (400_000.0 * 3.0 + 300_000.0 * 0.30 + 300_000.0 * 3.75) / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-9,
            "expected {expected}, got {cost}"
        );
    }

    // (e) End-to-end against the REAL pricing catalog: given N cache-write +
    //     M cache-read + K fresh input tokens for a catalogued model, the cost
    //     matches the catalog rates to the cent — write premium and read
    //     discount both applied. Guards the wiring from `tt_shared::pricing`
    //     into `compute_cost`, not just the synthetic-pricing math above.
    #[test]
    fn catalog_grounded_breakdown_matches_to_the_cent() {
        // claude-sonnet-4-6 catalog rates (data/pricing.toml):
        //   input 3.00, output 15.00, cache-read 0.30, cache-write 3.75 (5-min).
        let p = tt_shared::pricing::catalog()
            .latest("anthropic", "claude-sonnet-4-6")
            .expect("sonnet present in catalog");

        // K=500_000 fresh, M=300_000 cache-read, N=200_000 cache-write,
        // 100_000 output. prompt_tokens = K+M+N = 1_000_000.
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 100_000,
            total_tokens: 1_100_000,
            cached_tokens: 300_000,
            cache_creation_input_tokens: Some(200_000),
            cache_read_input_tokens: None,
        };
        let cost = compute_cost(&usage, Some(&p), Some(&p), 1.0).cost_usd;

        // 500K×$3.00 + 300K×$0.30 + 200K×$3.75 + 100K×$15.00, all /1M:
        //   1.50 + 0.09 + 0.75 + 1.50 = $3.84.
        let expected = (500_000.0 * 3.0 + 300_000.0 * 0.30 + 200_000.0 * 3.75 + 100_000.0 * 15.0)
            / 1_000_000.0;
        assert!(
            (cost - 3.84).abs() < 1e-9,
            "catalog-grounded cost should be exactly $3.84, got {cost}"
        );
        assert!(
            (cost - expected).abs() < 1e-9,
            "expected {expected}, got {cost}"
        );

        // The write premium must actually be billed: pricing the same writes at
        // the base input rate ($3.00) would undercount by 200K×($3.75−$3.00)/1M.
        let undercounted =
            (500_000.0 * 3.0 + 300_000.0 * 0.30 + 200_000.0 * 3.0 + 100_000.0 * 15.0) / 1_000_000.0;
        assert!(
            cost > undercounted,
            "write premium must raise cost above the base-input mispricing"
        );
        assert!(
            (cost - undercounted - 0.15).abs() < 1e-9,
            "premium delta = 200K × ($3.75 − $3.00)/1M = $0.15"
        );
    }

    // (f) compute_cost meters writes at the 5-minute tier (1.25×), NOT the
    //     1-hour tier (2×). The gateway only ever writes the 5-min tier, so the
    //     headline cost must use it; the 1-hour rate is available on the catalog
    //     for when a 1-hour write is introduced, but must not be applied here.
    #[test]
    fn writes_metered_at_five_min_not_one_hour_tier() {
        let p = tt_shared::pricing::catalog()
            .latest("anthropic", "claude-sonnet-4-6")
            .expect("sonnet present");
        // 1M cache-write only.
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_input_tokens: Some(1_000_000),
            cache_read_input_tokens: None,
        };
        let cost = compute_cost(&usage, Some(&p), Some(&p), 1.0).cost_usd;
        // 5-min tier = $3.75/M; 1-hour tier would be 2×$3.00 = $6.00/M.
        assert!(
            (cost - 3.75).abs() < 1e-9,
            "1M cache_write must bill at the 5-min tier ($3.75), got {cost}"
        );
        let one_hour = p
            .cache_write_rate_per_million(CacheWriteTier::OneHour)
            .expect("sonnet has a write premium");
        assert!(
            (one_hour - 6.00).abs() < 1e-9,
            "sanity: 1-hour tier is 2× base input ($6.00), not what compute_cost applied"
        );
    }
}

/// Tests for the provider-cache / TT-savings attribution split (P0 #12).
///
/// Rule: `saved_usd` (the headline) contains ONLY TokenTrimmer-caused savings;
/// the provider's automatic prompt-cache discount is reported separately as
/// `provider_cache_saved_usd` so the headline survives invoice reconciliation.
#[cfg(test)]
mod provider_cache_attribution_tests {
    use super::*;

    /// $3/$15 with a $0.30 cache-read discount and $3.75 write premium.
    fn pricing() -> ModelPricing {
        ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cached_input_per_million: Some(0.30),
            cache_write_per_million: Some(3.75),
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        }
    }

    /// Provider-reported cached tokens with NO TT optimization (same pricing
    /// for served and baseline — no routing, no TT cache): saved_usd == 0 and
    /// the provider-side discount is positive.
    #[test]
    fn provider_cached_tokens_without_tt_optimization_yield_zero_tt_saved() {
        let p = pricing();
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 500_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let bd = compute_cost(&usage, Some(&p), Some(&p), 1.0);
        // Actual bill: 500K fresh @ $3/M + 500K read @ $0.30/M = $1.65.
        assert!((bd.cost_usd - 1.65).abs() < 1e-9, "cost = {}", bd.cost_usd);
        // No-cache cost == baseline: 1M @ $3/M = $3.00.
        assert!(
            (bd.baseline_cost_usd - 3.0).abs() < 1e-9,
            "baseline = {}",
            bd.baseline_cost_usd
        );
        // The whole discount belongs to the provider…
        assert!(
            (bd.provider_cache_saved_usd - 1.35).abs() < 1e-9,
            "provider_cache_saved = {}",
            bd.provider_cache_saved_usd
        );
        // …and TT claims nothing.
        assert!(
            bd.tt_saved_usd().abs() < 1e-12,
            "tt_saved must be 0 with no TT optimization; got {}",
            bd.tt_saved_usd()
        );
    }

    /// Routing savings (cheaper served model) remain TT-attributed and exclude
    /// the provider's cache discount: tt_saved == baseline − served-model
    /// no-cache cost.
    #[test]
    fn routing_saving_excludes_provider_cache_discount() {
        let served = pricing(); // $3/$15
        let requested = ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        };
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 500_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let bd = compute_cost(&usage, Some(&served), Some(&requested), 1.0);
        // Baseline (requested model, no discount): $10.00.
        // Served no-cache cost: $3.00 → TT routing saving = $7.00.
        // Provider discount on the served model: $3.00 − $1.65 = $1.35.
        assert!(
            (bd.tt_saved_usd() - 7.0).abs() < 1e-9,
            "tt_saved = {}",
            bd.tt_saved_usd()
        );
        assert!(
            (bd.provider_cache_saved_usd - 1.35).abs() < 1e-9,
            "provider_cache_saved = {}",
            bd.provider_cache_saved_usd
        );
        // The two splits add up to the full apparent saving.
        let apparent = bd.baseline_cost_usd - bd.cost_usd;
        assert!(
            (bd.tt_saved_usd() + bd.provider_cache_saved_usd - apparent).abs() < 1e-9,
            "splits must sum to baseline − cost"
        );
    }

    /// A cache-write premium that exceeds the read discount must not produce a
    /// negative provider figure, and the excess must not inflate the TT claim.
    #[test]
    fn write_premium_dominating_clamps_provider_saved_at_zero() {
        let p = pricing();
        // All 1M prompt tokens are cache writes @ $3.75/M = $3.75 — more
        // expensive than the $3.00 no-cache cost.
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_input_tokens: Some(1_000_000),
            cache_read_input_tokens: None,
        };
        let bd = compute_cost(&usage, Some(&p), Some(&p), 1.0);
        assert_eq!(
            bd.provider_cache_saved_usd, 0.0,
            "never report a negative provider-side saving"
        );
        // cost ($3.75) > baseline ($3.00) → TT saving clamps at 0 too.
        assert_eq!(bd.tt_saved_usd(), 0.0);
    }

    /// The fee multiplier scales the provider-side figure consistently with
    /// cost and baseline.
    #[test]
    fn fee_multiplier_scales_provider_cache_saved() {
        let p = pricing();
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 500_000,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        };
        let base = compute_cost(&usage, Some(&p), Some(&p), 1.0);
        let scaled = compute_cost(&usage, Some(&p), Some(&p), 1.05);
        assert!(
            (scaled.provider_cache_saved_usd - base.provider_cache_saved_usd * 1.05).abs() < 1e-9,
            "provider_cache_saved must scale by the fee multiplier"
        );
    }
}

/// Tests for the per-tier TTL selection (rv-per-tier-ttl, spec §8.4).
///
/// Covers:
/// (a) `CallerTier::ttl_secs()` returns the correct values per tier.
/// (b) `effective_ttl_secs` returns the tier TTL when no override is present.
/// (c) Absent tier (None) falls back to the supplied default (24h behavior).
/// (d) `tt_extras` override wins over both tier and default.
#[cfg(test)]
mod tier_ttl_tests {
    use super::*;
    use tt_shared::CallerTier;

    const SECS_24H: u64 = 24 * 60 * 60;
    const SECS_7D: u64 = 7 * 24 * 60 * 60;
    const SECS_30D: u64 = 30 * 24 * 60 * 60;

    // (a) CallerTier::ttl_secs() returns the correct per-tier values.

    #[test]
    fn free_tier_ttl_is_24h() {
        assert_eq!(CallerTier::Free.ttl_secs(), SECS_24H);
    }

    #[test]
    fn pro_tier_ttl_is_7d() {
        assert_eq!(CallerTier::Pro.ttl_secs(), SECS_7D);
    }

    #[test]
    fn team_tier_ttl_is_7d() {
        // Team and Pro share the same 7d band per spec §8.4.
        assert_eq!(CallerTier::Team.ttl_secs(), SECS_7D);
    }

    #[test]
    fn scale_tier_ttl_is_30d() {
        assert_eq!(CallerTier::Scale.ttl_secs(), SECS_30D);
    }

    // (b) effective_ttl_secs returns the tier TTL when no override is present.

    #[test]
    fn no_override_free_tier_returns_24h() {
        assert_eq!(
            effective_ttl_secs(None, Some(CallerTier::Free), SECS_24H),
            SECS_24H
        );
    }

    #[test]
    fn no_override_pro_tier_returns_7d() {
        assert_eq!(
            effective_ttl_secs(None, Some(CallerTier::Pro), SECS_24H),
            SECS_7D
        );
    }

    #[test]
    fn no_override_team_tier_returns_7d() {
        assert_eq!(
            effective_ttl_secs(None, Some(CallerTier::Team), SECS_24H),
            SECS_7D
        );
    }

    #[test]
    fn no_override_scale_tier_returns_30d() {
        assert_eq!(
            effective_ttl_secs(None, Some(CallerTier::Scale), SECS_24H),
            SECS_30D
        );
    }

    // (c) Absent tier (None) falls back to the default — existing 24h behavior.

    #[test]
    fn absent_tier_uses_default() {
        assert_eq!(effective_ttl_secs(None, None, SECS_24H), SECS_24H);
    }

    #[test]
    fn absent_tier_uses_custom_default() {
        // When the caller passes a non-standard default, that default is honored.
        assert_eq!(effective_ttl_secs(None, None, 3600), 3600);
    }

    // (d) tt_extras override wins over both tier TTL and default.

    #[test]
    fn request_override_beats_tier_ttl() {
        let override_secs = 7200_u64;
        assert_eq!(
            effective_ttl_secs(Some(override_secs), Some(CallerTier::Scale), SECS_24H),
            override_secs
        );
    }

    #[test]
    fn request_override_beats_default() {
        let override_secs = 3600_u64;
        assert_eq!(
            effective_ttl_secs(Some(override_secs), None, SECS_24H),
            override_secs
        );
    }

    #[test]
    fn ttl_is_bounded_to_the_account_purge_fence_window() {
        assert_eq!(
            effective_ttl_secs(Some(0), None, SECS_24H),
            1,
            "zero-length overrides still need a valid Redis TTL"
        );
        assert_eq!(
            effective_ttl_secs(Some(u64::MAX), None, SECS_24H),
            crate::state::MAX_L1_TTL_SECS
        );
        assert_eq!(
            effective_ttl_secs(None, None, u64::MAX),
            crate::state::MAX_L1_TTL_SECS
        );
    }
}

#[cfg(test)]
mod l2_verify_gate_tests {
    use super::*;
    use std::sync::Arc;

    fn verify_cfg(epsilon: f32, min_agreement: f32) -> L2VerifyConfig {
        L2VerifyConfig {
            epsilon,
            min_agreement,
            tolerance_pct: 1.0,
            gate: Arc::new(tt_cache::AdaptiveClassThresholds::new(
                tt_cache::ClassThresholds::new(),
                tt_cache::FpGateTuning::default(),
            )),
        }
    }

    /// Gate off (`verify == None`) is ALWAYS Confident — today's behavior,
    /// regardless of similarity, signature presence, or text.
    #[test]
    fn l2_verify_decision_gate_disabled_is_always_confident() {
        for sim in [0.92_f32, 0.925, 0.99, 1.0] {
            for sig in [None, Some(0_i64), Some(tt_cache::lexical_sig("anything"))] {
                assert_eq!(
                    l2_verify_decision(None, sim, 0.92, sig, "whatever"),
                    L2VerifyDecision::Confident,
                    "gate off must always be Confident (sim {sim}, sig {sig:?})"
                );
            }
        }
    }

    /// A hit at or above `t_eff + epsilon` is confident — the lexical check
    /// never runs (a mismatching signature must not matter).
    #[test]
    fn l2_verify_decision_above_band_is_confident() {
        let v = verify_cfg(0.02, 0.75);
        let mismatching_sig = Some(tt_cache::lexical_sig("a completely different topic"));
        assert_eq!(
            l2_verify_decision(Some(&v), 0.94, 0.92, mismatching_sig, "[user] hello there"),
            L2VerifyDecision::Confident,
            "sim == t_eff + epsilon is confident"
        );
        assert_eq!(
            l2_verify_decision(Some(&v), 0.99, 0.92, mismatching_sig, "[user] hello there"),
            L2VerifyDecision::Confident
        );
    }

    /// In-band hits split on lexical agreement: a matching signature verifies,
    /// a topically-shifted one rejects.
    #[test]
    fn l2_verify_decision_in_band_splits_on_agreement() {
        let v = verify_cfg(0.02, 0.75);
        let query = "[user] how do i configure the retry policy for the payments api client";
        let same_sig = Some(tt_cache::lexical_sig(query));
        match l2_verify_decision(Some(&v), 0.93, 0.92, same_sig, query) {
            L2VerifyDecision::Verified(agreement) => {
                assert!(
                    agreement >= 0.75,
                    "identical text agrees fully: {agreement}"
                );
            }
            other => panic!("in-band + agreeing sig must be Verified, got {other:?}"),
        }
        let shifted_sig = Some(tt_cache::lexical_sig(
            "[user] please summarize the quarterly marketing report for the board",
        ));
        match l2_verify_decision(Some(&v), 0.93, 0.92, shifted_sig, query) {
            L2VerifyDecision::Rejected(agreement) => {
                assert!(agreement < 0.75, "topic shift agrees low: {agreement}");
            }
            other => panic!("in-band + shifted sig must be Rejected, got {other:?}"),
        }
    }

    /// An in-band hit on a pre-0018 row (no signature) fails OPEN.
    #[test]
    fn l2_verify_decision_in_band_missing_sig_is_unverifiable_fail_open() {
        let v = verify_cfg(0.02, 0.75);
        assert_eq!(
            l2_verify_decision(Some(&v), 0.93, 0.92, None, "[user] hello there"),
            L2VerifyDecision::Unverifiable,
            "legacy rows must fail open (serve), never reject"
        );
    }

    /// Below-threshold similarities never reach the decision in production
    /// (the lookup filters them), but the band boundary itself is in-band:
    /// sim == t_eff with an agreeing sig verifies.
    #[test]
    fn l2_verify_decision_band_lower_bound_is_in_band() {
        let v = verify_cfg(0.02, 0.75);
        let query = "[user] hello there";
        let sig = Some(tt_cache::lexical_sig(query));
        assert!(matches!(
            l2_verify_decision(Some(&v), 0.92, 0.92, sig, query),
            L2VerifyDecision::Verified(_)
        ));
    }
}

#[cfg(test)]
mod output_shaping_tests {
    use super::*;
    use futures::stream::BoxStream;
    use tt_shared::messages::ResponseFormat;
    use tt_shared::{ChatCompletionChunk, ProviderError};

    /// Minimal provider stub with controllable structured-output support and
    /// dropped-params — everything `maybe_minify_json` consults.
    struct ShapeProvider {
        schema: bool,
        drops: &'static [&'static str],
    }

    #[async_trait::async_trait]
    impl tt_shared::Provider for ShapeProvider {
        fn id(&self) -> &'static str {
            "shape-test"
        }
        fn models(&self) -> Vec<tt_shared::ModelInfo> {
            vec![]
        }
        fn pricing(&self, _m: &str) -> Option<ModelPricing> {
            None
        }
        fn dropped_params(&self, _req: &ChatCompletionRequest) -> Vec<String> {
            self.drops.iter().map(|s| s.to_string()).collect()
        }
        fn supports_response_schema(&self) -> bool {
            self.schema
        }
        async fn chat_completion(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<ChatCompletionResponse, ProviderError> {
            unreachable!("shapers never dispatch")
        }
        async fn chat_completion_stream(
            &self,
            _req: ChatCompletionRequest,
            _ctx: &RequestContext,
        ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>
        {
            unreachable!("shapers never dispatch")
        }
    }

    fn req_with_messages(messages: Vec<Message>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages,
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    fn sys(text: &str) -> Message {
        Message::System {
            content: MessageContent::Text(text.into()),
        }
    }

    fn user(text: &str) -> Message {
        Message::User {
            content: MessageContent::Text(text.into()),
            name: None,
        }
    }

    /// A flat non-strict schema that would normally make the CSV switch
    /// eligible. Composition tests use it to prove an owner guard, rather than
    /// a schema gate, left the request untouched.
    fn csv_switch_request() -> ChatCompletionRequest {
        let mut req = req_with_messages(vec![sys("Return the inventory."), user("list items")]);
        req.response_format = Some(ResponseFormat {
            r#type: "json_schema".into(),
            json_schema: Some(serde_json::json!({
                "name": "items",
                "strict": false,
                "schema": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "qty": {"type": "integer"}
                        },
                        "required": ["name", "qty"]
                    }
                }
            })),
        });
        req
    }

    /// A request that is otherwise eligible for the diff patch contract.
    fn diff_ready_request() -> ChatCompletionRequest {
        let mut req = req_with_messages(vec![sys("Edit precisely."), user("revise this")]);
        req.tt_extras.insert(
            crate::shaping::diff::TT_EXTRA_DIFF_PRIOR.to_string(),
            serde_json::json!("x".repeat(crate::shaping::diff::MIN_PRIOR_CHARS + 1)),
        );
        req
    }

    fn workflow_config(mode: Option<&str>) -> tt_routing::RouteWorkflow {
        tt_routing::RouteWorkflow {
            workflow_id: "00000000-0000-0000-0000-000000000001".into(),
            max_cost_usd: None,
            environment: None,
            mode: mode.map(str::to_string),
        }
    }

    fn assistant_text_response(texts: &[&str]) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "r".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "gpt-4o".into(),
            choices: texts
                .iter()
                .enumerate()
                .map(|(i, t)| Choice {
                    index: i as u32,
                    message: Message::Assistant {
                        content: Some(MessageContent::Text((*t).into())),
                        tool_calls: vec![],
                        name: None,
                    },
                    finish_reason: Some("stop".into()),
                })
                .collect(),
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
                cached_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        }
    }

    /// Route on: the deterministic instruction is appended to the LAST system
    /// message (byte-exact), the `output_minified` token is pushed, and the
    /// shaper reports it acted. Route off: the request is byte-identical and
    /// no warning is pushed.
    #[test]
    fn maybe_minify_json_injects_suffix_and_warns() {
        let p = ShapeProvider {
            schema: true,
            drops: &[],
        };
        let mut req = req_with_messages(vec![sys("First."), sys("You are a bot."), user("hi")]);
        let mut warnings = Vec::new();
        let applied = maybe_minify_json(&mut req, true, &p, &mut warnings);
        assert!(applied, "route opt-in must inject");
        assert_eq!(warnings, vec!["output_minified".to_string()]);
        match &req.messages[1] {
            Message::System {
                content: MessageContent::Text(t),
            } => {
                assert_eq!(t, &format!("You are a bot.{MINIFY_JSON_INSTRUCTION}"));
            }
            other => panic!("expected last system message text, got {other:?}"),
        }
        // The FIRST system message is untouched — only the last one grows.
        match &req.messages[0] {
            Message::System {
                content: MessageContent::Text(t),
            } => assert_eq!(t, "First."),
            other => panic!("unexpected {other:?}"),
        }

        // Route off → byte-identical request, no warning, returns false.
        let mut req2 = req_with_messages(vec![sys("You are a bot."), user("hi")]);
        let before = serde_json::to_string(&req2).unwrap();
        let mut w2 = Vec::new();
        assert!(!maybe_minify_json(&mut req2, false, &p, &mut w2));
        assert_eq!(serde_json::to_string(&req2).unwrap(), before);
        assert!(w2.is_empty());
    }

    /// No system message in the request → one is INSERTED at index 0 carrying
    /// the (lead-trimmed) instruction.
    #[test]
    fn maybe_minify_json_creates_system_message_when_absent() {
        let p = ShapeProvider {
            schema: true,
            drops: &[],
        };
        let mut req = req_with_messages(vec![user("hi")]);
        let mut warnings = Vec::new();
        assert!(maybe_minify_json(&mut req, true, &p, &mut warnings));
        assert_eq!(req.messages.len(), 2);
        match &req.messages[0] {
            Message::System {
                content: MessageContent::Text(t),
            } => assert_eq!(t, MINIFY_JSON_INSTRUCTION.trim_start()),
            other => panic!("expected inserted system message, got {other:?}"),
        }
        assert_eq!(warnings, vec!["output_minified".to_string()]);
    }

    /// A `Parts` system message gains a trailing text part (never corrupting
    /// existing parts).
    #[test]
    fn maybe_minify_json_appends_part_to_parts_system_message() {
        let p = ShapeProvider {
            schema: true,
            drops: &[],
        };
        let mut req = req_with_messages(vec![
            Message::System {
                content: MessageContent::Parts(vec![tt_shared::messages::ContentPart::Text {
                    text: "sys".into(),
                }]),
            },
            user("hi"),
        ]);
        let mut warnings = Vec::new();
        assert!(maybe_minify_json(&mut req, true, &p, &mut warnings));
        match &req.messages[0] {
            Message::System {
                content: MessageContent::Parts(parts),
            } => {
                assert_eq!(parts.len(), 2);
                match &parts[1] {
                    tt_shared::messages::ContentPart::Text { text } => {
                        assert_eq!(text, MINIFY_JSON_INSTRUCTION);
                    }
                    other => panic!("expected appended text part, got {other:?}"),
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// Grammar-locked structured output (json_schema honored natively by the
    /// served provider) is a no-op + `minify_skipped:structured_output`; a
    /// schema DOWNGRADED to json_object (B2) and a provider that drops
    /// response_format outright (Anthropic) both still inject.
    #[test]
    fn maybe_minify_json_skips_grammar_locked_schema() {
        // (a) json_schema + schema-supporting provider → grammar-locked: skip.
        let locked = ShapeProvider {
            schema: true,
            drops: &[],
        };
        let mut req = req_with_messages(vec![sys("s"), user("hi")]);
        req.response_format = Some(ResponseFormat {
            r#type: "json_schema".into(),
            json_schema: Some(serde_json::json!({"name":"x"})),
        });
        let before = serde_json::to_string(&req).unwrap();
        let mut w = Vec::new();
        assert!(!maybe_minify_json(&mut req, true, &locked, &mut w));
        assert_eq!(serde_json::to_string(&req).unwrap(), before, "no mutation");
        assert_eq!(w, vec!["minify_skipped:structured_output".to_string()]);

        // (b) post-B2 state: schema already downgraded to json_object on a
        // json_object-only provider → NOT grammar-locked: inject.
        let downgrading = ShapeProvider {
            schema: false,
            drops: &[],
        };
        let mut req = req_with_messages(vec![sys("s"), user("hi")]);
        req.response_format = Some(ResponseFormat {
            r#type: "json_object".into(),
            json_schema: None,
        });
        let mut w = Vec::new();
        assert!(maybe_minify_json(&mut req, true, &downgrading, &mut w));
        assert_eq!(w, vec!["output_minified".to_string()]);

        // (c) Anthropic-like provider that DROPS response_format outright →
        // nothing is grammar-locked upstream: inject.
        let dropping = ShapeProvider {
            schema: false,
            drops: &["response_format"],
        };
        let mut req = req_with_messages(vec![sys("s"), user("hi")]);
        req.response_format = Some(ResponseFormat {
            r#type: "json_schema".into(),
            json_schema: Some(serde_json::json!({"name":"x"})),
        });
        let mut w = Vec::new();
        assert!(maybe_minify_json(&mut req, true, &dropping, &mut w));
        assert_eq!(w, vec!["output_minified".to_string()]);
    }

    /// The estimate is grounded in the actual emission: minified JSON yields a
    /// positive pretty-minus-emitted delta; JSON the model emitted PRETTY
    /// anyway yields ~0 (never a fabricated -40%); prose / fence-wrapped /
    /// tool-call-only choices contribute 0; multiple choices sum.
    #[test]
    fn minify_saved_tokens_est_table() {
        let value = serde_json::json!({
            "name": "tokentrimmer",
            "items": [1, 2, 3, 4, 5],
            "nested": {"a": true, "b": null, "c": "text"}
        });
        let minified = serde_json::to_string(&value).unwrap();
        let pretty = serde_json::to_string_pretty(&value).unwrap();

        // Minified emission → positive delta.
        let resp = assistant_text_response(&[&minified]);
        let est = minify_saved_tokens_est("openai", "gpt-4o", &resp);
        assert!(est > 0, "minified JSON must yield a positive estimate");

        // Pretty emission of the SAME value → ~0 (the re-render IS the emission).
        let resp = assistant_text_response(&[&pretty]);
        let est_pretty = minify_saved_tokens_est("openai", "gpt-4o", &resp);
        assert_eq!(
            est_pretty, 0,
            "a model that ignored the instruction books ~0, not -40%"
        );

        // Prose → 0.
        let resp = assistant_text_response(&["The answer is 42, naturally."]);
        assert_eq!(minify_saved_tokens_est("openai", "gpt-4o", &resp), 0);

        // Fence-wrapped JSON → not valid JSON → 0 (no claim).
        let fenced = format!("```json\n{minified}\n```");
        let resp = assistant_text_response(&[&fenced]);
        assert_eq!(minify_saved_tokens_est("openai", "gpt-4o", &resp), 0);

        // Tool-call-only choice → 0.
        let mut resp = assistant_text_response(&[]);
        resp.choices.push(Choice {
            index: 0,
            message: Message::Assistant {
                content: None,
                tool_calls: vec![tt_shared::messages::ToolCall {
                    id: "t1".into(),
                    r#type: "function".into(),
                    function: tt_shared::messages::ToolCallFunction {
                        name: "f".into(),
                        arguments: "{}".into(),
                    },
                }],
                name: None,
            },
            finish_reason: Some("tool_calls".into()),
        });
        assert_eq!(minify_saved_tokens_est("openai", "gpt-4o", &resp), 0);

        // Multi-choice: two minified choices sum to twice one.
        let one =
            minify_saved_tokens_est("openai", "gpt-4o", &assistant_text_response(&[&minified]));
        let two = minify_saved_tokens_est(
            "openai",
            "gpt-4o",
            &assistant_text_response(&[&minified, &minified]),
        );
        assert_eq!(two, one * 2, "per-choice deltas must sum");
    }

    /// The estimate is priced at the BILLED output rate × fee into its own
    /// field; `tt_saved_usd()` / `cost_usd` / `baseline_cost_usd` are
    /// untouched (never folded into invoice-reconciled figures); the
    /// flex-applied path prices at the flex out-rate.
    #[test]
    fn compute_cost_full_minify_estimate_isolated() {
        let p = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: None,
            effective_at: Utc::now(),
        };
        let usage = Usage {
            prompt_tokens: 1000,
            completion_tokens: 1000,
            total_tokens: 2000,
            cached_tokens: 0,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        };

        let base = compute_cost_full(
            &usage,
            Some(&p),
            Some(&p),
            1.0,
            false,
            false,
            PassEffects::default(),
            0,
            crate::shaping::ShapeEffects::default(),
        );
        assert_eq!(base.minify_saved_est_usd, 0.0);
        // Document Lane D4a: the isolated vision-avoided estimate is ALWAYS 0.0
        // on the compute path (the seam that books it is D4c) and, like minify,
        // is never folded into cost/baseline.
        assert_eq!(base.doc_vision_saved_est_usd, 0.0);
        // Content-aware compression (P1a): 0.0 with default (empty) PassEffects —
        // the isolated estimate is booked only when content_compress removed
        // tokens, and (like minify/doc-vision) is never folded into cost/baseline.
        assert_eq!(base.content_compress_saved_est_usd, 0.0);

        let bd = compute_cost_full(
            &usage,
            Some(&p),
            Some(&p),
            1.0,
            false,
            false,
            PassEffects::default(),
            500,
            crate::shaping::ShapeEffects::default(),
        );
        // 500 tokens × $2/M = $0.001 at the standard output rate.
        assert!((bd.minify_saved_est_usd - 0.001).abs() < 1e-12);
        assert!(
            (bd.cost_usd - base.cost_usd).abs() < 1e-12,
            "estimate never folds into cost_usd"
        );
        assert!(
            (bd.baseline_cost_usd - base.baseline_cost_usd).abs() < 1e-12,
            "estimate never folds into baseline_cost_usd"
        );
        assert!(
            (bd.tt_saved_usd() - base.tt_saved_usd()).abs() < 1e-12,
            "estimate never enters the invoice-reconciled headline"
        );

        // Fee-applied like every other figure.
        let bd_fee = compute_cost_full(
            &usage,
            Some(&p),
            Some(&p),
            1.05,
            false,
            false,
            PassEffects::default(),
            500,
            crate::shaping::ShapeEffects::default(),
        );
        assert!((bd_fee.minify_saved_est_usd - 0.001 * 1.05).abs() < 1e-12);

        // Flex-applied path prices the estimate at the FLEX out-rate (the rate
        // the request was actually billed at).
        let flex_p = ModelPricing {
            flex_input_per_million: Some(0.5),
            flex_output_per_million: Some(1.0),
            ..p.clone()
        };
        let bd_flex = compute_cost_full(
            &usage,
            Some(&flex_p),
            Some(&flex_p),
            1.0,
            true,
            false,
            PassEffects::default(),
            500,
            crate::shaping::ShapeEffects::default(),
        );
        assert!((bd_flex.minify_saved_est_usd - 0.0005).abs() < 1e-12);
    }
    // --- Feature B: class-gated reasoning-token cap ---

    fn reasoning_model_info(caps: Vec<tt_shared::pricing::Capability>) -> tt_shared::ModelInfo {
        tt_shared::ModelInfo {
            id: "o3-mini".into(),
            provider: "shape-test".into(),
            capabilities: caps,
            max_input_tokens: 100_000,
            max_output_tokens: 100_000,
        }
    }

    /// Effort-arm matrix: lower-only semantics, the provider-default
    /// assumption on Reasoning-capable models, never-raise, unknown-value and
    /// non-reasoning-model refusals.
    #[test]
    fn maybe_cap_reasoning_effort_matrix() {
        use tt_shared::pricing::Capability;
        let p = ShapeProvider {
            schema: true,
            drops: &[],
        };
        let info = reasoning_model_info(vec![Capability::Text, Capability::Reasoning]);

        // high → low: capped + token.
        let mut req = req_with_messages(vec![user("hi")]);
        req.reasoning_effort = Some("high".into());
        let mut w = Vec::new();
        let applied =
            maybe_cap_reasoning(&mut req, Some("low"), None, &p, Some(&info), "r", &mut w);
        assert!(applied);
        assert_eq!(req.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(w, vec!["reasoning_capped:reasoning_effort:low".to_string()]);

        // Absent effort + Reasoning-capable model + cap low: provider default
        // ("medium") is assumed and lowered.
        let mut req = req_with_messages(vec![user("hi")]);
        let mut w = Vec::new();
        let applied =
            maybe_cap_reasoning(&mut req, Some("low"), None, &p, Some(&info), "r", &mut w);
        assert!(applied);
        assert_eq!(req.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(w, vec!["reasoning_capped:reasoning_effort:low".to_string()]);

        // Absent effort + cap medium: default medium is AT the cap → silent no-op.
        let mut req = req_with_messages(vec![user("hi")]);
        let mut w = Vec::new();
        let applied =
            maybe_cap_reasoning(&mut req, Some("medium"), None, &p, Some(&info), "r", &mut w);
        assert!(!applied);
        assert_eq!(req.reasoning_effort, None, "no injection at-or-below cap");
        assert!(w.is_empty(), "{w:?}");

        // low + cap medium: NEVER raise.
        let mut req = req_with_messages(vec![user("hi")]);
        req.reasoning_effort = Some("low".into());
        let mut w = Vec::new();
        let applied =
            maybe_cap_reasoning(&mut req, Some("medium"), None, &p, Some(&info), "r", &mut w);
        assert!(!applied);
        assert_eq!(req.reasoning_effort.as_deref(), Some("low"));
        assert!(w.is_empty(), "{w:?}");

        // Unknown requester effort value: refuse with a token, never rewrite.
        let mut req = req_with_messages(vec![user("hi")]);
        req.reasoning_effort = Some("turbo".into());
        let mut w = Vec::new();
        let applied =
            maybe_cap_reasoning(&mut req, Some("low"), None, &p, Some(&info), "r", &mut w);
        assert!(!applied);
        assert_eq!(req.reasoning_effort.as_deref(), Some("turbo"));
        assert_eq!(
            w,
            vec!["reasoning_cap_skipped:unknown_effort:turbo".to_string()]
        );

        // Non-Reasoning model + absent effort: never inject reasoning_effort
        // into a model that may reject it.
        let text_info = reasoning_model_info(vec![Capability::Text]);
        let mut req = req_with_messages(vec![user("hi")]);
        let mut w = Vec::new();
        let applied = maybe_cap_reasoning(
            &mut req,
            Some("low"),
            None,
            &p,
            Some(&text_info),
            "r",
            &mut w,
        );
        assert!(!applied);
        assert_eq!(req.reasoning_effort, None);
        assert_eq!(
            w,
            vec!["reasoning_cap_skipped:not_reasoning:gpt-4o".to_string()]
        );

        // Unknown model (no catalog info) + absent effort: same refusal.
        let mut req = req_with_messages(vec![user("hi")]);
        let mut w = Vec::new();
        let applied = maybe_cap_reasoning(&mut req, Some("low"), None, &p, None, "r", &mut w);
        assert!(!applied);
        assert_eq!(
            w,
            vec!["reasoning_cap_skipped:not_reasoning:gpt-4o".to_string()]
        );
    }

    /// The HARD class gate wins first: a code-classified request with BOTH
    /// caps configured is untouched and carries ONLY the class token.
    #[test]
    fn maybe_cap_reasoning_class_gate_wins() {
        use tt_shared::pricing::Capability;
        let p = ShapeProvider {
            schema: true,
            drops: &[],
        };
        let info = reasoning_model_info(vec![Capability::Reasoning]);
        let mut req = req_with_messages(vec![user("Refactor this module and debug the crash.")]);
        req.reasoning_effort = Some("high".into());
        req.extra.insert(
            "thinking".into(),
            serde_json::json!({"type":"enabled","budget_tokens":30000}),
        );
        let before = serde_json::to_string(&req).unwrap();
        let mut w = Vec::new();
        let applied = maybe_cap_reasoning(
            &mut req,
            Some("low"),
            Some(8192),
            &p,
            Some(&info),
            "r",
            &mut w,
        );
        assert!(!applied);
        assert_eq!(serde_json::to_string(&req).unwrap(), before, "untouched");
        assert_eq!(w, vec!["reasoning_cap_skipped:class:code".to_string()]);
    }

    /// Thinking-budget arm: an enabled config above the cap is lowered in
    /// place; below-cap / disabled configs are untouched; thinking is NEVER
    /// enabled; `max_tokens` / `max_completion_tokens` are NEVER mutated.
    #[test]
    fn maybe_cap_reasoning_thinking_budget() {
        let p = ShapeProvider {
            schema: true,
            // Anthropic-like surface: no reasoning_effort lever.
            drops: &["reasoning_effort"],
        };

        // Above-cap budget → rewritten to the cap.
        let mut req = req_with_messages(vec![user("hi")]);
        req.max_tokens = Some(50_000);
        req.max_completion_tokens = Some(40_000);
        req.extra.insert(
            "thinking".into(),
            serde_json::json!({"type":"enabled","budget_tokens":30000}),
        );
        let mut w = Vec::new();
        let applied = maybe_cap_reasoning(&mut req, None, Some(8192), &p, None, "r", &mut w);
        assert!(applied);
        assert_eq!(
            req.extra["thinking"],
            serde_json::json!({"type":"enabled","budget_tokens":8192})
        );
        assert_eq!(w, vec!["reasoning_capped:thinking_budget:8192".to_string()]);
        assert_eq!(req.max_tokens, Some(50_000), "max_tokens NEVER mutated");
        assert_eq!(req.max_completion_tokens, Some(40_000));

        // Below-cap budget → untouched, silent.
        let mut req = req_with_messages(vec![user("hi")]);
        req.max_tokens = Some(50_000);
        req.extra.insert(
            "thinking".into(),
            serde_json::json!({"type":"enabled","budget_tokens":2048}),
        );
        let mut w = Vec::new();
        let applied = maybe_cap_reasoning(&mut req, None, Some(8192), &p, None, "r", &mut w);
        assert!(!applied);
        assert_eq!(
            req.extra["thinking"],
            serde_json::json!({"type":"enabled","budget_tokens":2048})
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(req.max_tokens, Some(50_000));

        // Disabled thinking → untouched (NEVER enable thinking).
        let mut req = req_with_messages(vec![user("hi")]);
        req.extra
            .insert("thinking".into(), serde_json::json!({"type":"disabled"}));
        let mut w = Vec::new();
        let applied = maybe_cap_reasoning(&mut req, None, Some(8192), &p, None, "r", &mut w);
        assert!(!applied);
        assert_eq!(
            req.extra["thinking"],
            serde_json::json!({"type":"disabled"})
        );
        assert_eq!(
            w,
            vec!["reasoning_cap_skipped:unsupported:shape-test".to_string()],
            "a disabled config is no lever — the surface is honestly unsupported"
        );
        assert_eq!(req.max_tokens, None, "max_tokens NEVER mutated");
    }

    /// Neither lever exists (provider drops reasoning_effort AND no thinking
    /// config) while a cap is configured → one honest unsupported token.
    #[test]
    fn maybe_cap_reasoning_unsupported_surface() {
        let p = ShapeProvider {
            schema: true,
            drops: &["reasoning_effort"],
        };
        let mut req = req_with_messages(vec![user("hi")]);
        let before = serde_json::to_string(&req).unwrap();
        let mut w = Vec::new();
        let applied = maybe_cap_reasoning(&mut req, Some("low"), Some(8192), &p, None, "r", &mut w);
        assert!(!applied);
        assert_eq!(serde_json::to_string(&req).unwrap(), before, "untouched");
        assert_eq!(
            w,
            vec!["reasoning_cap_skipped:unsupported:shape-test".to_string()]
        );

        // Both caps None → fully silent.
        let mut req = req_with_messages(vec![user("hi")]);
        let mut w = Vec::new();
        assert!(!maybe_cap_reasoning(
            &mut req, None, None, &p, None, "r", &mut w
        ));
        assert!(w.is_empty());
    }

    /// A workflow detour replaces the direct completion before its
    /// response-side format-switch validator, token, and estimate machinery
    /// can run. The guard must therefore leave the caller schema and prompt
    /// byte-identical and yield no plan that could become an output claim.
    #[test]
    fn workflow_detour_skips_format_switch_without_mutation_or_output_plan() {
        let mut req = csv_switch_request();
        let before = serde_json::to_string(&req).unwrap();

        let outcome = prepare_route_format_switch(
            &mut req,
            Some("csv"),
            Some(FormatSwitchResponseOwner::WorkflowDetour),
            false,
        );

        assert!(matches!(
            outcome,
            RouteFormatSwitchPreparation::Skipped("workflow")
        ));
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "workflow detour must retain response_format and add no switch instruction"
        );
    }

    /// Shadow workflows keep the direct response path, so they retain the
    /// established opt-in format-switch behavior.
    #[test]
    fn shadow_workflow_keeps_direct_format_switch_behavior() {
        let mut req = csv_switch_request();

        let outcome = prepare_route_format_switch(&mut req, Some("csv"), None, false);

        assert!(matches!(
            outcome,
            RouteFormatSwitchPreparation::Applied(ref plan) if plan.label == "csv"
        ));
        assert!(
            req.response_format.is_none(),
            "direct switch drops the schema"
        );
        assert!(
            serde_json::to_string(&req)
                .unwrap()
                .contains("Respond ONLY with CSV data"),
            "direct switch appends its deterministic instruction"
        );
    }

    /// Streaming remains planner-owned: an otherwise real panel must preserve
    /// the existing `streaming` reason rather than changing observability to a
    /// response-owner reason.
    #[test]
    fn panel_streaming_keeps_existing_format_switch_streaming_reason() {
        let mut req = csv_switch_request();
        req.stream = true;
        let before = serde_json::to_string(&req).unwrap();

        let outcome = prepare_route_format_switch(
            &mut req,
            Some("csv"),
            Some(FormatSwitchResponseOwner::FusionPanel),
            false,
        );

        assert!(matches!(
            outcome,
            RouteFormatSwitchPreparation::Skipped("streaming")
        ));
        assert_eq!(serde_json::to_string(&req).unwrap(), before);
    }

    #[test]
    fn response_owner_gate_keeps_only_real_direct_paths_eligible() {
        let detour = workflow_config(None);
        let shadow = workflow_config(Some("shadow"));

        assert_eq!(
            non_direct_response_owner(true, None, false, false),
            Some("panel"),
            "a non-streaming Fusion panel owns its response"
        );
        assert_eq!(
            non_direct_response_owner(true, None, true, false),
            Some("panel"),
            "a streaming Fusion panel still owns its response"
        );
        assert_eq!(
            non_direct_response_owner(false, Some(&detour), false, false),
            Some("workflow"),
            "a non-shadow workflow replaces the direct non-streaming response"
        );
        assert_eq!(
            non_direct_response_owner(false, Some(&detour), true, false),
            None,
            "streaming workflow detours fall through to direct streaming"
        );
        assert_eq!(
            non_direct_response_owner(false, Some(&shadow), false, false),
            None,
            "shadow workflow results are not caller-visible"
        );
        assert_eq!(
            non_direct_response_owner(true, None, false, true),
            None,
            "skip_shadow deliberately takes the direct path"
        );
    }

    #[test]
    fn response_owners_skip_minify_without_mutating_internal_prompts() {
        let provider = ShapeProvider {
            schema: true,
            drops: &[],
        };

        for owner in ["panel", "workflow"] {
            let mut req = req_with_messages(vec![sys("Keep the original prompt."), user("hi")]);
            let before = serde_json::to_string(&req).unwrap();
            let mut warnings = Vec::new();
            let applied =
                prepare_route_minify_json(&mut req, true, &provider, Some(owner), &mut warnings);

            assert!(
                !applied,
                "{owner} must not inherit direct-response minify steering"
            );
            assert_eq!(
                serde_json::to_string(&req).unwrap(),
                before,
                "{owner} request must remain byte-identical"
            );
            assert_eq!(warnings, vec![format!("minify_skipped:{owner}")]);
        }

        let mut direct = req_with_messages(vec![sys("Direct response."), user("hi")]);
        let mut warnings = Vec::new();
        assert!(prepare_route_minify_json(
            &mut direct,
            true,
            &provider,
            None,
            &mut warnings,
        ));
        assert_eq!(warnings, vec!["output_minified".to_string()]);
        assert!(direct.messages.iter().any(|message| {
            matches!(
                message,
                Message::System {
                    content: MessageContent::Text(text),
                } if text.contains(MINIFY_JSON_INSTRUCTION)
            )
        }));
    }

    #[test]
    fn response_owners_skip_diff_without_dropping_the_caller_contract() {
        for owner in ["panel", "workflow"] {
            let mut req = diff_ready_request();
            let before = serde_json::to_string(&req).unwrap();

            let outcome = prepare_route_diff(&mut req, true, Some(owner));

            assert!(matches!(outcome, RouteDiffPreparation::Skipped(reason) if reason == owner));
            assert_eq!(
                serde_json::to_string(&req).unwrap(),
                before,
                "{owner} must retain diff_prior and avoid the patch instruction"
            );
        }

        let mut direct = diff_ready_request();
        let outcome = prepare_route_diff(&mut direct, true, None);
        assert!(matches!(outcome, RouteDiffPreparation::Applied(_)));
        assert!(
            !direct
                .tt_extras
                .contains_key(crate::shaping::diff::TT_EXTRA_DIFF_PRIOR),
            "the direct patch dispatch consumes the explicit prior"
        );
        assert!(matches!(
            direct.messages.last(),
            Some(Message::System {
                content: MessageContent::Text(text),
            }) if text == crate::shaping::diff::DIFF_INSTRUCTION
        ));

        let mut streaming_panel = diff_ready_request();
        streaming_panel.stream = true;
        let before = serde_json::to_string(&streaming_panel).unwrap();
        let outcome = prepare_route_diff(&mut streaming_panel, true, Some("panel"));
        assert!(matches!(
            outcome,
            RouteDiffPreparation::Skipped("streaming")
        ));
        assert_eq!(serde_json::to_string(&streaming_panel).unwrap(), before);
    }
}

// REL-3: detached telemetry writes are drained via a TaskTracker so a
// graceful shutdown (SIGTERM / rolling deploy) cannot abandon billing rows.
#[cfg(test)]
mod telemetry_drain_tests {
    use super::*;
    use chrono::Utc;
    use std::sync::Arc;
    use tokio_util::task::TaskTracker;
    use tt_telemetry::request_logs::{InMemoryRequestLogWriter, RequestLogRow};
    use uuid::Uuid;

    fn sample_row() -> RequestLogRow {
        RequestLogRow {
            id: Uuid::now_v7(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            ts: Utc::now(),
            provider: "openai".into(),
            requested_model: None,
            model: "gpt-4o".into(),
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
            cost_usd: 0.001,
            baseline_cost_usd: 0.001,
            provider_cache_saved_usd: 0.0,
            cache_bust_penalty_usd: 0.0,
            flex_saved_usd: 0.0,
            doc_compaction_saved_usd: 0.0,
            summarizer_tax_usd: 0.0,
            request_delta_evidence_state: RequestDeltaEvidenceState::Measured,
            cached: false,
            cache_layer: None,
            route_id: None,
            route_version_id: None,
            latency_ms: 50,
            upstream_latency_ms: None,
            status: 200,
            tag: None,
            error_class: None,
            trace_id: Some("test-drain".into()),
            truncated: false,
            shadow_model: None,
            shadow_cost_usd: None,
            traffic_split_arm: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            batch_eligible: false,
            batch_forgone_usd: 0.0,
            route_paused: false,
            minify_saved_est_usd: 0.0,
            format_switched: None,
            format_switch_saved_est_usd: 0.0,
            diff_applied: false,
            diff_saved_usd: 0.0,
            diff_failed: false,
            diff_failed_cost_usd: 0.0,
            retrieval_tokens_saved: 0,
            doc_compaction_tokens_removed: 0,
            compression_saved_usd: 0.0,
            compression_tokens_removed: 0,
            doc_vision_saved_est_usd: 0.0,
            run_id: None,
            node_id: None,
            content_compress_saved_est_usd: 0.0,
            content_compress_kind: None,
            l2_matched_entry_id: None,
            l2_similarity: None,
            l2_verdict: None,
        }
    }

    /// REL-3: when a `TaskTracker` is passed to `spawn_request_log`, the spawned
    /// write is tracked. After `close()` + `wait()`, the write is guaranteed to
    /// have completed — no sleep-polling needed.
    #[tokio::test]
    async fn spawn_request_log_with_tracker_drains_on_wait() {
        let writer = Arc::new(InMemoryRequestLogWriter::new());
        let tracker = TaskTracker::new();

        let writer_arc: Arc<dyn tt_telemetry::request_logs::RequestLogWriter> = writer.clone();
        spawn_request_log(Some(&tracker), Some(&writer_arc), sample_row());

        // Drain: close the tracker then wait for all tracked tasks to finish.
        tracker.close();
        tokio::time::timeout(std::time::Duration::from_secs(2), tracker.wait())
            .await
            .expect("telemetry drain must complete within 2 seconds");

        // NO sleep: the drain guarantees the write completed.
        assert_eq!(writer.rows().len(), 1, "write must be complete after drain");
    }

    /// REL-3: when no tracker is passed (`None`), `spawn_request_log` falls
    /// back to bare `tokio::spawn` — same behavior as before REL-3. Callers
    /// that don't have a tracker are unaffected.
    #[tokio::test]
    async fn spawn_request_log_without_tracker_falls_back_to_bare_spawn() {
        let writer = Arc::new(InMemoryRequestLogWriter::new());
        let writer_arc: Arc<dyn tt_telemetry::request_logs::RequestLogWriter> = writer.clone();

        spawn_request_log(None, Some(&writer_arc), sample_row());

        // No tracker: we have to yield control to let the bare spawn run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            writer.rows().len(),
            1,
            "bare spawn must still work when no tracker is passed"
        );
    }

    // ── P2: retry + drain interaction ──────────────────────────────────────────

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tt_telemetry::request_logs::{RequestLogError, RequestLogWriter};

    /// Test writer that takes a short async pause before recording each row
    /// (simulating a slow DB), and can fail TRANSIENTLY for the first N writes
    /// of a given row `id` before succeeding — to exercise the retry+drain path.
    struct SlowFlakyWriter {
        inner: Arc<InMemoryRequestLogWriter>,
        /// Number of LEADING transient failures to emit before the first success.
        transient_failures: AtomicUsize,
        delay: std::time::Duration,
    }

    impl SlowFlakyWriter {
        fn new(
            inner: Arc<InMemoryRequestLogWriter>,
            transient_failures: usize,
            delay: std::time::Duration,
        ) -> Self {
            Self {
                inner,
                transient_failures: AtomicUsize::new(transient_failures),
                delay,
            }
        }
    }

    #[async_trait::async_trait]
    impl RequestLogWriter for SlowFlakyWriter {
        async fn write(&self, row: RequestLogRow) -> Result<(), RequestLogError> {
            tokio::time::sleep(self.delay).await;
            // Burn down the leading-transient-failure budget first.
            loop {
                let remaining = self.transient_failures.load(Ordering::SeqCst);
                if remaining == 0 {
                    break;
                }
                if self
                    .transient_failures
                    .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    return Err(RequestLogError::Transient("flaky".into()));
                }
            }
            self.inner.write(row).await
        }
    }

    /// P2 drain: a SLOW tracked write is still in flight when shutdown begins;
    /// the drain (`close()` + `wait()`) must NOT resolve until that enqueued
    /// write has fully completed. Proven by asserting the row landed with no
    /// post-drain sleep.
    #[tokio::test]
    async fn drain_waits_for_in_flight_slow_write() {
        let inner = Arc::new(InMemoryRequestLogWriter::new());
        let slow: Arc<dyn RequestLogWriter> = Arc::new(SlowFlakyWriter::new(
            inner.clone(),
            0,
            std::time::Duration::from_millis(150),
        ));
        let tracker = TaskTracker::new();

        spawn_request_log(Some(&tracker), Some(&slow), sample_row());
        // Begin shutdown immediately — the 150ms write is still running.
        tracker.close();
        tokio::time::timeout(std::time::Duration::from_secs(2), tracker.wait())
            .await
            .expect("drain must complete within the bound");

        // NO post-drain sleep: if the drain didn't wait for the in-flight
        // write, this row would be missing.
        assert_eq!(
            inner.rows().len(),
            1,
            "drain must not resolve until the in-flight write finished"
        );
    }

    /// P2 retry + drain + idempotency: a write that fails transiently twice then
    /// succeeds, routed through `spawn_request_log`, lands EXACTLY ONE row after
    /// the drain — the retry happened, the drain waited for it, and the
    /// same-`id` retries did not double-insert.
    #[tokio::test]
    async fn tracked_write_retries_then_lands_single_row_on_drain() {
        let inner = Arc::new(InMemoryRequestLogWriter::new());
        // 2 leading transient failures < DEFAULT_WRITE_ATTEMPTS (3) → succeeds.
        let flaky: Arc<dyn RequestLogWriter> = Arc::new(SlowFlakyWriter::new(
            inner.clone(),
            2,
            std::time::Duration::from_millis(1),
        ));
        let tracker = TaskTracker::new();

        spawn_request_log(Some(&tracker), Some(&flaky), sample_row());
        tracker.close();
        tokio::time::timeout(std::time::Duration::from_secs(5), tracker.wait())
            .await
            .expect("drain must complete within the bound");

        assert_eq!(
            inner.rows().len(),
            1,
            "transient-then-success must persist exactly one row (no double-insert)"
        );
    }

    /// W0b Task 4: when `complete_once` copies `ctx.run_id` into the
    /// `RequestLogRow`, the writer spy must capture that run id. This test
    /// validates the writer-spy plumbing for the run_id field; it mirrors the
    /// pattern used in `complete_once` after the Task 4 change
    /// (`run_id: ctx.run_id` instead of `run_id: None`).
    ///
    /// Note: `complete_once` itself cannot be called without a real provider,
    /// so this test validates the mapping pattern (ctx.run_id → row.run_id →
    /// writer) using the existing InMemoryRequestLogWriter harness.
    #[tokio::test]
    async fn request_log_row_run_id_propagates_through_writer_spy() {
        let run_id = Uuid::new_v4();
        let writer = Arc::new(InMemoryRequestLogWriter::new());
        let tracker = TaskTracker::new();
        let writer_arc: Arc<dyn tt_telemetry::request_logs::RequestLogWriter> = writer.clone();

        // Simulate `complete_once`'s RequestLogRow construction after Task 4:
        //   run_id: ctx.run_id,   ← was `None` before the change
        //   node_id: ctx.node_id, ← was `None` and remains `None`
        let ctx_run_id: Option<Uuid> = Some(run_id);
        let ctx_node_id: Option<Uuid> = None;
        let mut row = sample_row();
        row.run_id = ctx_run_id;
        row.node_id = ctx_node_id;

        spawn_request_log(Some(&tracker), Some(&writer_arc), row);
        tracker.close();
        tokio::time::timeout(std::time::Duration::from_secs(2), tracker.wait())
            .await
            .expect("drain must complete");

        let rows = writer.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].run_id,
            Some(run_id),
            "run_id from ctx must be stamped on the request_log row"
        );
        assert_eq!(
            rows[0].node_id, None,
            "node_id remains None (no workflow nodes yet)"
        );
    }

    /// W0b Task 4 (review fix): `attribute_run` is the pure helper called by
    /// `complete_once` to copy `ctx.run_id`/`ctx.node_id` onto the log row.
    /// This test directly exercises the helper — if the body of `attribute_run`
    /// is ever reverted to a no-op (or the call in `complete_once` is removed),
    /// this test FAILS because the row fields stay `None`.
    ///
    /// Scenario A: both ids present → both copied.
    /// Scenario B: both ids absent → both remain None.
    #[test]
    fn attribute_run_copies_run_and_node_id() {
        use tt_shared::{
            context::{ProviderCredentials, SecretString},
            RequestContext,
        };

        fn make_ctx(run_id: Option<Uuid>, node_id: Option<Uuid>) -> RequestContext {
            RequestContext {
                budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
                trace_id: Uuid::nil(),
                org_id: Uuid::nil(),
                api_key_id: Uuid::nil(),
                credentials: ProviderCredentials {
                    api_key: SecretString::new("test"),
                    base_url: None,
                    extra_headers: vec![],
                },
                tag: None,
                deadline: None,
                run_id,
                node_id,
            }
        }

        // Scenario A: both fields present → attribute_run must copy them.
        let run = Uuid::new_v4();
        let node = Uuid::new_v4();
        let mut row = sample_row();
        attribute_run(&mut row, &make_ctx(Some(run), Some(node)));
        assert_eq!(
            row.run_id,
            Some(run),
            "attribute_run must copy ctx.run_id onto the row"
        );
        assert_eq!(
            row.node_id,
            Some(node),
            "attribute_run must copy ctx.node_id onto the row"
        );

        // Scenario B: both fields absent → row stays None.
        let mut row2 = sample_row();
        attribute_run(&mut row2, &make_ctx(None, None));
        assert_eq!(
            row2.run_id, None,
            "attribute_run must leave run_id None when ctx has None"
        );
        assert_eq!(
            row2.node_id, None,
            "attribute_run must leave node_id None when ctx has None"
        );
    }
}

// COST-3: the L2 miss path must not embed the query text a second time. These
// tests exercise `insert_into_l2` directly with a call-counting embedder.
#[cfg(test)]
mod l2_insert_embed_reuse_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tt_cache::l2::L2Cache;
    use tt_cache::{embed::EmbedError, EmbeddingProvider, InMemoryL2Cache};

    /// Embedder that counts `embed` calls and returns a fixed, distinctive
    /// vector (orthogonal to the precomputed one used below) so a lookup can
    /// tell which vector was actually stored.
    struct CountingEmbedder {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for CountingEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![0.0, 1.0])
        }
        fn model(&self) -> &str {
            "mock-embed"
        }
    }

    fn l2_config(calls: Arc<AtomicUsize>, cache: Arc<InMemoryL2Cache>) -> L2Config {
        L2Config {
            cache,
            embedder: Arc::new(CountingEmbedder { calls }),
            threshold: 0.5,
            class_thresholds: tt_cache::ClassThresholds::default(),
            verify: None,
            volatility_ttl: None,
        }
    }

    fn resp() -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "r".into(),
            object: "chat.completion".into(),
            created: 0,
            model: "gpt-4o".into(),
            choices: vec![],
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 5,
                total_tokens: 10,
                cached_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        }
    }

    #[tokio::test]
    async fn reuses_precomputed_embedding_without_calling_embedder() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = Arc::new(InMemoryL2Cache::new());
        let l2 = l2_config(calls.clone(), cache.clone());
        let org = Uuid::now_v7();
        // Orthogonal to the embedder's [0, 1] so the two are distinguishable.
        let precomputed = vec![1.0_f32, 0.0];

        insert_into_l2(
            l2,
            org,
            "the query text",
            resp(),
            "openai".to_string(),
            "gpt-4o".to_string(),
            3600,
            Some(0.001),
            RequestDeltaEvidenceState::Measured,
            Some(precomputed.clone()),
        )
        .await;

        // The embedder must NOT be called when an embedding is supplied.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "embedder should not be called on the reuse path"
        );

        // The precomputed embedding is what got stored: a lookup by it hits,
        // a lookup by the embedder's orthogonal vector misses.
        let hit = cache
            .lookup(org, &precomputed, 0.5, "gpt-4o", "mock-embed")
            .await
            .unwrap();
        assert!(hit.is_some(), "precomputed embedding should be recallable");
        let miss = cache
            .lookup(org, &[0.0, 1.0], 0.5, "gpt-4o", "mock-embed")
            .await
            .unwrap();
        assert!(miss.is_none(), "embedder's vector was not the one stored");
    }

    #[tokio::test]
    async fn embeds_once_when_no_precomputed_embedding() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cache = Arc::new(InMemoryL2Cache::new());
        let l2 = l2_config(calls.clone(), cache.clone());
        let org = Uuid::now_v7();

        insert_into_l2(
            l2,
            org,
            "the query text",
            resp(),
            "openai".to_string(),
            "gpt-4o".to_string(),
            3600,
            Some(0.001),
            RequestDeltaEvidenceState::Measured,
            None,
        )
        .await;

        // Fallback path embeds exactly once.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "embedder should be called once on the fallback path"
        );

        // The embedder's vector [0, 1] was stored.
        let hit = cache
            .lookup(org, &[0.0, 1.0], 0.5, "gpt-4o", "mock-embed")
            .await
            .unwrap();
        assert!(hit.is_some(), "fallback embedding should be stored");
    }
}
