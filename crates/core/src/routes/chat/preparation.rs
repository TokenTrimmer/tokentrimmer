//! Request validation, compatibility translation, and route-action preparation.

use super::*;
/// Run the shared per-request setup for a chat completion and bundle the result
/// into a [`Prepared`] for the streaming arm / [`complete_once`] / the
/// server-side agent loop.
///
/// This is the routing → route-action capture → request-side output shaping →
/// redaction → compression → agentic-budget → cache-behavior → body-capture
/// gating pipeline that the chat [`handler`] formerly ran inline before its
/// `if req.stream` branch. It mutates `ctx` (cross-provider credential
/// rebinding) and `req` (model rewrite + request shaping) in place, then moves
/// the final `req`/`provider` into the returned bundle. All early returns
/// (credential / cost-limit / model-not-found) propagate via `?` exactly as the
/// inline setup did.
///
/// The agent loop (slice 1a) calls this per turn (with a fresh `req`) so each
/// turn re-routes/redacts independently; the chat handler calls it once.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare(
    state: &AppState,
    ctx: &mut RequestContext,
    req: &mut ChatCompletionRequest,
    headers: &HeaderMap,
    mut provider: std::sync::Arc<dyn tt_shared::Provider>,
    provider_pin: Option<String>,
    forced_route: Option<String>,
    request_timeout: Option<Duration>,
    idempotency_key: String,
    raw_bearer: String,
    org_id: Uuid,
    source_provider_id: String,
    source_creds_missing: bool,
    caller_tier: Option<tt_shared::CallerTier>,
    l2_allowed: bool,
    retrieval_telemetry: RetrievalTelemetry,
    request_started: Instant,
    is_mechanical: bool,
    skip_shadow: bool,
) -> ApiResult<Prepared> {
    // 2c. Routing engine. Rewrite `req.model` if the org has a matching
    //     enabled route — must happen BEFORE the cache lookup (so L1 keys
    //     and L2 lookups use the routed model) and BEFORE provider
    //     dispatch. The cache is per-org with a ~60s TTL, so this is cheap.
    //     Unknown-org / no-store / no-match all fall through unchanged.
    // Capture the originally-requested model's pricing BEFORE routing may
    // rewrite `req.model`. `baseline_cost_usd` is priced against this so a
    // downgrade route's saving shows up in saved_usd / request_logs instead
    // of collapsing to ~0 (which happens when baseline is priced against the
    // cheap routed-to model).
    let requested_pricing = provider.pricing(&req.model);
    // The model the caller asked for, captured before routing rewrites
    // `req.model`. Recorded as `gen_ai.request.model` on the request span (the
    // served model becomes `gen_ai.response.model`).
    let requested_model = req.model.clone();
    // Sampled-quality-judge inputs, captured BEFORE routing rewrites the model
    // or rebinds the provider/credentials. The async judge (spawned only for a
    // ~2% sample of rerouted-DOWN requests, after the user response is returned)
    // re-dispatches the ORIGINAL model on the SOURCE provider to produce a
    // reference answer, then scores the served (cheaper) answer against it.
    // Cheap clones (a few Arc bumps + a request clone); they cost nothing on the
    // hot path when the judge is disabled because we only build the job later
    // when sampling actually fires.
    let judge_enabled = state.judge_config.enabled && state.judge_sink.is_some();
    let (mut judge_source_provider, mut judge_source_ctx, mut judge_original_req) = if judge_enabled
    {
        (Some(provider.clone()), Some(ctx.clone()), Some(req.clone()))
    } else {
        (None, None, None)
    };
    let route_match = apply_routing(state, ctx, req, forced_route.as_deref()).await?;
    let matched_route_id = route_match.as_ref().map(|m| m.route_id);
    // This is the immutable ledger ID captured with the runtime route cache
    // refresh. It is nullable by design; never fall back to a mutable route
    // revision when a legacy/skewed ledger cannot provide an identity.
    let matched_route_version_id = route_match.as_ref().and_then(|m| m.route_version_id);
    // The matched route is sticky-PAUSED: apply_routing suppressed the rewrite
    // and every cost lever (req.model is untouched). Captured before
    // `route_match` is consumed; drives the warnings token + metric +
    // request_logs marker below.
    let route_paused = route_match.as_ref().is_some_and(|m| m.paused);
    // The applied route's name (forced or condition-matched) for the
    // `X-TokenTrimmer-Route-Matched` response header, captured before
    // `route_match` is consumed below.
    let route_matched_name = route_match.as_ref().map(|m| m.route_name.clone());
    // A matched privacy route forces the request to skip the cache entirely.
    let route_disable_cache = route_match.as_ref().is_some_and(|m| m.disable_cache);
    // A matched route requesting OpenAI Flex (`service_tier="flex"`). Applied to
    // the upstream request below, gated on the served model's flex-eligibility.
    let route_flex = route_match.as_ref().is_some_and(|m| m.flex);
    // A matched route requesting the advisory batch-eligibility marker
    // (`RouteAction::batch`). Gated below (after routing/pin resolve the final
    // provider) on streaming / interactive / catalog batch rate — see
    // `maybe_mark_batch_eligible`. Advisory: dispatch is never detoured.
    let route_batch = route_match.as_ref().is_some_and(|m| m.batch);
    // Opt-in contract-changing output shaping (research Phase 3.3 + 3.4),
    // captured before `route_match` is consumed below. Both default
    // off/None — the planners do ZERO work for every unrouted request and
    // every route that did not opt in. Eligibility is enforced in code at
    // the request-build step (`shaping::*::plan_*`). Mutable: the canary
    // CONTROL arm clears both below ("served unchanged" includes the parse
    // contract).
    let mut route_format_switch = route_match.as_ref().and_then(|m| m.format_switch.clone());
    let mut route_diff = route_match.as_ref().is_some_and(|m| m.diff);
    // Hard interactive-ineligibility signal for the batch marker: a client that
    // declares "a human is waiting" via `X-TokenTrimmer-Interactive: 1` (set by
    // `tt chat` and the /tools loop) is never marked batch-eligible. Read
    // directly from the header map — deliberately NOT a RequestContext field.
    // Fails in the interactive-SAFE direction: ANY non-empty value other than
    // an explicit opt-out (`0` / `false`) counts as interactive, so a client
    // sending an unrecognized truthy spelling (`yes`, `on`, …) is never
    // silently treated as batch-markable. This gate becomes load-bearing when
    // the async Batch Lane ships — misparsing must err toward "human waiting".
    let interactive_client = headers
        .get("x-tokentrimmer-interactive")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .is_some_and(|s| !s.is_empty() && s != "0" && !s.eq_ignore_ascii_case("false"));
    // A matched route opting into minified-JSON output steering
    // (`RouteAction::minify_json`). Applied below by `maybe_minify_json` (after
    // the response_format downgrade so the grammar-lock check sees the FINAL
    // response_format, and before the request-pass stage / cache-key
    // derivation so the injected bytes are what gets cached and dispatched).
    let route_minify = route_match.as_ref().is_some_and(|m| m.minify_json);
    // A matched route capping reasoning spend (`RouteAction::reasoning_max_effort`
    // / `reasoning_budget_tokens`). Applied below by `maybe_cap_reasoning`
    // against the FINAL served provider/model (class-gated HARD, lower-only,
    // books $0 — metered only).
    let route_reasoning_max_effort = route_match
        .as_ref()
        .and_then(|m| m.reasoning_max_effort.clone());
    let route_reasoning_budget_tokens =
        route_match.as_ref().and_then(|m| m.reasoning_budget_tokens);
    // A matched route opting into the conservative compression pass
    // (`RouteAction::compress`). When false (the default — no route or a route
    // that did not enable it) the request-pass pipeline never runs and the
    // request is byte-for-byte unchanged.
    let route_compress = route_match.as_ref().is_some_and(|m| m.compress);
    // A matched route opting into the lossless document-compaction pass
    // (`RouteAction::doc_compaction`, Document Lane D2). When false (the
    // default — no route or a route that did not enable it) the doc-compaction
    // request-pass pipeline never runs and the request is byte-for-byte
    // unchanged.
    let route_doc_compaction = route_match.as_ref().is_some_and(|m| m.doc_compaction);
    // A matched route opting into the Document Lane distillation seam
    // (`RouteAction::document_lane`, D4c). It runs after the route/canary choice
    // (the opt-in and candidate target are now known) but before target-provider
    // rebind, pinning, panel admission, failover, and cache preparation. Only a
    // complete all-media conversion retains the target rewrite; otherwise raw
    // media and the caller model are restored. When false (the default), the
    // seam never runs — zero behavior change.
    let route_document_lane = route_match.as_ref().is_some_and(|m| m.document_lane);
    // A matched route opting into the content-aware compression pass
    // (`RouteAction::content_compress`, P1a). When false (the default — no route
    // or a route that did not enable it) the content_compress request-pass
    // pipeline never runs and the request is byte-for-byte unchanged.
    let route_content_compress = route_match.as_ref().is_some_and(|m| m.content_compress);
    // A matched route opting into the request-redaction guardrail
    // (`RouteAction::redact`). When false (the default) the redaction pass never
    // runs and the request is byte-for-byte unchanged. This is a SAFETY
    // transform — it strips PII/secrets from the OUTBOUND request before
    // dispatch; it never attributes a saving and surfaces a `redacted:<class>`
    // warning when it fires.
    let route_redact = route_match.as_ref().is_some_and(|m| m.redact);
    // A matched route opting into the agentic context budget
    // (`RouteAction::agentic_budget`). `None` for every unrouted request and
    // every route that did not opt in (the default), so the
    // `AgenticBudgetPlanner` is never constructed on that path and `req` stays
    // byte-for-byte unchanged. When `Some(_)`, the planner nets the
    // cache-prefix / field-drop / summarize / route levers AFTER redaction +
    // compression, just before dispatch (see the wiring point below). Off by
    // default is LOAD-BEARING: the default request path must be byte-identical.
    let route_agentic_budget = route_match.as_ref().and_then(|m| m.agentic_budget.clone());
    // The matched route's panel trigger (header-wins: consulted only when the
    // caller sent no `X-TokenTrimmer-Panel` header). `None` for the
    // overwhelming majority of routes — the single-model path is untouched.
    let route_panel = route_match.as_ref().and_then(|m| m.panel.clone());
    // The matched route's workflow-detour trigger (CO-1). Unlike `panel` there
    // is no header override — the route is the only trigger. `None` for the
    // overwhelming majority of routes (no workflow), so this clone is a cheap
    // `None` and the single-model path is untouched (off by default).
    let route_workflow = route_match.as_ref().and_then(|m| m.workflow.clone());
    // Redaction × judge sampling: the judge captures above hold the
    // PRE-redaction request — the judge job re-dispatches it verbatim to the
    // source provider for the baseline reference AND embeds its text in the
    // judge prompt (potentially a THIRD vendor serving the judge model). On a
    // redact route that would bypass the "the secret never reaches the
    // upstream provider" guarantee the redaction pass exists to enforce, so
    // the judge is skipped wholesale for redact routes (both the dispatch-path
    // and the L2-hit judge no-op via the all-or-nothing capture gate). The
    // measurement path must never out-leak the dispatch path.
    if route_redact {
        judge_source_provider = None;
        judge_source_ctx = None;
        judge_original_req = None;
    }
    // Canary traffic split (#454) + shadow mode, captured before `route_match`
    // is consumed below. `route_traffic_pct` is the configured split percentage
    // (None = unconditional rewrite). `route_shadow_model` is the discarded
    // shadow candidate. `route_target_model` is the canary arm's model — used to
    // revert to the originally-requested model when the split assigns this
    // request to the control arm.
    let route_traffic_pct = route_match.as_ref().and_then(|m| m.traffic_pct);
    let mut route_shadow_model = route_match.as_ref().and_then(|m| m.shadow_model.clone());
    let route_target_model = route_match.as_ref().map(|m| m.target_model.clone());
    // Per-request cost ceiling (V3d-2b) + the token estimate, captured before
    // `route_match` is consumed below.
    let route_max_cost_usd = route_match.as_ref().and_then(|m| m.max_cost_usd);
    let route_input_tokens = route_match
        .as_ref()
        .map(|m| m.input_tokens_estimate)
        .unwrap_or(0);
    // Ordered fallback model ids from the matched route (empty = no failover).
    let mut route_fallbacks: Vec<String> = route_match.map(|m| m.fallbacks).unwrap_or_default();
    // An incomplete/suppressed Document Lane conversion restores the original
    // model and must also suppress any later header fallback override. Otherwise
    // raw media could still reach a text fallback after the route chain itself
    // was cleared.
    let mut document_lane_blocks_fallbacks = false;
    let mut document_lane_warning: Option<&'static str> = None;
    let mut doc_distill_booking = crate::document_lane::seam::DistillBookkeeping::default();

    // ── Canary traffic split (#454) ──────────────────────────────────────────
    //
    // Evaluated HERE: AFTER the route match (so we know the candidate rewrite)
    // and BEFORE the cache lookup (so the L1 key / L2 lookup use the arm's actual
    // served model). When the matched route declares a `traffic_pct`, the sticky,
    // replica-independent split (`tt_routing::sticky_traffic_split`) decides the
    // arm from `(org_id, idempotency_key, traffic_pct)`:
    //
    //   * CANARY arm  → keep `apply_routing`'s rewrite (req.model == target_model).
    //   * CONTROL arm → REVERT req.model to the originally-requested model so the
    //     request is served unchanged; the route still attributes (route_id is
    //     stamped) but with `arm="control"` and NO routing saving (baseline ==
    //     served). `route_fallbacks` are dropped on the control arm — they belong
    //     to the canary target, not the reverted original.
    //
    // No `traffic_pct` (the common case) → unconditional rewrite, arm = None.
    let mut traffic_split_arm: Option<&'static str> = None;
    // A paused match is NOT a rewrite: baseline is priced against the served
    // (== requested) model, so the routing saving honestly books 0.
    let mut model_was_rewritten = matched_route_id.is_some() && !route_paused;
    if let Some(pct) = route_traffic_pct {
        let in_canary = tt_routing::sticky_traffic_split(ctx.org_id, &idempotency_key, pct);
        if in_canary {
            traffic_split_arm = Some("canary");
            // req.model already holds the canary target (apply_routing rewrote it).
        } else {
            traffic_split_arm = Some("control");
            // Revert to the originally-requested model. NOTE: only the MODEL
            // is reverted — route ACTIONS captured above (compress, and now
            // minify_json / reasoning caps) still apply on the control arm,
            // matching the long-standing compress precedent. A traffic split
            // therefore does not A/B the shaping actions themselves (an
            // action-only same-model route splits nothing); arm-level
            // shaped-vs-unshaped comparison comes from the paired-judge
            // channel, whose baseline is captured pre-shaping.
            if let Some(target) = route_target_model.as_deref() {
                if req.model == target {
                    req.model = requested_model.clone();
                }
            }
            // The control arm is NOT a rewrite: no provider re-resolve, baseline
            // priced against the served (== requested) model, no canary fallbacks.
            model_was_rewritten = false;
            route_fallbacks.clear();
            // The control arm's contract is "the request is served
            // UNCHANGED" — that must include the CONTRACT-CHANGING shaping
            // levers, not just the model rewrite: a CSV/bare body or a
            // patch-reconstructed artifact on the control arm would break
            // the caller's parse expectations AND pollute the canary
            // comparison with shaped responses. Non-contract levers
            // (flex/compress/batch) stay precedent-consistent on both arms,
            // and the redaction SAFETY guardrail is never arm-gated.
            route_format_switch = None;
            route_diff = false;
        }
    }
    let traffic_split_arm_owned = traffic_split_arm.map(str::to_string);

    // Sub-lever 3 (agent-loop only): down-route a mechanical sub-step turn to
    // the route's `route_mechanical_to` model, IF the route opted in AND is not
    // auto-paused. Keeping `matched_route_id` set => the existing paired-quality
    // judge + `route_autopause` treat it as a routed serving and self-revert on
    // regression. Placed BEFORE the `if model_was_rewritten` block so setting
    // `model_was_rewritten = true` triggers the existing provider/credential
    // (re)resolve for the cheaper `req.model` — the down-routed model's
    // provider/creds are used for dispatch. `is_mechanical` is always false on
    // the chat path (the handler passes `false`), so this block is inert there.
    // The unresolved-target warning is deferred to `mechanical_route_warning`
    // because the pre-dispatch `warnings` vec is not yet in scope here.
    let mut mechanical_route_warning: Option<String> = None;
    if is_mechanical && !route_paused {
        if let Some(target) = route_agentic_budget
            .as_ref()
            .and_then(|ab| ab.route_mechanical_to.clone())
        {
            if target != req.model {
                if state.registry.resolve(&target).is_some() {
                    req.model = target;
                    model_was_rewritten = true; // baseline priced vs the original model
                                                // provider is (re)resolved below for the new req.model
                } else {
                    mechanical_route_warning =
                        Some(format!("mechanical_route_unresolved:{target}"));
                }
            }
        }
    }

    // Document Lane D4c — after route/canary selection but before any
    // target-provider rebind, provider pin, panel admission, or failover/cache
    // setup. A complete transaction can safely keep the target rewrite because
    // every lane-targeted media part is now text. A disabled/failed/partial
    // sidecar transaction instead restores the raw request's caller model and
    // drops route/header fallbacks and shadow work, so raw media cannot leak to a
    // text target. Fusion panels and non-shadow workflow detours own their own
    // response/model compositions; they receive the raw request and explicitly
    // suppress this direct-path optimization rather than inheriting its booking.
    if route_document_lane && crate::document_lane::seam::request_has_lane_targeted_parts(req) {
        let owner = non_direct_response_owner(
            panel::panel_from_header(headers).is_some() || route_panel.is_some(),
            route_workflow.as_ref(),
            req.stream,
            skip_shadow,
        );
        if let Some(owner) = owner {
            document_lane_warning = Some(owner);
            document_lane_blocks_fallbacks = true;
            rollback_document_lane_route_rewrite(
                req,
                &requested_model,
                &mut model_was_rewritten,
                &mut route_fallbacks,
                &mut route_shadow_model,
            );
        } else {
            let harness = crate::document_lane::seam::DistillHarness::from_env();
            let distill_model = req.model.clone();
            match crate::document_lane::seam::distill_request_parts_with_outcome(
                &harness,
                &distill_model,
                req,
            )
            .await
            {
                crate::document_lane::seam::RequestDistillOutcome::Complete { booking } => {
                    tracing::info!(
                        target: "tokentrimmer.document_lane",
                        distilled_parts = booking.distilled_parts,
                        raw_image_tokens = booking.raw_image_tokens,
                        distilled_text_tokens = booking.distilled_text_tokens,
                        "document-lane seam distilled every media part to text"
                    );
                    doc_distill_booking = booking;
                }
                crate::document_lane::seam::RequestDistillOutcome::Incomplete => {
                    document_lane_warning = Some("incomplete");
                    document_lane_blocks_fallbacks = true;
                    rollback_document_lane_route_rewrite(
                        req,
                        &requested_model,
                        &mut model_was_rewritten,
                        &mut route_fallbacks,
                        &mut route_shadow_model,
                    );
                }
                crate::document_lane::seam::RequestDistillOutcome::NoEligibleParts => {
                    // The predicate above already proved otherwise. Preserve a
                    // fail-open no-op if a future request representation makes
                    // the scan and transaction disagree.
                }
            }
        }
    }

    if model_was_rewritten {
        // Provider may change when a route crosses providers (V3d-1); the
        // registry is the source of truth.
        provider = state
            .registry
            .resolve(&req.model)
            .ok_or_else(|| ApiError::ModelNotFound {
                model: req.model.clone(),
            })?;
        // Cross-provider rewrite: the credentials resolved above are for the
        // source provider. For a single-provider dispatch, re-resolve for the
        // target and fail closed if the org has no credential (never forward
        // the source key). The failover path resolves per-candidate below.
        if route_fallbacks.is_empty() && provider.id() != source_provider_id {
            match resolve_credentials_for(state, org_id, provider.id(), &raw_bearer, false).await {
                Some(c) => ctx.credentials = c,
                None => {
                    return Err(ApiError::MissingProviderCredential {
                        provider: provider.id().to_string(),
                    })
                }
            }
        }
    }

    // 2d. Explicit provider pin (X-TokenTrimmer-Provider) — overrides the
    //     routed/inferred provider; the routed model is kept. Fails closed on a
    //     cross-provider pin with no stored credential.
    let (pinned_provider, pin_creds) = apply_provider_override(
        state,
        provider_pin.as_deref(),
        org_id,
        &raw_bearer,
        &source_provider_id,
        provider,
    )
    .await?;
    provider = pinned_provider;
    if let Some(c) = pin_creds {
        ctx.credentials = c;
    }
    if provider_pin.is_some() {
        // An explicit provider pin must not fail over to a different provider, and
        // it suppresses the `X-TokenTrimmer-Fallback` override too: the failover
        // path re-resolves the primary candidate by model id and so cannot honor a
        // pinned primary provider. The pin wins (single-provider dispatch).
        route_fallbacks.clear();
    } else if !document_lane_blocks_fallbacks {
        if let Some(chain) = fallback_override_from_header(headers) {
            // `X-TokenTrimmer-Fallback` overrides the route-derived chain (no pin).
            route_fallbacks = chain;
        }
    }

    // Cost ceilings are evaluated only after an explicit provider pin has
    // settled the actual primary provider. The route ceiling retains its
    // existing rewrite-only scope (including active modifier-only routes),
    // while the header applies to every request. The route keeps its original
    // match-time input estimate; the header prices the whole final prompt.
    let header_cost_limit = cost_limit_from_header(headers)?;
    let combined = tt_shared::message_text_for_estimation(req);
    let header_input_tokens = tt_tokenize::estimate_tokens(provider.id(), &combined);
    let primary_pricing = provider.pricing(&req.model);
    if model_was_rewritten {
        enforce_cost_limit(
            route_max_cost_usd,
            primary_pricing.as_ref(),
            &req.model,
            route_input_tokens,
            req.max_tokens,
        )?;
    }
    enforce_cost_limit(
        header_cost_limit,
        primary_pricing.as_ref(),
        &req.model,
        header_input_tokens,
        req.max_tokens,
    )?;

    // The primary is now admitted before any cache lookup. When a chain is
    // configured, carry the same independent constraints into its candidates.
    // The route constraint keeps its historical match estimate; the header
    // constraint is re-estimated from the final prompt for each candidate's
    // tokenizer before it can obtain credentials, enter a breaker trial, or
    // make an upstream call.
    let route_failover_cost = if model_was_rewritten {
        route_max_cost_usd.map(|ceiling_usd| crate::failover::RouteCostConstraint {
            ceiling_usd,
            input_tokens: route_input_tokens,
            max_tokens: req.max_tokens,
        })
    } else {
        None
    };
    let header_failover_cost =
        header_cost_limit.map(|ceiling_usd| crate::failover::HeaderCostConstraint {
            ceiling_usd,
            max_tokens: req.max_tokens,
        });
    let failover_cost_check = (!route_fallbacks.is_empty()
        && (route_failover_cost.is_some() || header_failover_cost.is_some()))
    .then_some(crate::failover::CandidateCostCheck {
        route: route_failover_cost,
        header: header_failover_cost,
    });

    // Fusion panel resolution + fail-closed budget gate (spec
    // §6.4 steps 1-3). Runs HERE, before `Prepared` is built and before any
    // dispatch, so an over-budget / unpriceable / kill-switched panel 4xx/402s
    // with ZERO upstream calls. Off-by-default: an absent `X-TokenTrimmer-Panel`
    // header (the common case) leaves `panel = None` and the single-model path
    // wire-identical — the only added work is parsing one absent header.
    // Header-wins (D2): an explicit `X-TokenTrimmer-Panel` header beats a
    // matched route's `then.panel`. The header is consulted first; the route is
    // the fallback trigger only when the header is absent. Both feed the SAME
    // kill-switch / entitlement / budget gate / credential-resolution blocks
    // below (D3) — a route-triggered panel never bypasses a gate.
    enum PanelTrigger {
        /// Explicit `X-TokenTrimmer-Panel` header — extras come from `tt_extras.panel`.
        Header(panel::ArbiterStrategyKind),
        /// Matched route's `then.panel` — extras come from the `RoutePanel` fields.
        Route(tt_routing::RoutePanel),
    }
    let panel_trigger = match panel::panel_from_header(headers) {
        Some(strategy) => Some(PanelTrigger::Header(strategy)),
        None => route_panel.map(PanelTrigger::Route),
    };
    let (panel, panel_admission, panel_creds) = if let Some(trigger) = panel_trigger {
        // Kill-switch: an explicit panel request on a panel-disabled gateway is a
        // hard 403, never a silent fallback to single-model billing (spec §6.5).
        if !state.panel_enabled {
            return Err(ApiError::PanelDisabled);
        }
        // Entitlement: panel requires `state.panel_min_tier` or higher. `caller_tier`
        // is the prepare param (None ⇒ Free fallback). Default min Free ⇒ no-op.
        let caller = caller_tier.unwrap_or(tt_shared::CallerTier::Free);
        if panel::panel_tier_rank(caller) < panel::panel_tier_rank(state.panel_min_tier) {
            return Err(ApiError::Forbidden(format!(
                "panel: requires {:?} tier or higher",
                state.panel_min_tier
            )));
        }
        // Resolve the full config from the trigger source + env defaults. The
        // HEADER path reads `tt_extras.panel` for overrides (unchanged); the
        // ROUTE path maps the `RoutePanel` fields into a `PanelExtras`. An empty
        // member list (no extras, no defaults) errors here. `resolve` does the
        // ModelRef lift + member cap for both branches identically.
        //
        // `Option` so the route branch can DEFENSIVELY skip (yield `None`) when
        // its strategy string fails to parse at request time — which should
        // never happen post-`validate_panel`, but must fall through to the
        // single-model path, NEVER panic.
        let cfg: Option<panel::PanelConfig> = match trigger {
            PanelTrigger::Header(strategy) => Some(panel::PanelConfig::resolve(
                strategy,
                tt_shared::messages::parse_panel_extras(&req.tt_extras).as_ref(),
                &panel::PanelDefaults::from_env(),
            )?),
            PanelTrigger::Route(rp) => {
                // Authoritative request-time parse of the route's strategy
                // string. `validate_panel` already rejected unknown values at
                // route creation, so this should always parse — but a defensive
                // skip (fall through to single-model) is correct, NEVER a panic.
                match panel::ArbiterStrategyKind::parse(&rp.strategy) {
                    Some(strategy) => {
                        let extras = tt_shared::messages::PanelExtras {
                            members: rp.members,
                            arbiter_model: rp.arbiter,
                            quorum: rp.quorum,
                            max_cost_usd: rp.max_cost_usd,
                        };
                        Some(panel::PanelConfig::resolve(
                            strategy,
                            Some(&extras),
                            &panel::PanelDefaults::from_env(),
                        )?)
                    }
                    None => {
                        tracing::warn!(
                            strategy = %rp.strategy,
                            "route panel strategy failed to parse at request time \
                             (should have been caught by validate_panel) — skipping panel"
                        );
                        None
                    }
                }
            }
        };
        match cfg {
            // Defensive skip (route strategy unparseable): no panel, no gates,
            // no creds — the request continues on the single-model path.
            None => (None, None, std::collections::HashMap::new()),
            Some(cfg) => {
                // Fail-closed budget gate: prices the known static Fusion shape
                // (member fan-out plus strategy-specific arbiter fan-in/output),
                // including max_completion_tokens when it overrides max_tokens.
                // Any unpriceable member or a missing budget ⇒ 402 before any
                // dispatch. This remains an admission estimate, not a runtime
                // reservation or spending ceiling.
                let admission = panel::admit_panel_request_with_tokenizer_provider(
                    state,
                    &cfg,
                    req,
                    provider.id(),
                    cost_limit_from_header(headers)?,
                )?;
                // Per-member-provider credential pre-resolution (spec §6.4 step 4),
                // keyed by provider id. Mirrors the failover pre-resolution pattern
                // (distinct providers, first-seen order, resolve each once): the
                // raw-Bearer fallback is allowed ONLY for the source provider (the bearer
                // IS its key); cross-provider members with no stored org credential stay
                // absent. The following request-local preflight rejects an impossible
                // quorum or missing LLM-arbiter credential before dispatch; only extra
                // members beyond that feasible quorum can later be `skipped_no_cred`.
                // The arbiter provider is included so arbitration can dispatch on a
                // member-distinct provider.
                let mut provider_ids: Vec<String> = Vec::new();
                for m in cfg
                    .members
                    .iter()
                    .chain(std::iter::once(&cfg.arbiter_model))
                {
                    if let Some(p) = state.registry.resolve(&m.model) {
                        let pid = p.id().to_string();
                        if !provider_ids.contains(&pid) {
                            provider_ids.push(pid);
                        }
                    }
                }
                let mut creds: std::collections::HashMap<String, ProviderCredentials> =
                    std::collections::HashMap::new();
                for pid in provider_ids {
                    let allow_bearer = pid == source_provider_id;
                    if let Some(c) =
                        resolve_credentials_for(state, org_id, &pid, &raw_bearer, allow_bearer)
                            .await
                    {
                        creds.insert(pid, c);
                    }
                }
                // This is a credential-map feasibility fence, not a provider-health,
                // credential-validity, reservation, or runtime-readiness probe. It runs
                // before `Prepared` exists, so an impossible panel opens no upstream
                // request and creates no panel result/log row.
                panel::validate_panel_credential_preflight(state, &cfg, &creds)?;
                (Some(cfg), Some(admission), creds)
            }
        }
    } else {
        (None, None, std::collections::HashMap::new())
    };

    // Normalize the request for the routed provider and collect any pre-dispatch
    // warnings (B2: response_format_downgrade; B3 will add temperature_clamped).
    let mut warnings: Vec<String> = Vec::new();
    // A mechanical down-route whose `route_mechanical_to` model did not resolve
    // surfaces a warning and the original model is served unchanged (captured
    // above before `warnings` existed).
    if let Some(w) = mechanical_route_warning {
        warnings.push(w);
    }
    // Surface a paused-route passthrough on the warnings header (the
    // request_logs row carries the durable `route_paused` marker; this is the
    // caller-visible signal). The `route_paused_passthrough_total` metric is
    // recorded inside `apply_routing` — the single pause seam — so non-chat
    // ingresses (embeddings) count too.
    if route_paused {
        let name = route_matched_name.as_deref().unwrap_or("unknown");
        warnings.push(format!("route_paused:{name}"));
    }
    if let Some(reason) = document_lane_warning {
        warnings.push(format!("document_lane_not_applied:{reason}"));
    }

    // ── Request-side output shaping (research Phase 3.3 + 3.4) ──────────────
    //
    // MUST run BEFORE `maybe_downgrade_response_format` (the downgrade would
    // erase the json_schema shape the csv planner reads; once a switch/diff
    // applies, response_format is None and the downgrade no-ops) and before
    // cache-key derivation (the mutated request hashes to its own L1 key).
    // The diff and format planners gate on `req.stream` internally, so their
    // streaming behavior remains planner-owned. A resolved, non-shadow Fusion
    // panel or workflow detour owns the final response instead of the direct
    // dispatch, so transformations that rely on the direct response tail must
    // not mutate the request for an output path that cannot validate,
    // reconstruct, advertise, or book them. This applies equally to route- and
    // header-selected panels because both resolve to `panel` above.
    // `skip_shadow` means complete_once deliberately drops those owners and
    // takes the direct path, so it remains eligible. Finally,
    // format_switch × diff is config-rejected at route creation
    // (`validate_output_shaping`); defensively, if both somehow apply, diff
    // wins and the switch is skipped with the `conflict` token.
    let response_owner = non_direct_response_owner(
        panel.is_some(),
        route_workflow.as_ref(),
        req.stream,
        skip_shadow,
    );
    let diff_preparation = prepare_route_diff(req, route_diff, response_owner);
    let format_switch_owner = if !skip_shadow && panel.is_some() {
        Some(FormatSwitchResponseOwner::FusionPanel)
    } else if !skip_shadow
        && route_workflow
            .as_ref()
            .is_some_and(|cfg| cfg.mode.as_deref() != Some("shadow"))
    {
        Some(FormatSwitchResponseOwner::WorkflowDetour)
    } else {
        None
    };
    let diff_applies = matches!(&diff_preparation, RouteDiffPreparation::Applied(_));
    let mut format_switch_plan: Option<crate::shaping::format_switch::FormatSwitchPlan> = None;
    match prepare_route_format_switch(
        req,
        route_format_switch.as_deref(),
        format_switch_owner,
        diff_applies,
    ) {
        RouteFormatSwitchPreparation::Applied(p) => format_switch_plan = Some(p),
        RouteFormatSwitchPreparation::Skipped(r) => {
            warnings.push(format!("format_switch_skipped:{r}"));
            crate::metrics::record_format_switch_skip(r);
        }
        RouteFormatSwitchPreparation::NotRequested => {}
    }
    let mut diff_plan: Option<crate::shaping::diff::DiffPlan> = None;
    match diff_preparation {
        RouteDiffPreparation::Applied(plan) => diff_plan = Some(plan),
        RouteDiffPreparation::Skipped(reason) => {
            warnings.push(format!("diff_skipped:{reason}"));
            crate::metrics::record_diff("skipped", reason);
        }
        RouteDiffPreparation::NotRequested => {}
    }

    maybe_downgrade_response_format(req, provider.as_ref(), &mut warnings);
    maybe_clamp_temperature(req, provider.as_ref(), &mut warnings);

    // OpenAI Flex (route action): opt the upstream request into `service_tier:
    // "flex"` ONLY when the served model is flex-eligible (carries a Flex rate in
    // the catalog). An ineligible model is left untouched and a
    // `flex_not_applied:<model>` warning is surfaced. `flex_applied` drives the
    // cost computation below so savings attribute to the `flex` source. Evaluated
    // against the FINAL served provider/model (post-routing/pin/failover-primary).
    //
    // A selected Fusion panel clones this base request into each independently
    // priced member leg. Flex is intentionally a single-dispatch tier: applying
    // the primary model's eligibility to every member would forward
    // `service_tier="flex"` without a per-leg eligibility/accounting contract.
    // Suppress the route-originated opt-in whenever an actual panel was admitted;
    // the panel aggregate consequently makes no Flex billing claim either.
    let flex_applied = if route_flex && panel.is_some() {
        warnings.push("flex_not_applied:panel".to_string());
        false
    } else {
        maybe_apply_flex(req, route_flex, provider.as_ref(), &mut warnings)
    };

    // Advisory batch-eligibility marker (route action, research Phase 2.1):
    // never mutates the request or detours dispatch — the gateway is
    // synchronous today. `batch_marked` drives the request_logs tagging and
    // the forgone-discount attribution below. Hard ineligibility (streaming /
    // interactive) and the catalog-batch-rate gate are enforced inside.
    let batch_marked = maybe_mark_batch_eligible(
        req,
        route_batch,
        interactive_client,
        provider.as_ref(),
        &mut warnings,
    );

    // Minified-JSON output steering (route action, research Phase 3.1):
    // appends the deterministic instruction suffix to the system prompt.
    // Ordered AFTER `maybe_downgrade_response_format` (the grammar-lock check
    // must see the FINAL response_format) and BEFORE the request-pass stage,
    // so the injected bytes precede `SplitRequest::compute` and L1/L2 key
    // derivation — minify-route traffic keys separately from non-minify
    // traffic and the cached body is the minified-steered one. The constant
    // suffix is deterministic-on-ingress, so no provider prompt-cache bust is
    // booked (redaction precedent). `minify_applied` drives the per-response
    // ESTIMATE (non-streaming only), the metric, and judge eligibility.
    let minify_applied = prepare_route_minify_json(
        req,
        route_minify,
        provider.as_ref(),
        response_owner,
        &mut warnings,
    );

    // Class-gated reasoning-token cap (route action, research Phase 3.2):
    // lowers OpenAI-style `reasoning_effort` / Anthropic-style thinking
    // budgets on over-provisioned routes. Evaluated against the FINAL served
    // provider/model (post-routing/pin). Books $0 — the unspent thinking
    // tokens are only statistically visible; `reasoning_capped` feeds the
    // metric and judge eligibility (`output_shaped`).
    //
    // ORDERING COUPLING: this runs after `maybe_minify_json`, so when both
    // actions are configured on one route the class-gate classifier sees the
    // injected `MINIFY_JSON_INSTRUCTION` text too. Today no word in that
    // instruction matches any `reasoning_class` keyword set (verified), but a
    // rewording that introduces one (e.g. "function") would silently
    // class-gate every minify+cap request — if you edit either the
    // instruction or the keyword tables, re-check the intersection.
    let served_model_info = state.registry.model_info(&req.model).cloned();
    let reasoning_capped = maybe_cap_reasoning(
        req,
        route_reasoning_max_effort.as_deref(),
        route_reasoning_budget_tokens,
        provider.as_ref(),
        served_model_info.as_ref(),
        route_matched_name.as_deref().unwrap_or("none"),
        &mut warnings,
    );

    // Opt-in deterministic prefix normalization (gateway flag
    // `TT_PREFIX_NORMALIZATION`, default OFF): canonicalize tool definitions
    // (sort by name, key-sort JSON schemas) + system-prompt text (CRLF→LF,
    // trailing-whitespace strip, 3+ newline collapse) BEFORE cache-key
    // derivation, so identical-intent-but-not-byte-identical requests share
    // L1 keys and warmed provider prompt-cache prefixes. The transform is
    // deterministic on the ingress bytes (fixed orderings, fixed whitespace
    // rules) — no cache bust is booked because re-sends re-produce the same
    // canonical bytes. Tool ORDER can be behavior-relevant to model
    // tool-selection, which is why this is opt-in, and the fired marker on
    // the warnings header is the caller-visible signal.
    if state.prefix_normalization {
        *req = crate::passes::prefix_normalizer::normalize_request_prefix(std::mem::take(req));
        warnings.push("prefix_normalization_applied".to_string());
    }

    // ── Request-pass stage ────────────────────────────────────────────────
    //
    // Order: redaction (escape hatch) → compression pipeline (cache-aware
    // split + token-true gate) → cache classifier (always-on diagnostics).
    // The stage works on a cache-aware SPLIT of the request:
    // `SplitRequest::compute` derives the cache-stable prefix (everything the
    // provider's prompt cache keys on — Anthropic: the system prefix the
    // adapter marks per #126/#150 on a single-shot, the entire message list
    // on a cache-qualified multi-turn conversation; OpenAI-style positional
    // auto-cache: the entire message list whenever the prompt clears the
    // model's minimum) and passes only the volatile tail to the pipeline
    // mutably. The stable prefix is read-only BY TYPE — see the
    // `crate::passes` module docs. Busting a cache-warm prefix reprices ~0.1x
    // cache reads back to 1.0x, so a NON-deterministic transform that
    // deliberately mutates it must book the estimated cost as a NEGATIVE
    // savings entry (`CacheBustEstimate`) — never hide a cache bust.
    //
    // The model/pricing snapshot is taken AFTER routing/pin (the FINAL served
    // provider) so token counts and penalties price what the upstream bills.
    // Test/local providers carry no `prompt_cache_min_tokens`, which keeps
    // them on the all-volatile (pre-split) path.
    let pass_model = req.model.clone();
    let pass_pricing = provider.pricing(&pass_model);
    let pass_cx = crate::passes::PassContext {
        provider_id: provider.id(),
        model: &pass_model,
        pricing: pass_pricing.as_ref(),
    };

    // Request-redaction guardrail (`RouteAction::redact`): when the matched
    // route opted in, strip PII/secrets from the OUTBOUND request (user prose,
    // system blocks, tool-result content) BEFORE deriving cache keys /
    // dispatching, so the secret never reaches the upstream provider AND the
    // cache stores the redacted form. This is a SAFETY transform, NOT a savings
    // feature: no cost/saving is attributed to it. When it fires we append a
    // `redacted:<class>` entry to the warnings header naming WHICH field class
    // was redacted (system / body / tool_result) — the matched secret VALUES are
    // never placed in any header or log. Off by default (`route_redact` is false
    // for every unrouted request and every route that did not enable it), so
    // `req` is byte-for-byte unchanged on the default path. Runs before
    // compression so secrets are removed first; the `[REDACTED]` placeholder is
    // inert to the compression trims.
    //
    // Redaction needs whole-request reach — a secret inside the cache-stable
    // prefix MUST still be stripped (safety beats cost) — so it is the
    // escape-hatch user, not a pipeline pass: the handler consumes the split
    // via `mutate_whole_request`. Redaction is DETERMINISTIC on the ingress
    // bytes (fixed regexes, fixed `[REDACTED]` placeholder), so the dispatched
    // prefix is byte-identical on every request/turn of a conversation and
    // the provider's exact-prefix cache keeps hitting: NO bust occurs and the
    // returned `CacheBustEstimate` is zero by construction (see
    // `MutationDeterminism::DeterministicOnIngress`). A future
    // NON-deterministic escape-hatch user (e.g. periodic history compaction)
    // gets a real estimate from the same call and the booking below fires.
    let mut cache_bust = crate::passes::CacheBustEstimate::NONE;
    if route_redact {
        // Boundary + prefix token estimate are computed BEFORE the mutation:
        // the warm prefix the provider cached is the pre-mutation bytes.
        let split = crate::passes::SplitRequest::compute(req, &pass_cx);
        let stable_len = split.stable().messages.len();
        let (req_mut, bust_token) = split.mutate_whole_request(
            "redaction-1",
            crate::passes::MutationDeterminism::DeterministicOnIngress,
        );
        let hits = crate::passes::RedactionPass::new().redact_indexed(req_mut);
        if hits.is_empty() {
            // Nothing fired — the prefix bytes are untouched, nothing to book.
            bust_token.discard_unused();
        } else {
            let mut classes: Vec<crate::passes::RedactedField> =
                hits.iter().map(|h| h.field).collect();
            classes.sort_unstable();
            classes.dedup();
            // Do NOT log the redacted values — only the field classes that fired.
            tracing::debug!(
                org_id = %ctx.org_id,
                redacted_classes = ?classes,
                "redaction guardrail stripped PII/secrets from the outbound request"
            );
            for field in classes {
                warnings.push(field.warning_token().to_string());
            }
            if bust_token.busted_prefix_tokens > 0
                && stable_len > 0
                && hits.iter().any(|h| h.msg_index < stable_len)
            {
                // A NON-deterministic mutation fired INSIDE the cache-stable
                // prefix: the dispatched bytes diverge from the warm prefix,
                // the provider's exact-prefix match misses, and the prefix
                // re-bills at the full input rate instead of ~0.1x. Book the
                // negative entry — never hide a cache bust. (Unreachable for
                // redaction, whose deterministic estimate is zero; kept live
                // so the next escape-hatch user books automatically.)
                warnings.push(format!("cache_bust:{}", bust_token.source));
                crate::metrics::record_cache_bust(
                    bust_token.source,
                    bust_token.penalty_usd(pass_cx.pricing),
                );
                cache_bust = bust_token;
            } else {
                // Deterministic mutation (no bust by construction), hits only
                // in the volatile tail, or nothing cache-qualified — nothing
                // to book.
                bust_token.discard_unused();
            }
        }
    }

    // Request-pass pipeline (compression pass #1): when the matched route opted
    // in (`RouteAction::compress`), run the conservative, content-lossless trim
    // of non-prose VOLATILE-TAIL blocks BEFORE deriving cache keys /
    // dispatching. Off by default — `route_compress` is false for every
    // unrouted request and every route that did not enable it, so `req` is
    // byte-for-byte unchanged on the default path. Running it here (against the
    // FINAL served provider's tokenizer) means the trimmed prompt is what gets
    // cached AND dispatched, so the upstream meters the reduced prompt-token
    // count.
    //
    // The split is recomputed post-redaction (redaction may have changed the
    // bytes the boundary estimate keys on). `PassPipeline::run` applies the
    // TOKEN-TRUE GATE per pass: a transform that net-ADDS tokens is discarded
    // (fail-open to the original bytes), metered, surfaced as
    // `pass_rejected:<name>`, and books zero; the returned `tokens_removed` is
    // the pipeline-MEASURED tokenizer delta of committed passes (never a
    // pass's self-report), which drives the `compression` savings attribution
    // in the cost path below. A future, more-aggressive pass would attach a
    // Wave-B2 judge gate inside `PassPipeline::run`.
    //
    // TR-3: snapshot the PRE-compression request (the "before" side of the
    // prompt diff) when a compress pass will run AND a body-capture writer is
    // armed (cheap sync check — the per-org opt-in probe still runs once
    // later at the capture_request_json block; if capture turns out off, the
    // bytes are simply unused). The serialize is paid ONLY on the
    // route_compress + writer-armed path — the default path (no compress, or
    // no writer) is byte-identical + zero-cost.
    let writer_armed = state.body_capture_writer.is_some();
    let pre_compression_request_json: Option<Vec<u8>> = if route_compress
        && writer_armed
        && ctx.org_id != Uuid::nil()
    {
        match serde_json::to_vec(&req) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!(error = %e, "pre-compression request snapshot serialization failed");
                None
            }
        }
    } else {
        None
    };
    let compression_tokens_removed: u32 = if route_compress {
        let out = {
            let mut split = crate::passes::SplitRequest::compute(req, &pass_cx);
            crate::passes::PassPipeline::conservative_compression().run(&mut split, &pass_cx)
        };
        for name in &out.rejected {
            warnings.push(format!("pass_rejected:{name}"));
        }
        warnings.extend(out.warnings);
        if out.tokens_removed > 0 {
            tracing::debug!(
                org_id = %ctx.org_id,
                tokens_removed = out.tokens_removed,
                "compression pass removed input tokens"
            );
        }
        out.tokens_removed
    } else {
        0
    };

    // Lossless document-compaction pass (`RouteAction::doc_compaction`,
    // Document Lane D2): OFF BY DEFAULT — `route_doc_compaction` is false for
    // every unrouted request and every route that did not enable it, so `req`
    // is byte-for-byte unchanged on the default path. Runs SEPARATELY from the
    // compression pipeline (its own `PassPipeline::doc_compaction()`) so the
    // two levers' measured token deltas attribute to distinct savings buckets.
    // Same token-true gate + cache-span invariant apply; the returned
    // `tokens_removed` is the pipeline-MEASURED tokenizer delta of the committed
    // pass, which drives the `doc_compaction` savings attribution in the cost
    // path below. Recomputes the split so it sees the (post-redaction,
    // post-compression) DISPATCHED bytes.
    let doc_compaction_tokens_removed: u32 = if route_doc_compaction {
        let out = {
            let mut split = crate::passes::SplitRequest::compute(req, &pass_cx);
            crate::passes::PassPipeline::doc_compaction().run(&mut split, &pass_cx)
        };
        for name in &out.rejected {
            warnings.push(format!("pass_rejected:{name}"));
        }
        warnings.extend(out.warnings);
        if out.tokens_removed > 0 {
            tracing::debug!(
                org_id = %ctx.org_id,
                tokens_removed = out.tokens_removed,
                "doc-compaction pass removed input tokens"
            );
        }
        out.tokens_removed
    } else {
        0
    };

    // Content-aware compression pass (`RouteAction::content_compress`): OFF BY
    // DEFAULT — `route_content_compress` is false for every unrouted request and
    // every route that did not enable it, so `req` is byte-for-byte unchanged on
    // the default path. For each LARGE System/Tool block the dispatcher
    // classifies the content kind and applies a backend: CONTENT-PRESERVING
    // structural compaction (JSON whitespace-minify, CSV trailing-padding trim,
    // log repeated-line collapse) for JSON/CSV/log; the P1b LOSSY prose
    // EXTRACTIVE backend for Prose; and the P1c LOSSY AST backend for Code
    // (truncate long function bodies, keep imports/signatures, re-parse-verify) —
    // the latter two committed only behind the shared judge gate
    // (`state.summary_gate`, default closed → verbatim). Diff is classified but
    // left untouched (no Phase-1 backend). Same token-true
    // gate + cache-span invariant as the other passes; the returned `tokens_removed` is the
    // pipeline-MEASURED tokenizer delta, which drives the ISOLATED
    // `content_compress_saved_est_usd` estimate (NOT the baseline fold) below.
    // The flywheel records the dominant compacted kind (metrics only; raw
    // before/after capture is opt-in + off by default — see
    // `content_compress::capture`).
    let content_compress_kind: Option<String> = if route_content_compress {
        crate::content_compress::dominant_compactable_kind(&req.messages)
            .map(|k| k.as_str().to_string())
    } else {
        None
    };
    let content_compress_tokens_removed: u32 = if route_content_compress {
        let out = {
            let mut split = crate::passes::SplitRequest::compute(req, &pass_cx);
            // P1b/P1c: the LOSSY prose + AST-code backends ride the SAME judge
            // gate as the summarize lever (`state.summary_gate`, default
            // `NeverCommitGate`). Prose compresses only when the `"prose"` class
            // is judge-trusted, Code only when the `"code"` class is (independent
            // classes + 0.90-floor ratchets); otherwise each fails open to
            // verbatim (Code additionally re-parse-verifies — never serve broken
            // code). The structural JSON/CSV/log backends are unaffected
            // (content-preserving, no gate).
            //
            // P1d: a CaptureCtx is threaded so each compacted block's
            // before/after pair is recorded (the Phase-2 training flywheel).
            // record_pair is a NO-OP unless the instance opted in
            // (TT_COMPRESS_CAPTURE + TT_COMPRESS_CAPTURE_PATH), so this is
            // observability-only — never changes the dispatched bytes. The
            // per-block record_pair supersedes the old per-request
            // capture::record (the kind + tokens are both in the per-block
            // record).
            let capture = std::sync::Arc::new(crate::content_compress::CaptureCtx {
                org_id: ctx.org_id.to_string(),
                trace_id: ctx.trace_id.to_string(),
                model: pass_cx.model.to_string(),
                provider_id: pass_cx.provider_id.to_string(),
            });
            crate::passes::PassPipeline::content_compress_with_gates_and_capture(
                state.summary_gate.clone(),
                capture,
            )
            .run(&mut split, &pass_cx)
        };
        for name in &out.rejected {
            warnings.push(format!("pass_rejected:{name}"));
        }
        warnings.extend(out.warnings);
        if out.tokens_removed > 0 {
            tracing::debug!(
                org_id = %ctx.org_id,
                tokens_removed = out.tokens_removed,
                kind = content_compress_kind.as_deref().unwrap_or("none"),
                "content-compress pass removed input tokens"
            );
        }
        out.tokens_removed
    } else {
        0
    };

    // Agentic context budget (`RouteAction::agentic_budget`): OFF BY DEFAULT —
    // `route_agentic_budget` is `None` for every unrouted request (and every
    // route that did not opt in), so the planner is never constructed and `req`
    // is byte-identical on the default path. When a route opted in, the planner
    // nets the cache-prefix / field-drop / summarize / route levers per request
    // (they do NOT stack at face value — see `passes::agentic_budget`). It runs
    // AFTER redaction (`req` is already redacted above) and AFTER compression,
    // so every artifact it produces (any field-drop / summary) is built from the
    // POST-redaction, post-compression DISPATCHED bytes — never a pre-pipeline
    // clone (R5: the measurement path must never out-leak the dispatch path).
    // The planner returns the honest three-bucket accounting (field-drop /
    // summary tokens, summarizer tax, cache-bust penalty) plus any diagnostic
    // warning tokens (`cache_bust:<source>`, the cache-isolated subagent lane).
    let agentic_effects = if let Some(ab) = &route_agentic_budget {
        let out = {
            let mut split = crate::passes::SplitRequest::compute(req, &pass_cx);
            crate::passes::agentic_budget::AgenticBudgetPlanner::plan(ab, &mut split, &pass_cx)
        };
        warnings.extend(out.warnings);
        if out.effects.elide_field_drop_tokens_removed > 0
            || out.effects.elide_summary_tokens_removed > 0
        {
            tracing::debug!(
                org_id = %ctx.org_id,
                field_drop_tokens = out.effects.elide_field_drop_tokens_removed,
                summary_tokens = out.effects.elide_summary_tokens_removed,
                "agentic-budget planner removed input tokens"
            );
        }
        out.effects
    } else {
        crate::passes::PassEffects::default()
    };

    // Stable/volatile cache classifier — ALWAYS ON (observability-only, no
    // semantic change, so default-on is allowed): flags volatile markers
    // (timestamp / uuid / hex token) inside a would-be-stable cached prefix
    // via `cache_dynamic_prefix:<kind>` warning tokens + metrics, quantifying
    // the estimated per-request waste of the busted provider cache. Read-only;
    // it never injects `cache_control` (adapter-owned per #126/#150). Runs after
    // the agentic planner so it classifies the FINAL dispatched bytes.
    warnings.extend(crate::passes::CacheClassifierPass::classify(req, &pass_cx));

    // Aggregated pass effects for the cost path (threaded into both the
    // non-streaming and streaming `compute_cost_full` calls): the measured
    // compression delta, the (pre-fee) cache-bust penalty booked above, and the
    // agentic-budget planner's three honest buckets (field-drop / summary
    // tokens, summarizer tax, additional cache-bust). All buckets are zero on
    // the default (un-opted) path, so it remains byte-identical and books no new
    // spend.
    let pass_effects = crate::passes::PassEffects {
        compression_tokens_removed,
        doc_compaction_tokens_removed,
        content_compress_tokens_removed,
        cache_bust_penalty_usd: cache_bust.penalty_usd(pass_cx.pricing)
            + agentic_effects.cache_bust_penalty_usd,
        elide_field_drop_tokens_removed: agentic_effects.elide_field_drop_tokens_removed,
        elide_summary_tokens_removed: agentic_effects.elide_summary_tokens_removed,
        summarizer_tax_usd: agentic_effects.summarizer_tax_usd,
    };

    // For a failover chain, pre-resolve upstream credentials for every distinct
    // candidate provider that is not already known to exceed a request ceiling.
    // The ordered model list stays intact so the dispatcher can record the
    // precise cost rejection, but an over-ceiling cross-provider fallback must
    // not trigger a needless credential-store lookup. The raw-Bearer fallback
    // is allowed only for the source provider (the bearer is its key);
    // cross-provider candidates with no stored credential are skipped during
    // dispatch.
    let (failover_candidates, failover_creds): (
        Vec<String>,
        std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) = if route_fallbacks.is_empty() {
        (Vec::new(), std::collections::HashMap::new())
    } else {
        let candidates: Vec<String> = std::iter::once(req.model.clone())
            .chain(route_fallbacks.iter().cloned())
            .collect();
        // Distinct admitted candidate providers, first-seen order — resolve each
        // credential once. Candidates that violate a ceiling or have unknown
        // pricing remain in `candidates` for dispatch-time diagnostics, but are
        // deliberately omitted here so their credentials are never read.
        let mut provider_ids: Vec<String> = Vec::new();
        for m in &candidates {
            if let Some(p) = state.registry.resolve(m) {
                if failover_cost_check.is_some_and(|cost_check| {
                    !matches!(
                        cost_check.admission_for(p.as_ref(), m, req),
                        crate::failover::CandidateCostAdmission::Allowed
                    )
                }) {
                    continue;
                }
                let pid = p.id().to_string();
                if !provider_ids.contains(&pid) {
                    provider_ids.push(pid);
                }
            }
        }
        let mut map = std::collections::HashMap::new();
        let had_resolvable_candidates = !provider_ids.is_empty();
        for pid in provider_ids {
            let allow_bearer = pid == source_provider_id;
            if let Some(c) =
                resolve_credentials_for(state, org_id, &pid, &raw_bearer, allow_bearer).await
            {
                map.insert(pid, c);
            }
        }
        // BYO-only (P0 #9): when NO candidate provider has a resolvable
        // credential the dispatch loop would skip every candidate and return
        // an opaque 503 — surface the actionable missing-credential error for
        // the primary candidate's provider instead. (Candidates whose models
        // are unknown to the registry keep today's 503.)
        if had_resolvable_candidates && map.is_empty() {
            return Err(ApiError::MissingProviderCredential {
                provider: provider.id().to_string(),
            });
        }
        (candidates, map)
    };

    // BYO-only (P0 #9): single-provider dispatch where the serving provider is
    // still the source provider and the verified org has no stored credential
    // for it → actionable 400 BEFORE cache lookup and dispatch, instead of
    // forwarding the org's TokenTrimmer key upstream (a confusing upstream
    // 401). The other dispatch shapes are already covered: cross-provider
    // rewrites and pins re-resolve + fail closed above, failover chains skip
    // per-candidate (or error above when nothing resolves), and anonymous /
    // no-store callers never set `source_creds_missing`.
    if source_creds_missing
        && route_fallbacks.is_empty()
        && provider_pin.is_none()
        && provider.id() == source_provider_id
    {
        return Err(ApiError::MissingProviderCredential {
            provider: source_provider_id.clone(),
        });
    }

    // 2d. Determine cache behaviour for this request (Fix A §2.2 + Fix B §2.7).
    //     Resolved once here so all four call-sites (streaming L1 read,
    //     non-streaming L1 read, L2 read, L1/L2 insert) share a single decision.
    let mut cache_behavior = CacheBehavior::resolve(req);
    // `X-TokenTrimmer-Cache` overrides the request-body decision (header beats
    // body). force-write=(true,true) here overrides the eligibility gate that
    // `resolve()` may have applied; the tool-call exclusion at insert time is
    // unaffected, so tool-call responses are still never cached.
    if let Some((lookup, insert)) = cache_override_from_header(headers)? {
        cache_behavior.do_lookup = lookup;
        cache_behavior.do_insert = insert;
    }
    // A privacy route's disable_cache wins over both body and header.
    if route_disable_cache {
        cache_behavior.do_lookup = false;
        cache_behavior.do_insert = false;
    }
    // Diff requests skip the cache ENTIRELY (correctness over cache):
    // `cache_key` ignores `tt_extras`, so a request carrying a tt_extras
    // prior would share an L1 key with a different-prior request, and the
    // reconstructed body depends on the prior.
    if diff_plan.is_some() {
        cache_behavior.do_lookup = false;
        cache_behavior.do_insert = false;
    }
    // Per-org `semantic_cache_disabled` compliance control (a hard deal-blocker
    // for no-cache tenants): when the org has explicitly opted OUT of caching,
    // force BOTH lookup and insert off for this request. Resolved via the tier
    // resolver so it rides the same per-org `CachedTierResolver` cache the auth
    // middleware already populated earlier in this request (cache hit within the
    // 30s TTL → no extra DB round-trip on the hot path). The request remains
    // available on resolver errors, but `resolve_or_free` yields
    // `semantic_cache_disabled = true`: an unknown privacy value fails closed
    // for caching and can never silently re-enable storage for an opted-out
    // org. A successful absent/false read preserves existing cache behaviour.
    if let Some(resolver) = state.tier_resolver.as_ref() {
        let org_cache_disabled = crate::tier_resolver::resolve_or_free(resolver.as_ref(), org_id)
            .await
            .semantic_cache_disabled;
        cache_behavior.apply_org_cache_disabled(org_cache_disabled);
    }
    // Format-switch keeps L1 (the mutated request — instruction + no
    // response_format — hashes to its own exact, per-org key) but disables L2
    // for switched requests (lookup AND insert): similarity matching could
    // cross the instruction boundary and serve a verbose JSON answer under a
    // `format_switch` advertisement, or vice versa.
    let skip_l2 = format_switch_plan.is_some();

    // Body capture is gated on THREE conditions, all checked here so the
    // `x-tokentrimmer-captured` header is honest (present ONLY when a body is
    // truly persisted): (1) a writer is armed for the deployment, (2) the
    // caller resolved to a non-anonymous org, and (3) THAT org has opted in
    // via `request_body_capture_settings.enabled`. The opt-in is per-org while
    // a single `TT_MASTER_KEY` arms the writer for the whole deployment, so
    // without (3) every resolved org's response would falsely advertise
    // capture. The async opt-in probe runs at most once per request and only
    // when a writer is armed, so the default (capture-off) path is unaffected.
    let capture_enabled = match state.body_capture_writer.as_ref() {
        Some(writer) if ctx.org_id != Uuid::nil() => writer.is_capture_enabled(ctx.org_id).await,
        _ => false,
    };
    let capture_request_json = if capture_enabled {
        match serde_json::to_vec(&req) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!(error = %e, "request body capture serialization failed");
                None
            }
        }
    } else {
        None
    };

    // Bundle the shared setup. `req` is moved out (the caller's `&mut req` is
    // left empty — the handler/loop branches on `prep.req.stream`, never the
    // now-emptied caller local); `provider` is the final served provider.
    Ok(Prepared {
        provider,
        req: std::mem::take(req),
        cache_behavior,
        l2_allowed,
        skip_l2,
        route_matched_name,
        matched_route_id,
        matched_route_version_id,
        route_paused,
        requested_model,
        requested_pricing,
        model_was_rewritten,
        format_switch_plan,
        diff_plan,
        pass_effects,
        doc_distill_booking,
        content_compress_kind,
        minify_applied,
        reasoning_capped,
        flex_applied,
        batch_marked,
        caller_tier,
        skip_shadow,
        traffic_split_arm_owned,
        route_traffic_pct,
        route_shadow_model,
        failover_candidates,
        failover_creds,
        failover_cost_check,
        route_fallbacks,
        warnings,
        request_timeout,
        raw_bearer,
        retrieval_telemetry,
        request_started,
        capture_request_json,
        judge_source_provider,
        judge_source_ctx,
        judge_original_req,
        pre_compression_request_json,
        panel,
        panel_admission,
        panel_creds,
        workflow: route_workflow,
    })
}
