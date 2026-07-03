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
    parse_cache_control, CacheControlConfig, CacheMode, CacheWriteTier, ChatCompletionRequest,
    ChatCompletionResponse, Message, MessageContent, ModelPricing, RequestContext, Usage,
};

use crate::{
    middleware::retrieval::RetrievalTelemetry,
    middleware::trace::TraceId,
    passes::PassEffects,
    retry::{with_retry, RetryPolicy},
    routes::panel,
    routes::sse::{self, CacheInsertContext, StreamLogContext, StreamSpanContext},
    single_flight::wait_for_leader,
    state::{L1Config, L2Config, L2VerifyConfig},
    ApiError, ApiResult, AppState,
};

/// L2 cache TTL for newly-inserted entries. Spec §8.4 caps this per-tier
/// (24h / 7d / 30d); the gateway-level default is conservative until the
/// auth layer surfaces the caller's tier.
const L2_DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Pre-flight output-token estimate when a request doesn't set `max_tokens`.
/// Used only to estimate request cost for cost-based route conditions.
const DEFAULT_OUTPUT_TOKENS_ESTIMATE: u32 = 1000;

/// Estimated request cost (USD): input tokens at the input rate + output tokens
/// (from `max_tokens`, else the default) at the output rate. Shared by the
/// pre-rewrite cost condition (V3d-2a) and the post-rewrite ceiling (V3d-2b).
pub(crate) fn estimate_cost_usd(
    pricing: &ModelPricing,
    input_tokens: u32,
    max_tokens: Option<u32>,
) -> f64 {
    let output_est = max_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS_ESTIMATE);
    (f64::from(input_tokens) * pricing.input_per_million
        + f64::from(output_est) * pricing.output_per_million)
        / 1_000_000.0
}

/// Parse `X-TokenTrimmer-Cost-Limit-Usd` (a positive USD ceiling), if present
/// and well-formed. Malformed / non-positive values are ignored (no limit).
pub(crate) fn cost_limit_from_header(headers: &HeaderMap) -> Option<f64> {
    headers
        .get("x-tokentrimmer-cost-limit-usd")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
}

/// `X-TokenTrimmer-Provider` — an exact provider id to pin for this request
/// (lowercased; provider ids are lowercase). `None` when absent or blank.
pub(crate) fn provider_override_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tokentrimmer-provider")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// `X-TokenTrimmer-Route` — an exact route name to force (case-sensitive).
pub(crate) fn route_override_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tokentrimmer-route")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `X-TokenTrimmer-Fallback` — comma-separated override of the route's fallback
/// chain (bare model ids). Absent/blank → None (keep the route chain).
pub(crate) fn fallback_override_from_header(headers: &HeaderMap) -> Option<Vec<String>> {
    let raw = headers
        .get("x-tokentrimmer-fallback")
        .and_then(|v| v.to_str().ok())?;
    let chain: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if chain.is_empty() {
        None
    } else {
        Some(chain)
    }
}

/// `X-TokenTrimmer-Timeout-Ms` — per-request upstream timeout in ms (1..=600000).
/// Invalid / non-positive / over-max → None (no per-request timeout; the global
/// 600s limit still applies).
pub(crate) fn timeout_ms_from_header(headers: &HeaderMap) -> Option<u64> {
    const MAX_TIMEOUT_MS: u64 = 600_000;
    headers
        .get("x-tokentrimmer-timeout-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0 && *ms <= MAX_TIMEOUT_MS)
}

/// Apply an `X-TokenTrimmer-Provider` pin. Returns the provider to dispatch and,
/// when it differs from `current`, the credentials to use. The pin overrides the
/// routed/inferred provider (the routed model is kept). Cross-provider pins
/// re-resolve the target's stored credentials and fail closed (never forward the
/// source key); a pin to the source provider restores source credentials.
///
/// When a pin is present this ALWAYS returns `Some(credentials)` for the pinned
/// provider — even when it equals `current` — because routing may have left
/// `ctx.credentials` holding the SOURCE provider's key (the cross-provider
/// re-resolve in the routing block is deferred to per-candidate failover when a
/// route declares fallbacks). Returning `None` there would forward the source
/// key to the pinned target. The caller must apply the returned credentials.
///
/// # Errors
/// - [`ApiError::InvalidRequest`] if `pinned_id` is not a known provider id.
/// - [`ApiError::MissingProviderCredential`] if a cross-provider pin has no
///   stored credential, or (BYO-only, P0 #9) if a pin to the source provider
///   finds no stored credential for a verified org.
pub(crate) async fn apply_provider_override(
    state: &AppState,
    pinned_id: Option<&str>,
    org_id: Uuid,
    raw_bearer: &str,
    source_provider_id: &str,
    current: std::sync::Arc<dyn tt_shared::Provider>,
) -> ApiResult<(
    std::sync::Arc<dyn tt_shared::Provider>,
    Option<ProviderCredentials>,
)> {
    let Some(pinned_id) = pinned_id else {
        return Ok((current, None));
    };
    let pinned = state
        .registry
        .by_id(pinned_id)
        .ok_or_else(|| ApiError::InvalidRequest(format!("unknown provider: {pinned_id}")))?;
    // Always (re)resolve credentials for the pinned provider so they can never be
    // out of sync with the dispatched provider (see the note above). Cross-provider
    // pins require the target's stored credential and fail closed; a pin to the
    // source provider resolves source credentials (bearer fallback OK).
    let creds = if pinned.id() == source_provider_id {
        resolve_credentials(state, org_id, source_provider_id, raw_bearer)
            .await
            .ok_or_else(|| ApiError::MissingProviderCredential {
                provider: source_provider_id.to_string(),
            })?
    } else {
        resolve_credentials_for(state, org_id, pinned.id(), raw_bearer, false)
            .await
            .ok_or_else(|| ApiError::MissingProviderCredential {
                provider: pinned.id().to_string(),
            })?
    };
    Ok((pinned, Some(creds)))
}

/// Reject with 402 when the estimated request cost exceeds the header limit.
/// Permissive when pricing is unknown (can't prove an exceedance) — same
/// semantics as the route `max_cost_usd` ceiling.
pub(crate) fn enforce_cost_limit(
    limit: Option<f64>,
    pricing: Option<&ModelPricing>,
    input_tokens: u32,
    max_tokens: Option<u32>,
) -> ApiResult<()> {
    if let (Some(limit), Some(pr)) = (limit, pricing) {
        let est = estimate_cost_usd(pr, input_tokens, max_tokens);
        if est > limit {
            return Err(ApiError::CostLimitExceeded {
                estimated_usd: est,
                ceiling_usd: limit,
            });
        }
    }
    Ok(())
}

/// TTL for negative-cache entries (deterministic 4xx errors).
///
/// Short by design: a client error cached for too long would prevent legitimate
/// retries after the caller fixes their request.  60 s is enough to protect
/// against hot-loop bad-request storms while expiring fast enough not to
/// confuse operators.
const NEGATIVE_CACHE_TTL_SECS: u64 = 60;

/// Key prefix that separates negative-cache entries from positive-cache entries
/// in the shared L1 store.
const NEGATIVE_CACHE_PREFIX: &str = "neg:";

// ---------------------------------------------------------------------------
// Negative-cache helpers (rv-cache-key-canonicalization §2.20)
// ---------------------------------------------------------------------------

/// A minimal, serializable record stored as a negative-cache entry.
///
/// Contains just enough to reconstruct the original error response when the
/// negative cache is hit.
#[derive(serde::Serialize, serde::Deserialize)]
struct NegativeCacheEntry {
    /// HTTP status code of the original error response.
    status: u16,
    /// Human-readable error message, preserved verbatim.
    message: String,
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
fn is_deterministic_client_error(err: &ApiError) -> bool {
    use tt_shared::ProviderError;
    match err {
        // Our own 400 validation before even hitting the provider.
        ApiError::InvalidRequest(_) => true,
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
        | ApiError::PanelStrategyUnsupported { .. } => false,
    }
}

/// Derive the HTTP status code that would be returned for `err`.
fn error_status_code(err: &ApiError) -> u16 {
    use axum::http::StatusCode;
    use tt_shared::ProviderError;
    let status: StatusCode = match err {
        ApiError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
        ApiError::PaymentRequired => StatusCode::PAYMENT_REQUIRED,
        ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
        ApiError::ModelNotFound { .. } => StatusCode::NOT_FOUND,
        ApiError::MissingProviderCredential { .. } => StatusCode::BAD_REQUEST,
        ApiError::CostLimitExceeded { .. } => StatusCode::PAYMENT_REQUIRED,
        ApiError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        ApiError::RequestTimeout { .. } => StatusCode::REQUEST_TIMEOUT,
        ApiError::Provider(pe) => match pe {
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
        ApiError::PanelStrategyUnsupported { .. } => StatusCode::NOT_IMPLEMENTED,
    };
    status.as_u16()
}

/// Compute the L1 key for a negative-cache entry.
///
/// Uses the same namespaced positive key with a `"neg:"` prefix so the two
/// namespaces can never collide.
fn negative_l1_key(positive_key: &str) -> String {
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
fn effective_ttl_secs(
    request_override: Option<u64>,
    tier: Option<tt_shared::CallerTier>,
    default: u64,
) -> u64 {
    if let Some(secs) = request_override {
        return secs;
    }
    if let Some(t) = tier {
        return t.ttl_secs();
    }
    default
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
fn is_cache_eligible(req: &ChatCompletionRequest) -> bool {
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
fn client_requested_include_usage(req: &ChatCompletionRequest) -> bool {
    req.stream_options
        .as_ref()
        .and_then(|o| o.get("include_usage"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Returns `true` when any choice in the response contains tool calls.
/// Responses with tool calls are non-deterministic in call order/arguments and
/// must not be replayed from cache.
fn response_has_tool_calls(resp: &ChatCompletionResponse) -> bool {
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
    do_lookup: bool,
    /// Whether to insert a fresh response into cache. Also gated on
    /// `response_has_tool_calls` at insert time (checked separately).
    do_insert: bool,
    /// Per-request TTL override from `tt_extras`. `None` = use gateway default.
    ttl_secs: Option<u64>,
}

impl CacheBehavior {
    fn resolve(req: &ChatCompletionRequest) -> Self {
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
fn cache_override_from_header(headers: &HeaderMap) -> ApiResult<Option<(bool, bool)>> {
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
async fn try_negative_cache_hit(
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
async fn try_l1_hit(
    l1: &L1Config,
    l1_key: &str,
    ctx: &RequestContext,
    tracker: Option<&tokio_util::task::TaskTracker>,
    request_log_writer: Option<&std::sync::Arc<dyn RequestLogWriter>>,
    trace_id: Uuid,
    request_started: Instant,
    matched_route_id: Option<Uuid>,
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
                        trace_id,
                        request_started,
                        matched_route_id,
                        route_paused,
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
fn l2_task_class_for_chat() -> Option<tt_cache::TaskClass> {
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
async fn try_l2_hit(
    state: &AppState,
    l2: &L2Config,
    ctx: &RequestContext,
    req: &ChatCompletionRequest,
    current_pricing: Option<&ModelPricing>,
    request_log_writer: Option<&std::sync::Arc<dyn RequestLogWriter>>,
    trace_id: Uuid,
    request_started: Instant,
    matched_route_id: Option<Uuid>,
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
            spawn_request_log(
                state.telemetry_tracker.as_ref(),
                request_log_writer,
                request_log_for_l2_hit(
                    &entry,
                    ctx,
                    trace_id,
                    request_started,
                    matched_route_id,
                    route_paused,
                    baseline_cost_usd,
                    retrieval_tokens_saved,
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
    /// L2 quality-judge captures (PRE-redaction): the source provider/ctx/req
    /// re-dispatched for the reference answer. Consumed by value (passed by-ref
    /// into the L2-hit gate, moved into `maybe_spawn_quality_judge`).
    pub judge_source_provider: Option<std::sync::Arc<dyn tt_shared::Provider>>,
    pub judge_source_ctx: Option<RequestContext>,
    pub judge_original_req: Option<ChatCompletionRequest>,
    /// Resolved deep-research panel config when the request opted in via the
    /// `X-TokenTrimmer-Panel` header (Phase 1). `None` for every default-path
    /// request — the off-by-default invariant: an absent panel header leaves the
    /// single-model path wire-identical (the only added work is parsing one
    /// absent header + one `None` check at the top of [`complete_once`]). When
    /// `Some`, [`complete_once`] branches to [`panel::complete_panel`] BEFORE any
    /// cache / single-flight check (panels are non-deterministic and bypass both).
    pub panel: Option<panel::PanelConfig>,
    /// Per-provider credentials for the panel member set, keyed by **provider
    /// id** (spec §6.4 step 4). Resolved in [`prepare`] alongside `panel` using
    /// the same store-then-bearer-fallback pattern as the failover pre-resolution
    /// — `run_panel` records a member whose provider id is absent here as
    /// `skipped_no_cred`. Empty (and unused) when `panel` is `None`.
    pub panel_creds: std::collections::HashMap<String, ProviderCredentials>,
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
    /// Deep-research panel attribution object to merge into the serialized
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
    mut prep: Prepared,
) -> ApiResult<CompletionOutcome> {
    // Deep-research panel branch (Phase 1) — FIRST, before any cache /
    // single-flight check. Panels are non-deterministic (two same-model legs
    // must not coalesce) and bill as ONE aggregate row, so they bypass L1/L2 +
    // single-flight entirely (spec §6.5, invariant §2.1.5). `take()` leaves
    // `prep.panel = None`; the whole bundle (still owning `req`/`provider`/
    // `failover_creds`) is moved into `complete_panel`. For the overwhelming
    // majority of requests `prep.panel` is `None` (no panel header), so this is
    // a single cheap `Option::take` + `None` check and the path below is
    // wire-identical to today's single-model completion (off-by-default).
    if let Some(cfg) = prep.panel.take() {
        return panel::complete_panel(state, ctx, prep, cfg).await;
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
        minify_applied,
        reasoning_capped,
        flex_applied,
        batch_marked,
        caller_tier,
        traffic_split_arm_owned,
        route_traffic_pct,
        route_shadow_model,
        failover_candidates,
        failover_creds,
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
        // Already `take`n into the panel branch above (and `None` for the
        // single-model path that reaches here); bind to `_` to stay exhaustive.
        panel: _,
        // Panel-only; the single-model path never reads it.
        panel_creds: _,
    } = prep;
    // The handler built `ctx.trace_id` from the same trace-id it derived; the
    // carved pipeline reads `trace_id` directly (cache rows, L1 envelopes,
    // headers). They are identical by construction.
    let trace_id = ctx.trace_id;

    // 3a. L1 exact-match cache. Cheapest lookup — try first. Gated on
    //     cache eligibility (Fix A §2.2) and tt_extras.cache mode (Fix B §2.7).
    //     Best-effort: any Redis error falls through to L2/provider.
    let l1_key = state
        .l1
        .as_ref()
        .map(|_| namespaced_l1_key(ctx.org_id, &req));

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
                request_started,
                matched_route_id,
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
                request_started,
                matched_route_id,
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
                                                trace_id,
                                                request_started,
                                                matched_route_id,
                                                route_paused,
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
            let __dispatch = with_retry(&RetryPolicy::default(), || {
                provider.chat_completion(req.clone(), ctx)
            })
            .await;
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
            )
            .await
            .map_err(ApiError::from)
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
                    with_retry(&RetryPolicy::default(), || {
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
    let cost_breakdown = compute_cost_full(
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
    if minify_applied {
        crate::metrics::record_minify_estimate(
            route_matched_name.as_deref().unwrap_or("none"),
            minify_saved_tokens,
            cost_breakdown.minify_saved_est_usd,
        );
    }
    let cost_usd = cost_breakdown.cost_usd;
    let baseline_cost_usd = cost_breakdown.baseline_cost_usd;
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
        model: model_used.clone(),
        input_tokens: response.usage.prompt_tokens as i32,
        output_tokens: response.usage.completion_tokens as i32,
        cached_tokens: response.usage.cached_tokens as i32,
        cost_usd,
        baseline_cost_usd,
        provider_cache_saved_usd,
        // Fee-applied, matching the header/span figure — keeps the
        // row-derived TT headline equal to `tt_saved_usd()`.
        cache_bust_penalty_usd: cost_breakdown.cache_bust_penalty_usd,
        cached: false,
        cache_layer: None,
        route_id: matched_route_id,
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
        // Document Lane D4: ISOLATED, ESTIMATED vision-avoided saving (own
        // column, migration 0032; never folded into cost/baseline/saved).
        // Always 0.0 in D4a — the seam that books it is D4c.
        doc_vision_saved_est_usd: cost_breakdown.doc_vision_saved_est_usd,
        // Agent-run grain (W0b Task 4): stamped via `attribute_run` below
        // so the ctx→row mapping is independently unit-testable.
        run_id: None,
        node_id: None,
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

    // 2. Pull api_key from "Authorization: Bearer <key>" if present. This is
    //    the customer's TokenTrimmer key — the auth middleware already
    //    verified it (when configured); we re-read it here only to detect the
    //    sandbox `tt_test_*` short-circuit.
    let raw_bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .unwrap_or("")
        .to_string();

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

    // Idempotency key for the sticky canary traffic split (#454). Precedence:
    //   1. `X-Idempotency-Key` header (client-supplied stable per-logical-request
    //      key — the strongest signal; a retry carrying the same key is sticky to
    //      the same arm).
    //   2. else the `trace_id` string (stable across a single request lifecycle,
    //      and across replicas for that request, but NOT across client retries).
    //   3. else a fresh uuid — only reached when neither exists, which for this
    //      handler is effectively never (trace_id always resolves above). A fresh
    //      uuid means the request is NOT sticky across retries; that is acceptable
    //      and documented: it just makes that one request's arm a self-consistent
    //      one-off. The split itself stays a pure function of (org, key, pct).
    let idempotency_key = headers
        .get("x-idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| {
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
    let (org_id, api_key_id, caller_tier) = match auth_ctx.as_deref() {
        Some(c) => (c.org_id, c.key_id, c.tier),
        None => (Uuid::nil(), Uuid::nil(), None),
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
        if let Some(cfg) = prep.panel.take() {
            // `complete_panel_streaming` returns `Result<Response, ApiError>`;
            // `ApiError` is `IntoResponse`, so a fail-closed error (quorum-unmet
            // 502, arbiter-establishment failure) becomes a proper non-200
            // response — and, critically, returns BEFORE any stream is opened
            // (no 200, no request_logs row).
            return crate::routes::panel::complete_panel_streaming(&state, &ctx, prep, cfg).await;
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
    let route_shadow_model = route_match.as_ref().and_then(|m| m.shadow_model.clone());
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
        // Per-request cost ceiling (V3d-2b): reject when the rerouted model's
        // estimated cost still exceeds the route's max_cost_usd. Permissive when
        // pricing is unknown (can't prove an exceedance).
        if let Some(ceiling) = route_max_cost_usd {
            if let Some(pr) = provider.pricing(&req.model) {
                let routed_cost = estimate_cost_usd(&pr, route_input_tokens, req.max_tokens);
                if routed_cost > ceiling {
                    return Err(ApiError::CostLimitExceeded {
                        estimated_usd: routed_cost,
                        ceiling_usd: ceiling,
                    });
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
    } else if let Some(chain) = fallback_override_from_header(headers) {
        // `X-TokenTrimmer-Fallback` overrides the route-derived chain (no pin).
        route_fallbacks = chain;
    }

    // Per-request cost ceiling from the `X-TokenTrimmer-Cost-Limit-Usd` header.
    // Applies to every request (routed or not), priced on the final model.
    // Estimate input tokens from the ENTIRE prompt (system + all turns), matching
    // the streaming/failover paths — counting only the last user message would
    // undercount multi-turn / large-system-prompt requests and let an over-limit
    // request slip past the cap.
    {
        let combined = tt_shared::message_text_for_estimation(req);
        let cl_input_tokens = tt_tokenize::estimate_tokens(provider.id(), &combined);
        enforce_cost_limit(
            cost_limit_from_header(headers),
            provider.pricing(&req.model).as_ref(),
            cl_input_tokens,
            req.max_tokens,
        )?;
    }

    // Deep-research panel resolution + fail-closed budget gate (Phase 1, spec
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
    let (panel, panel_creds) = if let Some(trigger) = panel_trigger {
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
            None => (None, std::collections::HashMap::new()),
            Some(cfg) => {
                // Fail-closed budget gate: sums fee-aware estimates over (N members +
                // arbiter); any unpriceable member or a missing budget ⇒ 402 before any
                // dispatch. Uses the SAME whole-prompt input-token estimate the
                // single-model cost ceiling above uses, on the post-routing request.
                let combined = tt_shared::message_text_for_estimation(req);
                let panel_input_tokens = tt_tokenize::estimate_tokens(provider.id(), &combined);
                panel::panel_budget_gate(
                    state,
                    &cfg,
                    panel_input_tokens,
                    req.max_tokens,
                    cost_limit_from_header(headers),
                )?;
                // Per-member-provider credential pre-resolution (spec §6.4 step 4),
                // keyed by provider id. Mirrors the failover pre-resolution pattern
                // (distinct providers, first-seen order, resolve each once): the
                // raw-Bearer fallback is allowed ONLY for the source provider (the bearer
                // IS its key); cross-provider members with no stored org credential are
                // simply absent here and `run_panel` records them as `skipped_no_cred`
                // (never dispatched, never billed). The arbiter provider is included so
                // arbitration can dispatch on a member-distinct provider.
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
                (Some(cfg), creds)
            }
        }
    } else {
        (None, std::collections::HashMap::new())
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

    // ── Request-side output shaping (research Phase 3.3 + 3.4) ──────────────
    //
    // MUST run BEFORE `maybe_downgrade_response_format` (the downgrade would
    // erase the json_schema shape the csv planner reads; once a switch/diff
    // applies, response_format is None and the downgrade no-ops) and before
    // cache-key derivation (the mutated request hashes to its own L1 key).
    // Both planners gate on `req.stream` internally, so the streaming branch
    // below is untouched by construction. format_switch × diff is
    // config-rejected at route creation (`validate_output_shaping`);
    // defensively, if both somehow apply, diff wins and the switch is skipped
    // with the `conflict` token.
    let diff_decision = crate::shaping::diff::plan_diff(req, route_diff);
    let format_switch_requested =
        if matches!(diff_decision, Some(crate::shaping::ShapeDecision::Apply(_)))
            && route_format_switch.is_some()
        {
            warnings.push("format_switch_skipped:conflict".to_string());
            crate::metrics::record_format_switch_skip("conflict");
            None
        } else {
            route_format_switch.as_deref()
        };
    let mut format_switch_plan: Option<crate::shaping::format_switch::FormatSwitchPlan> = None;
    match crate::shaping::format_switch::plan_format_switch(req, format_switch_requested) {
        Some(crate::shaping::ShapeDecision::Apply(p)) => {
            crate::shaping::format_switch::apply_format_switch_request(req, &p);
            format_switch_plan = Some(p);
        }
        Some(crate::shaping::ShapeDecision::Skip(r)) => {
            warnings.push(format!("format_switch_skipped:{r}"));
            crate::metrics::record_format_switch_skip(r);
        }
        None => {}
    }
    let mut diff_plan: Option<crate::shaping::diff::DiffPlan> = None;
    match diff_decision {
        Some(crate::shaping::ShapeDecision::Apply(p)) => {
            // No pre-mutation clone is kept: the fail-closed re-emit is
            // derived from the DISPATCHED request at the failure site
            // (`unapply_diff_request` — drop the instruction, restore the
            // plan's response_format) so it inherits every dispatch-path
            // normalization. A pre-pipeline clone would bypass the
            // redaction guardrail on a redact+diff route and dispatch
            // un-flexed bytes that `compute_cost_full` prices at flex
            // rates.
            crate::shaping::diff::apply_diff_request(req, &p);
            diff_plan = Some(p);
        }
        Some(crate::shaping::ShapeDecision::Skip(r)) => {
            warnings.push(format!("diff_skipped:{r}"));
            crate::metrics::record_diff("skipped", r);
        }
        None => {}
    }

    maybe_downgrade_response_format(req, provider.as_ref(), &mut warnings);
    maybe_clamp_temperature(req, provider.as_ref(), &mut warnings);

    // OpenAI Flex (route action): opt the upstream request into `service_tier:
    // "flex"` ONLY when the served model is flex-eligible (carries a Flex rate in
    // the catalog). An ineligible model is left untouched and a
    // `flex_not_applied:<model>` warning is surfaced. `flex_applied` drives the
    // cost computation below so savings attribute to the `flex` source. Evaluated
    // against the FINAL served provider/model (post-routing/pin/failover-primary).
    let flex_applied = maybe_apply_flex(req, route_flex, provider.as_ref(), &mut warnings);

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
    let minify_applied = maybe_minify_json(req, route_minify, provider.as_ref(), &mut warnings);

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

    // ── Request-pass stage ───────────────────────────────────────────────────
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

    // Content-aware compression pass (`RouteAction::content_compress`, P1a): OFF
    // BY DEFAULT — `route_content_compress` is false for every unrouted request
    // and every route that did not enable it, so `req` is byte-for-byte
    // unchanged on the default path. For each LARGE non-prose System/Tool block
    // the dispatcher classifies the content kind and applies a
    // CONTENT-PRESERVING structural backend (JSON whitespace-minify, CSV
    // trailing-padding trim, log repeated-line collapse); Code/Prose are
    // classified but left untouched in P1a. Same token-true gate + cache-span
    // invariant as the other passes; the returned `tokens_removed` is the
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
            crate::passes::PassPipeline::content_compress().run(&mut split, &pass_cx)
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
            crate::content_compress::capture::record(
                content_compress_kind.as_deref(),
                out.tokens_removed,
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
    // provider in the candidate set. The raw-Bearer fallback is allowed only for
    // the source provider (the bearer is its key); cross-provider candidates with
    // no stored credential are skipped during dispatch.
    let (failover_candidates, failover_creds): (
        Vec<String>,
        std::collections::HashMap<String, tt_shared::context::ProviderCredentials>,
    ) = if route_fallbacks.is_empty() {
        (Vec::new(), std::collections::HashMap::new())
    } else {
        let candidates: Vec<String> = std::iter::once(req.model.clone())
            .chain(route_fallbacks.iter().cloned())
            .collect();
        // Distinct candidate providers, first-seen order — resolve each one's
        // credential once.
        let mut provider_ids: Vec<String> = Vec::new();
        for m in &candidates {
            if let Some(p) = state.registry.resolve(m) {
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
    // 30s TTL → no extra DB round-trip on the hot path). Fail-soft: on any
    // resolver error `resolve_or_free` yields Free defaults with
    // `semantic_cache_disabled = false`, so a DB blip never disables caching for
    // a non-opted-out org. Per the documented compliance tradeoff (see
    // `PostgresTierResolver::fetch_semantic_cache_disabled`), a transient blip
    // can conversely re-enable caching for an opted-out org — accepted to match
    // the gateway-wide fail-open precedent. Zero behaviour change when unset.
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
        route_paused,
        requested_model,
        requested_pricing,
        model_was_rewritten,
        format_switch_plan,
        diff_plan,
        pass_effects,
        minify_applied,
        reasoning_capped,
        flex_applied,
        batch_marked,
        caller_tier,
        traffic_split_arm_owned,
        route_traffic_pct,
        route_shadow_model,
        failover_candidates,
        failover_creds,
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
        panel,
        panel_creds,
    })
}

/// Dispatch the **streaming** arm of a chat completion from a [`Prepared`]
/// bundle: the L1 fake-stream cache hit, the live (single-provider or failover)
/// stream establishment, the streaming cost/telemetry/cache-insert wiring, and
/// the SSE response assembly. Byte-for-byte the chat [`handler`]'s former inline
/// `if req.stream` arm — it now reads its inputs from `prep` (destructured into
/// the exact locals the arm always used) instead of the handler's setup locals.
async fn handle_streaming(
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
        route_paused,
        requested_model,
        requested_pricing,
        model_was_rewritten,
        format_switch_plan: _,
        diff_plan: _,
        pass_effects,
        minify_applied,
        reasoning_capped: _,
        flex_applied,
        batch_marked: _,
        caller_tier,
        traffic_split_arm_owned,
        route_traffic_pct,
        route_shadow_model: _,
        failover_candidates,
        failover_creds,
        route_fallbacks,
        warnings,
        request_timeout,
        raw_bearer: _,
        retrieval_telemetry,
        request_started,
        capture_request_json,
        judge_source_provider: _,
        judge_source_ctx: _,
        judge_original_req: _,
        // Panels never reach the streaming arm — the handler forces a
        // panel-configured request through `complete_once` (Phase 1 panels are
        // non-streaming; the buffered arbiter answer is returned). This is `None`
        // by construction here.
        panel: _,
        panel_creds: _,
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
        let l1_key = state
            .l1
            .as_ref()
            .map(|_| namespaced_l1_key(ctx.org_id, &req));
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
                                trace_id,
                                request_started,
                                matched_route_id,
                                route_paused,
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
                        let hit_cost = CostBreakdown {
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
                            // Document Lane vision-avoided saving (D4c sets it).
                            doc_vision_saved_est_usd: 0.0,
                        };
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
                        let fake = sse::fake_stream_from_response(entry.response);
                        // L1 hit already logged above; no need for a second row.
                        let mut resp = with_route_matched(
                            sse::stream_response(fake, &provider, trace_id, None),
                            route_matched_name.as_deref(),
                        );
                        // P0-1/P0-3: settle the served request as a cache hit.
                        // This fake-stream path passes `None` log_ctx to
                        // `stream_response`, which takes the simple-passthrough
                        // branch with no DropGuard — so the streamed-dispatch
                        // settle at `sse.rs` NEVER runs here. Settle inline (as
                        // the non-streaming CacheHit arm does) so the served
                        // counter advances (the COGS guard) while the billed
                        // monthly counter does NOT — a streaming cache hit does
                        // not consume an included request. Without this, a free
                        // tenant using `stream:true` could serve unbounded cache
                        // hits and never trip the served ceiling.
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
                )
                .await
                .map_err(ApiError::from)
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

/// The process-wide model-alias canonicalizer used for cache-key derivation,
/// built once from the operator-curated `model_aliases.toml`. The map is EMPTY
/// by default, so this is byte-for-byte identical to the no-op key derivation
/// until an asserted-identical snapshot→alias pair is configured — at which point
/// a dated snapshot and its floating alias share one L1/L2 cache entry instead of
/// fragmenting (a pure hit-rate win; see the correctness contract in the TOML).
fn alias_canonicalizer() -> &'static AliasMapCanonicalizer {
    static CANON: std::sync::OnceLock<AliasMapCanonicalizer> = std::sync::OnceLock::new();
    CANON.get_or_init(|| {
        AliasMapCanonicalizer::new(tt_shared::model_aliases::model_aliases().clone())
    })
}

/// Per-org namespaced L1 cache key. Prepending `org_id` keeps tenants
/// isolated within a shared Redis instance.
fn namespaced_l1_key(org_id: Uuid, req: &ChatCompletionRequest) -> String {
    format!("{}:{}", org_id, cache_key_with(req, alias_canonicalizer()))
}

/// If `req` asks for `response_format: json_schema` but the routed provider
/// supports only `json_object`, rewrite it to `json_object` (dropping the
/// schema) and record a `response_format_downgrade` warning. Providers that
/// drop `response_format` outright (Anthropic) are left to B1's param_dropped.
fn maybe_downgrade_response_format(
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
fn maybe_clamp_temperature(
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
fn maybe_apply_flex(
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
fn maybe_mark_batch_eligible(
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
fn maybe_minify_json(
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
fn maybe_cap_reasoning(
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
fn minify_saved_tokens_est(
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
fn attach_warnings(
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
fn attach_warning_tokens(headers: &mut axum::http::HeaderMap, tokens: &[String]) {
    if tokens.is_empty() {
        return;
    }
    if let Ok(v) = tokens.join(",").parse() {
        headers.insert("x-tokentrimmer-warnings", v);
    }
}

/// Build the response for an L1 cache hit.
///
/// Cost is always 0 on a hit. Baseline is taken from the envelope (set at
/// insert time when the original miss ran through pricing). Pre-envelope
/// entries surface as `version == 0`; for those we fall back to the
/// conservative synthetic baseline from cached [`Usage`] until they
/// TTL out.
fn build_hit_l1_response(entry: L1Entry, trace_id: Uuid) -> Response {
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
    };
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
fn synthetic_baseline_from_usage(usage: &Usage) -> f64 {
    let input = usage.prompt_tokens as f64 * 1.0 / 1_000_000.0;
    let output = usage.completion_tokens as f64 * 2.0 / 1_000_000.0;
    input + output
}

/// Build the response for an L2 cache hit. Re-deserializes the cached body
/// and stamps the standard X-TokenTrimmer-* headers, with `Cache: hit-l2`.
/// `baseline_cost_usd` is the catalog-derived baseline resolved by
/// [`l2_entry_baseline`].
fn build_hit_l2_response(
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
fn sandbox_response(req: &ChatCompletionRequest, trace_id_str: &str) -> Response {
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
fn l2_entry_baseline(entry: &CacheEntry, current_pricing: Option<&ModelPricing>) -> f64 {
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
async fn insert_into_l2(
    l2: L2Config,
    org_id: Uuid,
    query_text: &str,
    response: ChatCompletionResponse,
    _provider_id: String,
    model_used: String,
    ttl_secs: u64,
    baseline_cost_usd: Option<f64>,
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

/// Result of [`compute_cost`]: the canonical cost/savings split.
///
/// Attribution rule (P0 #12 — invoice-reconciliation honesty): the headline
/// `saved_usd` may only contain savings *caused by TokenTrimmer* (routing to a
/// cheaper model, TT L1/L2 cache hits, failover choices). Discounts the
/// provider applies automatically to its own bill — prompt-cache read
/// discounts net of cache-write premiums — are surfaced separately as
/// `provider_cache_saved_usd` so the TT headline survives reconciliation
/// against the provider invoice.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CostBreakdown {
    /// What the provider actually bills (cache discounts included, fee applied).
    /// When the request was served via OpenAI Flex this is the **flex-rate**
    /// cost (~50% of standard).
    pub cost_usd: f64,
    /// What the request would have cost with no TokenTrimmer optimisation:
    /// the originally-requested model at full input price, no cache discount.
    pub baseline_cost_usd: f64,
    /// Provider-side automatic cache discount: served-model cost with no
    /// caching minus actual cost, clamped at 0 (a cache-write premium can make
    /// the cached request *more* expensive; we never report negative savings).
    pub provider_cache_saved_usd: f64,
    /// Savings attributed to the OpenAI **Flex** service tier specifically — the
    /// difference between the synchronous (standard) baseline cost and the flex
    /// cost for this token usage, at the served model. A distinct savings source
    /// from routing/cache so the headline + methodology can name it. Zero when
    /// the request was not served via flex. Already included in
    /// [`tt_saved_usd`](Self::tt_saved_usd) (flex lowers `cost_usd`).
    pub flex_saved_usd: f64,
    /// Savings attributed to the conservative **compression pass** specifically
    /// — the cost of the input tokens the pass removed before dispatch, priced
    /// at the served model's input rate (fee-applied). A distinct savings source
    /// from routing/cache/flex so the headline + methodology can name it. Zero
    /// when the request was not compressed. Already included in
    /// [`tt_saved_usd`](Self::tt_saved_usd): the removed tokens raise
    /// `baseline_cost_usd` (priced on the pre-compression prompt count) above the
    /// realized `cost_usd` (priced on the reduced count), so the baseline − cost
    /// delta picks the compression saving up. Catalog-priced like every other
    /// source; consistent with the provider-cache-vs-TT attribution rules (this
    /// is a genuine TT-caused reduction in billed input tokens, not a provider
    /// discount, so it belongs in the TT headline).
    pub compression_saved_usd: f64,
    /// Savings attributed to the lossless **document-compaction pass**
    /// specifically (Document Lane D2) — the cost of the input tokens the pass
    /// removed from LARGE non-prose documents before dispatch, priced at the
    /// served model's input rate (fee-applied). A distinct savings source from
    /// compression so the headline + methodology can name it. Zero when the
    /// route did not opt into `doc_compaction`. Already included in
    /// [`tt_saved_usd`](Self::tt_saved_usd) via the SAME baseline fold as
    /// `compression_saved_usd`: the removed tokens raise `baseline_cost_usd`
    /// above the realized `cost_usd`, so the `baseline − cost` delta picks the
    /// doc-compaction saving up. A genuine TT-caused reduction in billed input
    /// tokens (text-only, token-true-gated), not a provider discount.
    pub doc_compaction_saved_usd: f64,
    /// NEGATIVE savings entry: the estimated cost induced by a deliberate
    /// NON-deterministic stable-prefix mutation (a booked
    /// `CacheBustEstimate`; no shipped transform books one today — redaction
    /// is ingress-deterministic and busts nothing) — the prefix tokens
    /// repriced from the ~0.1x cache-read rate back to the full input rate,
    /// fee-applied. Zero on every request whose stable prefix was untouched.
    /// It REDUCES [`tt_saved_usd`](Self::tt_saved_usd) pre-clamp
    /// (conservative in TT's disfavor, same precedent as the cache-write
    /// premium) but is NEVER folded into `cost_usd` / `baseline_cost_usd`:
    /// it is an estimate of induced FUTURE cost, and those two fields must
    /// reconcile against the realized provider invoice. Persisted on the
    /// `request_logs` row (migration 0016) so the row-derived ledger agrees
    /// with the header/span headline.
    pub cache_bust_penalty_usd: f64,
    /// NEGATIVE savings entry: the REAL auxiliary-LLM spend of the agentic
    /// budget's summarizer calls (Sub-lever 2b), fee-applied. Aux spend is
    /// taxed, never free (spec §4.4 item 3) — so it REDUCES
    /// [`tt_saved_usd`](Self::tt_saved_usd) pre-clamp (the loop win is honestly
    /// net-of-tax, the cache-bust precedent) but is NEVER folded into
    /// `cost_usd` / `baseline_cost_usd`: those reconcile against the realized
    /// provider invoice, and the summarizer call bills the org on its OWN
    /// credentials (it is not part of THIS request's served dispatch). Surfaced
    /// on its own `X-TokenTrimmer-Summarizer-Tax-Usd` header. 0.0 on every
    /// request that ran no summarizer (all default-path traffic). An UNMETERED
    /// summarizer call (no catalog price / timed-out-but-possibly-billed) is
    /// NEVER coerced to a phantom `0.0` here — the wiring (Task 9) surfaces it
    /// as an honest warning rather than booking unknown spend as free.
    pub summarizer_tax_usd: f64,
    /// FORGONE batch discount (USD): what the async Batch Lane would have
    /// saved on this request — realized cost minus the served model's
    /// batch-rate cost on the full prompt+completion, floored at 0, fee-
    /// applied. ADVISORY: the gateway dispatched synchronously and billed
    /// `cost_usd`; this is NEVER included in `tt_saved_usd()` or `saved_usd`
    /// (nothing was actually saved — the savings-ledger headline must stay
    /// invoice-reconcilable). Surfaced on its own
    /// `X-TokenTrimmer-Batch-Forgone-Usd` header and persisted on the
    /// `request_logs` row (migration 0017). 0.0 unless the request was marked
    /// batch-eligible AND the served model carries catalog batch rates.
    pub batch_forgone_usd: f64,
    /// ESTIMATED saving from minified-JSON output steering
    /// (`RouteAction::minify_json`, research Phase 3.1): the pretty-printed
    /// re-rendering of the emitted JSON, re-tokenized with the served model's
    /// tokenizer, minus the tokens actually emitted — priced at the output
    /// rate the request was actually billed at (flex out-rate when flex
    /// applied, else standard), fee-applied. An ESTIMATE of an unmeasurable
    /// counterfactual (the model might have emitted minified JSON anyway):
    /// NEVER included in [`tt_saved_usd`](Self::tt_saved_usd) / `saved-usd`
    /// and never folded into `cost_usd` / `baseline_cost_usd` (those
    /// reconcile against the invoice). Surfaced on its own
    /// `X-TokenTrimmer-Minify-Saved-Est-Usd` header and `request_logs` column
    /// (migration 0020). 0.0 when the instruction was not injected, when the
    /// response is not valid JSON, and on streaming (estimate not computed in
    /// v1 — metered only).
    pub minify_saved_est_usd: f64,
    /// MEASURED diff-lane saving (research Phase 3.4): the output tokens the
    /// applied patch avoided billing (tokenized reconstructed artifact −
    /// billed patch completion tokens) priced at the served model's output
    /// rate, fee-applied. Both sides are real tokenizer counts on real
    /// strings — the brief's "genuinely measurable" case — so it rides the
    /// [`tt_saved_usd`](Self::tt_saved_usd) headline via the baseline fold
    /// (the compression precedent) AND is isolated here for the methodology
    /// breakdown. Zero when no diff applied.
    pub diff_saved_usd: f64,
    /// ESTIMATED format-switch saving (research Phase 3.3): tokens of a
    /// JSON-equivalent reconstruction minus tokens of the emitted body, at
    /// the served output rate, fee-applied. A LABELED ESTIMATE ("Est" in the
    /// header name) — NEVER folded into baseline / [`tt_saved_usd`]
    /// (Self::tt_saved_usd): a reconstruction is not an invoice figure (the
    /// batch_forgone precedent). Zero when no switch validated or the
    /// reconstruction was not computable ($0 + meter).
    pub format_switch_saved_est_usd: f64,
    /// Realized cost of a FAILED diff patch attempt on a fail-closed double
    /// dispatch, fee-applied. FOLDED into `cost_usd` (real invoice spend for
    /// this trace — budget/spend-sink must see it) AND duplicated here so a
    /// CFO can unpick the retry tax. The baseline stays re-emit-only, so a
    /// pure-failure trace's headline clamps to 0 — the honest outcome. Zero
    /// when no diff failed.
    pub diff_failed_cost_usd: f64,
    /// ESTIMATED vision-avoided saving from the Document Lane seam (D4): when the
    /// pre-routing distillation seam swaps an image/document part for distilled
    /// TEXT, the request that actually dispatched never contained the image, so
    /// this saving is a COUNTERFACTUAL (the raw image tokens that WOULD have been
    /// billed minus the distilled text tokens, priced at the input rate; $0 for
    /// Gemini per the D0 direction guard). Like `minify_saved_est_usd` it is
    /// ISOLATED: NEVER folded into `cost_usd` / `baseline_cost_usd` /
    /// [`tt_saved_usd`](Self::tt_saved_usd) (those reconcile against the realized
    /// invoice — a request that never sent the image cannot be invoice-reconciled
    /// on it). Surfaced on its own `X-TokenTrimmer-Doc-Vision-Saved-Est-Usd`
    /// header + `request_logs` column (migration 0032). **Always 0.0 in D4a**
    /// (substrate only — the seam that sets a non-zero value is D4c).
    pub doc_vision_saved_est_usd: f64,
}

impl CostBreakdown {
    /// TokenTrimmer-attributed savings: baseline minus actual cost, minus the
    /// provider-side cache discount (which TokenTrimmer did not cause), minus
    /// any booked cache-bust penalty (a cost TokenTrimmer DID cause).
    ///
    /// With no routing/caching by TT this is exactly 0 even when the provider
    /// reports cached tokens. When a cache-write premium exceeds the read
    /// discount (`provider_cache_saved_usd` clamped to 0), the premium reduces
    /// the TT claim instead — conservative in TT's disfavor; the cache-bust
    /// penalty follows the same precedent (it subtracts pre-clamp, so a bust
    /// can wipe the headline to 0 but never report a negative saving). The
    /// summarizer-LLM tax (`summarizer_tax_usd`, REAL aux spend) follows the
    /// SAME pre-clamp precedent — the loop win is reported net-of-tax — and is
    /// likewise never folded into `cost_usd` / `baseline_cost_usd`. Flex
    /// savings are included here automatically: serving via flex lowers
    /// `cost_usd`, so the baseline − cost delta picks the flex saving up (and
    /// `flex_saved_usd` isolates the flex component for the methodology
    /// breakdown).
    pub fn tt_saved_usd(&self) -> f64 {
        (self.baseline_cost_usd
            - self.cost_usd
            - self.provider_cache_saved_usd
            - self.cache_bust_penalty_usd
            - self.summarizer_tax_usd)
            .max(0.0)
    }
}

/// Compute the [`CostBreakdown`] from token usage and pricing.
///
/// `pricing` is the served model's rate; `cost_usd` meters each prompt-token
/// bucket at its catalog rate — fresh input at `input_per_million`, cache reads
/// at the discounted `cached_input_per_million`, and cache writes
/// (`cache_creation_input_tokens`) at the cache-write premium. Writes are priced
/// at the **5-minute TTL tier** (`cache_write_per_million`, ~1.25× base input):
/// that is the only tier the gateway writes (the Anthropic adapter emits a bare
/// `ephemeral` breakpoint, which Anthropic defaults to the 5-minute TTL), and
/// the provider's flat `cache_creation_input_tokens` carries no per-tier split.
/// See the inline note in the body and
/// [`tt_shared::pricing::CacheWriteTier`].
///
/// `baseline_pricing` is the rate the request WOULD have paid without any
/// TokenTrimmer optimisation — i.e. the originally-requested model's rate at
/// full input price with no cache discount. When routing did not rewrite the
/// model, callers pass the same pricing for both so the baseline reflects the
/// served model's pre-discount cost. If `baseline_pricing` is `None`, it
/// falls back to `pricing` (conservative: reports no routing saving).
///
/// Attribution note: provider-reported cache reads/writes are attributed to
/// the *provider* side in full. For OpenAI/Gemini they are automatic. For
/// Anthropic the gateway's adapter may have injected the `cache_control`
/// breakpoint itself (model-aware prompt-cache-minimum gate in
/// `tt-provider-anthropic::translate`), but the usage that flows back carries
/// no signal distinguishing TT-injected breakpoints from caller-driven reuse,
/// so the whole class is conservatively credited to the provider rather than
/// inflating the TT headline.
pub(crate) fn compute_cost(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
) -> CostBreakdown {
    compute_cost_with_flex(usage, pricing, baseline_pricing, fee_multiplier, false)
}

/// Like [`compute_cost`] but with a `flex_applied` flag for requests served via
/// OpenAI's Flex service tier (`service_tier="flex"`).
///
/// When `flex_applied` is true, `cost_usd` is metered at the served model's
/// **flex** rates (~50% of standard) and [`CostBreakdown::flex_saved_usd`] is set
/// to the standard-vs-flex delta on this token usage at the served model — the
/// synchronous (standard) baseline cost minus the flex cost — a distinct savings
/// source named `flex`. Flex is only ever applied to a flex-eligible model (the
/// caller gates on [`ModelPricing::flex_eligible`]); if for some reason the
/// served model carries no flex rate, the flex path is a no-op and pricing falls
/// back to standard (no phantom saving).
///
/// Cache attribution is unchanged: provider-side cache discounts are still
/// computed against the served model's *standard* rates and surfaced via
/// `provider_cache_saved_usd`. For the flex-cost figure we conservatively apply
/// flex rates to the full prompt + completion (the hermetic flex path carries no
/// cached tokens; OpenAI's additional flex prompt-cache discount is not modeled
/// here, keeping the flex saving an exact, reconcilable standard−flex delta).
pub(crate) fn compute_cost_with_flex(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
    flex_applied: bool,
) -> CostBreakdown {
    compute_cost_full(
        usage,
        pricing,
        baseline_pricing,
        fee_multiplier,
        flex_applied,
        false,
        PassEffects::default(),
        0,
        crate::shaping::ShapeEffects::default(),
    )
}

/// Like [`compute_cost_with_flex`] but additionally attributes the
/// request-pass [`PassEffects`]: the conservative **compression pass** saving
/// and any **cache-bust penalty** (negative savings entry).
///
/// `effects.compression_tokens_removed` is the pipeline-MEASURED input-token
/// count the request-pass pipeline trimmed before dispatch (0 when the pass
/// did not run; the token-true gate guarantees it is never an inflation).
/// Those tokens are no longer in `usage.prompt_tokens` (the upstream metered
/// the reduced prompt), so the realized `cost_usd` already excludes them. To
/// attribute the saving we:
///
/// - value the removed tokens at the served model's **standard input rate**
///   (fee-applied) → [`CostBreakdown::compression_saved_usd`]: an exact,
///   invoice-reconcilable reduction in billed input tokens, and
/// - add that amount to `baseline_cost_usd` so the no-TT baseline reflects the
///   *uncompressed* prompt the request would have sent without TokenTrimmer.
///   This keeps [`CostBreakdown::tt_saved_usd`] honest — the compression saving
///   shows up in the headline as `baseline − cost`, the same way routing/flex
///   savings do.
///
/// Compression is a genuine TT-caused reduction in the input the customer sends
/// upstream (not a provider discount), so it belongs in the TT headline —
/// consistent with the provider-cache-vs-TT attribution rules.
///
/// `effects.doc_compaction_tokens_removed` (Document Lane D2) is the lossless
/// document-compaction pass's pipeline-MEASURED input-token removal — identical
/// in kind to compression (text-only, token-true-gated) but attributed to its
/// OWN [`CostBreakdown::doc_compaction_saved_usd`] bucket for the methodology
/// breakdown. It is valued at the served input rate and folded into the baseline
/// the SAME way as compression (via `input_tokens_removed`), so it rides
/// [`CostBreakdown::tt_saved_usd`] through `baseline − cost` without
/// double-counting against `compression_saved_usd` (the two buckets partition
/// the removed tokens). Zero when the route did not opt into `doc_compaction`.
///
/// `effects.elide_field_drop_tokens_removed` + `effects.elide_summary_tokens_removed`
/// are the agentic budget's Sub-lever 2 input-token removals (field-drop is
/// lossless + token-true-gated; summary tokens are counted only once the blind
/// paired judge committed the rewrite, caveat C1). They are pipeline-MEASURED
/// billed-input reductions identical in kind to compression, so they are summed
/// WITH `compression_tokens_removed` and valued / baseline-folded exactly the
/// same — riding [`CostBreakdown::tt_saved_usd`] via `baseline − cost`.
///
/// `effects.cache_bust_penalty_usd` is the (pre-fee) estimated cost of a
/// deliberate stable-prefix mutation booked via
/// [`CacheBustEstimate`](crate::passes::CacheBustEstimate). It lands
/// fee-applied in [`CostBreakdown::cache_bust_penalty_usd`] and reduces
/// [`CostBreakdown::tt_saved_usd`] pre-clamp — but is NEVER folded into
/// `cost_usd` / `baseline_cost_usd` (an estimate of induced future cost must
/// not contaminate fields that reconcile against the realized invoice). Caveat
/// C3: when the estimate is priced from the Anthropic cl100k proxy it
/// UNDER-counts (~15–20%), so the penalty is systematically LOW — acceptable
/// ONLY because under-booking a negative favors TT and the figure never reaches
/// the invoice fields.
///
/// `effects.summarizer_tax_usd` is the REAL auxiliary-LLM spend of the
/// summarizer calls (Sub-lever 2b). Aux spend is taxed, never free (spec §4.4
/// item 3): it lands fee-applied in [`CostBreakdown::summarizer_tax_usd`] and
/// reduces [`CostBreakdown::tt_saved_usd`] pre-clamp (the loop win is reported
/// net-of-tax) — but, like the cache-bust penalty, is NEVER folded into
/// `cost_usd` / `baseline_cost_usd` (the summarizer call bills the org on its
/// own credentials, not THIS request's served dispatch).
///
/// `batch_marked` flags a request the advisory batch-eligibility route action
/// marked (see `maybe_mark_batch_eligible`). It changes NO realized figure:
/// it only populates [`CostBreakdown::batch_forgone_usd`] — the discount the
/// async Batch Lane would have delivered, priced from the served model's REAL
/// catalog batch rate against the realized (flex-or-standard, cache-metered)
/// cost. A served model with no batch tier (possible after failover) forgoes
/// 0.0 — never a fabricated 0.5×.
///
/// `minify_saved_tokens_est` is the tokenizer-grounded minify estimate from
/// [`minify_saved_tokens_est`] (0 when the instruction was not injected, the
/// response was not valid JSON, or on streaming). Priced inside at the BILLED
/// output rate — the flex out-rate when `flex_applied` and flex rates exist,
/// else the standard rate — fee-applied, into
/// [`CostBreakdown::minify_saved_est_usd`] ONLY. Like `batch_forgone_usd`, it
/// changes NO realized or headline figure.
/// `shape` carries the response-side output-shaping effects
/// ([`crate::shaping::ShapeEffects`], research Phase 3.3 + 3.4), attributed
/// per the measured-vs-estimated line:
///
/// - `diff_output_tokens_saved` (MEASURED — real tokenizer counts on both
///   sides) is valued at the served output rate into
///   [`CostBreakdown::diff_saved_usd`] and the SAME token count raises
///   `baseline_cost_usd` at the baseline output rate, so the saving rides
///   [`CostBreakdown::tt_saved_usd`] exactly like compression.
/// - `format_switch_saved_est_usd` (ESTIMATE — a JSON-equivalent
///   reconstruction) lands fee-applied in its OWN field and is NEVER folded
///   into baseline / the headline (the batch_forgone precedent).
/// - `diff_failed_cost_usd` (REALIZED spend of a failed patch attempt) is
///   FOLDED into `cost_usd` — the trace's cost must reconcile against the
///   invoice, which billed both dispatches — and duplicated into its own
///   field so it can be unpicked. Baseline stays re-emit-only ⇒ a
///   pure-failure trace's headline clamps to 0.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_cost_full(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
    flex_applied: bool,
    batch_marked: bool,
    effects: PassEffects,
    minify_saved_tokens_est: u32,
    shape: crate::shaping::ShapeEffects,
) -> CostBreakdown {
    let Some(pricing) = pricing else {
        return CostBreakdown::default();
    };

    // Token breakdown (no double-counting):
    //   cache_read   = cached_tokens (already a subset of prompt_tokens)
    //   cache_write  = cache_creation_input_tokens (also in prompt_tokens)
    //   fresh_input  = prompt_tokens - cache_read - cache_write
    //
    // Rates:
    //   cache_read  → cached_input_per_million  (or base if absent)
    //   cache_write → cache_write_per_million   (5-min tier; or base if absent)
    //   fresh_input → input_per_million
    let cache_read = usage.cached_tokens.min(usage.prompt_tokens);
    let cache_write = usage
        .cache_creation_input_tokens
        .unwrap_or(0)
        .min(usage.prompt_tokens.saturating_sub(cache_read));
    let fresh_input = usage
        .prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);

    // Use cached rate when available; fall back to regular input rate.
    let cached_rate = pricing
        .cached_input_per_million
        .unwrap_or(pricing.input_per_million);
    // Cache-write TTL tier (write-premium selection):
    //
    // Anthropic bills cache *writes* at a per-TTL premium — the default 5-minute
    // ephemeral tier at ~1.25× base input, the opt-in 1-hour tier at ~2×. We
    // meter at the **5-minute tier** because that is the only tier the gateway
    // ever writes: the Anthropic adapter injects a bare
    // `cache_control: {"type": "ephemeral"}` with no `ttl` field (see
    // `tt-provider-anthropic::translate::maybe_inject_cache_control`), and
    // Anthropic defaults a bare `ephemeral` breakpoint to the 5-minute TTL.
    // The flat `cache_creation_input_tokens` the provider returns carries no
    // per-tier breakdown (the granular `cache_creation` split is an opt-in beta
    // we do not request), so there is no signal that would let us attribute any
    // write to the 1-hour tier even if one occurred. The 2× one-hour rate is
    // available via `ModelPricing::cache_write_rate_per_million(OneHour)` for
    // when a 1-hour write is introduced. Fall back to the base input rate when
    // the model documents no write premium (non-Anthropic — cost unchanged).
    let write_rate = pricing
        .cache_write_rate_per_million(CacheWriteTier::FiveMin)
        .unwrap_or(pricing.input_per_million);

    let standard_cost_usd = (fresh_input as f64) * pricing.input_per_million / 1_000_000.0
        + (cache_read as f64) * cached_rate / 1_000_000.0
        + (cache_write as f64) * write_rate / 1_000_000.0
        + (usage.completion_tokens as f64) * pricing.output_per_million / 1_000_000.0;

    // Flex (OpenAI service_tier="flex"): when applied AND the served model
    // carries a flex rate, the actual bill is the flex-rate cost (~50% of
    // standard). The flex saving is the standard−flex delta on this usage at the
    // served model, priced on the full prompt + completion so the figure is an
    // exact, invoice-reconcilable difference. Falls back to standard if a flex
    // opt-in ever reaches a model with no flex rate (no phantom saving).
    let (cost_usd, flex_cost_basis) = match (flex_applied, pricing.flex_rates_per_million()) {
        (true, Some((flex_in, flex_out))) => {
            let flex_cost = (usage.prompt_tokens as f64) * flex_in / 1_000_000.0
                + (usage.completion_tokens as f64) * flex_out / 1_000_000.0;
            (flex_cost, Some(flex_cost))
        }
        _ => (standard_cost_usd, None),
    };
    // Standard cost at the served model on the SAME basis the flex cost uses
    // (full prompt + completion, no cache discount) — the comparison point for
    // the flex saving so the delta is exactly standard − flex.
    let standard_full_cost_usd = (usage.prompt_tokens as f64) * pricing.input_per_million
        / 1_000_000.0
        + (usage.completion_tokens as f64) * pricing.output_per_million / 1_000_000.0;
    let flex_saved_usd = match flex_cost_basis {
        Some(flex_cost) => (standard_full_cost_usd - flex_cost).max(0.0),
        None => 0.0,
    };

    // Batch (advisory, research Phase 2.1): the FORGONE Batch-API discount —
    // what the request would have saved had it gone through the (future) async
    // Batch Lane. Priced from the served model's REAL catalog batch rate on
    // the full prompt + completion (no cache-discount stacking — the same
    // conservative basis as the flex cost and `tt_shared::batch_advisor`),
    // compared against the realized pre-fee `cost_usd` so the figure is "50%
    // off the actual dispatch cost". Floored at 0. NEVER added to any realized
    // or saved figure: the gateway dispatched synchronously and billed
    // `cost_usd` in full.
    let batch_forgone_usd = if batch_marked {
        match pricing.batch_rates_per_million() {
            Some((batch_in, batch_out)) => {
                let batch_cost = (usage.prompt_tokens as f64) * batch_in / 1_000_000.0
                    + (usage.completion_tokens as f64) * batch_out / 1_000_000.0;
                (cost_usd - batch_cost).max(0.0)
            }
            // Failover may serve a model with no batch tier — no real rate,
            // no fabricated claim.
            None => 0.0,
        }
    } else {
        0.0
    };

    // Served-model cost as if no provider caching had occurred: all prompt
    // tokens at the full input rate. The delta against the (standard) cost is the
    // provider's automatic cache discount (read discount net of any
    // cache-write premium) — savings the provider grants with or without
    // TokenTrimmer, so they are excluded from the TT-attributed figure. Computed
    // on standard rates (flex never widens the cache-attributed figure).
    let no_cache_cost_usd = standard_full_cost_usd;

    // Baseline: full input × input rate + output × output rate (no cache
    // discount), priced against the originally-requested model.
    let baseline_pricing = baseline_pricing.unwrap_or(pricing);
    let baseline_cost_usd = (usage.prompt_tokens as f64) * baseline_pricing.input_per_million
        / 1_000_000.0
        + (usage.completion_tokens as f64) * baseline_pricing.output_per_million / 1_000_000.0;

    // Compression saving: the input tokens the pass removed are no longer in
    // `usage.prompt_tokens`, so the realized cost already excludes them. Value
    // them at the served model's STANDARD input rate (a genuine, reconcilable
    // reduction in billed input tokens) and add the SAME amount to the baseline
    // so the no-TT baseline reflects the *uncompressed* prompt — the
    // `baseline − cost` headline then includes the compression saving. Zero when
    // the pass did not run.
    //
    // The agentic budget's Sub-lever 2 input-token removals ride the SAME
    // bucket: `elide_field_drop_tokens_removed` (lossless, token-true-gated) and
    // `elide_summary_tokens_removed` (lossy, but only counted AFTER the blind
    // paired judge COMMITTED the rewrite) are both genuine, pipeline-MEASURED
    // reductions in billed input tokens — identical in kind to compression — so
    // they are valued at the served input rate and folded into the baseline the
    // same way. (Caveat C1: summary tokens enter this sum only once judge-gated;
    // the planner books the un-summarized count otherwise.) The summarizer TAX
    // for those calls is a separate negative entry below — never netted here.
    let compression_input_tokens_removed = effects.compression_tokens_removed
        + effects.elide_field_drop_tokens_removed
        + effects.elide_summary_tokens_removed;
    // Document-compaction (Document Lane D2) is a SEPARATE lossless input-token
    // removal lever. It is valued into its OWN bucket for the methodology
    // breakdown but folded into the baseline the SAME way as compression, so
    // the two never double-count in the `baseline − cost` headline (each token
    // is removed from the dispatched prompt exactly once and re-added to the
    // baseline exactly once).
    let doc_compaction_tokens_removed = effects.doc_compaction_tokens_removed;
    // Total removed input tokens folded into the baseline (all lossless,
    // token-true-gated reductions of billed input).
    let input_tokens_removed = compression_input_tokens_removed + doc_compaction_tokens_removed;
    let compression_saved_usd =
        (compression_input_tokens_removed as f64) * pricing.input_per_million / 1_000_000.0;
    let doc_compaction_saved_usd =
        (doc_compaction_tokens_removed as f64) * pricing.input_per_million / 1_000_000.0;
    // Fold the removed-token value into the baseline at the baseline model's
    // input rate (what the customer would have paid sending the un-trimmed
    // prompt to the baseline model). Includes BOTH the compression and the
    // doc-compaction removals so each lever's saving rides `baseline − cost`.
    let baseline_compression_usd =
        (input_tokens_removed as f64) * baseline_pricing.input_per_million / 1_000_000.0;

    // Minify estimate: the saved-output-token estimate priced at the rate the
    // request's output was actually BILLED at — the flex out-rate when flex
    // applied (and the model carries one), else the standard output rate.
    // Lands in its own ESTIMATE field only; never touches cost/baseline/
    // headline (those reconcile against the invoice).
    let billed_output_rate = match (flex_applied, pricing.flex_rates_per_million()) {
        (true, Some((_, flex_out))) => flex_out,
        _ => pricing.output_per_million,
    };
    let minify_saved_est_usd = (minify_saved_tokens_est as f64) * billed_output_rate / 1_000_000.0;
    // Diff saving (research Phase 3.4, MEASURED): the output tokens the
    // applied patch avoided billing, valued at the served output rate, with
    // the SAME token count folded into the baseline at the baseline model's
    // output rate (what the customer would have paid receiving the full
    // re-emission without TokenTrimmer) — the compression precedent, so the
    // saving rides the `baseline − cost` headline. Zero when no diff applied.
    let diff_saved_usd =
        f64::from(shape.diff_output_tokens_saved) * pricing.output_per_million / 1_000_000.0;
    let baseline_diff_usd = f64::from(shape.diff_output_tokens_saved)
        * baseline_pricing.output_per_million
        / 1_000_000.0;

    // Apply the provider surcharge (e.g. OpenRouter's 5% BYOK fee) to all
    // figures so the saved splits stay consistent (same scale factor). The
    // provider-cache discount is metered against the STANDARD cost (not the
    // flex cost) so flex and cache savings stay independent and don't
    // double-count. The failed-patch cost (`shape.diff_failed_cost_usd`,
    // pre-fee) folds into `cost_usd` BEFORE the fee — both dispatches carry
    // the same provider surcharge on the real invoice.
    CostBreakdown {
        cost_usd: (cost_usd + shape.diff_failed_cost_usd) * fee_multiplier,
        baseline_cost_usd: (baseline_cost_usd + baseline_compression_usd + baseline_diff_usd)
            * fee_multiplier,
        provider_cache_saved_usd: ((no_cache_cost_usd - standard_cost_usd) * fee_multiplier)
            .max(0.0),
        flex_saved_usd: flex_saved_usd * fee_multiplier,
        compression_saved_usd: compression_saved_usd * fee_multiplier,
        doc_compaction_saved_usd: doc_compaction_saved_usd * fee_multiplier,
        cache_bust_penalty_usd: effects.cache_bust_penalty_usd * fee_multiplier,
        summarizer_tax_usd: effects.summarizer_tax_usd * fee_multiplier,
        batch_forgone_usd: batch_forgone_usd * fee_multiplier,
        minify_saved_est_usd: minify_saved_est_usd * fee_multiplier,
        diff_saved_usd: diff_saved_usd * fee_multiplier,
        format_switch_saved_est_usd: shape.format_switch_saved_est_usd * fee_multiplier,
        diff_failed_cost_usd: shape.diff_failed_cost_usd * fee_multiplier,
        // Document Lane (D4a): always 0 — the pre-routing distillation seam that
        // books a non-zero vision-avoided saving on this isolated field is D4c.
        // Isolated: NOT folded into cost_usd/baseline_cost_usd above.
        doc_vision_saved_est_usd: 0.0,
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
    // A route fired AND (the served model is cheaper — reroute-DOWN — OR the
    // request was output-shaped). See the doc comment: the pre-routing
    // capture already provides the un-shaped baseline counterfactual, so an
    // action-only shaped route (same target model, identical pricing) is
    // judge-gateable too.
    if matched_route_id.is_none() {
        return;
    }
    if !(qs::is_downgrade(requested_pricing, served_pricing, &response.usage)
        || output_shaped
        || response_shaped)
    {
        return;
    }
    // Deterministic ~2% sample keyed on the trace id.
    if !qs::should_sample(trace_id, state.judge_config.sample_rate) {
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
fn request_log_for_l1_hit(
    entry: &L1Entry,
    ctx: &RequestContext,
    trace_id: Uuid,
    request_started: Instant,
    route_id: Option<Uuid>,
    route_paused: bool,
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
        cached: true,
        cache_layer: Some("l1".into()),
        route_id,
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
        // Document Lane D4: cache hits never run the seam → 0.
        doc_vision_saved_est_usd: 0.0,
        // run_id/node_id stamped in Task 4 (agentic loop context).
        run_id: None,
        node_id: None,
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
    trace_id: Uuid,
    request_started: Instant,
    route_id: Option<Uuid>,
    route_paused: bool,
    baseline_cost_usd: f64,
    retrieval_tokens_saved: i64,
) -> RequestLogRow {
    RequestLogRow {
        id: Uuid::now_v7(),
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        ts: Utc::now(),
        provider: "cache".into(),
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
        cached: true,
        cache_layer: Some("l2".into()),
        route_id,
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
        // Document Lane D4: cache hits never run the seam → 0.
        doc_vision_saved_est_usd: 0.0,
        // run_id/node_id stamped in Task 4 (agentic loop context).
        run_id: None,
        node_id: None,
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

/// Outcome of evaluating the routing engine against a request: the matched
/// route's id (for `request_logs.route_id` attribution) plus its ordered
/// fallback model ids (for failover dispatch). Empty `fallbacks` = the route
/// declared no failover targets.
pub(crate) struct RouteMatch {
    pub(crate) route_id: Uuid,
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
    /// The matched route's opt-in **deep-research panel** trigger
    /// (`RouteAction::panel`). `Some(_)` makes a matched request fan out across
    /// the panel members + arbiter — but only when the caller did NOT send an
    /// explicit `X-TokenTrimmer-Panel` header (the header wins; the route is the
    /// fallback trigger, resolved in `prepare`). A COST lever: forced to `None`
    /// on a paused route (no panel on a paused route), and `None` by default so
    /// the un-opted single-model path is byte-identical.
    pub(crate) panel: Option<tt_routing::RoutePanel>,
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

    // `m` is `&Route` (inferred from the engine accessors below) regardless of arm.
    let m = match forced_route {
        Some(name) => engine
            .find_by_name(name)
            .ok_or_else(|| ApiError::InvalidRequest(format!("unknown route: {name}")))?,
        None => match engine.evaluate_with_signals(
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
        ) {
            Some(r) => r,
            None => return Ok(None),
        },
    };
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
    // explicit POST /v1/routes/:id/resume.
    if m.paused {
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
            route_name: m.name.clone(),
            paused: true,
            // ALL cost levers off (fail-safe expensive direction):
            fallbacks: vec![],
            max_cost_usd: None,
            flex: false,
            compress: false,
            doc_compaction: false,
            content_compress: false,
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
            // The deep-research panel is a COST lever (it fans out across N
            // members + an arbiter) — suppressed on a paused route, so a paused
            // panel route flows to the originally-requested single model.
            panel: None,
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
    Ok(Some(RouteMatch {
        route_id,
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
    }))
}

#[cfg(test)]
mod cache_header_tests {
    use super::*;
    use axum::http::HeaderMap;

    fn hv(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-tokentrimmer-cache", v.parse().unwrap());
        h
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
            model: "gpt-4o".into(),
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
            cost_usd: 0.001,
            baseline_cost_usd: 0.001,
            provider_cache_saved_usd: 0.0,
            cache_bust_penalty_usd: 0.0,
            cached: false,
            cache_layer: None,
            route_id: None,
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
            doc_vision_saved_est_usd: 0.0,
            run_id: None,
            node_id: None,
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
