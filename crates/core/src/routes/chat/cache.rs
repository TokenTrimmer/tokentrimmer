//! Cache eligibility, lookup, replay, insertion, and attribution policy.

use super::*;
/// L2 cache TTL for newly-inserted entries. Spec §8.4 caps this per-tier
/// (24h / 7d / 30d); the gateway-level default is conservative until the
/// auth layer surfaces the caller's tier.
pub(super) const L2_DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// TTL for negative-cache entries (deterministic 4xx errors).
///
/// Short by design: a client error cached for too long would prevent legitimate
/// retries after the caller fixes their request.  60 s is enough to protect
/// against hot-loop bad-request storms while expiring fast enough not to
/// confuse operators.
pub(super) const NEGATIVE_CACHE_TTL_SECS: u64 = 60;

/// Key prefix that separates negative-cache entries from positive-cache entries
/// in the shared L1 store.
pub(super) const NEGATIVE_CACHE_PREFIX: &str = "neg:";

// ---------------------------------------------------------------------------
// Negative-cache helpers (rv-cache-key-canonicalization §2.20)
// ---------------------------------------------------------------------------

/// A minimal, serializable record stored as a negative-cache entry.
///
/// Contains just enough to reconstruct the original error response when the
/// negative cache is hit.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct NegativeCacheEntry {
    /// HTTP status code of the original error response.
    pub(super) status: u16,
    /// Human-readable error message, preserved verbatim.
    pub(super) message: String,
}

/// Returns `true` when `err` is a **deterministic** client error that is safe
/// to negative-cache.
///
/// The only errors that qualify are those that will produce the same failure
/// for every identical re-submission of the same request:
/// - `ProviderError::InvalidRequest` (upstream 400 / 422)
/// - `ProviderError::ProviderUpstream` with a 4xx status OTHER THAN 429
///   (which is rate-limited and transient)
/// - `ApiError::InvalidRequest` (our own 400 validation — e.g. malformed JSON)
///
/// Errors that MUST NOT be negative-cached:
/// - 429 / `RateLimited` — transient; retry must always reach the provider.
/// - 408 / `Timeout` — transient.
/// - 5xx / `ProviderUpstream { status >= 500 }` — server-side transient.
/// - `Network` — transient.
/// - `Internal` / `Deserialize` — our own bugs; must not suppress retries.
pub(super) fn is_deterministic_client_error(err: &ApiError) -> bool {
    use tt_shared::ProviderError;
    match err {
        // Our own 400 validation before even hitting the provider.
        ApiError::InvalidRequest(_) | ApiError::RouteValidation { .. } => true,
        // Provider returned a deterministic 4xx (but NOT 429).
        ApiError::Provider(pe) => match pe {
            ProviderError::InvalidRequest(_) => true,
            ProviderError::ProviderUpstream { status, .. } => {
                // 4xx client errors are deterministic; 429 is rate-limited
                // (transient) so it is explicitly excluded.
                *status >= 400 && *status < 500 && *status != 429
            }
            // Transient or server-side errors — never cache.
            ProviderError::RateLimited { .. }
            | ProviderError::BudgetExceeded { .. }
            | ProviderError::BudgetPriceUnknown { .. }
            | ProviderError::BudgetUnavailable(_)
            | ProviderError::Timeout { .. }
            | ProviderError::Network(_)
            | ProviderError::ModelNotFound { .. }
            | ProviderError::Unauthorized(_)
            | ProviderError::Deserialize(_)
            | ProviderError::Internal(_)
            | ProviderError::Unsupported(_) => false,
        },
        // All other ApiError variants are either auth/rate/server errors — never cache.
        ApiError::Unauthorized
        | ApiError::PaymentRequired
        | ApiError::Forbidden(_)
        | ApiError::ModelNotFound { .. }
        // Config-dependent (org may add the credential / raise the ceiling) —
        // must not negative-cache.
        | ApiError::MissingProviderCredential { .. }
        | ApiError::PanelCredentialPreflight { .. }
        | ApiError::PriceUnknown { .. }
        | ApiError::CostLimitExceeded { .. }
        | ApiError::RateLimited { .. }
        | ApiError::RequestTimeout { .. }
        | ApiError::Internal(_)
        | ApiError::NotFound(_)
        | ApiError::ServiceUnavailable(_)
        // Agent-run control-flow signal (not a provider response) — never cache.
        | ApiError::Conflict(_)
        // Panel errors — kill-switch can be toggled, and panel conditions are
        // runtime-dependent. Never negative-cache any of them.
        | ApiError::PanelDisabled
        | ApiError::PanelQuorumUnmet { .. }
        | ApiError::PanelModelCapabilityUnavailable { .. }
        | ApiError::PanelStrategyUnsupported { .. } => false,
    }
}

/// Derive the HTTP status code that would be returned for `err`.
pub(super) fn error_status_code(err: &ApiError) -> u16 {
    use axum::http::StatusCode;
    use tt_shared::ProviderError;
    let status: StatusCode = match err {
        ApiError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        ApiError::RouteValidation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
        ApiError::PaymentRequired => StatusCode::PAYMENT_REQUIRED,
        ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
        ApiError::ModelNotFound { .. } => StatusCode::NOT_FOUND,
        ApiError::MissingProviderCredential { .. } => StatusCode::BAD_REQUEST,
        ApiError::PanelCredentialPreflight { .. } => StatusCode::BAD_REQUEST,
        ApiError::PriceUnknown { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        ApiError::CostLimitExceeded { .. } => StatusCode::PAYMENT_REQUIRED,
        ApiError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        ApiError::RequestTimeout { .. } => StatusCode::REQUEST_TIMEOUT,
        ApiError::Provider(pe) => match pe {
            ProviderError::BudgetExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            ProviderError::BudgetPriceUnknown { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            ProviderError::BudgetUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ProviderError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            ProviderError::ProviderUpstream { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            _ => StatusCode::BAD_GATEWAY,
        },
        ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ApiError::NotFound(_) => StatusCode::NOT_FOUND,
        ApiError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        ApiError::Conflict(_) => StatusCode::CONFLICT,
        ApiError::PanelDisabled => StatusCode::FORBIDDEN,
        ApiError::PanelQuorumUnmet { .. } => StatusCode::BAD_GATEWAY,
        ApiError::PanelModelCapabilityUnavailable { .. } => StatusCode::BAD_REQUEST,
        ApiError::PanelStrategyUnsupported { .. } => StatusCode::NOT_IMPLEMENTED,
    };
    status.as_u16()
}

/// Compute the L1 key for a negative-cache entry.
///
/// Uses the same namespaced positive key with a `"neg:"` prefix so the two
/// namespaces can never collide.
pub(super) fn negative_l1_key(positive_key: &str) -> String {
    format!("{NEGATIVE_CACHE_PREFIX}{positive_key}")
}

/// Select the effective cache TTL (seconds) for an insert operation.
///
/// Priority (highest → lowest):
/// 1. Per-request TTL override from `tt_extras.cache.ttl_secs` (Fix B §2.7).
/// 2. Tier-based TTL from the caller's subscription tier (spec §8.4).
/// 3. The conservative 24h gateway default when no tier is known.
///
/// The `tier` argument comes from `ApiKeyContext::tier`, which is injected by
/// the cloud tier-resolution layer (`rv-tier-limits-enforcement`). Until that
/// layer is wired, `tier` is always `None` and the 24h default applies —
/// preserving current behavior for all requests.
pub(super) fn effective_ttl_secs(
    request_override: Option<u64>,
    tier: Option<tt_shared::CallerTier>,
    default: u64,
) -> u64 {
    if let Some(secs) = request_override {
        return secs.clamp(1, crate::state::MAX_L1_TTL_SECS);
    }
    if let Some(t) = tier {
        return t.ttl_secs().min(crate::state::MAX_L1_TTL_SECS);
    }
    default.clamp(1, crate::state::MAX_L1_TTL_SECS)
}

// ---------------------------------------------------------------------------
// Cache-eligibility gate (Fix A §2.2) + tt_extras cache-control (Fix B §2.7)
// ---------------------------------------------------------------------------

/// Returns `true` when the request parameters are deterministic enough that
/// caching the response is safe and correct.
///
/// A request is NOT eligible when:
/// - `temperature` is set and > 0.0  (non-deterministic sampling)
/// - `top_p` is set and < 1.0        (nucleus sampling narrows distribution)
/// - `n` is set and > 1              (caller wants multiple distinct completions)
/// - `seed` is set                   (caller controls RNG — implies they want
///   repeatable variance, not a cached result)
///
/// Note: `seed` being set makes the request *deterministic per-model*, but
/// caching it could return a different model's output on a later lookup, which
/// violates the caller's intent. Skip caching to be safe.
pub(super) fn is_cache_eligible(req: &ChatCompletionRequest) -> bool {
    if req.temperature.is_some_and(|t| t > 0.0) {
        return false;
    }
    if req.top_p.is_some_and(|p| p < 1.0) {
        return false;
    }
    if req.n.is_some_and(|n| n > 1) {
        return false;
    }
    if req.seed.is_some() {
        return false;
    }
    true
}

/// Whether the client explicitly set `stream_options.include_usage = true`.
///
/// The gateway always forces `include_usage` upstream for its own accounting,
/// but only *forwards* an OpenAI-native final usage chunk to the client when the
/// client actually asked for one. `stream_options` is kept as an opaque JSON
/// value, so probe the `include_usage` boolean directly.
pub(super) fn client_requested_include_usage(req: &ChatCompletionRequest) -> bool {
    req.stream_options
        .as_ref()
        .and_then(|o| o.get("include_usage"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Returns `true` when any choice in the response contains tool calls.
/// Responses with tool calls are non-deterministic in call order/arguments and
/// must not be replayed from cache.
pub(super) fn response_has_tool_calls(resp: &ChatCompletionResponse) -> bool {
    resp.choices.iter().any(|c| {
        if let Message::Assistant { tool_calls, .. } = &c.message {
            !tool_calls.is_empty()
        } else {
            false
        }
    })
}

/// Consolidated cache behaviour for a single request, derived from both
/// eligibility (Fix A) and the caller's `tt_extras.cache` override (Fix B).
#[derive(Debug)]
pub(crate) struct CacheBehavior {
    /// Whether to attempt a cache lookup (L1 + L2).
    pub(super) do_lookup: bool,
    /// Whether to insert a fresh response into cache. Also gated on
    /// `response_has_tool_calls` at insert time (checked separately).
    pub(super) do_insert: bool,
    /// Per-request TTL override from `tt_extras`. `None` = use gateway default.
    pub(super) ttl_secs: Option<u64>,
}

impl CacheBehavior {
    pub(super) fn resolve(req: &ChatCompletionRequest) -> Self {
        // Fix A: structural eligibility — non-deterministic params skip both.
        if !is_cache_eligible(req) {
            return Self {
                do_lookup: false,
                do_insert: false,
                ttl_secs: None,
            };
        }

        // Fix B: parse caller's tt_extras.cache override.
        let ctrl: CacheControlConfig = parse_cache_control(&req.tt_extras).unwrap_or_default();

        match ctrl.mode {
            CacheMode::Normal => Self {
                do_lookup: true,
                do_insert: true,
                ttl_secs: ctrl.ttl_secs,
            },
            CacheMode::Bypass => Self {
                do_lookup: false,
                do_insert: false,
                ttl_secs: None,
            },
            CacheMode::Refresh => Self {
                do_lookup: false,
                do_insert: true,
                ttl_secs: ctrl.ttl_secs,
            },
            CacheMode::ReadOnly => Self {
                do_lookup: true,
                do_insert: false,
                ttl_secs: None,
            },
        }
    }

    /// Force BOTH lookup and insert off when the org has opted OUT of caching
    /// via the per-org `semantic_cache_disabled` compliance control. A no-op
    /// when `disabled` is false, so orgs that have not opted out keep today's
    /// behaviour exactly. When true this forces the request past both the L1
    /// exact-match and the L2 semantic cache (no read, no write) — the correct
    /// posture for a no-cache compliance tenant.
    pub(crate) fn apply_org_cache_disabled(&mut self, disabled: bool) {
        if disabled {
            self.do_lookup = false;
            self.do_insert = false;
        }
    }
}

/// `X-TokenTrimmer-Cache` → `(do_lookup, do_insert)` per the documented modes.
/// Absent/blank → `None`. Unknown value → `400` (the four values are documented).
pub(super) fn cache_override_from_header(headers: &HeaderMap) -> ApiResult<Option<(bool, bool)>> {
    let Some(raw) = headers
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    let v = raw.trim().to_ascii_lowercase();
    if v.is_empty() {
        return Ok(None);
    }
    let pair = match v.as_str() {
        "disabled" => (false, false),
        "read-only" => (true, false),
        "bypass" => (false, true),
        "force-write" => (true, true),
        other => {
            return Err(ApiError::InvalidRequest(format!(
                "invalid X-TokenTrimmer-Cache value: {other} (expected disabled, read-only, bypass, or force-write)"
            )))
        }
    };
    Ok(Some(pair))
}

/// Negative-cache lookup (step 3a-neg). If a prior identical request received a
/// deterministic 4xx that was stored under `neg:{l1_key}`, serve the cached
/// error immediately. `None` falls through to the positive lookups. Best-effort:
/// any cache/deserialize error is logged and treated as a miss.
pub(super) async fn try_negative_cache_hit(
    l1: &L1Config,
    l1_key: &str,
    route_matched_name: Option<&str>,
) -> Option<Response> {
    let neg_key = negative_l1_key(l1_key);
    match l1.cache.get(&neg_key).await {
        Ok(Some(bytes)) => {
            match serde_json::from_slice::<NegativeCacheEntry>(&bytes) {
                Ok(neg) => {
                    tracing::debug!(
                        key = %neg_key,
                        status = neg.status,
                        "negative cache hit — short-circuiting provider call"
                    );
                    // Reconstruct and return the cached error response.
                    let err_body = serde_json::json!({
                        "error": {
                            "message": neg.message,
                            "type": "invalid_request_error",
                            "code": "cached_client_error",
                            "param": null
                        }
                    });
                    let status = axum::http::StatusCode::from_u16(neg.status)
                        .unwrap_or(axum::http::StatusCode::BAD_REQUEST);
                    let mut resp = (status, Json(err_body)).into_response();
                    if let Ok(v) = "neg-hit".parse() {
                        resp.headers_mut().insert("x-tokentrimmer-cache", v);
                    }
                    Some(with_route_matched(resp, route_matched_name))
                }
                Err(e) => {
                    // Deserialization failure is non-fatal; fall through.
                    tracing::warn!(
                        error = %e,
                        key = %neg_key,
                        "negative cache entry deserialization failed — ignoring"
                    );
                    None
                }
            }
        }
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(error = %e, "negative cache lookup error — ignoring");
            None
        }
    }
}

/// L1 exact-match lookup (step 3a, positive — runs after the negative cache).
/// `None` falls through to L2.
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_l1_hit(
    l1: &L1Config,
    l1_key: &str,
    ctx: &RequestContext,
    tracker: Option<&tokio_util::task::TaskTracker>,
    request_log_writer: Option<&std::sync::Arc<dyn RequestLogWriter>>,
    trace_id: Uuid,
    requested_model: &str,
    request_started: Instant,
    matched_route_id: Option<Uuid>,
    matched_route_version_id: Option<i64>,
    route_paused: bool,
    retrieval_tokens_saved: i64,
    route_matched_name: Option<&str>,
) -> Option<Response> {
    match l1.cache.get(l1_key).await {
        Ok(Some(bytes)) => match L1Entry::from_bytes(&bytes) {
            Ok(entry) => {
                metrics::counter!("cache_lookups_total", "tier" => "l1", "result" => "hit")
                    .increment(1);
                // Log the L1 hit before returning. The hit baseline is
                // either the envelope's own value or the synthetic
                // fallback for pre-envelope cache rows.
                spawn_request_log(
                    tracker,
                    request_log_writer,
                    request_log_for_l1_hit(
                        &entry,
                        ctx,
                        requested_model,
                        trace_id,
                        request_started,
                        RouteLogAttribution {
                            route_id: matched_route_id,
                            route_version_id: matched_route_version_id,
                            paused: route_paused,
                        },
                        retrieval_tokens_saved,
                    ),
                );
                Some(with_route_matched(
                    build_hit_l1_response(entry, trace_id),
                    route_matched_name,
                ))
            }
            Err(e) => {
                tracing::warn!(error = %e, key = %l1_key, "l1 cache entry failed to deserialize");
                None
            }
        },
        Ok(None) => {
            metrics::counter!("cache_lookups_total", "tier" => "l1", "result" => "miss")
                .increment(1);
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "l1 lookup failed");
            None
        }
    }
}

/// The L2 [`tt_cache::TaskClass`] for a `POST /v1/chat/completions` request.
///
/// This is the single, extensible derivation point for the request's task class
/// (V1 has exactly one class: `ChatCompletions`). A future endpoint
/// (embeddings re-rank, messages-ingress, …) gets its own derivation and the
/// per-class threshold map (`L2Config::class_thresholds`) keys off the result.
pub(super) fn l2_task_class_for_chat() -> Option<tt_cache::TaskClass> {
    Some(tt_cache::TaskClass::ChatCompletions)
}

/// What the L2 false-positive verify gate decided about a candidate hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum L2VerifyDecision {
    /// `similarity >= t_eff + epsilon`, or the gate is disabled — serve as
    /// today.
    Confident,
    /// In the ambiguous band with lexical agreement `>= min_agreement` —
    /// serve (carries the agreement for telemetry).
    Verified(f32),
    /// In the ambiguous band but the entry predates migration 0018 (no
    /// signature) — fail-open serve, counted distinctly.
    Unverifiable,
    /// In the ambiguous band with agreement `< min_agreement` — treat as a
    /// MISS (fall through to a normal provider dispatch; no hit bump, no
    /// judge, no cache savings booked).
    Rejected(f32),
}

/// Pure decision function for the L2 verify gate (unit-test target).
///
/// `verify == None` (the gate off — today's default) is ALWAYS `Confident`:
/// behavior is byte-identical to the pre-gate gateway. With the gate on, a hit
/// at `similarity >= effective_threshold + epsilon` is confident; an
/// in-band hit (`effective_threshold <= similarity < + epsilon`) is checked
/// for lexical agreement between the entry's one-way signature and the
/// incoming query text. Entries without a signature (pre-0018 rows) fail
/// OPEN — the gate must never turn legacy rows into an outage.
pub(crate) fn l2_verify_decision(
    verify: Option<&L2VerifyConfig>,
    similarity: f32,
    effective_threshold: f32,
    entry_sig: Option<i64>,
    query_text: &str,
) -> L2VerifyDecision {
    let Some(verify) = verify else {
        return L2VerifyDecision::Confident;
    };
    if similarity >= effective_threshold + verify.epsilon {
        return L2VerifyDecision::Confident;
    }
    let Some(entry_sig) = entry_sig else {
        return L2VerifyDecision::Unverifiable;
    };
    let agreement = tt_cache::lexical_agreement(entry_sig, tt_cache::lexical_sig(query_text));
    if agreement >= verify.min_agreement {
        L2VerifyDecision::Verified(agreement)
    } else {
        L2VerifyDecision::Rejected(agreement)
    }
}

/// L2 semantic-cache lookup (step 3b). `None` falls through to dispatch.
/// `Some(Err(_))` preserves the original `build_hit_l2_response(...)?` error
/// propagation (a hit whose body fails to deserialize). Best-effort on the
/// embed/lookup side: those errors are treated as a miss.
///
/// `current_pricing` is the catalog rate for `req.model` (== the entry's
/// model — lookup filters on it), used only as the [`l2_entry_baseline`]
/// fallback for rows that predate migration 0010.
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_l2_hit(
    state: &AppState,
    l2: &L2Config,
    ctx: &RequestContext,
    req: &ChatCompletionRequest,
    current_pricing: Option<&ModelPricing>,
    request_log_writer: Option<&std::sync::Arc<dyn RequestLogWriter>>,
    trace_id: Uuid,
    requested_model: &str,
    request_started: Instant,
    matched_route_id: Option<Uuid>,
    matched_route_version_id: Option<i64>,
    route_paused: bool,
    retrieval_tokens_saved: i64,
    route_matched_name: Option<&str>,
    raw_bearer: &str,
    judge_source_provider: Option<&std::sync::Arc<dyn tt_shared::Provider>>,
    judge_source_ctx: Option<&RequestContext>,
    judge_original_req: Option<&ChatCompletionRequest>,
    // Out-param: the embedding computed for the lookup, surfaced so the miss
    // path can reuse it for the L2 insert instead of embedding the identical
    // query text a second time (COST-3). Set on every path that produced an
    // embedding (hit, verify-reject, or miss); left untouched only when no
    // context text exists or the embed call failed.
    lookup_embedding_out: &mut Option<Vec<f32>>,
) -> Option<ApiResult<Response>> {
    let query_text = l2_context_text(req)?;
    let query_vec = match l2.embedder.embed(&query_text).await {
        Ok(v) => v,
        Err(_) => return None,
    };
    // Surface the embedding for reuse on the miss/insert path. `query_text` is
    // derived from the (already redaction/compression-shaped) `req` and is not
    // mutated again before the insert, so this vector is the embedding of the
    // exact text the insert would otherwise re-embed.
    *lookup_embedding_out = Some(query_vec.clone());
    // Derive the L2 task class for this request. `/chat/completions` is the only
    // v1 class; the helper is the single, extensible derivation point. The
    // effective threshold is resolved through the adaptive gate when the verify
    // gate is wired (its ratchet is floored by the same `class_thresholds`
    // base, so `effective >= 0.92` holds by construction), and through the
    // static per-class config otherwise — identical to `lookup_classed`.
    let task_class = l2_task_class_for_chat();
    let effective_threshold = match l2.verify.as_ref() {
        Some(v) => v.gate.effective_threshold(task_class),
        None => l2.class_thresholds.threshold_for(task_class),
    };
    match l2
        .cache
        .lookup(
            ctx.org_id,
            &query_vec,
            effective_threshold,
            &req.model,
            l2.embedder.model(),
        )
        .await
    {
        Ok(Some((entry, similarity))) => {
            // FP verify gate (research Phase 2.2): a hit in the ambiguous band
            // just above the threshold is checked for lexical agreement before
            // it is served — an unguarded near-miss hit is a silent wrong
            // answer. Gate off (`l2.verify == None`, the default) is always
            // Confident — byte-identical to today.
            let decision = l2_verify_decision(
                l2.verify.as_ref(),
                similarity,
                effective_threshold,
                entry.lexical_sig,
                &query_text,
            );
            // Metric only when the gate is wired: a gate-off deployment's
            // telemetry stream stays identical to pre-gate builds, and the
            // counter cleanly distinguishes "gate off" (absent) from "gate
            // on, hit confident" (`result="confident"`).
            if l2.verify.is_some() {
                let verify_result = match decision {
                    L2VerifyDecision::Confident => "confident",
                    L2VerifyDecision::Verified(_) => "verified",
                    L2VerifyDecision::Unverifiable => "unverifiable",
                    L2VerifyDecision::Rejected(_) => "rejected",
                };
                metrics::counter!("cache_l2_verify_total", "result" => verify_result).increment(1);
            }
            if let L2VerifyDecision::Rejected(agreement) = decision {
                // Treated as a MISS: fall through to a normal provider
                // dispatch. No hit-count bump, no L2-hit judge, no cache log
                // row, no savings booked — automatically honest.
                tracing::debug!(
                    entry_id = %entry.id,
                    similarity,
                    agreement,
                    "L2 verify gate rejected an ambiguous-band hit — treating as miss"
                );
                metrics::counter!(
                    "cache_lookups_total", "tier" => "l2", "result" => "verify_reject"
                )
                .increment(1);
                return None;
            }
            // Whether the served hit fell in the ambiguous band (gate on,
            // t_eff <= sim < t_eff + epsilon): its judged verdict is the FP
            // estimator's signal.
            let in_band = matches!(
                decision,
                L2VerifyDecision::Verified(_) | L2VerifyDecision::Unverifiable
            );
            metrics::counter!("cache_lookups_total", "tier" => "l2", "result" => "hit")
                .increment(1);
            // Cache hit — best-effort bump and return.
            let _ = l2.cache.bump_hit_count(entry.id).await;
            // 3b-judge. Close the QualityRiskBand → L2 join: spawn the sampled
            // async quality judge on the SERVED-FROM-L2 response. This is the
            // dedicated judge-on-L2-hit path — the only production code path
            // that constructs an `L2EvictionTarget`, so a clearly-degraded
            // cached answer (a near-duplicate paraphrase that the threshold
            // admitted but the judge rejects) gets evicted and can't be served
            // again. Detached + deterministically sampled → zero user latency
            // and bounded extra spend. The judge re-dispatches the ORIGINAL
            // request to its source provider for the reference answer; an L2
            // hit never re-runs routing, so the served (cached) model equals
            // the requested model.
            maybe_spawn_l2_hit_judge(
                state,
                l2,
                &entry,
                similarity,
                in_band,
                task_class,
                trace_id,
                ctx.org_id,
                raw_bearer,
                judge_source_provider,
                judge_source_ctx,
                judge_original_req,
            );
            // Resolve the baseline ONCE so the response headers and the
            // request_logs row report the same figure.
            let baseline_cost_usd = l2_entry_baseline(&entry, current_pricing);
            let request_delta_evidence_state = entry.request_delta_evidence_state;
            spawn_request_log(
                state.telemetry_tracker.as_ref(),
                request_log_writer,
                request_log_for_l2_hit(
                    &entry,
                    ctx,
                    requested_model,
                    trace_id,
                    request_started,
                    matched_route_id,
                    matched_route_version_id,
                    route_paused,
                    baseline_cost_usd,
                    request_delta_evidence_state,
                    retrieval_tokens_saved,
                    similarity,
                    decision,
                ),
            );
            Some(
                build_hit_l2_response(entry, similarity, trace_id, baseline_cost_usd)
                    .map(|resp| with_route_matched(resp, route_matched_name)),
            )
        }
        Ok(None) => {
            metrics::counter!("cache_lookups_total", "tier" => "l2", "result" => "miss")
                .increment(1);
            None
        }
        Err(_) => None,
    }
}
/// The cost shape of a served L1 hit. This is deliberately separate from the
/// cached entry's original miss cost: the request being served now made no
/// provider dispatch, so its realized cost is zero and its entire priced
/// envelope baseline is the TokenTrimmer-attributed saving.
pub(super) fn l1_cache_hit_cost_breakdown(baseline_cost_usd: f64) -> CostBreakdown {
    CostBreakdown {
        cost_usd: 0.0,
        baseline_cost_usd,
        ..Default::default()
    }
}

/// A streaming cache receipt is only possible for an envelope that carries the
/// insertion-time baseline. Do not turn a legacy raw response's token counts
/// into a claimed dollar saving by re-pricing them at stream time.
pub(super) fn l1_cache_stream_attribution(entry: &L1Entry) -> Option<sse::CacheStreamAttribution> {
    if entry.is_legacy_format() {
        return None;
    }
    let usage = &entry.response.usage;
    sse::CacheStreamAttribution::l1(
        entry.baseline_cost_usd,
        usage.prompt_tokens,
        usage.completion_tokens,
        usage.cached_tokens,
    )
}

/// owned by the chat route. Always identify a served L1 hit as `cache`; emit
/// dollar headers only when the same stored envelope baseline also backs the
/// terminal usage receipt.
pub(super) fn attach_l1_cache_stream_headers(
    headers: &mut axum::http::HeaderMap,
    trace_id: Uuid,
    model_used: &str,
    attribution: Option<sse::CacheStreamAttribution>,
) {
    if let Some(attribution) = attribution {
        let cost = l1_cache_hit_cost_breakdown(attribution.baseline_cost_usd());
        attach_cost_headers(headers, trace_id, "cache", model_used, &cost);
    } else {
        if let Ok(v) = trace_id.to_string().parse() {
            headers.insert("x-tokentrimmer-trace-id", v);
        }
        if let Ok(v) = "cache".parse() {
            headers.insert("x-tokentrimmer-provider", v);
        }
        if let Ok(v) = model_used.parse() {
            headers.insert("x-tokentrimmer-model-used", v);
        }
    }
    if let Ok(v) = "hit-l1".parse() {
        headers.insert("x-tokentrimmer-cache", v);
    }
}

/// Build the response for an L1 cache hit.
///
/// Cost is always 0 on a hit. Baseline is taken from the envelope (set at
/// insert time when the original miss ran through pricing). Pre-envelope
/// entries surface as `version == 0`; for those we fall back to the
/// conservative synthetic baseline from cached [`Usage`] until they
/// TTL out.
pub(super) fn build_hit_l1_response(entry: L1Entry, trace_id: Uuid) -> Response {
    let baseline_cost_usd = if entry.is_legacy_format() {
        synthetic_baseline_from_usage(&entry.response.usage)
    } else {
        entry.baseline_cost_usd
    };
    let model_used = entry.response.model.clone();
    let input_tokens = entry.response.usage.prompt_tokens;
    let output_tokens = entry.response.usage.completion_tokens;

    let mut http_response = Json(entry.response).into_response();
    // A TT cache hit never reaches the provider — the full baseline is
    // TT-attributed (saved == baseline) and there is no provider-side
    // cache discount.
    let cost = l1_cache_hit_cost_breakdown(baseline_cost_usd);
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        "cache",
        &model_used,
        &cost,
    );
    if let Ok(v) = "hit-l1".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
    // A cache hit never re-runs routing, so the requested model equals the
    // served (cached) model here.
    record_request_span_attributes(
        &model_used,
        &model_used,
        "cache",
        span_cost(&cost, input_tokens, output_tokens),
        "hit-l1",
        None,
        // Cache hit — no canary split/shadow recorded.
        None,
        None,
        None,
    );
    http_response
}

/// Fallback baseline cost reconstruction from a cached response's [`Usage`].
/// Used only for legacy pre-envelope L1 entries; new inserts carry baseline
/// in the [`L1Entry`] directly.
pub(super) fn synthetic_baseline_from_usage(usage: &Usage) -> f64 {
    let input = usage.prompt_tokens as f64 * 1.0 / 1_000_000.0;
    let output = usage.completion_tokens as f64 * 2.0 / 1_000_000.0;
    input + output
}

/// Build the response for an L2 cache hit. Re-deserializes the cached body
/// and stamps the standard X-TokenTrimmer-* headers, with `Cache: hit-l2`.
/// `baseline_cost_usd` is the catalog-derived baseline resolved by
/// [`l2_entry_baseline`].
pub(super) fn build_hit_l2_response(
    entry: CacheEntry,
    _similarity: f32,
    trace_id: Uuid,
    baseline_cost_usd: f64,
) -> ApiResult<Response> {
    let response: ChatCompletionResponse = serde_json::from_slice(&entry.response)
        .map_err(|e| ApiError::Internal(format!("l2 cache deserialize: {e}")))?;

    // Cost is zero on cache hit (no provider call). Baseline reflects what the
    // request would have cost without our cache; saved == baseline (100%
    // savings, all TT-attributed) with no provider-side cache discount.
    let provider_id = "cache".to_string();
    let model_used = entry.model.clone();
    let input_tokens = response.usage.prompt_tokens;
    let output_tokens = response.usage.completion_tokens;
    let cost = CostBreakdown {
        cost_usd: 0.0,
        baseline_cost_usd,
        provider_cache_saved_usd: 0.0,
        flex_saved_usd: 0.0,
        compression_saved_usd: 0.0,
        doc_compaction_saved_usd: 0.0,
        cache_bust_penalty_usd: 0.0,
        summarizer_tax_usd: 0.0,
        batch_forgone_usd: 0.0,
        minify_saved_est_usd: 0.0,
        diff_saved_usd: 0.0,
        format_switch_saved_est_usd: 0.0,
        diff_failed_cost_usd: 0.0,
        // Document Lane vision-avoided saving — the seam that sets a non-zero
        // value is D4c; a cache hit / non-seam path always books 0.
        doc_vision_saved_est_usd: 0.0,
        // Cache hit → no dispatch → no content_compress.
        content_compress_saved_est_usd: 0.0,
    };
    let mut http_response = Json(response).into_response();
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        &provider_id,
        &model_used,
        &cost,
    );
    if let Ok(v) = "hit-l2".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
    // A cache hit never re-runs routing, so the requested model equals the
    // served (cached) model here.
    record_request_span_attributes(
        &model_used,
        &model_used,
        &provider_id,
        span_cost(&cost, input_tokens, output_tokens),
        "hit-l2",
        None,
        // Cache hit — no canary split/shadow recorded.
        None,
        None,
        None,
    );
    Ok(http_response)
}

/// Build a deterministic synthetic response for a `tt_test_*` sandbox key.
///
/// Skips provider dispatch entirely — useful for customer E2E tests that need
/// the full Gateway response shape (headers + body) without burning real LLM
/// tokens. The response body echoes a fixed sentence that includes the
/// requested model name so test assertions can verify routing.
pub(super) fn sandbox_response(req: &ChatCompletionRequest, trace_id_str: &str) -> Response {
    let response = ChatCompletionResponse {
        id: format!("chatcmpl-sandbox-{}", Uuid::now_v7()),
        object: "chat.completion".into(),
        created: chrono::Utc::now().timestamp(),
        model: req.model.clone(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text(format!(
                    "[sandbox] TokenTrimmer test response for model={}",
                    req.model
                ))),
                tool_calls: vec![],
                name: None,
            },
            finish_reason: Some("stop".into()),
        }],
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 12,
            total_tokens: 22,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        },
    };

    let trace_id = Uuid::parse_str(trace_id_str).unwrap_or_else(|_| Uuid::now_v7());
    let mut http_response = Json(response).into_response();
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        "sandbox",
        &req.model,
        &CostBreakdown::default(),
    );
    if let Ok(v) = "sandbox".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
    http_response
}

/// Baseline cost (USD) an L2 hit avoided — the figure `saved_usd` is computed
/// from (cost on a hit is zero, so saved == baseline).
///
/// Resolution order (truth over flattery — never fabricate a number):
///
/// 1. The baseline stored on the row at insert time (migration 0010+),
///    computed from the versioned pricing catalog by [`compute_cost`] on the
///    original miss. Authoritative: it reflects the catalog rates in force
///    when the response was actually produced.
/// 2. Rows predating the migration carry `None`, but still store the chat
///    model and exact token counts — so re-price those against the CURRENT
///    catalog (`current_pricing` is the registry's rate for the entry's
///    model, which equals the request's post-routing model because
///    `L2Cache::lookup` filters on it). Same shape as [`compute_cost`]'s
///    baseline arm: full input at the input rate + output at the output
///    rate, no cache discount. The provider fee multiplier is deliberately
///    NOT applied — slightly under-reporting beats overstating savings for
///    legacy rows.
/// 3. Model absent from the catalog: report 0 saved rather than invent a
///    rate (the old hardcoded $1/M·$2/M placeholder overstated savings
///    ~6.7x for cheap models and understated 15x+ for expensive ones).
pub(super) fn l2_entry_baseline(entry: &CacheEntry, current_pricing: Option<&ModelPricing>) -> f64 {
    if let Some(stored) = entry.baseline_cost_usd {
        return stored;
    }
    match current_pricing {
        Some(p) => {
            (entry.input_tokens as f64 * p.input_per_million
                + entry.output_tokens as f64 * p.output_per_million)
                / 1_000_000.0
        }
        None => 0.0,
    }
}

/// Background L2 insert. Swallows errors with a tracing log — never blocks
/// the user request.
///
/// `ttl_secs` is the pre-resolved TTL (already factoring in the tt_extras
/// override and the caller's tier per spec §8.4 / rv-per-tier-ttl).
/// The caller must call `effective_ttl_secs` before spawning this task.
///
/// `baseline_cost_usd` is the catalog-derived baseline the miss path computed
/// via [`compute_cost`]; it is stored on the row so a later hit reports honest
/// savings. `None` when the served model was absent from the pricing catalog —
/// stored as NULL so the hit path re-prices against the catalog of THAT day
/// (the model may have been added by then) instead of freezing a $0 figure.
#[allow(clippy::too_many_arguments)]
pub(super) async fn insert_into_l2(
    l2: L2Config,
    org_id: Uuid,
    query_text: &str,
    response: ChatCompletionResponse,
    _provider_id: String,
    model_used: String,
    ttl_secs: u64,
    baseline_cost_usd: Option<f64>,
    request_delta_evidence_state: RequestDeltaEvidenceState,
    // Embedding the L2 lookup already computed for this exact `query_text`
    // (COST-3). When `Some`, it is reused verbatim — avoiding a second,
    // identical embedding call (COGS + latency). When `None` (no lookup ran,
    // or it failed to embed), fall back to embedding here exactly as before.
    precomputed_embedding: Option<Vec<f32>>,
) {
    let embedding_model = l2.embedder.model().to_string();
    let embed = match precomputed_embedding {
        Some(v) => v,
        None => match l2.embedder.embed(query_text).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "l2 embed during insert failed");
                return;
            }
        },
    };
    let response_bytes = match serde_json::to_vec(&response) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "l2 response serialization failed");
            return;
        }
    };
    let ttl = Duration::from_secs(ttl_secs);
    let now = Utc::now();
    // `model` is the model that actually SERVED the response (`response.model` /
    // `served_model`). On cross-model failover this differs from the requested
    // model, so a failover-served response is recalled only for the model that
    // produced it — never the originally-requested one (whose later requests
    // re-miss and dispatch fresh). This is intentional, not a recall bug:
    // `L2Cache::lookup` filters on `chat_model` precisely to stop a request for
    // model X from being served output generated by model Y. Indexing this entry
    // additionally under the requested model would defeat that isolation
    // guarantee (a gpt-4o request could recall a claude-haiku response), so we
    // deliberately do NOT alias it. See audit finding "L2 recall gap when
    // routing/failover rewrites the served model" (2026-06-06 review).
    let entry = CacheEntry {
        id: Uuid::now_v7(),
        org_id,
        embedding: embed,
        response: response_bytes,
        model: model_used,
        embedding_model,
        input_tokens: response.usage.prompt_tokens,
        output_tokens: response.usage.completion_tokens,
        baseline_cost_usd,
        request_delta_evidence_state,
        hit_count: 0,
        quality_score: None,
        judge_verdict: None,
        created_at: now,
        expires_at: now + chrono::Duration::from_std(ttl).unwrap_or_default(),
        // One-way lexical signature of the embedded context text (never the
        // text itself), stored unconditionally so the verify gate has data on
        // every entry before an operator opts in. Harmless when the gate is
        // off.
        lexical_sig: Some(tt_cache::lexical_sig(query_text)),
    };
    if let Err(e) = l2.cache.insert(entry).await {
        tracing::warn!(error = %e, "l2 cache insert failed");
    }
}
