//! Provider streaming dispatch and stream response assembly.

use super::*;
/// Dispatch the **streaming** arm of a chat completion from a [`Prepared`]
/// bundle: the L1 fake-stream cache hit, the live (single-provider or failover)
/// stream establishment, the streaming cost/telemetry/cache-insert wiring, and
/// the SSE response assembly. Byte-for-byte the chat [`handler`]'s former inline
/// `if req.stream` arm — it now reads its inputs from `prep` (destructured into
/// the exact locals the arm always used) instead of the handler's setup locals.
pub(super) async fn handle_streaming(
    state: &AppState,
    ctx: &RequestContext,
    prep: Prepared,
) -> ApiResult<Response> {
    // Destructure into the exact locals the streaming arm reads (the fields it
    // does not use are bound to `_` so the move is explicit). `trace_id` mirrors
    // the handler's former local (`== ctx.trace_id` by construction).
    let Prepared {
        provider,
        req,
        cache_behavior,
        l2_allowed,
        skip_l2: _,
        route_matched_name,
        matched_route_id,
        matched_route_version_id,
        route_paused,
        requested_model,
        requested_pricing,
        model_was_rewritten,
        format_switch_plan: _,
        diff_plan: _,
        pass_effects,
        // Streaming defers the D4c-v2 vision-avoided estimate to the non-streaming
        // path (the seam still ran pre-split, so the distillation + downgrade are
        // real; the isolated $ figure books $0 on a streamed row — same v1 posture
        // as `content_compress_kind` / the minify estimate).
        doc_distill_booking: _,
        // Streaming defers the content_compress isolated estimate + flywheel
        // label to the non-streaming path in v1 (the pass still runs pre-split,
        // so the metered token reduction is real; the isolated estimate books $0
        // on a streamed row, matching the minify-estimate v1 posture).
        content_compress_kind: _,
        minify_applied,
        reasoning_capped: _,
        flex_applied,
        batch_marked: _,
        caller_tier,
        skip_shadow: _,
        traffic_split_arm_owned,
        route_traffic_pct,
        route_shadow_model: _,
        failover_candidates,
        failover_creds,
        failover_cost_check,
        route_fallbacks,
        warnings,
        request_timeout,
        raw_bearer: _,
        retrieval_telemetry,
        request_started,
        capture_request_json,
        pre_compression_request_json: _,
        judge_source_provider: _,
        judge_source_ctx: _,
        judge_original_req: _,
        // Panels never reach the streaming arm — the handler forces a
        // panel-configured request through `complete_once` (Phase 1 panels are
        // non-streaming; the buffered arbiter answer is returned). This is `None`
        // by construction here.
        panel: _,
        panel_admission: _,
        panel_creds: _,
        // `handle_streaming` is only reached after the handler's streaming guard
        // has already `take`n any `workflow` config (warned + dropped for the
        // single-model fallback); `None` by construction here.
        workflow: _,
    } = prep;
    let trace_id = ctx.trace_id;
    {
        // 3α. L1 fake-stream — when a streaming request has a cached
        //     response, synthesize an SSE stream from the cached body
        //     instead of dispatching live. The chunk key matches the
        //     non-stream branch's `namespaced_l1_key` so streaming and
        //     non-streaming variants of the same prompt share cache
        //     entries.
        // L1 fake-stream lookup — gated on cache eligibility (Fix A) and
        // tt_extras.cache mode (Fix B).
        let l1_key = state.l1.as_ref().map(|_| namespaced_l1_key(ctx, &req));
        if cache_behavior.do_lookup {
            if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key.as_ref()) {
                if let Ok(Some(bytes)) = l1.cache.get(key).await {
                    if let Ok(entry) = L1Entry::from_bytes(&bytes) {
                        spawn_request_log(
                            state.telemetry_tracker.as_ref(),
                            state.request_log_writer.as_ref(),
                            request_log_for_l1_hit(
                                &entry,
                                ctx,
                                &requested_model,
                                trace_id,
                                request_started,
                                RouteLogAttribution {
                                    route_id: matched_route_id,
                                    route_version_id: matched_route_version_id,
                                    paused: route_paused,
                                },
                                retrieval_telemetry.tokens_saved,
                            ),
                        );
                        // Record OTel GenAI semconv + cost span attributes for the
                        // fake-stream L1 hit. The cost is already known here (a hit
                        // never reaches a provider → cost 0, full baseline saved),
                        // and `Span::current()` is still the `http_request` span,
                        // so we stamp synchronously — mirroring the non-streaming
                        // `build_hit_l1_response`. The `None` log_ctx below means
                        // the SSE drop guard records nothing, so without this the
                        // streaming L1 hit would be invisible to the dashboards.
                        let baseline_cost_usd = if entry.is_legacy_format() {
                            synthetic_baseline_from_usage(&entry.response.usage)
                        } else {
                            entry.baseline_cost_usd
                        };
                        let hit_cost = l1_cache_hit_cost_breakdown(baseline_cost_usd);
                        record_request_span_attributes(
                            &entry.response.model,
                            &entry.response.model,
                            "cache",
                            span_cost(
                                &hit_cost,
                                entry.response.usage.prompt_tokens,
                                entry.response.usage.completion_tokens,
                            ),
                            "hit-l1",
                            None,
                            // Cache hit — no canary split/shadow recorded.
                            None,
                            None,
                            None,
                        );
                        // Only an envelope-written baseline may become a
                        // terminal cache cost receipt. A legacy raw response
                        // remains safely replayable, but its synthetic
                        // telemetry baseline is not promoted into client-facing
                        // savings evidence.
                        let cache_attribution = l1_cache_stream_attribution(&entry);
                        let cached_model = entry.response.model.clone();
                        let fake = sse::fake_stream_from_response(entry.response);
                        // L1 hit already logged above; no need for a second row
                        // or a live-stream DropGuard. The cache-specific stream
                        // queues its verified terminal usage receipt before
                        // `[DONE]` only after the fake stream reaches clean EOF.
                        let mut resp = with_route_matched(
                            sse::cache_hit_stream_response(
                                fake,
                                &provider,
                                trace_id,
                                cache_attribution,
                            ),
                            route_matched_name.as_deref(),
                        );
                        attach_l1_cache_stream_headers(
                            resp.headers_mut(),
                            trace_id,
                            &cached_model,
                            cache_attribution,
                        );
                        // P0-1/P0-3: settle the served request as a cache hit.
                        // This fake-stream path has no DropGuard, so the
                        // streamed-dispatch settle at `sse.rs` NEVER runs here.
                        // Settle inline (as the non-streaming CacheHit arm does)
                        // so the served counter advances (the COGS guard) while
                        // the billed monthly counter does NOT — a streaming cache
                        // hit does not consume an included request. Without this,
                        // a free tenant using `stream:true` could serve unbounded
                        // cache hits and never trip the served ceiling.
                        state
                            .spend_sink()
                            .settle(ctx.org_id, ctx.api_key_id, true, Utc::now());
                        // Pre-dispatch tokens (route_paused / redacted /
                        // shaping skips) must survive on hit responses too.
                        attach_warning_tokens(resp.headers_mut(), &warnings);
                        return Ok(resp);
                    }
                }
            }
        }

        // No cache hit (or no L1 wired / cache skipped) — dispatch live to the provider.
        // Estimate input tokens from the request messages using tt_tokenize
        // (tiktoken for openai/anthropic, chars/4 for others) so that the
        // streaming input estimate is consistent with routing and /v1/preview
        // rather than a raw byte-length heuristic (§2.15).
        let estimated_input_tokens = tt_tokenize::estimate_tokens(
            provider.id(),
            &tt_shared::message_text_for_estimation(&req),
        ) as i32;

        // Establish the stream. When the matched route declares fallbacks, fail
        // over across the candidate chain (initial establishment only — a
        // mid-stream error cannot move to another provider); otherwise retry
        // the single provider. `provider`/`served_model` are rebound to whoever
        // actually served so cost/telemetry attribute correctly.
        let __primary = provider.id();
        let __stream_outcome = with_request_timeout(request_timeout, async {
            if route_fallbacks.is_empty() {
                // Retry the initial stream establishment on transient errors (before
                // any chunk is yielded); mid-stream errors are not retried.
                let __started = std::time::Instant::now();
                let __stream_result = with_retry(&RetryPolicy::default(), || {
                    provider.chat_completion_stream(req.clone(), ctx)
                })
                .await;
                let __elapsed = __started.elapsed();
                crate::metrics::record_provider_latency(provider.id(), "chat_stream", __elapsed);
                // Feed the rolling p95 window on successful stream establishment
                // (time-to-first-byte). See the non-streaming hook above.
                if __stream_result.is_ok() {
                    let __ms = u32::try_from(__elapsed.as_millis()).unwrap_or(u32::MAX);
                    state
                        .latency_tracker
                        .record(provider.id(), &req.model, __ms);
                }
                let stream = __stream_result?;
                Ok((provider, req.model.clone(), stream))
            } else {
                // Build the capability check for the streaming failover path.
                let cap_required = tt_shared::RequiredCapabilities::from_request(&req);
                let cap_est_tokens = estimated_input_tokens.max(0) as u64;
                crate::failover::dispatch_stream_with_failover(
                    &state.registry,
                    &state.breaker,
                    &RetryPolicy::default(),
                    &failover_candidates,
                    &req,
                    ctx,
                    &failover_creds,
                    Utc::now(),
                    Some(crate::failover::CapCheck {
                        required: &cap_required,
                        estimated_tokens: cap_est_tokens,
                    }),
                    failover_cost_check,
                )
                .await
                .map_err(map_failover_error)
            }
        })
        .await;
        // Attributed to the primary provider: the request deadline spans any
        // failover loop, so the in-flight candidate at timeout isn't known here
        // without threading it out of dispatch_stream_with_failover.
        if matches!(__stream_outcome, Err(ApiError::RequestTimeout { .. })) {
            crate::metrics::record_provider_timeout(__primary, "chat_stream");
        }
        let (provider, served_model, stream) = __stream_outcome?;

        // Whether this trace's body was actually handed to the capture sink for
        // persistence: `capture_request_json` is `Some` only when a writer is
        // armed, the org is non-anonymous, AND the org opted in
        // (`is_capture_enabled` above), so the header is honest. Captured before
        // the Option is consumed so the streaming response can advertise it
        // (§6.2). On the streaming path only the REQUEST body is persisted (the
        // streamed response is not re-captured); the header therefore signals
        // request-body capture for opted-in orgs.
        let body_captured = capture_request_json.is_some();
        if let Some(request_json) = capture_request_json {
            spawn_body_capture(
                state.telemetry_tracker.as_ref(),
                state.body_capture_writer.as_ref(),
                BodyCaptureRecord {
                    org_id: ctx.org_id,
                    api_key_id: ctx.api_key_id,
                    trace_id: trace_id.to_string(),
                    endpoint: "/v1/chat/completions".into(),
                    provider: provider.id().to_string(),
                    model: served_model.clone(),
                    request_json,
                    response_json: None,
                    // TR-3: the streaming path binds the pre-compression
                    // snapshot to `_` (deferred — the streaming diff is a
                    // follow-up; the non-streaming path hosts the diff for now).
                    pre_compression_request_json: None,
                    ts: Utc::now(),
                },
            );
        }

        // Build the cache-insert context for the streaming miss path
        // (§rv-l2-streaming-cache-write). On clean completion the DropGuard
        // reconstructs a ChatCompletionResponse from the accumulated data and
        // inserts it into L1/L2 — best-effort, after the final chunk.
        //
        // Gated on: do_insert=true AND at least one cache backend is wired.
        // The guard closure uses the cost_usd/baseline_cost_usd it already
        // computes for the request_logs row, so the L1Entry envelope carries
        // accurate savings figures without repeating the pricing calculation.
        //
        // L2 tier gate: the L2 portion is gated on `l2_allowed` (Pro/Team/Scale
        // only). Free/None callers write to L1 only. `sse.rs` already no-ops the
        // L2 insert when `ins.l2` is None, so setting it to None here is the
        // correct minimal fix (rv-streaming-l2-tier-gate).
        let stream_cache_insert =
            if cache_behavior.do_insert && (state.l1.is_some() || state.l2.is_some()) {
                // Only populate the L2 handle and query text for paid-tier callers.
                let l2_for_insert = if l2_allowed { state.l2.clone() } else { None };
                let l2_query_text = l2_for_insert
                    .as_ref()
                    .and_then(|_| tt_cache::l2_context_text(&req));
                let ttl = effective_ttl_secs(
                    cache_behavior.ttl_secs,
                    caller_tier,
                    state
                        .l1
                        .as_ref()
                        .map(|l| l.ttl_secs)
                        .unwrap_or(L2_DEFAULT_TTL.as_secs()),
                );
                // Volatility-class TTL for the L2 insert only (opt-in,
                // shorten-only). `ttl_secs` keeps governing L1 unchanged —
                // L1 is an exact-byte match, not a near-miss risk.
                let l2_ttl_secs = match (l2_for_insert.as_ref(), l2_query_text.as_deref()) {
                    (Some(l2cfg), Some(qt)) => crate::cache_volatility::l2_ttl_with_volatility(
                        ttl,
                        cache_behavior.ttl_secs.is_some(),
                        qt,
                        l2cfg.volatility_ttl.as_ref(),
                    ),
                    _ => ttl,
                };
                Some(CacheInsertContext {
                    l1: state.l1.clone(),
                    l2: l2_for_insert,
                    l1_key: l1_key.clone().unwrap_or_default(),
                    l2_query_text,
                    ttl_secs: ttl,
                    l2_ttl_secs,
                    model: served_model.clone(),
                    provider_id: provider.id().to_string(),
                    org_id: ctx.org_id,
                })
            } else {
                None
            };

        // OTel GenAI semconv + cost span attributes for the streaming path. The
        // cost is only known once the stream drains, and the `http_request` span
        // has already exited by the time the SSE body is polled — so capture the
        // span handle here (cloning keeps it open) and let the DropGuard stamp
        // the attributes onto it once the cost is computed, mirroring the
        // non-streaming `record_request_span_attributes` call. Without this,
        // every streaming request would carry none of the gen_ai.*/tokentrimmer.*
        // attributes and the dashboards would undercount streaming traffic.
        //
        // Cache state: `miss` when a cache layer is wired (the L1 fake-stream
        // lookup above didn't hit), `none` when neither L1 nor L2 is configured.
        let stream_cache_state = if state.l1.is_some() || state.l2.is_some() {
            "miss"
        } else {
            "none"
        };
        let span_ctx = Some(StreamSpanContext {
            span: tracing::Span::current(),
            provider_id: provider.id().to_string(),
            request_model: requested_model.clone(),
            response_model: served_model.clone(),
            cache_outcome: stream_cache_state.to_string(),
            route: route_matched_name.clone(),
            // Canary split pct (additive span attr); shadow mode is non-streaming
            // so it is never set on the streaming span.
            traffic_split_pct: route_traffic_pct,
        });

        // Build a StreamLogContext whenever telemetry, span attributes, or cache
        // insertion is needed. writer=None skips the request_logs row without
        // preventing span recording / cache writes (tests, dev mode without a DB).
        let needs_tracking = state.request_log_writer.is_some()
            || stream_cache_insert.is_some()
            || span_ctx.is_some();
        let log_ctx = if needs_tracking {
            Some(StreamLogContext {
                writer: state.request_log_writer.as_ref().map(|w| w.clone()),
                // Drain the detached streaming request_logs write on graceful
                // shutdown (REL-3 / P2), so a rolling deploy / SIGTERM mid-stream
                // doesn't abandon the committed billing row.
                tracker: state.telemetry_tracker.clone(),
                org_id: ctx.org_id,
                api_key_id: ctx.api_key_id,
                trace_id,
                provider_id: provider.id().to_string(),
                requested_model: requested_model.clone(),
                model: served_model.clone(),
                input_tokens: estimated_input_tokens,
                cached_tokens: 0,
                pricing: provider.pricing(&served_model),
                // Baseline against the originally-requested model when the model
                // was actually rewritten (canary arm / unconditional rewrite), so
                // the streamed request_logs row carries the real routing saving. A
                // control-arm request reverted to the original model is NOT a
                // rewrite → baseline == served (no phantom saving).
                baseline_pricing: if model_was_rewritten {
                    requested_pricing.clone()
                } else {
                    provider.pricing(&served_model)
                },
                route_id: matched_route_id,
                route_version_id: matched_route_version_id,
                tag: ctx.tag.clone(),
                request_started,
                spend_sink: state.spend_sink(),
                // Thread provider surcharge through so the streaming path applies
                // it to both cost and baseline, matching the non-streaming path (§2.13).
                fee_multiplier: provider.fee_multiplier(),
                // Thread the Flex opt-in through so the streaming cost math meters
                // at flex rates and attributes the standard-vs-flex saving to the
                // `flex` source, matching the non-streaming path (FLEX-REWRITE (2)).
                flex_applied,
                // Thread the request-pass effects through so the streaming cost
                // math attributes the standard-vs-compressed saving to the
                // `compression` source AND carries any cache-bust penalty,
                // matching the non-streaming path.
                pass_effects,
                retrieval_tokens_saved: retrieval_telemetry.tokens_saved,
                cache_insert: stream_cache_insert,
                // Honor stream_options.include_usage end-to-end: emit an
                // OpenAI-native final usage chunk when the client asked for it.
                include_usage: client_requested_include_usage(&req),
                span_ctx,
                // Canary arm for the streamed request_logs row (None when no
                // split). Shadow mode never fires on the streaming path.
                traffic_split_arm: traffic_split_arm_owned.clone(),
                // Paused-route passthrough marker for the streamed row.
                route_paused,
                // Single-model streaming dispatch carries no panel context; the
                // streaming-panel path (Phase 5 Task 6) sets this. Off-by-default.
                panel: None,
            })
        } else {
            None
        };

        // Minify on a streaming request: the instruction + warning applied
        // pre-dispatch; v1 books $0 and only METERS the event (the estimate
        // needs the full response text — a documented follow-up could
        // re-tokenize at stream end via the cache-insert reconstruction).
        if minify_applied {
            crate::metrics::record_minify_estimate(
                route_matched_name.as_deref().unwrap_or("none"),
                0,
                0.0,
            );
        }
        let mut resp = with_route_matched(
            sse::stream_response(stream, &provider, trace_id, log_ctx),
            route_matched_name.as_deref(),
        );
        attach_warnings(
            resp.headers_mut(),
            provider.as_ref(),
            &req,
            &served_model,
            &warnings,
        );
        // Present ONLY when the org opted in and the request body was persisted
        // to the encrypted capture sink — absent on the default (capture-off)
        // path AND for armed-but-not-opted-in orgs (the `is_capture_enabled`
        // gate above), so the header never claims capture for an unstored body.
        if body_captured {
            resp.headers_mut().insert(
                "x-tokentrimmer-captured",
                axum::http::HeaderValue::from_static("true"),
            );
        }
        Ok(resp)
    }
}
/// Run one routed, metered, cached **non-streaming** completion: the L1/L2
/// (and negative-cache) lookups, single-flight coalescing, provider dispatch
/// (single + `dispatch_with_failover`), response-side output shaping, cost
/// computation, L1/L2 insert, the `request_logs` row, body capture, the sampled
/// quality judge, and the OTel request-span attributes — returning the typed
/// response + header metadata ([`CompletionOutcome`]) instead of building the
/// HTTP `Response`. The chat [`handler`]'s non-streaming arm calls this and
/// assembles the final `Response`; the future server-side agent loop (slice 1a)
/// calls this per turn through a `TurnCompleter` seam.
///
/// Behavior-preserving by construction: this is the verbatim non-streaming arm
/// of [`handler`], with the early cache-hit returns yielding
/// [`CompletionOutcome::CacheHit`] and the dispatched tail yielding
/// [`CompletionOutcome::Dispatched`].
pub(crate) async fn complete_once(
    state: &AppState,
    ctx: &RequestContext,
    prep: Prepared,
) -> ApiResult<CompletionOutcome> {
    let retry = RetryPolicy::default();
    complete_once_with_retry_policy(state, ctx, prep, &retry).await
}

/// Execute one capped workflow turn after the shared route/action preparation.
///
/// A numeric workflow reservation covers one priceable provider turn. Route
/// rewrites remain available, but work that can add an unreserved upstream leg
/// is removed: retries, failover, shadow/panel/workflow fan-out, quality judging,
/// and diff re-emission. The final routed/shaped request is re-priced against
/// the node's effective remaining cap before the one allowed attempt.
pub(crate) async fn complete_once_budgeted_workflow(
    state: &AppState,
    ctx: &RequestContext,
    mut prep: Prepared,
    max_cost_usd: f64,
) -> ApiResult<CompletionOutcome> {
    prep.panel = None;
    prep.panel_admission = None;
    prep.panel_creds.clear();
    prep.workflow = None;
    prep.route_shadow_model = None;
    prep.route_fallbacks.clear();
    prep.failover_candidates.clear();
    prep.failover_creds.clear();
    prep.failover_cost_check = None;
    prep.judge_source_provider = None;
    prep.judge_source_ctx = None;
    prep.judge_original_req = None;
    if let Some(plan) = prep.diff_plan.take() {
        crate::shaping::diff::unapply_diff_request(&mut prep.req, &plan);
    }
    prep.warnings
        .push("workflow_budget_single_dispatch".to_string());

    admit_budgeted_workflow_dispatch(&prep.req, max_cost_usd)?;

    let retry = RetryPolicy::default().capped(1);
    complete_once_with_retry_policy(state, ctx, prep, &retry).await
}

fn admit_budgeted_workflow_dispatch(
    req: &ChatCompletionRequest,
    max_cost_usd: f64,
) -> ApiResult<()> {
    let routed_estimate = crate::routes::agent_run_budget::estimate_next_turn_cost(
        &req.model,
        &req.messages,
        req.max_tokens,
    )
    .filter(|estimate| estimate.is_finite() && *estimate >= 0.0)
    .ok_or_else(|| {
        ApiError::InvalidRequest(
            "workflow budget dispatch could not price the final routed request".into(),
        )
    })?;
    if crate::routes::agent_run_budget::would_exceed(0.0, Some(routed_estimate), Some(max_cost_usd))
    {
        return Err(ApiError::InvalidRequest(
            "workflow budget dispatch rejected the final routed request before provider work"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod workflow_budget_dispatch_tests {
    use super::*;

    fn request(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Text("hello".into()),
                name: None,
            }],
            max_tokens: Some(64),
            ..Default::default()
        }
    }

    #[test]
    fn final_routed_request_must_be_priceable_and_fit_the_effective_cap() {
        let known = request("gpt-4o-mini");
        let estimate = crate::routes::agent_run_budget::estimate_next_turn_cost(
            &known.model,
            &known.messages,
            known.max_tokens,
        )
        .expect("catalog model");

        assert!(admit_budgeted_workflow_dispatch(&known, estimate).is_ok());
        assert!(admit_budgeted_workflow_dispatch(&known, estimate / 2.0).is_err());
        assert!(admit_budgeted_workflow_dispatch(&request("unknown-model"), 1.0).is_err());
    }
}

async fn complete_once_with_retry_policy(
    state: &AppState,
    ctx: &RequestContext,
    mut prep: Prepared,
    retry_policy: &RetryPolicy,
) -> ApiResult<CompletionOutcome> {
    // Fusion panel branch — FIRST, before any cache /
    // single-flight check. Panels are non-deterministic (two same-model legs
    // must not coalesce) and bill as ONE aggregate row, so they bypass L1/L2 +
    // single-flight entirely (spec §6.5, invariant §2.1.5). `take()` leaves
    // `prep.panel = None`; the whole bundle (still owning `req`/`provider`/
    // `failover_creds`) is moved into `complete_panel`. For the overwhelming
    // majority of requests `prep.panel` is `None` (no panel header), so this is
    // a single cheap `Option::take` + `None` check and the path below is
    // wire-identical to today's single-model completion (off-by-default).
    //
    // CO-4: an at-breach PauseShadow org (the auth pre-flight flagged
    // `skip_shadow`) skips the panel detour — a breach must not double spend
    // via the fan-out. Drop the config (the request flows through as a
    // single-model dispatch instead). Also zero the shadow_model route below
    // + the workflow detour (a shadow-mode workflow doubles upstream spend
    // by design — workflows.rs:1063) for the same reason.
    if prep.skip_shadow {
        prep.panel = None;
        prep.panel_admission = None;
        prep.route_shadow_model = None;
        prep.workflow = None;
        prep.warnings
            .push("budget-breach: shadow/panel/workflow routes skipped (PauseShadow)".to_string());
    }
    if let Some(cfg) = prep.panel.take() {
        let admission = prep.panel_admission.take().ok_or_else(|| {
            ApiError::Internal("panel configuration missing its admission proof".to_string())
        })?;
        return panel::complete_panel(state, ctx, prep, cfg, admission).await;
    }
    // Workflow-detour branch (CO-1) — before cache / single-flight, for the
    // same reason as the panel branch: a workflow is non-deterministic (a
    // matched route must not coalesce two identical requests) and bills as ONE
    // aggregate row. `take()` leaves `prep.workflow = None`. SHADOW mode runs
    // the workflow alongside the normal dispatch (cost recorded separately, no
    // response substitution) and falls through to the single-model path below —
    // a best-effort shadow error never fails the request. DETOUR mode replaces
    // the upstream call: the workflow's final synthesized answer becomes the
    // chat response. Streaming workflow detour is unsupported in v1 (a
    // `stream:true` request on a workflow route has already fallen through to
    // `handle_streaming` in the handler, which warns + runs single-model). For
    // the overwhelming majority of requests `prep.workflow` is `None`, so this
    // is a single cheap `Option::take` + `None` check and the path below is
    // wire-identical to today's single-model completion (off-by-default).
    if let Some(cfg) = prep.workflow.take() {
        // `mode: None` defaults to "detour" (validated to detour|shadow at route
        // creation in `validate_workflow`).
        let is_detour = cfg.mode.as_deref() != Some("shadow");
        if is_detour {
            return workflows::complete_workflow(state, ctx, prep, cfg).await;
        } else {
            // Shadow: run the workflow for its cost/receipt only, then continue
            // to the normal single-model dispatch. Best-effort: an error in the
            // shadow workflow is recorded as a warning, never propagated.
            if let Err(msg) = workflows::run_workflow_shadow(state, ctx, &prep, &cfg).await {
                prep.warnings.push(format!("workflow-shadow: {msg}"));
            }
        }
    }
    // Destructure the prepared setup into locals with the exact names + types
    // the carved pipeline (the former handler non-streaming arm) reads, so the
    // body below is byte-for-byte the handler's. `state`/`ctx` are the params;
    // everything else is moved out of `prep`.
    let Prepared {
        provider,
        req,
        cache_behavior,
        l2_allowed,
        skip_l2,
        route_matched_name,
        matched_route_id,
        matched_route_version_id,
        route_paused,
        requested_model,
        requested_pricing,
        // `complete_once` prices its baseline from `matched_route_id.is_some()`
        // (its own pre-existing rule), so the streaming-only rewrite flag is
        // ignored here.
        model_was_rewritten: _,
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
        skip_shadow: _,
        traffic_split_arm_owned,
        route_traffic_pct,
        route_shadow_model,
        failover_candidates,
        failover_creds,
        failover_cost_check,
        route_fallbacks,
        mut warnings,
        request_timeout,
        raw_bearer,
        retrieval_telemetry,
        request_started,
        capture_request_json,
        judge_source_provider,
        judge_source_ctx,
        judge_original_req,
        // TR-3: the pre-compression snapshot (None unless route_compress +
        // writer-armed in `prepare`); consumed below by the body-capture build
        // when the compress pass committed.
        pre_compression_request_json,
        // Already `take`n into the panel branch above (and `None` for the
        // single-model path that reaches here); bind to `_` to stay exhaustive.
        panel: _,
        // Panel-only; the single-model path never reads it.
        panel_admission: _,
        // Panel-only; the single-model path never reads it.
        panel_creds: _,
        // Already `take`n into the workflow branch above (and `None` otherwise).
        workflow: _,
    } = prep;
    // The handler built `ctx.trace_id` from the same trace-id it derived; the
    // carved pipeline reads `trace_id` directly (cache rows, L1 envelopes,
    // headers). They are identical by construction.
    let trace_id = ctx.trace_id;

    // 3a. L1 exact-match cache. Cheapest lookup — try first. Gated on
    //     cache eligibility (Fix A §2.2) and tt_extras.cache mode (Fix B §2.7).
    //     Best-effort: any Redis error falls through to L2/provider.
    let l1_key = state.l1.as_ref().map(|_| namespaced_l1_key(ctx, &req));

    // 3a/3a-neg. Negative cache, then L1 exact-match. Gated on cache
    // eligibility + tt_extras.cache mode; best-effort (errors fall through).
    if cache_behavior.do_lookup {
        if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key.as_ref()) {
            if let Some(resp) = try_negative_cache_hit(l1, key, route_matched_name.as_deref()).await
            {
                return Ok(CompletionOutcome::CacheHit(resp));
            }
            if let Some(mut resp) = try_l1_hit(
                l1,
                key,
                ctx,
                state.telemetry_tracker.as_ref(),
                state.request_log_writer.as_ref(),
                trace_id,
                &requested_model,
                request_started,
                matched_route_id,
                matched_route_version_id,
                route_paused,
                retrieval_telemetry.tokens_saved,
                route_matched_name.as_deref(),
            )
            .await
            {
                // A validated switch is the ONLY thing ever inserted
                // under a switched key (3e suppresses fail-open bodies),
                // so a hit here is genuinely switched — advertise it
                // exactly like the dispatch path. Other pre-dispatch
                // tokens (route_paused / redacted / shaping skips)
                // survive on hit responses too.
                if let Some(plan) = format_switch_plan.as_ref() {
                    warnings.push(format!("format_switch:{}", plan.label));
                }
                attach_warning_tokens(resp.headers_mut(), &warnings);
                return Ok(CompletionOutcome::CacheHit(resp));
            }
        }
    }

    // 3b. L2 semantic cache. Gated additionally on l2_allowed, and OFF
    // for format-switched requests (`skip_l2` — similarity matching could
    // cross the instruction boundary).
    //
    // Captures the embedding `try_l2_hit` computes for the lookup so the
    // miss path can reuse it for the L2 insert instead of re-embedding the
    // identical query text (COST-3). `None` until a lookup runs and embeds.
    let mut l2_lookup_vec: Option<Vec<f32>> = None;
    if cache_behavior.do_lookup && l2_allowed && !skip_l2 {
        if let Some(l2) = state.l2.as_ref() {
            // Current catalog rate for the (post-routing) request model —
            // the legacy-row fallback in `l2_entry_baseline`. The entry's
            // model always equals `req.model` here (lookup filters on it).
            let current_pricing = provider.pricing(&req.model);
            if let Some(result) = try_l2_hit(
                state,
                l2,
                ctx,
                &req,
                current_pricing.as_ref(),
                state.request_log_writer.as_ref(),
                trace_id,
                &requested_model,
                request_started,
                matched_route_id,
                matched_route_version_id,
                route_paused,
                retrieval_telemetry.tokens_saved,
                route_matched_name.as_deref(),
                &raw_bearer,
                judge_source_provider.as_ref(),
                judge_source_ctx.as_ref(),
                judge_original_req.as_ref(),
                &mut l2_lookup_vec,
            )
            .await
            {
                // No format_switch token here: switched requests skip L2
                // entirely (`skip_l2`), so an L2 hit is never switched.
                return result.map(|mut resp| {
                    attach_warning_tokens(resp.headers_mut(), &warnings);
                    CompletionOutcome::CacheHit(resp)
                });
            }
        }
    }

    // 3b.5. Single-flight coalescing for cache-eligible non-streaming requests.
    //
    // When multiple concurrent requests share the same L1 key and all miss
    // L1+L2, only the FIRST (leader) dispatches to the provider.  The rest
    // (followers) wait up to FOLLOWER_TIMEOUT and then re-read L1; if the
    // leader has populated it they serve from cache.  If the leader fails or
    // the timeout fires, followers fall through to their own provider dispatch
    // (correctness over coalescing).
    //
    // Scope: cache-eligible, do_lookup=true, non-streaming, L1 configured.
    // Streaming single-flight is a follow-up (broadcasting an SSE stream to
    // multiple waiters is non-trivial).
    let mut single_flight_guard = None::<crate::single_flight::LeaderGuard>;
    if cache_behavior.do_lookup {
        if let Some(sf_key) = l1_key.as_deref() {
            match state.single_flight.try_become_leader(sf_key) {
                Ok(guard) => {
                    // We are the leader — proceed to provider dispatch below
                    // and call guard.complete() after populating L1.
                    tracing::debug!(key = %sf_key, "single-flight: became leader");
                    single_flight_guard = Some(guard);
                }
                Err(rx) => {
                    // We are a follower — wait for the leader to finish.
                    tracing::debug!(key = %sf_key, "single-flight: following leader");
                    let populated = wait_for_leader(rx).await;
                    if populated {
                        // Re-read L1; if it's there, serve it directly.
                        if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key.as_deref()) {
                            match l1.cache.get(key).await {
                                Ok(Some(bytes)) => match L1Entry::from_bytes(&bytes) {
                                    Ok(entry) => {
                                        tracing::debug!(
                                            key = %key,
                                            "single-flight: follower served from populated L1"
                                        );
                                        spawn_request_log(
                                            state.telemetry_tracker.as_ref(),
                                            state.request_log_writer.as_ref(),
                                            request_log_for_l1_hit(
                                                &entry,
                                                ctx,
                                                &requested_model,
                                                trace_id,
                                                request_started,
                                                RouteLogAttribution {
                                                    route_id: matched_route_id,
                                                    route_version_id: matched_route_version_id,
                                                    paused: route_paused,
                                                },
                                                retrieval_telemetry.tokens_saved,
                                            ),
                                        );
                                        let mut resp = with_route_matched(
                                            build_hit_l1_response(entry, trace_id),
                                            route_matched_name.as_deref(),
                                        );
                                        // Same advertisement contract as
                                        // the direct L1-hit return above.
                                        if let Some(plan) = format_switch_plan.as_ref() {
                                            warnings.push(format!("format_switch:{}", plan.label));
                                        }
                                        attach_warning_tokens(resp.headers_mut(), &warnings);
                                        return Ok(CompletionOutcome::CacheHit(resp));
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            key = %key,
                                            "single-flight: follower L1 re-read deserialized failed, falling through"
                                        );
                                    }
                                },
                                Ok(None) => {
                                    tracing::debug!(
                                        key = %key,
                                        "single-flight: follower L1 re-read empty, falling through"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "single-flight: follower L1 re-read error, falling through"
                                    );
                                }
                            }
                        }
                    } else {
                        tracing::debug!(
                            key = %sf_key,
                            "single-flight: leader failed or timed out, follower dispatching independently"
                        );
                    }
                    // Fall through to provider dispatch (leader failed /
                    // L1 re-read miss / timeout).
                }
            }
        }
    }

    // 3c. No cache hit — dispatch to provider. When the matched route
    //     declared fallbacks, fail over across the candidate chain
    //     (primary first, then each fallback) skipping providers whose
    //     circuit breaker is open; otherwise dispatch the single provider
    //     with retry. `provider` is rebound to whichever provider actually
    //     served the request so cost/headers/telemetry below reflect it.
    let __primary = provider.id();
    let primary_dispatch = with_request_timeout(request_timeout, async {
        if route_fallbacks.is_empty() {
            let __started = std::time::Instant::now();
            let __dispatch =
                with_retry(retry_policy, || provider.chat_completion(req.clone(), ctx)).await;
            let __elapsed = __started.elapsed();
            crate::metrics::record_provider_latency(provider.id(), "chat", __elapsed);
            // Feed the rolling p95 window (the live signal behind the
            // `upstream_latency_ms_p95_gt` route condition) on success only —
            // errored/short-circuited dispatches aren't representative
            // upstream latency. Keyed by the served `(provider, model)`.
            if __dispatch.is_ok() {
                let __ms = u32::try_from(__elapsed.as_millis()).unwrap_or(u32::MAX);
                state
                    .latency_tracker
                    .record(provider.id(), &req.model, __ms);
            }
            __dispatch
                .map(|resp| (provider, resp))
                .map_err(ApiError::from)
        } else {
            // Build the capability check for the failover path.
            let cap_required = tt_shared::RequiredCapabilities::from_request(&req);
            let cap_est_tokens = {
                let combined = tt_shared::message_text_for_estimation(&req);
                tt_tokenize::estimate_tokens(provider.id(), &combined) as u64
            };
            crate::failover::dispatch_with_failover(
                &state.registry,
                &state.breaker,
                retry_policy,
                &failover_candidates,
                &req,
                ctx,
                &failover_creds,
                Utc::now(),
                Some(crate::failover::CapCheck {
                    required: &cap_required,
                    estimated_tokens: cap_est_tokens,
                }),
                failover_cost_check,
            )
            .await
            .map_err(map_failover_error)
        }
    });

    // Canary SHADOW dispatch (#454): when the matched route declares a
    // `shadow_model`, run it CONCURRENTLY with the primary — same prompt,
    // shadow model, non-streaming, single candidate, NO failover, its own
    // short deadline. The shadow response is DISCARDED; only its cost is kept
    // (in a separate column / span attr). Opt-in only: `route_shadow_model`
    // is None for every request whose route did not set it, so the default
    // path runs the primary alone with zero added work. We base the shadow on
    // `req` AFTER redaction/compression so it exercises the exact prompt the
    // primary dispatches. `tokio::join!` polls both on this task so the
    // shadow never blocks the primary beyond their concurrent overlap.
    let (dispatch_result, shadow_outcome): (ApiResult<_>, Option<ShadowOutcome>) =
        if let Some(shadow_model) = route_shadow_model.as_deref() {
            let shadow_fut = dispatch_shadow(state, ctx, &req, shadow_model, &raw_bearer);
            let (primary, shadow) = tokio::join!(primary_dispatch, shadow_fut);
            (primary, Some(shadow))
        } else {
            (primary_dispatch.await, None)
        };

    // Attributed to the primary provider: the request deadline spans any
    // failover loop, so the in-flight candidate at timeout isn't known here
    // without threading it out of dispatch_with_failover.
    if matches!(dispatch_result, Err(ApiError::RequestTimeout { .. })) {
        crate::metrics::record_provider_timeout(__primary, "chat");
    }

    // 3c-neg. Negative-cache write on deterministic client errors.
    //
    // When the provider returned a deterministic 4xx (e.g. InvalidRequest /
    // ProviderUpstream 400..=499 excluding 429), store a short-lived entry
    // in L1 under "neg:{l1_key}" so identical repeat requests are served
    // from the negative cache instead of re-hitting the provider.
    //
    // Gated on cache_behavior.do_insert — the same flag used for positive
    // cache inserts — so Bypass/ReadOnly mode suppresses negative caching
    // as well as positive caching.
    //
    // NEVER caches: 429/RateLimited, timeout, 5xx, network, internal errors.
    if let Err(ref err) = dispatch_result {
        if cache_behavior.do_insert && is_deterministic_client_error(err) {
            if let (Some(l1), Some(pos_key)) = (state.l1.as_ref(), l1_key.as_ref()) {
                let neg_key = negative_l1_key(pos_key);
                let entry = NegativeCacheEntry {
                    status: error_status_code(err),
                    message: err.to_string(),
                };
                match serde_json::to_vec(&entry) {
                    Ok(bytes) => {
                        let l1_clone = l1.clone();
                        tokio::spawn(async move {
                            if let Err(e) = l1_clone
                                .cache
                                .set(&neg_key, &bytes, NEGATIVE_CACHE_TTL_SECS)
                                .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    key = %neg_key,
                                    "negative cache insert failed"
                                );
                            } else {
                                tracing::debug!(
                                    key = %neg_key,
                                    "negative cache entry stored"
                                );
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "negative cache entry serialization failed"
                        );
                    }
                }
            }
        }
        // Also drop any single-flight guard so followers don't wait forever.
        drop(single_flight_guard.take());
    }

    // P0-1/P0-3 budget accounting (B3): a fully-failed dispatch (provider 5xx /
    // timeout / failover-exhausted) returns HERE via `?`, BEFORE the
    // `spend_sink().settle(...)` and the `cached: false` `request_logs` row
    // below. This is deliberate: a failed request writes NO billable
    // `request_logs` row (the row is spawned only on the 200 success path), so
    // settle-on-success-only keeps the gateway counters CONSISTENT with the
    // cloud overage meter (which counts `request_logs WHERE NOT cached`). A
    // failed request therefore advances neither the billed nor the served
    // counter — the per-minute rate window (counted pre-flight in `check`)
    // backstops abuse via repeated failing requests.
    let (provider, response) = dispatch_result?;
    let mut response = response;

    // ── Response-side output shaping (research Phase 3.3 + 3.4) ─────────
    //
    // Runs BEFORE pricing/caching/telemetry so every downstream consumer
    // (cost math, L1 envelope, request_logs, judge) sees the response the
    // CALLER receives. Neither arm can 5xx: format-switch fails OPEN
    // (untouched body), diff fails CLOSED to a full re-emit — and if even
    // the re-emit dispatch errors, the raw patch passes through marked
    // `diff_degraded` (a 5xx after a successful upstream call is the
    // worst outcome).
    let mut shape_effects = crate::shaping::ShapeEffects::default();
    let mut format_switch_outcome: Option<&'static str> = None;
    let mut format_switch_failed = false;
    let mut diff_applied = false;
    let mut diff_failed = false;
    if let Some(plan) = format_switch_plan.as_ref() {
        // Tool-call-only responses yield empty assistant text → the
        // validator rejects → fail-open, same arm as a prose mismatch.
        let body = response_assistant_text(&response);
        // Truncation gate FIRST: a max-token-cut emission can end
        // exactly at a record-line boundary and pass the per-line arity
        // check, silently serving a SHORTER record set than the model
        // intended — whereas the JSON contract being replaced would
        // have been detectably invalid on truncation. The switched
        // contract must not be weaker, so a non-"stop" finish_reason
        // fails OPEN like any other validation mismatch.
        let validated = if response_emission_truncated(&response) {
            Err("truncated")
        } else {
            crate::shaping::format_switch::validate_switched_body(&body, &plan.format)
        };
        match validated {
            Ok(stripped) => {
                // Tokenizer-grounded ESTIMATE only (labeled "Est"
                // everywhere): not computable ⇒ book $0 + meter.
                match crate::shaping::format_switch::estimate_saved_tokens(
                    provider.id(),
                    &response.model,
                    &stripped,
                    &plan.format,
                ) {
                    Some(saved_tokens) => {
                        if let Some(p) = provider.pricing(&response.model) {
                            shape_effects.format_switch_saved_est_usd =
                                f64::from(saved_tokens) * p.output_per_million / 1_000_000.0;
                        }
                    }
                    None => crate::metrics::record_format_switch_unestimated(),
                }
                set_assistant_text(&mut response, stripped);
                warnings.push(format!("format_switch:{}", plan.label));
                format_switch_outcome = Some(plan.label);
                crate::metrics::record_format_switch(plan.label, "applied");
            }
            Err(_reason) => {
                // Fail OPEN: the body passes through untouched, $0
                // booked, and 3e below never caches an unswitched body
                // under the switched key.
                warnings.push(format!("format_switch_failed:{}", plan.label));
                format_switch_failed = true;
                crate::metrics::record_format_switch(plan.label, "failed");
            }
        }
    }
    if let Some(plan) = diff_plan.as_ref() {
        let patch_text = response_assistant_text(&response);
        // Truncation gate FIRST (fail closed): a patch cut by the
        // provider's token limit exactly at a `>>>>>>> REPLACE`
        // boundary parses as a valid-but-INCOMPLETE multi-block patch
        // and would silently serve a PARTIALLY edited artifact — the
        // unique-anchor and JSON-validity checks cannot detect a
        // missing trailing block. Mid-block truncation already fails
        // closed as Malformed; this closes the boundary case.
        let reconstructed = if response_emission_truncated(&response) {
            Err(crate::shaping::diff::DiffError::Truncated)
        } else {
            crate::shaping::diff::reconstruct(plan, &patch_text)
        };
        match reconstructed {
            Ok(artifact) => {
                // MEASURED saving: both sides are real tokenizer counts
                // on real strings — the reconstructed artifact the caller
                // receives vs the patch tokens the provider billed.
                let artifact_tokens = tt_tokenize::estimate_tokens_for_model(
                    provider.id(),
                    &response.model,
                    &artifact,
                );
                let billed = u32::try_from(response.usage.completion_tokens).unwrap_or(u32::MAX);
                shape_effects.diff_output_tokens_saved = artifact_tokens.saturating_sub(billed);
                set_assistant_text(&mut response, artifact);
                // `usage` deliberately STAYS the provider-billed patch
                // usage — the invoice shows the short patch; the savings
                // being real is the point (documented contract).
                warnings.push("diff_applied".to_string());
                diff_applied = true;
                crate::metrics::record_diff("applied", "");
            }
            Err(e) => {
                // FAIL CLOSED → full re-emit. Book the failed attempt's
                // realized (pre-fee) cost first — it is real invoice
                // spend.
                let patch_pricing = provider.pricing(&response.model);
                shape_effects.diff_failed_cost_usd = compute_cost(
                    &response.usage,
                    patch_pricing.as_ref(),
                    patch_pricing.as_ref(),
                    1.0,
                )
                .cost_usd;
                warnings.push(format!("diff_failed:{}", e.reason()));
                diff_failed = true;
                crate::metrics::record_diff("failed", e.reason());
                // The re-emit request is derived from the DISPATCHED
                // request — drop the patch instruction, restore the
                // caller's response_format — NOT from a pre-pipeline
                // clone, so it inherits every dispatch-path
                // normalization: the redaction guardrail (a
                // redact+diff route's re-emit must never out-leak the
                // dispatch path — same invariant that skips the judge
                // wholesale on redact routes), compression, the flex
                // tier (`compute_cost_full` prices the metered re-emit
                // usage with `flex_applied`), and the temperature
                // clamp. The restored response_format then needs the
                // provider-compat downgrade the patch dispatch never
                // needed (its response_format was None).
                let mut reemit_req = req.clone();
                crate::shaping::diff::unapply_diff_request(&mut reemit_req, plan);
                maybe_downgrade_response_format(&mut reemit_req, provider.as_ref(), &mut warnings);
                // Single provider, no failover chain — the chain already
                // chose this provider for the patch dispatch.
                let reemit = with_request_timeout(request_timeout, async {
                    with_retry(retry_policy, || {
                        provider.chat_completion(reemit_req.clone(), ctx)
                    })
                    .await
                    .map_err(ApiError::from)
                })
                .await;
                match reemit {
                    Ok(full) => response = full,
                    Err(err) => {
                        // Last resort: never 5xx after a successful
                        // upstream call — serve the raw patch response
                        // marked degraded. The trace billed exactly ONE
                        // dispatch (the failed re-emit errored, nothing
                        // billed), and that dispatch IS the response
                        // being metered below — so the separate
                        // failed-attempt booking must be zeroed or
                        // cost_usd would double-count the patch call.
                        shape_effects.diff_failed_cost_usd = 0.0;
                        tracing::warn!(
                            error = %err,
                            "diff fail-closed re-emit dispatch failed — serving raw patch marked diff_degraded"
                        );
                        warnings.push("diff_degraded".to_string());
                        crate::metrics::record_diff("degraded", "reemit_error");
                    }
                }
            }
        }
    }

    // 3d. Compute cost via provider pricing table BEFORE caching — the L1
    //     envelope carries baseline_cost_usd so hit responses can report
    //     accurate savings without re-running pricing later.
    let pricing = provider.pricing(&response.model);

    // Warn when a model is absent from the pricing catalog so the request
    // is priced at $0. This is distinct from a local provider (ollama,
    // vllm, lmstudio) where pricing() intentionally returns Some(zero) —
    // those providers never return None. A None here means the model is
    // simply missing from data/pricing.toml and the cost will be recorded
    // as zero, silently under-counting spend. Update pricing.toml to fix.
    if pricing.is_none() {
        tracing::warn!(
            provider = provider.id(),
            model = %response.model,
            "model absent from pricing catalog — request cost recorded as $0; \
             update data/pricing.toml to restore accurate cost tracking"
        );
        metrics::counter!(
            "catalog_zero_price_total",
            "provider" => provider.id(),
            "model" => response.model.clone(),
        )
        .increment(1);
    }

    // Baseline is priced against the originally-requested model when a route
    // rewrote it; otherwise against the served model (same pricing → no
    // routing saving, only cache/discount savings).
    let baseline_pricing = if matched_route_id.is_some() {
        requested_pricing.clone()
    } else {
        pricing.clone()
    };
    // Minify ESTIMATE (research Phase 3.1): grounded in the actual
    // emission — the pretty re-render of the emitted JSON re-tokenized
    // with the served model's tokenizer, minus the tokens actually
    // emitted. 0 when the instruction was not injected or the response is
    // not valid JSON (no claim).
    let minify_saved_tokens = if minify_applied {
        minify_saved_tokens_est(provider.id(), &response.model, &response)
    } else {
        0
    };
    let mut cost_breakdown = compute_cost_full(
        &response.usage,
        pricing.as_ref(),
        baseline_pricing.as_ref(),
        provider.fee_multiplier(),
        flex_applied,
        batch_marked,
        pass_effects,
        minify_saved_tokens,
        shape_effects,
    );
    // Document Lane D4c-v2: price the isolated vision-avoided saving from the
    // post-match seam's bookkeeping (raw image tokens the distilled-away image
    // parts WOULD have spent vs the distilled text tokens they now spend) via
    // D0's `document_projection::project`, at the served model's input rate.
    // `project` applies the Gemini direction guard ($0 for Gemini — page-images
    // are priced flat and cheaper than distilled text) + clamps negatives to 0.
    // Fail-open to $0: no pricing, nothing distilled (the common no-sidecar /
    // no-image path), or a $0 projection → the field stays at its compute_cost_full
    // default of 0.0. NEVER folded into cost_usd/baseline/tt_saved_usd (it is a
    // counterfactual the request never sent — not invoice-reconcilable).
    // A fallback may serve a different model than the one used to distill the
    // request. Its image/text token formulas can differ, so fail open to $0
    // rather than price a counterfactual with the wrong model's rate.
    if doc_distill_booking.distilled_parts > 0 && response.model == req.model {
        if let Some(p) = pricing.as_ref() {
            let proj = tt_preview::document_projection::project(
                doc_distill_booking.raw_image_tokens,
                doc_distill_booking.distilled_text_tokens,
                p.input_per_million,
                &response.model,
            );
            cost_breakdown.doc_vision_saved_est_usd = proj.projected_savings_usd;
        }
    }
    if minify_applied {
        crate::metrics::record_minify_estimate(
            route_matched_name.as_deref().unwrap_or("none"),
            minify_saved_tokens,
            cost_breakdown.minify_saved_est_usd,
        );
    }
    let cost_usd = cost_breakdown.cost_usd;
    let baseline_cost_usd = cost_breakdown.baseline_cost_usd;
    let request_delta_evidence_state =
        cost_breakdown.request_delta_evidence_state(pricing.is_some(), baseline_pricing.is_some());
    crate::metrics::record_request_measurement("chat", request_delta_evidence_state);
    // headline saved_usd (header) is TT-attributed only — the provider's
    // automatic cache discount is excluded by `CostBreakdown::tt_saved_usd`
    // and surfaced via its own header/ledger field.
    let provider_cache_saved_usd = cost_breakdown.provider_cache_saved_usd;

    // Record realized spend into the same enforcer the pre-flight check uses
    // (dynamic_budget on the tier-aware path) so the monthly_cap_usd hard stop trips.
    state
        .spend_sink()
        .record(ctx.org_id, ctx.api_key_id, cost_usd, Utc::now());
    // P0-1/P0-3: settle the served request. This is the dispatched (non-cached)
    // tail — the request hit the provider — so it advances BOTH the billed
    // monthly counter and the served counter. Cache hits settle with
    // `cached=true` at the `complete_once` consumer (they never reach here).
    state
        .spend_sink()
        .settle(ctx.org_id, ctx.api_key_id, false, Utc::now());

    let provider_id = provider.id().to_string();
    let model_used = response.model.clone();
    // Token counts for the request-span attributes, captured before
    // `response` is moved into the HTTP body below.
    let input_tokens = response.usage.prompt_tokens;
    let output_tokens = response.usage.completion_tokens;

    // 3e. Best-effort L1 insert. Gated on do_insert (Fix A + Fix B) and
    //     the response not containing tool_calls (non-deterministic output).
    //     Errors are logged but never block the request.
    //
    //     Single-flight note: when this request is the single-flight leader
    //     we must ensure the L1 entry is visible before signalling followers.
    //     To do that we await the insert inline (rather than spawning it) and
    //     then call guard.complete().  When no guard is held the normal
    //     fire-and-forget spawn is used so the non-coalesced path is unchanged.
    let response_has_tools = response_has_tool_calls(&response);
    // `!format_switch_failed`: a fail-open (unswitched) body must never
    // be cached under the switched request's key — every hit under a
    // switched key must be genuinely switched (the hit-path
    // advertisement above depends on this invariant).
    if cache_behavior.do_insert && !response_has_tools && !format_switch_failed {
        if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key) {
            let entry = L1Entry::new(
                response.clone(),
                baseline_cost_usd,
                cost_usd,
                provider_id.clone(),
                request_delta_evidence_state,
            );
            match entry.to_bytes() {
                Ok(bytes) => {
                    let l1_clone = l1.clone();
                    // TTL priority: tt_extras override > tier-based TTL >
                    // L1 config default (spec §8.4 / rv-per-tier-ttl).
                    let ttl =
                        effective_ttl_secs(cache_behavior.ttl_secs, caller_tier, l1_clone.ttl_secs);
                    if let Some(guard) = single_flight_guard.take() {
                        // Leader path: await the insert so followers can
                        // read it from L1 immediately after we signal them.
                        if let Err(e) = l1_clone.cache.set(&key, &bytes, ttl).await {
                            tracing::warn!(error = %e, "l1 cache insert failed (leader)");
                            // Drop guard without calling complete() — followers
                            // will fall through to their own dispatch.
                            drop(guard);
                        } else {
                            // Insert succeeded; signal followers.
                            guard.complete();
                        }
                    } else {
                        // Non-leader path: fire-and-forget as before.
                        tokio::spawn(async move {
                            if let Err(e) = l1_clone.cache.set(&key, &bytes, ttl).await {
                                tracing::warn!(error = %e, "l1 cache insert failed");
                            }
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "l1 envelope serialization failed");
                    // Guard drops here (single_flight_guard still Some if
                    // we didn't take it above); followers fall through.
                    drop(single_flight_guard.take());
                }
            }
        } else {
            // No L1 configured — drop any guard so followers fall through.
            drop(single_flight_guard.take());
        }
    } else {
        // Insert skipped (tool calls or do_insert=false) — drop guard so
        // followers are not left waiting indefinitely.
        drop(single_flight_guard.take());
    }

    // 3f. Best-effort L2 insert. Same gate as L1, plus the format-switch
    // L2 opt-out (`skip_l2`): a switched body must never become a
    // similarity-served answer for an unswitched near-duplicate.
    if cache_behavior.do_insert && !response_has_tools && l2_allowed && !skip_l2 {
        if let Some(l2) = state.l2.as_ref() {
            if let Some(query_text) = l2_context_text(&req) {
                let l2_provider_id = provider_id.clone();
                let l2_model_used = response.model.clone();
                let response_clone = response.clone();
                let l2_clone = l2.clone();
                let org_id = ctx.org_id;
                // TTL priority: tt_extras override > tier-based TTL >
                // L2_DEFAULT_TTL (spec §8.4 / rv-per-tier-ttl).
                let l2_ttl_secs = effective_ttl_secs(
                    cache_behavior.ttl_secs,
                    caller_tier,
                    L2_DEFAULT_TTL.as_secs(),
                );
                // Volatility-class TTL (opt-in, shorten-only, L2-scoped):
                // a volatile query's entry expires sooner so a stale
                // realtime answer can't be re-served for the full base
                // TTL. An explicit tt_extras override always wins.
                let l2_ttl_secs = crate::cache_volatility::l2_ttl_with_volatility(
                    l2_ttl_secs,
                    cache_behavior.ttl_secs.is_some(),
                    &query_text,
                    l2.volatility_ttl.as_ref(),
                );
                // Store the catalog-derived baseline on the row so later
                // hits report honest savings. None (→ NULL) when the model
                // is absent from the catalog: the hit path then re-prices
                // against the catalog current at hit time instead of
                // freezing a meaningless $0.
                let l2_baseline = pricing.as_ref().map(|_| baseline_cost_usd);
                // Reuse the embedding the L2 lookup already computed for the
                // identical query text (COST-3); `None` when no lookup ran
                // (e.g. do_lookup=false) — `insert_into_l2` then embeds.
                let l2_lookup_embedding = l2_lookup_vec.take();
                tokio::spawn(async move {
                    insert_into_l2(
                        l2_clone,
                        org_id,
                        &query_text,
                        response_clone,
                        l2_provider_id,
                        l2_model_used,
                        l2_ttl_secs,
                        l2_baseline,
                        request_delta_evidence_state,
                        l2_lookup_embedding,
                    )
                    .await;
                });
            }
        }
    }

    // 3g. Best-effort request_logs row. Cache-miss path: cached=false,
    //     cache_layer=None. L1/L2-hit paths log their own rows where
    //     they early-return.
    //
    // Canary shadow attribution: when a shadow fired, record its model +
    // cost in their OWN columns (NEVER folded into `cost_usd`). `shadow_cost`
    // is `Some` only when the shadow succeeded (a failed shadow still logs
    // its model with `None` cost so the attempt is auditable). The doubled
    // spend is thus visible and reconcilable as a distinct experiment cost.
    let shadow_model_logged = shadow_outcome.as_ref().map(|s| s.model.clone());
    let shadow_cost_logged = shadow_outcome
        .as_ref()
        .filter(|s| s.succeeded)
        .map(|s| s.cost_usd);
    let mut log_row = RequestLogRow {
        id: Uuid::now_v7(),
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        ts: Utc::now(),
        provider: provider_id.clone(),
        // Preserve the exact matcher input separately from the final served
        // model so historical `model_in` evidence never has to infer it from
        // routing/failover output.
        requested_model: Some(requested_model.clone()),
        model: model_used.clone(),
        input_tokens: response.usage.prompt_tokens.min(i32::MAX as u64) as i32,
        output_tokens: response.usage.completion_tokens.min(i32::MAX as u64) as i32,
        cached_tokens: response.usage.cached_tokens.min(i32::MAX as u64) as i32,
        cost_usd,
        baseline_cost_usd,
        provider_cache_saved_usd,
        // Fee-applied, matching the header/span figure — keeps the
        // row-derived TT headline equal to `tt_saved_usd()`.
        cache_bust_penalty_usd: cost_breakdown.cache_bust_penalty_usd,
        // Persist every cost-breakdown component surfaced by the response so
        // the dashboard/reporting plane never has to infer a zero from a
        // missing column. `summarizer_tax_usd` remains a tax, not saving.
        flex_saved_usd: cost_breakdown.flex_saved_usd,
        doc_compaction_saved_usd: cost_breakdown.doc_compaction_saved_usd,
        summarizer_tax_usd: cost_breakdown.summarizer_tax_usd,
        request_delta_evidence_state,
        cached: false,
        cache_layer: None,
        route_id: matched_route_id,
        route_version_id: matched_route_version_id,
        latency_ms: request_started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        upstream_latency_ms: None,
        status: 200,
        tag: ctx.tag.clone(),
        error_class: None,
        trace_id: Some(trace_id.to_string()),
        truncated: false,
        shadow_model: shadow_model_logged.clone(),
        shadow_cost_usd: shadow_cost_logged,
        traffic_split_arm: traffic_split_arm_owned.clone(),
        // Raw provider prompt-cache counts (research Phase 0.2):
        // None (NULL) when the provider didn't report the field,
        // Some(0) when it explicitly reported zero. The shadow
        // dispatch is discarded — only the SERVED response's cache
        // telemetry is recorded (shadow cost has its own columns).
        cache_read_input_tokens: opt_tokens_i32(response.usage.cache_read_input_tokens),
        cache_creation_input_tokens: opt_tokens_i32(response.usage.cache_creation_input_tokens),
        // Advisory batch-eligibility marker (research Phase 2.1).
        // `batch_eligible` records route INTENT (the marker survived
        // the hard-ineligibility gate); `batch_forgone_usd` is the
        // PRICED claim — 0.0 when failover served a model with no
        // catalog batch tier, while `batch_eligible` stays true so the
        // route's intent remains auditable.
        batch_eligible: batch_marked,
        batch_forgone_usd: cost_breakdown.batch_forgone_usd,
        route_paused,
        // ESTIMATED minify saving — own column (migration 0020),
        // never folded into cost/baseline/saved.
        minify_saved_est_usd: cost_breakdown.minify_saved_est_usd,
        // Output shaping (research Phase 3.3 + 3.4). `format_switched`
        // is set ONLY on a VALIDATED switch; the est/saved/failed-cost
        // figures come from the same breakdown the headers carry.
        format_switched: format_switch_outcome.map(str::to_string),
        format_switch_saved_est_usd: cost_breakdown.format_switch_saved_est_usd,
        diff_applied,
        diff_saved_usd: cost_breakdown.diff_saved_usd,
        diff_failed,
        diff_failed_cost_usd: cost_breakdown.diff_failed_cost_usd,
        retrieval_tokens_saved: retrieval_telemetry.tokens_saved,
        // Document Lane D2: token-denominated record of what the lossless
        // doc-compaction pass removed (0 unless the route opted in).
        doc_compaction_tokens_removed: pass_effects.doc_compaction_tokens_removed as i64,
        // TR-2: the conservative `compress` pass's MEASURED saving + token
        // count (0 unless the route opted into `compress`). The USD figure is
        // the fee-applied value already on `cost_breakdown` (what the
        // `X-TokenTrimmer-Compression-Saved-Usd` header carries); the token
        // count is the raw `pass_effects.compression_tokens_removed`. Both
        // fold into the saved-usd headline via the baseline fold (above).
        compression_saved_usd: cost_breakdown.compression_saved_usd,
        compression_tokens_removed: pass_effects.compression_tokens_removed as i64,
        // Document Lane D4c-v2: ISOLATED, ESTIMATED vision-avoided saving (own
        // column, migration 0032; never folded into cost/baseline/saved). Priced
        // from the post-match seam's DistillBooking via D0's
        // document_projection::project (Gemini guard + fail-open); $0 when the
        // route did not opt in / the sidecar is disabled / nothing distilled.
        doc_vision_saved_est_usd: cost_breakdown.doc_vision_saved_est_usd,
        // Agent-run grain (W0b Task 4): stamped via `attribute_run` below
        // so the ctx→row mapping is independently unit-testable.
        run_id: None,
        node_id: None,
        // Content-aware compression (P1a): the ISOLATED estimated saving (own
        // column, migration 0033; never folded into cost/baseline/saved) + the
        // flywheel kind label, recorded only when the pass removed tokens.
        content_compress_saved_est_usd: cost_breakdown.content_compress_saved_est_usd,
        content_compress_kind: if pass_effects.content_compress_tokens_removed > 0 {
            content_compress_kind.clone()
        } else {
            None
        },
        // This is a dispatched row (not an L2 hit) → no L2 provenance.
        l2_matched_entry_id: None,
        l2_similarity: None,
        l2_verdict: None,
    };
    // Agent-run grain (W0b Task 4): inherit run_id/node_id from ctx so every
    // row produced under an agent run carries the run's id.  `None` for
    // standalone (non-agent) requests.  Extracted into `attribute_run` so the
    // mapping can be verified by the `attribute_run_copies_run_and_node_id`
    // unit test without needing a live provider.
    attribute_run(&mut log_row, ctx);
    spawn_request_log(
        state.telemetry_tracker.as_ref(),
        state.request_log_writer.as_ref(),
        log_row,
    );

    // Whether this trace's body was actually handed to the capture sink for
    // persistence: `capture_request_json` is `Some` only when a writer is
    // armed, the org is non-anonymous, AND the org opted in
    // (`is_capture_enabled` above), so the header reflects a body that was
    // truly stored. Captured before the Option is consumed so the response
    // can advertise it.
    let body_captured = capture_request_json.is_some();
    if let Some(request_json) = capture_request_json {
        let response_json = match serde_json::to_vec(&response) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!(error = %e, "response body capture serialization failed");
                None
            }
        };
        spawn_body_capture(
            state.telemetry_tracker.as_ref(),
            state.body_capture_writer.as_ref(),
            BodyCaptureRecord {
                org_id: ctx.org_id,
                api_key_id: ctx.api_key_id,
                trace_id: trace_id.to_string(),
                endpoint: "/v1/chat/completions".into(),
                provider: provider_id.clone(),
                model: model_used.clone(),
                request_json,
                response_json,
                // TR-3: only persist the pre-compression snapshot when the
                // compress pass actually committed (removed > 0 tokens) — a pass
                // that removed nothing produces an identical before/after, so
                // storing it would waste bytes + show an empty diff. The
                // snapshot was captured in `prepare` only on the compress path.
                pre_compression_request_json: if pass_effects.compression_tokens_removed > 0 {
                    pre_compression_request_json
                } else {
                    None
                },
                ts: Utc::now(),
            },
        );
    }

    // Per-route provider-cache counters from the same authoritative usage
    // the row records.
    crate::metrics::record_provider_cache_usage(
        &provider_id,
        route_matched_name.as_deref(),
        response.usage.cache_read_input_tokens,
        response.usage.cache_creation_input_tokens,
    );

    // 3h. Sampled async quality judge on rerouted-DOWN traffic. Spawns a
    //     detached task ONLY when: the judge is enabled + a sink is wired,
    //     a route rewrote the model, the served model is cheaper than the
    //     originally-requested one (a true downgrade priced on realized
    //     usage), this task class (chat-completions) is in scope, and the
    //     trace falls in the deterministic ~2% sample. The judge runs AFTER
    //     this point and never touches `http_response`, so it adds ZERO
    //     latency to the user request (see `quality_sample::spawn_quality_judge`).
    maybe_spawn_quality_judge(
        state,
        matched_route_id,
        &requested_model,
        &response,
        requested_pricing.as_ref(),
        pricing.as_ref(),
        // Output-shaped requests are judge-eligible even without a price
        // downgrade — the un-shaped pre-routing capture is the paired
        // counterfactual.
        minify_applied || reasoning_capped,
        trace_id,
        ctx.org_id,
        &raw_bearer,
        judge_source_provider,
        judge_source_ctx,
        judge_original_req,
        // A shaped response (validated format-switch or applied diff)
        // samples the judge even without a model downgrade — shaping is
        // exactly what the #155 gate exists to police.
        format_switch_outcome.is_some() || diff_applied,
        // P2a: a content_compress-only same-model request is judge-eligible when
        // the pass removed tokens (the label-gap closure — see the predicate).
        pass_effects.content_compress_tokens_removed > 0,
    );

    // 5. Return the typed response + header metadata. The chat wrapper (and
    //    the agent loop) build the HTTP response from this; here we only
    //    compute the cache-state header value and record the request-span
    //    attributes (same `tracing::Span::current()` as the wrapper tail).
    // Cache state: miss when ANY cache layer is configured but didn't hit;
    // none when both are disabled.
    let cache_state = if state.l1.is_some() || state.l2.is_some() {
        "miss"
    } else {
        "none"
    };

    // Record OTel GenAI semconv + TokenTrimmer cost attributes on the
    // request span from the same per-request values the headers carry (no
    // recompute). The served model may differ from the requested one after
    // routing / cross-model failover.
    record_request_span_attributes(
        &requested_model,
        &model_used,
        &provider_id,
        span_cost(&cost_breakdown, input_tokens, output_tokens),
        cache_state,
        route_matched_name.as_deref(),
        // Canary span attributes — additive; each omitted when its value is
        // None (no split / no shadow). The shadow cost is recorded SEPARATELY
        // here, never folded into `tokentrimmer.cost_usd` (carried by
        // `cost_breakdown` above).
        route_traffic_pct,
        shadow_model_logged.as_deref(),
        shadow_cost_logged,
    );

    Ok(CompletionOutcome::Dispatched {
        response,
        headers: Box::new(CompletionHeaders {
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
            // Single-model dispatch never carries a panel body (off-by-default).
            panel_body: None,
        }),
    })
}
