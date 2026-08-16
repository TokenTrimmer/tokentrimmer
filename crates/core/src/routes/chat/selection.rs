//! Route candidate selection and auditable application outcomes.

use super::*;
/// Outcome of evaluating the routing engine against a request: the matched
/// route's id (for `request_logs.route_id` attribution) plus its ordered
/// fallback model ids (for failover dispatch). Empty `fallbacks` = the route
/// declared no failover targets.
pub(crate) struct RouteMatch {
    pub(crate) route_id: Uuid,
    /// Immutable `public.route_versions.id` captured in the same cached runtime
    /// route refresh as this definition. Never a mutable route revision.
    pub(crate) route_version_id: Option<i64>,
    pub(crate) route_name: String,
    /// The matched route is sticky-PAUSED (quality regression / manual pause):
    /// the rewrite was suppressed and every cost lever below is disabled —
    /// `target_model` echoes the originally-requested model, `fallbacks` is
    /// empty, flex/compress/traffic_pct/shadow/max_cost are all off. SAFETY
    /// levers (`redact`, `disable_cache`) stay live: pausing a quality gate
    /// must never disable a privacy guardrail. The route still attributes
    /// (`route_id` stamped, `route_paused` marker on the request_logs row).
    pub(crate) paused: bool,
    pub(crate) fallbacks: Vec<String>,
    pub(crate) disable_cache: bool,
    pub(crate) max_cost_usd: Option<f64>,
    pub(crate) input_tokens_estimate: u32,
    /// The matched route requested OpenAI Flex (`service_tier="flex"`). The
    /// actual opt-in is gated on the served model's flex-eligibility at the
    /// request-build step (an ineligible model is left untouched + warned).
    pub(crate) flex: bool,
    /// The matched route requested the advisory **batch-eligibility** marker
    /// (`RouteAction::batch`). Gated at the request-build step on streaming /
    /// interactive (`X-TokenTrimmer-Interactive`) / the served model's catalog
    /// batch rate — see `maybe_mark_batch_eligible`. Advisory only: the
    /// synchronous gateway never detours dispatch for it.
    pub(crate) batch: bool,
    /// The matched route opted into the conservative compression pass
    /// (`RouteAction::compress`). When true the gateway runs the request-pass
    /// pipeline before dispatch; off by default (no pass runs otherwise).
    pub(crate) compress: bool,
    /// The matched route opted into the lossless document-compaction pass
    /// (`RouteAction::doc_compaction`, Document Lane D2). When true the gateway
    /// runs the doc-compaction request-pass pipeline before dispatch; off by
    /// default (no pass runs otherwise). A COST lever: suppressed on a paused
    /// route.
    pub(crate) doc_compaction: bool,
    /// The matched route opted into the content-aware compression pass
    /// (`RouteAction::content_compress`, P1a). When true the gateway runs the
    /// content_compress request-pass pipeline before dispatch; off by default
    /// (no pass runs otherwise). A COST lever: suppressed on a paused route.
    pub(crate) content_compress: bool,
    /// The matched route opted into the Document Lane post-match distillation
    /// seam (`RouteAction::document_lane`, Document Lane D4c). When true the
    /// gateway converts every eligible media part before target-provider setup;
    /// an incomplete transaction restores the caller model and raw request, and
    /// a complete transaction may keep the text-model downgrade + book the
    /// isolated `doc_vision_saved_est_usd`. A COST lever: suppressed on a paused
    /// route. Off by default (no distillation runs otherwise).
    pub(crate) document_lane: bool,
    /// The matched route opted into the request-redaction guardrail
    /// (`RouteAction::redact`). When true the gateway redacts PII/secrets from
    /// the outbound request before dispatch (a SAFETY transform, not a saving);
    /// off by default (no redaction runs otherwise).
    pub(crate) redact: bool,
    /// The matched route requested the opt-in **format switch**
    /// (`RouteAction::format_switch`, research Phase 3.3): `Some("csv")` /
    /// `Some("bare")`. Eligibility (schema shape, streaming, tools, n>1,
    /// strict structured output) is enforced at the request-build step — see
    /// `shaping::format_switch::plan_format_switch`. A COST lever: suppressed
    /// on a paused route.
    pub(crate) format_switch: Option<String>,
    /// The matched route requested opt-in **delta/diff responses**
    /// (`RouteAction::diff`, research Phase 3.4). The prior seam + gates are
    /// enforced at the request-build step — see `shaping::diff::plan_diff`.
    /// A COST lever: suppressed on a paused route.
    pub(crate) diff: bool,
    /// The matched route's canary `traffic_pct` (0-100), or `None` for an
    /// unconditional rewrite. When `Some(pct)`, the handler evaluates the sticky
    /// split (`tt_routing::sticky_traffic_split`) AFTER the rewrite to decide
    /// whether this request stays on the canary (`target_model`) arm or is
    /// reverted to its originally-requested model (the control arm).
    pub(crate) traffic_pct: Option<u32>,
    /// The matched route's shadow-mode candidate (`RouteAction::shadow_model`),
    /// or `None`. When `Some(model)`, the handler ALSO dispatches `model` as a
    /// discarded shadow (non-streaming, single candidate, no failover) and
    /// records its cost separately. `target_model` (the rewrite) is captured
    /// alongside so the handler can revert a control-arm request without
    /// re-reading the route.
    pub(crate) shadow_model: Option<String>,
    /// The route's `target_model` (the canary arm's model) — captured so the
    /// handler can revert `req.model` to the originally-requested model when the
    /// sticky split assigns this request to the CONTROL arm.
    pub(crate) target_model: String,
    /// The matched route opted into minified-JSON output steering
    /// (`RouteAction::minify_json`, research Phase 3.1). Applied at the
    /// request-build step via `maybe_minify_json` (grammar-locked structured
    /// output skips with a warning). A COST lever: forced off on a paused
    /// route.
    pub(crate) minify_json: bool,
    /// The matched route's `reasoning_effort` cap
    /// (`RouteAction::reasoning_max_effort`, research Phase 3.2). Applied at
    /// the request-build step via `maybe_cap_reasoning` (class-gated HARD;
    /// lower-only). A COST lever: forced to `None` on a paused route.
    pub(crate) reasoning_max_effort: Option<String>,
    /// The matched route's thinking-budget cap
    /// (`RouteAction::reasoning_budget_tokens`). Same gating as
    /// `reasoning_max_effort`; never expressed via `max_tokens`.
    pub(crate) reasoning_budget_tokens: Option<u32>,
    /// The matched route's opt-in **agentic context budget**
    /// (`RouteAction::agentic_budget`, plan `2026-06-13-agentic-cost-context-budget`).
    /// `Some(_)` opts the matched loop traffic into the route-grained planner
    /// (`tt_core::passes::agentic_budget::AgenticBudgetPlanner`) that nets the
    /// cache-prefix / field-drop / summarize / route levers per request. A COST
    /// lever: forced to `None` on a paused route. `None` by default — the
    /// planner is never constructed on the un-opted path, so that path is
    /// byte-identical (off by default, load-bearing).
    pub(crate) agentic_budget: Option<tt_routing::AgenticBudget>,
    /// The matched route's opt-in **Fusion panel** trigger
    /// (`RouteAction::panel`). `Some(_)` makes a matched request fan out across
    /// the panel members + arbiter — but only when the caller did NOT send an
    /// explicit `X-TokenTrimmer-Panel` header (the header wins; the route is the
    /// fallback trigger, resolved in `prepare`). A COST lever: forced to `None`
    /// on a paused route (no panel on a paused route), and `None` by default so
    /// the un-opted single-model path is byte-identical.
    pub(crate) panel: Option<tt_routing::RoutePanel>,
    /// The matched route's opt-in **workflow detour** trigger
    /// (`RouteAction.workflow`, CO-1). `Some(_)` makes a matched request run
    /// the referenced workflow instead of (detour) or alongside (shadow) the
    /// upstream call — resolved in `complete_once` BEFORE cache (workflows are
    /// non-deterministic, same reason panels bypass cache). A COST lever:
    /// forced to `None` on a paused route (no workflow detour on a paused
    /// route), and `None` by default so the un-opted single-model path is
    /// byte-identical (off by default, load-bearing — same invariant as
    /// `panel`). Streaming workflow detour is a follow-up; a `stream:true`
    /// request on a workflow route falls through to the single-model path with
    /// a warning (the workflow detour is a non-streaming aggregate, like
    /// `panel`'s Phase-1 non-streaming arm).
    pub(crate) workflow: Option<tt_routing::RouteWorkflow>,
}

/// Post-selection boundary reached by the live gateway routing seam.
///
/// This deliberately stops before canary assignment and the downstream action
/// pipeline. It is operational explanation, not an assertion that every
/// configured action executed.
#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum RouteApplicationOutcome {
    NoMatch,
    ForcedRouteNotFound,
    CapabilitySuppressed,
    Paused,
    AcceptedForActionPipeline,
}

#[derive(serde::Serialize)]
struct RouteApplicationTrace<'a> {
    application_outcome: RouteApplicationOutcome,
    decision: &'a tt_routing::RouteDecisionTrace,
}

/// Emit the value-free live trace only at debug level. This is intentionally
/// not request-log persistence, a customer API, or a complete action trace.
/// Keeping the organization identifier outside the serialized payload also
/// makes the payload itself safe to inspect for accidental request values.
fn record_route_application_trace(
    org_id: Uuid,
    outcome: RouteApplicationOutcome,
    decision: &tt_routing::RouteDecisionTrace,
) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    let trace = RouteApplicationTrace {
        application_outcome: outcome,
        decision,
    };
    match serde_json::to_string(&trace) {
        Ok(trace) => tracing::debug!(
            %org_id,
            route_application_trace = %trace,
            "route application trace"
        ),
        Err(error) => tracing::debug!(
            %org_id,
            %error,
            "route application trace serialization failed"
        ),
    }
}

/// A forced route that can't be honored is a `400`; absence of routing is fine
/// for an unforced request.
fn forced_miss(forced: Option<&str>) -> ApiResult<Option<RouteMatch>> {
    match forced {
        Some(name) => Err(ApiError::InvalidRequest(format!("unknown route: {name}"))),
        None => Ok(None),
    }
}

/// Look up the org's routing engine (cached ~60s) and evaluate it against
/// the incoming request. On a match, rewrites `req.model` in place and
/// returns the matched route (id + fallbacks) so callers can stamp the id on
/// the request_logs row and fail over across the fallback chain. A forced route
/// (`X-TokenTrimmer-Route`) bypasses condition evaluation; an unknown forced
/// route name is a `400`. Returns `None` (and does not modify `req`) when:
///
/// - no routing store is configured (dev / free tier),
/// - the request has no resolvable org (synthetic context),
/// - the backend errors (we log + fall through — never fail user traffic),
/// - or no enabled route matches.
pub(crate) async fn apply_routing(
    state: &AppState,
    ctx: &RequestContext,
    req: &mut ChatCompletionRequest,
    forced_route: Option<&str>,
) -> ApiResult<Option<RouteMatch>> {
    let Some(store) = state.routing_store.as_ref() else {
        return forced_miss(forced_route);
    };
    if ctx.org_id == Uuid::nil() {
        return forced_miss(forced_route);
    }

    let engine = match store.engine_for(ctx.org_id).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, org_id = %ctx.org_id, "routing store lookup failed — passing request through unrouted");
            return Ok(None); // never fail user traffic on a transient backend error
        }
    };

    // Input-tokens estimate for the route conditions. Counts the ENTIRE prompt
    // (system + every turn) via the shared `message_text_for_estimation` helper —
    // the SAME text live dispatch and the capability guard below tokenize, so a
    // route decision mirrors what actually gets dispatched (and billed). (`/v1/
    // preview` reports a near-identical count, but inserts per-message/part newline
    // separators, so it can differ by ~1 char per message near a token boundary.)
    // Counting only the last user message undercounts multi-turn / large-system-
    // prompt requests, under-firing cost conditions and the route `max_cost_usd`
    // ceiling. Tokenizer choice is keyed on the originally-requested model's
    // provider (resolved before any rewrite).
    let req_provider = state.registry.resolve(&req.model);
    let provider_id = req_provider.as_ref().map(|p| p.id()).unwrap_or("");
    let combined = tt_shared::message_text_for_estimation(req);
    let input_tokens = tt_tokenize::estimate_tokens(provider_id, &combined);

    // Estimated request cost (USD) on the originally-requested model, for
    // cost-based route conditions. Output tokens are unknown pre-flight: use
    // `max_tokens` when set, else a default. `None` when the model has no pricing
    // — cost conditions then never fire (mirrors other unknown-data conditions).
    let estimated_cost_usd = req_provider
        .as_ref()
        .and_then(|p| p.pricing(&req.model))
        .map(|pr| estimate_cost_usd(&pr, input_tokens, req.max_tokens));

    // Live, gateway-observed p95 upstream latency for the originally-requested
    // `(provider, model)`, feeding the `upstream_latency_ms_p95_gt` condition.
    // `None` until the in-process rolling window has enough samples for this key
    // (cold start) — which makes the latency condition FALSE, never a fabricated
    // match. Computed once here (all routes evaluate the same requested model).
    let observed_p95_ms = if provider_id.is_empty() {
        None
    } else {
        state.latency_tracker.p95(provider_id, &req.model)
    };

    // Forced routing has its own trace mode: it does not evaluate conditions or
    // imply that condition/priority selected the named route. Normal routing
    // prepares one snapshot and uses the canonical traced matcher directly.
    let evaluation = match forced_route {
        Some(name) => engine.evaluate_forced_route_with_trace(name),
        None => engine.evaluate_with_signals_and_trace(
            req,
            ctx,
            input_tokens,
            estimated_cost_usd,
            observed_p95_ms,
            // Reasoning-class signal for `not_reasoning_class` conditions.
            // Computed only when some route uses it (cheap deterministic
            // substring match, no LLM call); reuses the `combined` text
            // already built for token estimation above.
            engine.uses_reasoning_class()
                && crate::reasoning_class::classify(&combined.to_lowercase()).is_some(),
        ),
    };
    let tt_routing::RoutingEvaluation {
        matched_route,
        trace,
    } = evaluation;
    let Some(m) = matched_route else {
        if let Some(name) = forced_route {
            record_route_application_trace(
                ctx.org_id,
                RouteApplicationOutcome::ForcedRouteNotFound,
                &trace,
            );
            return Err(ApiError::InvalidRequest(format!("unknown route: {name}")));
        }
        record_route_application_trace(ctx.org_id, RouteApplicationOutcome::NoMatch, &trace);
        return Ok(None);
    };
    // The cached engine preserves the ledger ID that the store captured in the
    // same database snapshot as `m`. Missing ledger provenance stays NULL; a
    // route revision is intentionally not a substitute.
    let route_version_id = engine.route_version_id(m.id);
    // Sticky pause (research Phase 2.3): a paused route still MATCHES — the
    // request attributes to it (route_id stamped, warnings token, request_logs
    // marker) — but the rewrite and every other COST lever are suppressed, so
    // the request flows to the originally-requested model (the EXPENSIVE,
    // quality-safe direction). SAFETY/privacy levers stay ON: pausing a
    // quality gate must never disable a privacy guardrail. This single seam
    // covers chat (incl. streaming), embeddings, and the messages ingress; a
    // forced `X-TokenTrimmer-Route` header does NOT bypass a pause (the
    // quality gate wins). `req.model` is untouched → `is_downgrade` is false →
    // no judge samples on a paused route → its verdict window freezes → the
    // pause is naturally sticky (plus the durable route_pauses row) until an
    // explicit POST /v1/routes/:id/resume?expected_revision=N.
    if m.paused {
        record_route_application_trace(ctx.org_id, RouteApplicationOutcome::Paused, &trace);
        tracing::info!(
            org_id = %ctx.org_id,
            route_id = %m.id,
            route = %m.name,
            "route_paused: rewrite suppressed — passing through on the requested model"
        );
        // Metric lives at this single seam so EVERY ingress that routes
        // (chat, streaming, /v1/messages, embeddings) counts its paused
        // passthroughs — embeddings has no warnings-header or request_logs
        // plumbing, so this counter is its only pause-visibility signal.
        crate::metrics::record_route_paused_passthrough(&m.name);
        return Ok(Some(RouteMatch {
            batch: false,
            route_id: m.id,
            route_version_id,
            route_name: m.name.clone(),
            paused: true,
            // ALL cost levers off (fail-safe expensive direction):
            fallbacks: vec![],
            max_cost_usd: None,
            flex: false,
            compress: false,
            doc_compaction: false,
            content_compress: false,
            // Paused route → Document Lane suppressed (COST lever).
            document_lane: false,
            format_switch: None,
            diff: false,
            traffic_pct: None,
            shadow_model: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            // The agentic context budget is a COST lever (it nets caching /
            // elision / routing for savings) — suppressed on a paused route,
            // exactly like compress/flex/format_switch above.
            agentic_budget: None,
            // The Fusion panel is a COST lever (it fans out across N
            // members + an arbiter) — suppressed on a paused route, so a paused
            // panel route flows to the originally-requested single model.
            panel: None,
            // The workflow detour is a COST lever (a matched workflow runs real
            // multi-step spend) — suppressed on a paused route, exactly like the
            // panel above.
            workflow: None,
            // SAFETY/privacy levers stay ON (pausing a quality gate must never
            // disable a privacy guardrail):
            disable_cache: m.then.disable_cache,
            redact: m.then.redact,
            input_tokens_estimate: input_tokens,
            target_model: req.model.clone(), // no rewrite
        }));
    }

    let route_id = m.id;
    let route_name = m.name.clone();
    let fallbacks = m.then.fallbacks.clone();
    let disable_cache = m.then.disable_cache;
    let max_cost_usd = m.then.max_cost_usd;
    let flex = m.then.flex;
    let batch = m.then.batch;
    let compress = m.then.compress;
    let doc_compaction = m.then.doc_compaction;
    let content_compress = m.then.content_compress;
    let document_lane = m.then.document_lane;
    let redact = m.then.redact;
    let format_switch = m.then.format_switch.clone();
    let diff = m.then.diff;
    let traffic_pct = m.then.traffic_pct;
    let shadow_model = m.then.shadow_model.clone();
    // Resolve the effective target ONCE. A modifier-only route
    // (`then.target_model == None`) keeps the caller's chosen model — every
    // downstream cost/canary/telemetry seam keeps seeing a concrete model
    // EQUAL to the caller's, so the routing delta is 0 and only the route's
    // modifier savings (e.g. agentic_budget) are booked. A `Some` target
    // behaves exactly as before (rewrite to that model).
    let effective_target = m
        .then
        .target_model
        .clone()
        .unwrap_or_else(|| req.model.clone());
    let is_model_rewrite = m.then.target_model.is_some();
    let target_model_for_split = effective_target.clone();
    let minify_json = m.then.minify_json;
    let reasoning_max_effort = m.then.reasoning_max_effort.clone();
    let reasoning_budget_tokens = m.then.reasoning_budget_tokens;
    let agentic_budget = m.then.agentic_budget.clone();

    // Capability guard: before committing the rewrite, check that the
    // target model supports everything the request requires. When ModelInfo
    // is unknown (not in the catalog) we are permissive — only skip when
    // we positively know a capability is missing.
    let required_caps = tt_shared::RequiredCapabilities::from_request(req);
    let estimated_tokens = u64::from(input_tokens);
    if let Some(info) = state.registry.model_info(&effective_target) {
        if !required_caps.satisfied_by(info, estimated_tokens) {
            let reasons = required_caps.skip_reasons(info, estimated_tokens);
            tracing::info!(
                org_id = %ctx.org_id,
                route_id = %route_id,
                model = %effective_target,
                reasons = ?reasons,
                "route_skipped_capability: rewrite target lacks required capabilities, passing through unchanged"
            );
            record_route_application_trace(
                ctx.org_id,
                RouteApplicationOutcome::CapabilitySuppressed,
                &trace,
            );
            // Do not rewrite req.model — return None so the request
            // continues with the original model.
            return Ok(None);
        }
    }

    // Only rewrite the model for a `Some`-target route. A modifier-only route
    // (`effective_target == req.model`) leaves `req.model` untouched so the
    // caller's chosen model is what gets dispatched; only the route's other
    // then-effects apply (routing delta = 0).
    let original = if is_model_rewrite {
        std::mem::replace(&mut req.model, effective_target.clone())
    } else {
        req.model.clone()
    };
    tracing::debug!(
        org_id = %ctx.org_id,
        route_id = %route_id,
        from = %original,
        to = %req.model,
        modifier_only = !is_model_rewrite,
        fallbacks = ?fallbacks,
        "routing rewrite"
    );
    record_route_application_trace(
        ctx.org_id,
        RouteApplicationOutcome::AcceptedForActionPipeline,
        &trace,
    );
    Ok(Some(RouteMatch {
        route_id,
        route_version_id,
        route_name,
        paused: false,
        fallbacks,
        disable_cache,
        max_cost_usd,
        input_tokens_estimate: input_tokens,
        flex,
        batch,
        compress,
        doc_compaction,
        content_compress,
        document_lane,
        redact,
        format_switch,
        diff,
        traffic_pct,
        shadow_model,
        target_model: target_model_for_split,
        minify_json,
        reasoning_max_effort,
        reasoning_budget_tokens,
        agentic_budget,
        // Active route's panel trigger (header-wins fallback, resolved in
        // `prepare`). `m.then.panel` is `None` for the overwhelming majority of
        // routes (no panel), so this clone is a cheap `None`.
        panel: m.then.panel.clone(),
        // Active route's workflow detour (CO-1). `m.then.workflow` is `None`
        // for the overwhelming majority of routes (no workflow), so this clone
        // is a cheap `None`. Resolved in `complete_once` before cache.
        workflow: m.then.workflow.clone(),
    }))
}
