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
use tt_cache::{key::cache_key, l2_context_text, CacheEntry, L1Entry};
use tt_telemetry::request_logs::{RequestLogRow, RequestLogWriter};
use uuid::Uuid;

use tt_auth::ApiKeyContext;
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::Choice,
    parse_cache_control, CacheControlConfig, CacheMode, CacheWriteTier, ChatCompletionRequest,
    ChatCompletionResponse, Message, MessageContent, ModelPricing, RequestContext, Usage,
};

use crate::{
    middleware::trace::TraceId,
    passes::PassEffects,
    retry::{with_retry, RetryPolicy},
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
        | ApiError::ServiceUnavailable(_) => false,
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
struct CacheBehavior {
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
    request_log_writer: Option<&std::sync::Arc<dyn RequestLogWriter>>,
    trace_id: Uuid,
    request_started: Instant,
    matched_route_id: Option<Uuid>,
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
                    request_log_writer,
                    request_log_for_l1_hit(
                        &entry,
                        ctx,
                        trace_id,
                        request_started,
                        matched_route_id,
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
    route_matched_name: Option<&str>,
    raw_bearer: &str,
    judge_source_provider: Option<&std::sync::Arc<dyn tt_shared::Provider>>,
    judge_source_ctx: Option<&RequestContext>,
    judge_original_req: Option<&ChatCompletionRequest>,
) -> Option<ApiResult<Response>> {
    let query_text = l2_context_text(req)?;
    let query_vec = match l2.embedder.embed(&query_text).await {
        Ok(v) => v,
        Err(_) => return None,
    };
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
            let verify_result = match decision {
                L2VerifyDecision::Confident => "confident",
                L2VerifyDecision::Verified(_) => "verified",
                L2VerifyDecision::Unverifiable => "unverifiable",
                L2VerifyDecision::Rejected(_) => "rejected",
            };
            metrics::counter!("cache_l2_verify_total", "result" => verify_result).increment(1);
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
                request_log_writer,
                request_log_for_l2_hit(
                    &entry,
                    ctx,
                    trace_id,
                    request_started,
                    matched_route_id,
                    baseline_cost_usd,
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
    headers: HeaderMap,
    Json(mut req): Json<ChatCompletionRequest>,
) -> ApiResult<Response> {
    // Wall-clock start — fed into `request_logs.latency_ms`.
    let request_started = Instant::now();

    // 1. Resolve provider — 404 for unknown models. (May be re-resolved after
    //    routing rewrites req.model below.) `resolve` falls back to provider
    //    inference for valid-but-unlisted model ids so they dispatch instead
    //    of 404ing.
    let mut provider =
        state
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
    };

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
    let route_match = apply_routing(&state, &ctx, &mut req, forced_route.as_deref()).await?;
    let matched_route_id = route_match.as_ref().map(|m| m.route_id);
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
    // A matched route opting into the conservative compression pass
    // (`RouteAction::compress`). When false (the default — no route or a route
    // that did not enable it) the request-pass pipeline never runs and the
    // request is byte-for-byte unchanged.
    let route_compress = route_match.as_ref().is_some_and(|m| m.compress);
    // A matched route opting into the request-redaction guardrail
    // (`RouteAction::redact`). When false (the default) the redaction pass never
    // runs and the request is byte-for-byte unchanged. This is a SAFETY
    // transform — it strips PII/secrets from the OUTBOUND request before
    // dispatch; it never attributes a saving and surfaces a `redacted:<class>`
    // warning when it fires.
    let route_redact = route_match.as_ref().is_some_and(|m| m.redact);
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
    let mut model_was_rewritten = matched_route_id.is_some();
    if let Some(pct) = route_traffic_pct {
        let in_canary = tt_routing::sticky_traffic_split(ctx.org_id, &idempotency_key, pct);
        if in_canary {
            traffic_split_arm = Some("canary");
            // req.model already holds the canary target (apply_routing rewrote it).
        } else {
            traffic_split_arm = Some("control");
            // Revert to the originally-requested model — serve unchanged.
            if let Some(target) = route_target_model.as_deref() {
                if req.model == target {
                    req.model = requested_model.clone();
                }
            }
            // The control arm is NOT a rewrite: no provider re-resolve, baseline
            // priced against the served (== requested) model, no canary fallbacks.
            model_was_rewritten = false;
            route_fallbacks.clear();
        }
    }
    let traffic_split_arm_owned = traffic_split_arm.map(str::to_string);

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
            match resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, false).await {
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
        &state,
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
    } else if let Some(chain) = fallback_override_from_header(&headers) {
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
        let combined = tt_shared::message_text_for_estimation(&req);
        let cl_input_tokens = tt_tokenize::estimate_tokens(provider.id(), &combined);
        enforce_cost_limit(
            cost_limit_from_header(&headers),
            provider.pricing(&req.model).as_ref(),
            cl_input_tokens,
            req.max_tokens,
        )?;
    }

    // Normalize the request for the routed provider and collect any pre-dispatch
    // warnings (B2: response_format_downgrade; B3 will add temperature_clamped).
    let mut warnings: Vec<String> = Vec::new();
    maybe_downgrade_response_format(&mut req, provider.as_ref(), &mut warnings);
    maybe_clamp_temperature(&mut req, provider.as_ref(), &mut warnings);

    // OpenAI Flex (route action): opt the upstream request into `service_tier:
    // "flex"` ONLY when the served model is flex-eligible (carries a Flex rate in
    // the catalog). An ineligible model is left untouched and a
    // `flex_not_applied:<model>` warning is surfaced. `flex_applied` drives the
    // cost computation below so savings attribute to the `flex` source. Evaluated
    // against the FINAL served provider/model (post-routing/pin/failover-primary).
    let flex_applied = maybe_apply_flex(&mut req, route_flex, provider.as_ref(), &mut warnings);

    // Advisory batch-eligibility marker (route action, research Phase 2.1):
    // never mutates the request or detours dispatch — the gateway is
    // synchronous today. `batch_marked` drives the request_logs tagging and
    // the forgone-discount attribution below. Hard ineligibility (streaming /
    // interactive) and the catalog-batch-rate gate are enforced inside.
    let batch_marked = maybe_mark_batch_eligible(
        &req,
        route_batch,
        interactive_client,
        provider.as_ref(),
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
        let split = crate::passes::SplitRequest::compute(&mut req, &pass_cx);
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
            let mut split = crate::passes::SplitRequest::compute(&mut req, &pass_cx);
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

    // Stable/volatile cache classifier — ALWAYS ON (observability-only, no
    // semantic change, so default-on is allowed): flags volatile markers
    // (timestamp / uuid / hex token) inside a would-be-stable cached prefix
    // via `cache_dynamic_prefix:<kind>` warning tokens + metrics, quantifying
    // the estimated per-request waste of the busted provider cache. Read-only;
    // it never injects `cache_control` (adapter-owned per #126/#150).
    warnings.extend(crate::passes::CacheClassifierPass::classify(&req, &pass_cx));

    // Aggregated pass effects for the cost path (threaded into both the
    // non-streaming and streaming `compute_cost_full` calls): the measured
    // compression delta plus the (pre-fee) cache-bust penalty booked above.
    let pass_effects = crate::passes::PassEffects {
        compression_tokens_removed,
        cache_bust_penalty_usd: cache_bust.penalty_usd(pass_cx.pricing),
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
                resolve_credentials_for(&state, org_id, &pid, &raw_bearer, allow_bearer).await
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
    let mut cache_behavior = CacheBehavior::resolve(&req);
    // `X-TokenTrimmer-Cache` overrides the request-body decision (header beats
    // body). force-write=(true,true) here overrides the eligibility gate that
    // `resolve()` may have applied; the tool-call exclusion at insert time is
    // unaffected, so tool-call responses are still never cached.
    if let Some((lookup, insert)) = cache_override_from_header(&headers)? {
        cache_behavior.do_lookup = lookup;
        cache_behavior.do_insert = insert;
    }
    // A privacy route's disable_cache wins over both body and header.
    if route_disable_cache {
        cache_behavior.do_lookup = false;
        cache_behavior.do_insert = false;
    }

    // 3. Branch: streaming vs non-streaming.
    if req.stream {
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
                            state.request_log_writer.as_ref(),
                            request_log_for_l1_hit(
                                &entry,
                                &ctx,
                                trace_id,
                                request_started,
                                matched_route_id,
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
                            cache_bust_penalty_usd: 0.0,
                            batch_forgone_usd: 0.0,
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
                        return Ok(with_route_matched(
                            sse::stream_response(fake, &provider, trace_id, None),
                            route_matched_name.as_deref(),
                        ));
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
                    provider.chat_completion_stream(req.clone(), &ctx)
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
                    &ctx,
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
                cache_insert: stream_cache_insert,
                // Honor stream_options.include_usage end-to-end: emit an
                // OpenAI-native final usage chunk when the client asked for it.
                include_usage: client_requested_include_usage(&req),
                span_ctx,
                // Canary arm for the streamed request_logs row (None when no
                // split). Shadow mode never fires on the streaming path.
                traffic_split_arm: traffic_split_arm_owned.clone(),
            })
        } else {
            None
        };

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
        Ok(resp)
    } else {
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
                if let Some(resp) =
                    try_negative_cache_hit(l1, key, route_matched_name.as_deref()).await
                {
                    return Ok(resp);
                }
                if let Some(resp) = try_l1_hit(
                    l1,
                    key,
                    &ctx,
                    state.request_log_writer.as_ref(),
                    trace_id,
                    request_started,
                    matched_route_id,
                    route_matched_name.as_deref(),
                )
                .await
                {
                    return Ok(resp);
                }
            }
        }

        // 3b. L2 semantic cache. Gated additionally on l2_allowed.
        if cache_behavior.do_lookup && l2_allowed {
            if let Some(l2) = state.l2.as_ref() {
                // Current catalog rate for the (post-routing) request model —
                // the legacy-row fallback in `l2_entry_baseline`. The entry's
                // model always equals `req.model` here (lookup filters on it).
                let current_pricing = provider.pricing(&req.model);
                if let Some(result) = try_l2_hit(
                    &state,
                    l2,
                    &ctx,
                    &req,
                    current_pricing.as_ref(),
                    state.request_log_writer.as_ref(),
                    trace_id,
                    request_started,
                    matched_route_id,
                    route_matched_name.as_deref(),
                    &raw_bearer,
                    judge_source_provider.as_ref(),
                    judge_source_ctx.as_ref(),
                    judge_original_req.as_ref(),
                )
                .await
                {
                    return result;
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
                                                state.request_log_writer.as_ref(),
                                                request_log_for_l1_hit(
                                                    &entry,
                                                    &ctx,
                                                    trace_id,
                                                    request_started,
                                                    matched_route_id,
                                                ),
                                            );
                                            return Ok(with_route_matched(
                                                build_hit_l1_response(entry, trace_id),
                                                route_matched_name.as_deref(),
                                            ));
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
                    provider.chat_completion(req.clone(), &ctx)
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
                    &ctx,
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
                let shadow_fut = dispatch_shadow(&state, &ctx, &req, shadow_model, &raw_bearer);
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

        let (provider, response) = dispatch_result?;

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
        let cost_breakdown = compute_cost_full(
            &response.usage,
            pricing.as_ref(),
            baseline_pricing.as_ref(),
            provider.fee_multiplier(),
            flex_applied,
            batch_marked,
            pass_effects,
        );
        let cost_usd = cost_breakdown.cost_usd;
        let baseline_cost_usd = cost_breakdown.baseline_cost_usd;
        // headline saved_usd (header) is TT-attributed only — the provider's
        // automatic cache discount is excluded by `CostBreakdown::tt_saved_usd`
        // and surfaced via its own header/ledger field.
        let provider_cache_saved_usd = cost_breakdown.provider_cache_saved_usd;

        // Record realized spend into the same enforcer the pre-flight check uses
        // (dynamic_budget on the tier-aware path) so the monthly_cap_usd hard stop trips.
        state.spend_sink().record(ctx.org_id, cost_usd, Utc::now());

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
        if cache_behavior.do_insert && !response_has_tools {
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
                        let ttl = effective_ttl_secs(
                            cache_behavior.ttl_secs,
                            caller_tier,
                            l1_clone.ttl_secs,
                        );
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

        // 3f. Best-effort L2 insert. Same gate as L1.
        if cache_behavior.do_insert && !response_has_tools && l2_allowed {
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
        spawn_request_log(
            state.request_log_writer.as_ref(),
            RequestLogRow {
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
                cache_creation_input_tokens: opt_tokens_i32(
                    response.usage.cache_creation_input_tokens,
                ),
                // Advisory batch-eligibility marker (research Phase 2.1).
                // `batch_eligible` records route INTENT (the marker survived
                // the hard-ineligibility gate); `batch_forgone_usd` is the
                // PRICED claim — 0.0 when failover served a model with no
                // catalog batch tier, while `batch_eligible` stays true so the
                // route's intent remains auditable.
                batch_eligible: batch_marked,
                batch_forgone_usd: cost_breakdown.batch_forgone_usd,
            },
        );

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
            &state,
            matched_route_id,
            &requested_model,
            &response,
            requested_pricing.as_ref(),
            pricing.as_ref(),
            trace_id,
            ctx.org_id,
            &raw_bearer,
            judge_source_provider,
            judge_source_ctx,
            judge_original_req,
        );

        // 5. Serialize body and attach TokenTrimmer extension headers.
        let mut http_response = Json(response).into_response();
        attach_cost_headers(
            http_response.headers_mut(),
            trace_id,
            &provider_id,
            &model_used,
            &cost_breakdown,
        );
        // Cache state: miss when ANY cache layer is configured but didn't hit;
        // none when both are disabled.
        let cache_state = if state.l1.is_some() || state.l2.is_some() {
            "miss"
        } else {
            "none"
        };
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

/// Per-org namespaced L1 cache key. Prepending `org_id` keeps tenants
/// isolated within a shared Redis instance.
fn namespaced_l1_key(org_id: Uuid, req: &ChatCompletionRequest) -> String {
    format!("{}:{}", org_id, cache_key(req))
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
        cache_bust_penalty_usd: 0.0,
        batch_forgone_usd: 0.0,
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
        cache_bust_penalty_usd: 0.0,
        batch_forgone_usd: 0.0,
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
) {
    let embedding_model = l2.embedder.model().to_string();
    let embed = match l2.embedder.embed(query_text).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "l2 embed during insert failed");
            return;
        }
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
    /// can wipe the headline to 0 but never report a negative saving). Flex
    /// savings are included here automatically: serving via flex lowers
    /// `cost_usd`, so the baseline − cost delta picks the flex saving up (and
    /// `flex_saved_usd` isolates the flex component for the methodology
    /// breakdown).
    pub fn tt_saved_usd(&self) -> f64 {
        (self.baseline_cost_usd
            - self.cost_usd
            - self.provider_cache_saved_usd
            - self.cache_bust_penalty_usd)
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
/// `effects.cache_bust_penalty_usd` is the (pre-fee) estimated cost of a
/// deliberate stable-prefix mutation booked via
/// [`CacheBustEstimate`](crate::passes::CacheBustEstimate). It lands
/// fee-applied in [`CostBreakdown::cache_bust_penalty_usd`] and reduces
/// [`CostBreakdown::tt_saved_usd`] pre-clamp — but is NEVER folded into
/// `cost_usd` / `baseline_cost_usd` (an estimate of induced future cost must
/// not contaminate fields that reconcile against the realized invoice).
///
/// `batch_marked` flags a request the advisory batch-eligibility route action
/// marked (see `maybe_mark_batch_eligible`). It changes NO realized figure:
/// it only populates [`CostBreakdown::batch_forgone_usd`] — the discount the
/// async Batch Lane would have delivered, priced from the served model's REAL
/// catalog batch rate against the realized (flex-or-standard, cache-metered)
/// cost. A served model with no batch tier (possible after failover) forgoes
/// 0.0 — never a fabricated 0.5×.
pub(crate) fn compute_cost_full(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
    flex_applied: bool,
    batch_marked: bool,
    effects: PassEffects,
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
    let compression_saved_usd =
        (effects.compression_tokens_removed as f64) * pricing.input_per_million / 1_000_000.0;
    // Fold the removed-token value into the baseline at the baseline model's
    // input rate (what the customer would have paid sending the uncompressed
    // prompt to the baseline model).
    let baseline_compression_usd = (effects.compression_tokens_removed as f64)
        * baseline_pricing.input_per_million
        / 1_000_000.0;

    // Apply the provider surcharge (e.g. OpenRouter's 5% BYOK fee) to all
    // figures so the saved splits stay consistent (same scale factor). The
    // provider-cache discount is metered against the STANDARD cost (not the
    // flex cost) so flex and cache savings stay independent and don't
    // double-count.
    CostBreakdown {
        cost_usd: cost_usd * fee_multiplier,
        baseline_cost_usd: (baseline_cost_usd + baseline_compression_usd) * fee_multiplier,
        provider_cache_saved_usd: ((no_cache_cost_usd - standard_cost_usd) * fee_multiplier)
            .max(0.0),
        flex_saved_usd: flex_saved_usd * fee_multiplier,
        compression_saved_usd: compression_saved_usd * fee_multiplier,
        cache_bust_penalty_usd: effects.cache_bust_penalty_usd * fee_multiplier,
        batch_forgone_usd: batch_forgone_usd * fee_multiplier,
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
        // ADVISORY forgone Batch-API discount for batch-eligible requests —
        // what the future async Batch Lane would have saved, priced from the
        // served model's real catalog batch rate. NEVER included in
        // `saved-usd` (the request was dispatched synchronously and billed
        // `cost-usd` in full). 0.000000 for all unmarked traffic.
        (
            "x-tokentrimmer-batch-forgone-usd",
            format!("{:.6}", cost.batch_forgone_usd),
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
fn spawn_request_log(writer: Option<&std::sync::Arc<dyn RequestLogWriter>>, row: RequestLogRow) {
    let Some(writer) = writer else { return };
    let writer = writer.clone();
    tokio::spawn(async move {
        if let Err(e) = writer.write(row).await {
            tracing::warn!(error = %e, "request_logs write failed");
        }
    });
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
/// - a route rewrote the model (`matched_route_id.is_some()`),
/// - the served model is cheaper than the originally-requested one (a true
///   downgrade priced on realized usage — [`crate::quality_sample::is_downgrade`]),
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
    trace_id: Uuid,
    org_id: Uuid,
    raw_bearer: &str,
    judge_source_provider: Option<std::sync::Arc<dyn tt_shared::Provider>>,
    judge_source_ctx: Option<RequestContext>,
    judge_original_req: Option<ChatCompletionRequest>,
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
    // Reroute-DOWN only: a route fired AND the served model is cheaper.
    if matched_route_id.is_none() {
        return;
    }
    if !qs::is_downgrade(requested_pricing, served_pricing, &response.usage) {
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
    // Hourly per-instance spend cap — consumed only by samples that would
    // otherwise spawn (the deterministic sample above already passed), so the
    // cap meters real would-be judge dispatches. Dispatch-path judging is
    // unaffected.
    if !state.l2_hit_judge_limiter.try_acquire() {
        metrics::counter!("cache_l2_judge_capped_total").increment(1);
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
    }
}

/// Build the `request_logs` row for an L2 cache hit. `baseline_cost_usd` is
/// the catalog-derived baseline resolved by [`l2_entry_baseline`].
fn request_log_for_l2_hit(
    entry: &CacheEntry,
    ctx: &RequestContext,
    trace_id: Uuid,
    request_started: Instant,
    route_id: Option<Uuid>,
    baseline_cost_usd: f64,
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
    /// The matched route opted into the request-redaction guardrail
    /// (`RouteAction::redact`). When true the gateway redacts PII/secrets from
    /// the outbound request before dispatch (a SAFETY transform, not a saving);
    /// off by default (no redaction runs otherwise).
    pub(crate) redact: bool,
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
    let input_tokens = {
        let combined = tt_shared::message_text_for_estimation(req);
        tt_tokenize::estimate_tokens(provider_id, &combined)
    };

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
        ) {
            Some(r) => r,
            None => return Ok(None),
        },
    };
    let route_id = m.id;
    let route_name = m.name.clone();
    let fallbacks = m.then.fallbacks.clone();
    let disable_cache = m.then.disable_cache;
    let max_cost_usd = m.then.max_cost_usd;
    let flex = m.then.flex;
    let batch = m.then.batch;
    let compress = m.then.compress;
    let redact = m.then.redact;
    let traffic_pct = m.then.traffic_pct;
    let shadow_model = m.then.shadow_model.clone();
    let target_model_for_split = m.then.target_model.clone();

    // Capability guard: before committing the rewrite, check that the
    // target model supports everything the request requires. When ModelInfo
    // is unknown (not in the catalog) we are permissive — only skip when
    // we positively know a capability is missing.
    let required_caps = tt_shared::RequiredCapabilities::from_request(req);
    let estimated_tokens = u64::from(input_tokens);
    if let Some(info) = state.registry.model_info(&m.then.target_model) {
        if !required_caps.satisfied_by(info, estimated_tokens) {
            let reasons = required_caps.skip_reasons(info, estimated_tokens);
            tracing::info!(
                org_id = %ctx.org_id,
                route_id = %route_id,
                model = %m.then.target_model,
                reasons = ?reasons,
                "route_skipped_capability: rewrite target lacks required capabilities, passing through unchanged"
            );
            // Do not rewrite req.model — return None so the request
            // continues with the original model.
            return Ok(None);
        }
    }

    let original = std::mem::replace(&mut req.model, m.then.target_model.clone());
    tracing::debug!(
        org_id = %ctx.org_id,
        route_id = %route_id,
        from = %original,
        to = %req.model,
        fallbacks = ?fallbacks,
        "routing rewrite"
    );
    Ok(Some(RouteMatch {
        route_id,
        route_name,
        fallbacks,
        disable_cache,
        max_cost_usd,
        input_tokens_estimate: input_tokens,
        flex,
        batch,
        compress,
        redact,
        traffic_pct,
        shadow_model,
        target_model: target_model_for_split,
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
        );
        assert!((no_bust.tt_saved_usd() - 4.0).abs() < 1e-9);
        assert_eq!(no_bust.cache_bust_penalty_usd, 0.0);

        // A $1.50 bust reduces the headline to 2.5 — cost/baseline unchanged.
        let effects = PassEffects {
            compression_tokens_removed: 0,
            cache_bust_penalty_usd: 1.5,
        };
        let bd = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            effects,
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
        );
        assert!((bd_fee.cache_bust_penalty_usd - 1.5 * 1.05).abs() < 1e-9);

        // A penalty larger than the savings clamps the headline at 0 — never
        // a negative saving.
        let big = PassEffects {
            compression_tokens_removed: 0,
            cache_bust_penalty_usd: 100.0,
        };
        let clamped = compute_cost_full(
            &usage,
            Some(&served),
            Some(&requested),
            1.0,
            false,
            false,
            big,
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
