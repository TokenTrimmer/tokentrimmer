//! Compatibility translation, output shaping, and response warning headers.

use super::*;
/// The non-direct response owner that makes a route format switch inapplicable.
///
/// A Fusion panel and a workflow detour bypass the direct completion tail where
/// a switch is validated, advertised, and assigned its isolated estimate. The
/// request must therefore stay in its caller-requested structured form before
/// either owner fans it out or consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FormatSwitchResponseOwner {
    FusionPanel,
    WorkflowDetour,
}

impl FormatSwitchResponseOwner {
    const fn skip_reason(self) -> &'static str {
        match self {
            Self::FusionPanel => "panel",
            Self::WorkflowDetour => "workflow",
        }
    }
}

/// Result of deciding and, only when safe, applying a route format switch.
///
/// Keeping mutation in this one helper makes the composition contract
/// testable: every skipped outcome leaves the request byte-identical and has
/// no plan that a response-owning branch could later advertise or book.
pub(super) enum RouteFormatSwitchPreparation {
    NotRequested,
    Applied(crate::shaping::format_switch::FormatSwitchPlan),
    Skipped(&'static str),
}

/// Apply an eligible route format switch, unless another response owner takes
/// the request first.
///
/// Streaming intentionally bypasses the owner guard so the established
/// `format_switch_skipped:streaming` planner token remains the first reason.
/// Likewise, a shadow workflow is passed as no owner: its direct response
/// still follows the normal format-switch contract.
pub(super) fn prepare_route_format_switch(
    req: &mut ChatCompletionRequest,
    route_format_switch: Option<&str>,
    response_owner: Option<FormatSwitchResponseOwner>,
    diff_applies: bool,
) -> RouteFormatSwitchPreparation {
    let Some(requested) = route_format_switch else {
        return RouteFormatSwitchPreparation::NotRequested;
    };

    if !req.stream {
        if let Some(owner) = response_owner {
            return RouteFormatSwitchPreparation::Skipped(owner.skip_reason());
        }
    }
    if diff_applies {
        return RouteFormatSwitchPreparation::Skipped("conflict");
    }

    match crate::shaping::format_switch::plan_format_switch(req, Some(requested)) {
        Some(crate::shaping::ShapeDecision::Apply(plan)) => {
            crate::shaping::format_switch::apply_format_switch_request(req, &plan);
            RouteFormatSwitchPreparation::Applied(plan)
        }
        Some(crate::shaping::ShapeDecision::Skip(reason)) => {
            RouteFormatSwitchPreparation::Skipped(reason)
        }
        // `requested` is Some above, so the planner cannot return None. Keep
        // the default explicitly fail-open if that invariant ever changes.
        None => RouteFormatSwitchPreparation::NotRequested,
    }
}

/// The response owner that bypasses the direct-completion tail for request
/// transformations that rely on that tail to reconstruct or account for their
/// result.
///
/// A selected Fusion panel owns both streaming and non-streaming responses.
/// A workflow only owns a non-streaming response: streaming workflow detours
/// deliberately fall through to the direct streaming path. `skip_shadow`
/// likewise deliberately drops either detour and preserves the direct path.
pub(super) fn non_direct_response_owner(
    panel_selected: bool,
    workflow: Option<&tt_routing::RouteWorkflow>,
    request_stream: bool,
    skip_shadow: bool,
) -> Option<&'static str> {
    if skip_shadow {
        return None;
    }
    if panel_selected {
        return Some("panel");
    }
    if !request_stream && workflow.is_some_and(|cfg| cfg.mode.as_deref() != Some("shadow")) {
        return Some("workflow");
    }
    None
}

/// Restore a raw-media request to the caller-selected dispatch path when the
/// optional Document Lane transaction cannot safely support the route rewrite.
///
/// The route remains attributed, but no target-model rewrite, route/header
/// fallback, or shadow dispatch may carry an unconverted document/image into a
/// text-oriented path. Keeping this in one helper makes the rollback complete
/// and preserves the original request bytes except for later independent safety
/// transforms such as redaction.
pub(super) fn rollback_document_lane_route_rewrite(
    req: &mut ChatCompletionRequest,
    requested_model: &str,
    model_was_rewritten: &mut bool,
    route_fallbacks: &mut Vec<String>,
    route_shadow_model: &mut Option<String>,
) {
    req.model = requested_model.to_string();
    *model_was_rewritten = false;
    route_fallbacks.clear();
    *route_shadow_model = None;
}

/// Result of deciding and, only when safe, applying a route diff.
///
/// Keeping the raw mutation inside this owner-aware helper prevents a panel or
/// workflow detour from inheriting the patch-only prompt / dropped response
/// contract that only the direct completion tail knows how to reconstruct.
pub(super) enum RouteDiffPreparation {
    NotRequested,
    Applied(crate::shaping::diff::DiffPlan),
    Skipped(&'static str),
}

/// Apply an eligible route diff unless another response owner takes the
/// request first.
///
/// The planner retains precedence for streaming so existing
/// `diff_skipped:streaming` observability remains unchanged. A shadow workflow
/// is passed as no owner because its caller-visible response is still direct.
pub(super) fn prepare_route_diff(
    req: &mut ChatCompletionRequest,
    requested: bool,
    response_owner: Option<&'static str>,
) -> RouteDiffPreparation {
    if !requested {
        return RouteDiffPreparation::NotRequested;
    }
    if !req.stream {
        if let Some(owner) = response_owner {
            return RouteDiffPreparation::Skipped(owner);
        }
    }

    match crate::shaping::diff::plan_diff(req, true) {
        Some(crate::shaping::ShapeDecision::Apply(plan)) => {
            // No pre-mutation clone is kept: the fail-closed re-emit is
            // derived from the DISPATCHED request at the failure site
            // (`unapply_diff_request` — drop the instruction, restore the
            // plan's response_format) so it inherits every dispatch-path
            // normalization. A pre-pipeline clone would bypass the redaction
            // guardrail on a redact+diff route and dispatch un-flexed bytes
            // that `compute_cost_full` prices at flex rates.
            crate::shaping::diff::apply_diff_request(req, &plan);
            RouteDiffPreparation::Applied(plan)
        }
        Some(crate::shaping::ShapeDecision::Skip(reason)) => RouteDiffPreparation::Skipped(reason),
        // `requested` is true above, so the planner cannot return None. Keep
        // the default explicitly fail-open if that invariant ever changes.
        None => RouteDiffPreparation::NotRequested,
    }
}

/// The process-wide model-alias canonicalizer used for cache-key derivation,
/// built once from the operator-curated `model_aliases.toml`. The map is EMPTY
/// by default, so this is byte-for-byte identical to the no-op key derivation
/// until an asserted-identical snapshot→alias pair is configured — at which point
/// a dated snapshot and its floating alias share one L1/L2 cache entry instead of
/// fragmenting (a pure hit-rate win; see the correctness contract in the TOML).
pub(super) fn alias_canonicalizer() -> &'static AliasMapCanonicalizer {
    static CANON: std::sync::OnceLock<AliasMapCanonicalizer> = std::sync::OnceLock::new();
    CANON.get_or_init(|| {
        AliasMapCanonicalizer::new(tt_shared::model_aliases::model_aliases().clone())
    })
}

/// Principal-scoped L1 cache key. Verified callers retain the per-org namespace.
/// DB-less BYOK callers have no organization, so use a one-way digest of their
/// provider bearer instead of the shared nil UUID. This preserves useful local
/// exact caching without allowing two anonymous credentials to read or poison
/// each other's entries. The plaintext credential never enters Redis.
pub(super) fn namespaced_l1_key(ctx: &RequestContext, req: &ChatCompletionRequest) -> String {
    let namespace = if ctx.org_id == Uuid::nil() {
        format!(
            "byok:{}",
            blake3::hash(ctx.credentials.api_key.expose().as_bytes()).to_hex()
        )
    } else {
        ctx.org_id.to_string()
    };
    format!("{namespace}:{}", cache_key_with(req, alias_canonicalizer()))
}

/// If `req` asks for `response_format: json_schema` but the routed provider
/// supports only `json_object`, rewrite it to `json_object` (dropping the
/// schema) and record a `response_format_downgrade` warning. Providers that
/// drop `response_format` outright (Anthropic) are left to B1's param_dropped.
pub(super) fn maybe_downgrade_response_format(
    req: &mut ChatCompletionRequest,
    provider: &dyn tt_shared::Provider,
    warnings: &mut Vec<String>,
) {
    let is_schema = req
        .response_format
        .as_ref()
        .is_some_and(|rf| rf.r#type == "json_schema");
    if !is_schema || provider.supports_response_schema() {
        return;
    }
    if provider
        .dropped_params(req)
        .iter()
        .any(|p| p == "response_format")
    {
        return;
    }
    req.response_format = Some(tt_shared::messages::ResponseFormat {
        r#type: "json_object".to_string(),
        json_schema: None,
    });
    warnings.push("response_format_downgrade".to_string());
}

/// Clamp `req.temperature` to the routed provider's accepted range, recording a
/// `temperature_clamped` warning when the value actually changed. Skips a
/// temperature that the provider drops outright (reasoning models — B1
/// param_dropped) so the two warnings don't both fire.
pub(super) fn maybe_clamp_temperature(
    req: &mut ChatCompletionRequest,
    provider: &dyn tt_shared::Provider,
    warnings: &mut Vec<String>,
) {
    let Some(t) = req.temperature else {
        return;
    };
    if provider
        .dropped_params(req)
        .iter()
        .any(|p| p == "temperature")
    {
        return;
    }
    let (lo, hi) = provider.temperature_range();
    // Detect out-of-range directly (no float-equality): catches every overshoot,
    // including a single-ulp one, and leaves a NaN untouched.
    if t < lo || t > hi {
        req.temperature = Some(t.clamp(lo, hi));
        warnings.push("temperature_clamped".to_string());
    }
}

/// Apply the Flex route action: when `requested` is true, set
/// `service_tier="flex"` on the upstream request — but ONLY for a flex-eligible
/// served model. Eligibility is catalog-driven
/// ([`ModelPricing::flex_eligible`]): OpenAI lists Flex prices only for
/// supported models (gpt-5.x); o3/o4-mini and every non-OpenAI model carry no
/// Flex rate and are ineligible. For an ineligible model the request is left
/// untouched and a `flex_not_applied:<model>` warning is surfaced via the
/// existing warnings mechanism.
///
/// `service_tier` rides through to the upstream via the request's serde-flatten
/// `extra` map (it is not a typed field) — see `tt_shared::messages`. Returns
/// whether flex was actually applied, so the cost path can attribute the
/// standard-vs-flex saving to the `flex` source.
pub(super) fn maybe_apply_flex(
    req: &mut ChatCompletionRequest,
    requested: bool,
    provider: &dyn tt_shared::Provider,
    warnings: &mut Vec<String>,
) -> bool {
    if !requested {
        return false;
    }
    let eligible = provider
        .pricing(&req.model)
        .is_some_and(|p| p.flex_eligible());
    if !eligible {
        // Do NOT set service_tier on an ineligible model — sending flex to a
        // model that does not support it risks a provider rejection, and it
        // would never get the discount. Surface the no-op as a warning.
        warnings.push(format!("flex_not_applied:{}", req.model));
        return false;
    }
    req.extra
        .insert("service_tier".to_string(), serde_json::json!("flex"));
    true
}

/// Advisory batch-eligibility gate (`RouteAction::batch`, research Phase 2.1).
/// NEVER mutates `req` (the gateway is synchronous — there is no batch
/// dispatch to opt into today, unlike flex's `service_tier` injection).
/// Returns whether the request is marked batch-eligible for telemetry /
/// forgone-savings attribution; a marked request still dispatches and bills
/// normally, signalled by the `batch_deferred_unavailable` warning.
///
/// Hard ineligibility (enforced in code, not docs): streaming requests and
/// interactive clients (`X-TokenTrimmer-Interactive`, set by `tt chat` and the
/// /tools loop) are cleared with a `batch_ineligible:<reason>` warning — the
/// provider Batch APIs run on a ≤24h window, which breaks any interactive UX.
/// A served model with no catalog batch tier is not marked
/// (`batch_not_available:<model>`) — no real rate, no claim. Check order is
/// fixed: streaming > interactive > rate. Fail-open by construction: only
/// pushes warnings and returns a bool — a marked request can never 5xx.
pub(super) fn maybe_mark_batch_eligible(
    req: &ChatCompletionRequest,
    requested: bool,
    interactive_client: bool,
    provider: &dyn tt_shared::Provider,
    warnings: &mut Vec<String>,
) -> bool {
    if !requested {
        return false;
    }
    if req.stream {
        warnings.push("batch_ineligible:streaming".into());
        return false;
    }
    if interactive_client {
        warnings.push("batch_ineligible:interactive".into());
        return false;
    }
    let eligible = provider
        .pricing(&req.model)
        .is_some_and(|p| p.batch_eligible());
    if !eligible {
        warnings.push(format!("batch_not_available:{}", req.model));
        return false;
    }
    warnings.push("batch_deferred_unavailable".into());
    true
}

/// Deterministic minified-JSON instruction suffix (`RouteAction::minify_json`,
/// research Phase 3.1). A compile-time constant => the injection is
/// DETERMINISTIC ON INGRESS (same route config + same request bytes -> same
/// dispatched bytes), so per the redaction precedent it can never bust a
/// provider prompt cache and books no `CacheBustEstimate`. Conditionally
/// phrased ("When responding with JSON") so it is inert for non-JSON answers —
/// the booking below is additionally gated on the response actually parsing
/// as JSON.
///
/// EDITING THIS TEXT: `maybe_cap_reasoning` classifies the request AFTER this
/// suffix is injected, so the instruction must never contain a
/// `crate::reasoning_class` keyword (e.g. "function", "theorem") or every
/// minify+cap request would silently class-gate. Re-check the keyword tables
/// when rewording.
pub(crate) const MINIFY_JSON_INSTRUCTION: &str =
    "\n\nWhen responding with JSON, emit it minified: no indentation, no newlines, and no spaces between JSON tokens.";

/// Apply the minify route action only when the direct completion path owns the
/// caller-visible response.
///
/// A Fusion panel fans out the prepared request (including a header-selected
/// panel), while a non-shadow workflow consumes it for its own result. Neither
/// path has the direct tail's minify validation or accounting contract, so a
/// requested action becomes an explicit no-op rather than silently steering
/// those internal prompts. Streaming panels are still response owners; a
/// streaming workflow is not, because that detour falls through to direct
/// streaming. `response_owner` encodes those distinctions.
pub(super) fn prepare_route_minify_json(
    req: &mut ChatCompletionRequest,
    requested: bool,
    provider: &dyn tt_shared::Provider,
    response_owner: Option<&'static str>,
    warnings: &mut Vec<String>,
) -> bool {
    if requested {
        if let Some(owner) = response_owner {
            warnings.push(format!("minify_skipped:{owner}"));
            return false;
        }
    }
    maybe_minify_json(req, requested, provider, warnings)
}

/// Apply the minify-JSON route action: append [`MINIFY_JSON_INSTRUCTION`] to
/// the LAST system message (inserting one at index 0 when the request has
/// none) and push the `output_minified` warnings token. Returns whether the
/// instruction was injected — drives the per-response estimate, the metric,
/// and (via `output_shaped`) judge eligibility.
///
/// Grammar-lock guard: when the request carries `response_format: json_schema`
/// AND the served provider honors it natively (strict structured output — the
/// provider already controls whitespace via the grammar) the instruction is a
/// no-op upstream, so we skip with `minify_skipped:structured_output` and make
/// NO claim. Evaluated AFTER `maybe_downgrade_response_format` so the check
/// sees the FINAL response_format: a schema B2 downgraded to `json_object`,
/// or one the provider drops outright (Anthropic), is NOT grammar-locked and
/// still benefits from the instruction.
///
/// Why no request-side `response_format` requirement: the route opt-in IS the
/// machine-consumer assertion, the instruction is conditionally phrased, and
/// booking is gated on the response actually parsing as JSON — a non-JSON
/// answer is unaffected and books $0.
///
/// Fail-open by construction: only mutates the system text / pushes warnings;
/// can never error.
pub(super) fn maybe_minify_json(
    req: &mut ChatCompletionRequest,
    requested: bool,
    provider: &dyn tt_shared::Provider,
    warnings: &mut Vec<String>,
) -> bool {
    if !requested {
        return false;
    }
    let is_schema = req
        .response_format
        .as_ref()
        .is_some_and(|rf| rf.r#type == "json_schema");
    if is_schema
        && provider.supports_response_schema()
        && !provider
            .dropped_params(req)
            .iter()
            .any(|p| p == "response_format")
    {
        warnings.push("minify_skipped:structured_output".to_string());
        return false;
    }
    let last_system = req
        .messages
        .iter_mut()
        .rev()
        .find(|m| matches!(m, Message::System { .. }));
    match last_system {
        Some(Message::System { content }) => match content {
            MessageContent::Text(t) => t.push_str(MINIFY_JSON_INSTRUCTION),
            MessageContent::Parts(parts) => {
                parts.push(tt_shared::messages::ContentPart::Text {
                    text: MINIFY_JSON_INSTRUCTION.to_string(),
                });
            }
        },
        _ => {
            req.messages.insert(
                0,
                Message::System {
                    content: MessageContent::Text(MINIFY_JSON_INSTRUCTION.trim_start().to_string()),
                },
            );
        }
    }
    warnings.push("output_minified".to_string());
    true
}

/// Apply the class-gated reasoning-token cap (`RouteAction::reasoning_max_effort`
/// / `reasoning_budget_tokens`, research Phase 3.2). Returns whether a cap was
/// actually applied — drives metering and (via `output_shaped`) judge
/// eligibility. Fail-open: mutates ONLY the reasoning params / warnings; never
/// errors; NEVER touches `max_tokens` / `max_completion_tokens` (Anthropic's
/// max_tokens INCLUDES thinking — bounding it would truncate the answer).
///
/// Decision order (first refusal wins; every act/refusal pushes its warnings
/// token and meters):
/// 1. Both caps `None` → silent no-op (the off-by-default path).
/// 2. HARD class gate: a request classified math/code/legal/medical
///    (`crate::reasoning_class`) is NEVER capped — capping where reasoning IS
///    the work yields confidently-wrong answers
///    (`reasoning_cap_skipped:class:<c>`).
/// 3. Effort arm (OpenAI-style `reasoning_effort`, only when the served
///    surface carries the lever — i.e. the provider does not drop it):
///    lower-only on the `minimal < low < medium < high` ladder. An absent
///    effort on a catalog-Reasoning-capable model is treated as the provider
///    default ("medium" — documented assumption) and lowered when the cap is
///    "low"; an absent effort on an unknown/non-Reasoning model refuses
///    (`reasoning_cap_skipped:not_reasoning:<model>`) — never inject
///    `reasoning_effort` into a model that may reject it. An unrecognized
///    requester value refuses (`reasoning_cap_skipped:unknown_effort:<v>`).
/// 4. Thinking arm (Anthropic-style `extra["thinking"]`): an ENABLED config
///    whose `budget_tokens` exceeds the cap is lowered in place
///    (`reasoning_capped:thinking_budget:<cap>`). Absent / disabled /
///    at-or-below configs are untouched — the cap NEVER enables thinking.
/// 5. When neither lever exists for this request (effort dropped by the
///    provider AND no enabled thinking config) while a cap is configured, one
///    honest `reasoning_cap_skipped:unsupported:<provider>` token is pushed.
///    Known corner: a route configuring ONLY `reasoning_budget_tokens`, hit
///    by a request on an effort-capable surface with no thinking config,
///    no-ops silently — the surface DOES carry a lever (so `unsupported`
///    would be a lie), there was just nothing for the configured cap to
///    lower. Zero cost impact ($0 is booked regardless).
///
/// Books $0 ALWAYS: `Usage` carries no reasoning-token field, so the unspent
/// thinking tokens are only statistically visible — the event is metered
/// (`reasoning_capped_total{route,lever,cap}`) and the judge-tax-netted route
/// savings (#163) tell the truth over the window.
pub(super) fn maybe_cap_reasoning(
    req: &mut ChatCompletionRequest,
    max_effort: Option<&str>,
    budget_tokens: Option<u32>,
    provider: &dyn tt_shared::Provider,
    model_info: Option<&tt_shared::ModelInfo>,
    route_name: &str,
    warnings: &mut Vec<String>,
) -> bool {
    if max_effort.is_none() && budget_tokens.is_none() {
        return false;
    }
    // HARD class gate, first: refuse where reasoning IS the work.
    let text = tt_shared::message_text_for_estimation(req).to_lowercase();
    if let Some(class) = crate::reasoning_class::classify(&text) {
        warnings.push(format!("reasoning_cap_skipped:class:{}", class.as_str()));
        crate::metrics::record_reasoning_cap_skipped("class");
        return false;
    }

    /// `minimal(0) < low(1) < medium(2) < high(3)`; `None` = unrecognized.
    fn effort_rank(e: &str) -> Option<u8> {
        match e {
            "minimal" => Some(0),
            "low" => Some(1),
            "medium" => Some(2),
            "high" => Some(3),
            _ => None,
        }
    }

    let mut applied = false;
    let effort_lever = !provider
        .dropped_params(req)
        .iter()
        .any(|p| p == "reasoning_effort");

    // Effort arm.
    if let (Some(cap), true) = (max_effort, effort_lever) {
        match effort_rank(cap) {
            None => {
                // Defensive only: validation rejects this at route-create
                // time, but route JSON is schemaless JSONB — never act on a
                // cap we cannot rank.
                tracing::warn!(cap, "unrecognized reasoning_max_effort cap — skipping");
            }
            Some(cap_rank) => match req.reasoning_effort.as_deref() {
                Some(current) => match effort_rank(current) {
                    Some(cur_rank) if cur_rank > cap_rank => {
                        req.reasoning_effort = Some(cap.to_string());
                        warnings.push(format!("reasoning_capped:reasoning_effort:{cap}"));
                        crate::metrics::record_reasoning_capped(
                            route_name,
                            "reasoning_effort",
                            cap,
                        );
                        applied = true;
                    }
                    Some(_) => {} // at-or-below the cap: silent no-op.
                    None => {
                        let current = current.to_string();
                        warnings.push(format!("reasoning_cap_skipped:unknown_effort:{current}"));
                        crate::metrics::record_reasoning_cap_skipped("unknown_effort");
                    }
                },
                None => {
                    let reasoning_capable = model_info.is_some_and(|i| {
                        i.capabilities
                            .contains(&tt_shared::pricing::Capability::Reasoning)
                    });
                    if reasoning_capable {
                        // Documented assumption: the provider default for an
                        // absent effort on a reasoning model is "medium".
                        if 2 > cap_rank {
                            req.reasoning_effort = Some(cap.to_string());
                            warnings.push(format!("reasoning_capped:reasoning_effort:{cap}"));
                            crate::metrics::record_reasoning_capped(
                                route_name,
                                "reasoning_effort",
                                cap,
                            );
                            applied = true;
                        }
                    } else {
                        warnings.push(format!("reasoning_cap_skipped:not_reasoning:{}", req.model));
                        crate::metrics::record_reasoning_cap_skipped("not_reasoning");
                    }
                }
            },
        }
    }

    // Thinking arm. A lever exists only for an ENABLED config — the cap never
    // enables thinking on a request that didn't ask for it.
    let thinking_enabled = req
        .extra
        .get("thinking")
        .and_then(|v| v.as_object())
        .is_some_and(|o| o.get("type").and_then(|t| t.as_str()) == Some("enabled"));
    if let (Some(cap), true) = (budget_tokens, thinking_enabled) {
        if let Some(obj) = req
            .extra
            .get_mut("thinking")
            .and_then(|v| v.as_object_mut())
        {
            if let Some(budget) = obj.get("budget_tokens").and_then(serde_json::Value::as_u64) {
                if budget > u64::from(cap) {
                    obj.insert("budget_tokens".to_string(), serde_json::json!(cap));
                    warnings.push(format!("reasoning_capped:thinking_budget:{cap}"));
                    crate::metrics::record_reasoning_capped(
                        route_name,
                        "thinking_budget",
                        &cap.to_string(),
                    );
                    applied = true;
                }
            }
        }
    }

    // Honest unsupported-surface token: a cap is configured but this request
    // carries NO lever (effort dropped by the provider AND no enabled
    // thinking config).
    if !applied && !effort_lever && !thinking_enabled {
        warnings.push(format!(
            "reasoning_cap_skipped:unsupported:{}",
            provider.id()
        ));
        crate::metrics::record_reasoning_cap_skipped("unsupported");
    }
    applied
}

/// ESTIMATED output tokens saved by minification: for each choice whose
/// assistant text parses as JSON (`serde_json::Value`, after trim), the
/// pretty-printed re-rendering (`serde_json::to_string_pretty` — 2-space
/// indent, the documented counterfactual basis) is re-tokenized with the
/// served model's tokenizer and the per-choice delta
/// `pretty_tokens.saturating_sub(emitted_tokens)` is summed. Non-JSON /
/// fence-wrapped / tool-call-only choices contribute 0 — if the response is
/// not valid JSON we book ZERO (no claim). A model that ignored the
/// instruction and emitted pretty JSON yields a ~0 delta by construction —
/// the estimate is grounded in the actual emission, never a -40% constant.
pub(super) fn minify_saved_tokens_est(
    provider_id: &str,
    model: &str,
    response: &ChatCompletionResponse,
) -> u32 {
    let mut total: u32 = 0;
    for choice in &response.choices {
        let Message::Assistant {
            content: Some(content),
            ..
        } = &choice.message
        else {
            continue; // tool-call-only / non-assistant choices book nothing.
        };
        let text = match content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    tt_shared::messages::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        };
        let trimmed = text.trim();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue; // not valid JSON → no claim for this choice.
        };
        let Ok(pretty) = serde_json::to_string_pretty(&value) else {
            continue;
        };
        let emitted = tt_tokenize::estimate_tokens_for_model(provider_id, model, trimmed);
        let pretty_tokens = tt_tokenize::estimate_tokens_for_model(provider_id, model, &pretty);
        total = total.saturating_add(pretty_tokens.saturating_sub(emitted));
    }
    total
}

/// Attach `X-TokenTrimmer-Warnings`: the model-dependent `param_dropped:<name>`
/// tokens (computed here against `served_model`) plus any pre-dispatch `extra`
/// tokens (e.g. `response_format_downgrade`). Comma-joined; no-op when empty.
///
/// `served_model` is the model that actually served the request — under
/// cross-model failover this differs from `req.model`, and some drops
/// (reasoning-model `temperature`) are model-dependent, so they must be
/// evaluated against the served model, not the originally-requested one.
pub(super) fn attach_warnings(
    headers: &mut axum::http::HeaderMap,
    provider: &dyn tt_shared::Provider,
    req: &ChatCompletionRequest,
    served_model: &str,
    extra: &[String],
) {
    let dropped = if req.model == served_model {
        provider.dropped_params(req)
    } else {
        // Failover rebound to a different model — evaluate drops against it.
        let mut served = req.clone();
        served.model = served_model.to_string();
        provider.dropped_params(&served)
    };
    let mut tokens: Vec<String> = dropped
        .into_iter()
        .map(|p| format!("param_dropped:{p}"))
        .collect();
    tokens.extend(extra.iter().cloned());
    if tokens.is_empty() {
        return;
    }
    if let Ok(v) = tokens.join(",").parse() {
        headers.insert("x-tokentrimmer-warnings", v);
    }
}

/// Attach ONLY the pre-dispatch warning tokens (`route_paused:*`,
/// `redacted:*`, `format_switch*`, `*_skipped:*`, …) to a NON-dispatch
/// (cache-hit) response — no `param_dropped:*` evaluation because no dispatch
/// happened. Closes the pre-existing gap where hit responses silently lost
/// every pre-dispatch token. Comma-joined; no-op when empty (mirrors
/// [`attach_warnings`]).
pub(super) fn attach_warning_tokens(headers: &mut axum::http::HeaderMap, tokens: &[String]) {
    if tokens.is_empty() {
        return;
    }
    if let Ok(v) = tokens.join(",").parse() {
        headers.insert("x-tokentrimmer-warnings", v);
    }
}
