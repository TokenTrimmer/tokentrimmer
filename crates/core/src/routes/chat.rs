//! `POST /v1/chat/completions` — OpenAI-compatible chat completion.
//!
//! Dispatch pipeline:
//!   1. Resolve provider from `request.model` via the registry.
//!   2. Build a synthetic [`RequestContext`] (real auth lands in Week 7).
//!   3. For `stream: false` (Week 12 +): try L2 semantic cache lookup; on hit
//!      return the cached response with `X-TokenTrimmer-Cache: hit-l2`.
//!   4. Otherwise dispatch to provider, then best-effort insert into L2 cache.
//!   5. For `stream: true`: dispatch streaming directly (fake-stream from
//!      cache is `w7-fake-stream-cache`, blocked).
//!
//! Deferred:
//!   - Real auth middleware populating `RequestContext` org_id (W7).
//!   - Routing rule engine (W4).
//!   - L1 exact-match lookup (W7 cache middleware).
//!   - Telemetry / audit row write (W7 telemetry pipeline).

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
    parse_cache_control, CacheControlConfig, CacheMode, ChatCompletionRequest,
    ChatCompletionResponse, Message, MessageContent, ModelPricing, RequestContext, Usage,
};

use crate::{
    middleware::trace::TraceId,
    retry::{with_retry, RetryPolicy},
    routes::sse::{self, CacheInsertContext, StreamLogContext},
    single_flight::wait_for_leader,
    state::L2Config,
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
///   stored credential.
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
        resolve_credentials(state, org_id, source_provider_id, raw_bearer).await
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
    let credentials = resolve_credentials(&state, org_id, provider.id(), &raw_bearer).await;

    let mut ctx = RequestContext {
        trace_id,
        org_id,
        api_key_id,
        credentials,
        tag: headers
            .get("x-tokentrimmer-tag")
            .and_then(|v| v.to_str().ok())
            .map(String::from),
        deadline: None,
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
    let route_match = apply_routing(&state, &ctx, &mut req, forced_route.as_deref()).await?;
    let matched_route_id = route_match.as_ref().map(|m| m.route_id);
    // The applied route's name (forced or condition-matched) for the
    // `X-TokenTrimmer-Route-Matched` response header, captured before
    // `route_match` is consumed below.
    let route_matched_name = route_match.as_ref().map(|m| m.route_name.clone());
    // A matched privacy route forces the request to skip the cache entirely.
    let route_disable_cache = route_match.as_ref().is_some_and(|m| m.disable_cache);
    // Per-request cost ceiling (V3d-2b) + the token estimate, captured before
    // `route_match` is consumed below.
    let route_max_cost_usd = route_match.as_ref().and_then(|m| m.max_cost_usd);
    let route_input_tokens = route_match
        .as_ref()
        .map(|m| m.input_tokens_estimate)
        .unwrap_or(0);
    // Ordered fallback model ids from the matched route (empty = no failover).
    let mut route_fallbacks: Vec<String> = route_match.map(|m| m.fallbacks).unwrap_or_default();
    if matched_route_id.is_some() {
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
        // An explicit provider pin must not fail over to a different provider.
        route_fallbacks.clear();
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
        for pid in provider_ids {
            let allow_bearer = pid == source_provider_id;
            if let Some(c) =
                resolve_credentials_for(&state, org_id, &pid, &raw_bearer, allow_bearer).await
            {
                map.insert(pid, c);
            }
        }
        (candidates, map)
    };

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
        let estimated_input_tokens = {
            let provider_id_for_est = provider.id();
            let combined_text: String = req
                .messages
                .iter()
                .map(|m| match m {
                    Message::User { content, .. } | Message::System { content } => match content {
                        MessageContent::Text(s) => s.as_str().to_owned(),
                        MessageContent::Parts(parts) => parts
                            .iter()
                            .filter_map(|p| match p {
                                tt_shared::ContentPart::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                    },
                    Message::Assistant { content, .. } => match content {
                        Some(MessageContent::Text(s)) => s.clone(),
                        Some(MessageContent::Parts(parts)) => parts
                            .iter()
                            .filter_map(|p| match p {
                                tt_shared::ContentPart::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                        None => String::new(),
                    },
                    Message::Tool { content, .. } => match content {
                        MessageContent::Text(s) => s.clone(),
                        MessageContent::Parts(parts) => parts
                            .iter()
                            .filter_map(|p| match p {
                                tt_shared::ContentPart::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                    },
                })
                .collect();
            tt_tokenize::estimate_tokens(provider_id_for_est, &combined_text) as i32
        };

        // Establish the stream. When the matched route declares fallbacks, fail
        // over across the candidate chain (initial establishment only — a
        // mid-stream error cannot move to another provider); otherwise retry
        // the single provider. `provider`/`served_model` are rebound to whoever
        // actually served so cost/telemetry attribute correctly.
        let (provider, served_model, stream) = if route_fallbacks.is_empty() {
            // Retry the initial stream establishment on transient errors (before
            // any chunk is yielded); mid-stream errors are not retried.
            let stream = with_retry(&RetryPolicy::default(), || {
                provider.chat_completion_stream(req.clone(), &ctx)
            })
            .await?;
            (provider, req.model.clone(), stream)
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
            .await?
        };

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
                Some(CacheInsertContext {
                    l1: state.l1.clone(),
                    l2: l2_for_insert,
                    l1_key: l1_key.clone().unwrap_or_default(),
                    l2_query_text,
                    ttl_secs: ttl,
                    model: served_model.clone(),
                    provider_id: provider.id().to_string(),
                    org_id: ctx.org_id,
                })
            } else {
                None
            };

        // Build a StreamLogContext whenever either telemetry or cache insertion
        // is needed. writer=None skips the request_logs row without preventing
        // cache writes (tests, dev mode without a DB).
        let needs_tracking = state.request_log_writer.is_some() || stream_cache_insert.is_some();
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
                // Baseline against the originally-requested model when routed, so
                // the streamed request_logs row carries the real routing saving.
                baseline_pricing: if matched_route_id.is_some() {
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
                cache_insert: stream_cache_insert,
            })
        } else {
            None
        };

        Ok(with_route_matched(
            sse::stream_response(stream, &provider, trace_id, log_ctx),
            route_matched_name.as_deref(),
        ))
    } else {
        // 3a. L1 exact-match cache. Cheapest lookup — try first. Gated on
        //     cache eligibility (Fix A §2.2) and tt_extras.cache mode (Fix B §2.7).
        //     Best-effort: any Redis error falls through to L2/provider.
        let l1_key = state
            .l1
            .as_ref()
            .map(|_| namespaced_l1_key(ctx.org_id, &req));

        // 3a-neg. Negative-cache lookup — check before positive L1/L2.
        //
        // If a previous identical request received a deterministic 4xx from the
        // provider, the error is stored in L1 under "neg:{l1_key}" with a short
        // TTL (NEGATIVE_CACHE_TTL_SECS).  Serve the cached error immediately to
        // avoid re-hitting the provider with a request that will fail again.
        //
        // Gated on the same cache_behavior.do_lookup flag so bypass/read-only
        // semantics are respected.  Only wired for the non-streaming path
        // (streaming errors are not deterministic in the same way).
        if cache_behavior.do_lookup {
            if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key.as_ref()) {
                let neg_key = negative_l1_key(key);
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
                                return Ok(with_route_matched(resp, route_matched_name.as_deref()));
                            }
                            Err(e) => {
                                // Deserialization failure is non-fatal; fall through.
                                tracing::warn!(
                                    error = %e,
                                    key = %neg_key,
                                    "negative cache entry deserialization failed — ignoring"
                                );
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "negative cache lookup error — ignoring");
                    }
                }
            }
        }

        if cache_behavior.do_lookup {
            if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key.as_ref()) {
                match l1.cache.get(key).await {
                    Ok(Some(bytes)) => match L1Entry::from_bytes(&bytes) {
                        Ok(entry) => {
                            // Log the L1 hit before returning. The hit baseline is
                            // either the envelope's own value or the synthetic
                            // fallback for pre-envelope cache rows.
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
                            tracing::warn!(error = %e, key = %key, "l1 cache entry failed to deserialize");
                        }
                    },
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "l1 lookup failed"),
                }
            }
        }

        // 3b. Try L2 semantic cache lookup before dispatching to the provider.
        //     Gated on cache eligibility + tt_extras.cache mode.
        if cache_behavior.do_lookup && l2_allowed {
            if let Some(l2) = state.l2.as_ref() {
                if let Some(query_text) = l2_context_text(&req) {
                    if let Ok(query_vec) = l2.embedder.embed(&query_text).await {
                        if let Ok(Some((entry, similarity))) = l2
                            .cache
                            .lookup(
                                ctx.org_id,
                                &query_vec,
                                l2.threshold,
                                &req.model,
                                l2.embedder.model(),
                            )
                            .await
                        {
                            // Cache hit — best-effort bump and return.
                            let _ = l2.cache.bump_hit_count(entry.id).await;
                            spawn_request_log(
                                state.request_log_writer.as_ref(),
                                request_log_for_l2_hit(
                                    &entry,
                                    &ctx,
                                    trace_id,
                                    request_started,
                                    matched_route_id,
                                ),
                            );
                            return Ok(with_route_matched(
                                build_hit_l2_response(entry, similarity, trace_id)?,
                                route_matched_name.as_deref(),
                            ));
                        }
                    }
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
        let dispatch_result: ApiResult<_> = if route_fallbacks.is_empty() {
            with_retry(&RetryPolicy::default(), || {
                provider.chat_completion(req.clone(), &ctx)
            })
            .await
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
        };

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
        }

        // Baseline is priced against the originally-requested model when a route
        // rewrote it; otherwise against the served model (same pricing → no
        // routing saving, only cache/discount savings).
        let baseline_pricing = if matched_route_id.is_some() {
            requested_pricing.clone()
        } else {
            pricing.clone()
        };
        let (cost_usd, baseline_cost_usd) = compute_cost(
            &response.usage,
            pricing.as_ref(),
            baseline_pricing.as_ref(),
            provider.fee_multiplier(),
        );
        let saved_usd = (baseline_cost_usd - cost_usd).max(0.0_f64);

        // Record realized spend into the same enforcer the pre-flight check uses
        // (dynamic_budget on the tier-aware path) so the monthly_cap_usd hard stop trips.
        state.spend_sink().record(ctx.org_id, cost_usd, Utc::now());

        let provider_id = provider.id().to_string();
        let model_used = response.model.clone();

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
                    tokio::spawn(async move {
                        insert_into_l2(
                            l2_clone,
                            org_id,
                            &query_text,
                            response_clone,
                            l2_provider_id,
                            l2_model_used,
                            l2_ttl_secs,
                        )
                        .await;
                    });
                }
            }
        }

        // 3g. Best-effort request_logs row. Cache-miss path: cached=false,
        //     cache_layer=None. L1/L2-hit paths log their own rows where
        //     they early-return.
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
            },
        );

        // 5. Serialize body and attach TokenTrimmer extension headers.
        let mut http_response = Json(response).into_response();
        attach_cost_headers(
            http_response.headers_mut(),
            trace_id,
            &provider_id,
            &model_used,
            cost_usd,
            baseline_cost_usd,
            saved_usd,
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
        Ok(http_response)
    }
}

/// Per-org namespaced L1 cache key. Prepending `org_id` keeps tenants
/// isolated within a shared Redis instance.
fn namespaced_l1_key(org_id: Uuid, req: &ChatCompletionRequest) -> String {
    format!("{}:{}", org_id, cache_key(req))
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

    let mut http_response = Json(entry.response).into_response();
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        "cache",
        &model_used,
        0.0,
        baseline_cost_usd,
        baseline_cost_usd,
    );
    if let Ok(v) = "hit-l1".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
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
fn build_hit_l2_response(
    entry: CacheEntry,
    _similarity: f32,
    trace_id: Uuid,
) -> ApiResult<Response> {
    let response: ChatCompletionResponse = serde_json::from_slice(&entry.response)
        .map_err(|e| ApiError::Internal(format!("l2 cache deserialize: {e}")))?;

    // Cost is zero on cache hit (no provider call). Baseline reflects what the
    // request would have cost without our cache.
    // Use the cached response's own pricing-derivable usage to fill baseline —
    // the model field on the entry is the provider model that originally served it.
    let baseline_cost_usd = synthetic_baseline_from_entry(&entry);
    let saved_usd = baseline_cost_usd; // 100% savings on a clean hit

    let provider_id = "cache".to_string();
    let model_used = entry.model.clone();
    let mut http_response = Json(response).into_response();
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        &provider_id,
        &model_used,
        0.0,
        baseline_cost_usd,
        saved_usd,
    );
    if let Ok(v) = "hit-l2".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
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
        },
    };

    let trace_id = Uuid::parse_str(trace_id_str).unwrap_or_else(|_| Uuid::now_v7());
    let mut http_response = Json(response).into_response();
    attach_cost_headers(
        http_response.headers_mut(),
        trace_id,
        "sandbox",
        &req.model,
        0.0,
        0.0,
        0.0,
    );
    if let Ok(v) = "sandbox".parse() {
        http_response
            .headers_mut()
            .insert("x-tokentrimmer-cache", v);
    }
    http_response
}

/// Reconstruct a rough baseline cost for a cached entry. We don't have the
/// pricing table for `entry.model` here (no Provider reference), so use a
/// conservative default that produces non-zero savings without claiming more
/// than the cached call would have actually cost. This is a placeholder until
/// the L2 row carries its own original cost in a later schema migration.
fn synthetic_baseline_from_entry(entry: &CacheEntry) -> f64 {
    // Default to $1/M input, $2/M output — within an order of magnitude of
    // most chat models. Conservative; real per-model pricing is wired when
    // the L2 schema gains a `baseline_cost_usd` column.
    let input = entry.input_tokens as f64 * 1.0 / 1_000_000.0;
    let output = entry.output_tokens as f64 * 2.0 / 1_000_000.0;
    input + output
}

/// Background L2 insert. Swallows errors with a tracing log — never blocks
/// the user request.
///
/// `ttl_secs` is the pre-resolved TTL (already factoring in the tt_extras
/// override and the caller's tier per spec §8.4 / rv-per-tier-ttl).
/// The caller must call `effective_ttl_secs` before spawning this task.
async fn insert_into_l2(
    l2: L2Config,
    org_id: Uuid,
    query_text: &str,
    response: ChatCompletionResponse,
    _provider_id: String,
    model_used: String,
    ttl_secs: u64,
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
    let entry = CacheEntry {
        id: Uuid::now_v7(),
        org_id,
        embedding: embed,
        response: response_bytes,
        model: model_used,
        embedding_model,
        input_tokens: response.usage.prompt_tokens,
        output_tokens: response.usage.completion_tokens,
        hit_count: 0,
        created_at: now,
        expires_at: now + chrono::Duration::from_std(ttl).unwrap_or_default(),
    };
    if let Err(e) = l2.cache.insert(entry).await {
        tracing::warn!(error = %e, "l2 cache insert failed");
    }
}

/// Compute `(actual_cost_usd, baseline_cost_usd)` from token usage and pricing.
///
/// `pricing` is the served model's rate; `actual_cost_usd` applies the
/// cached-token discount when `usage.cached_tokens > 0`.
///
/// `baseline_pricing` is the rate the request WOULD have paid without any
/// TokenTrimmer optimisation — i.e. the originally-requested model's rate at
/// full input price with no cache discount. When routing did not rewrite the
/// model, callers pass the same pricing for both so the baseline reflects the
/// served model's pre-discount cost. If `baseline_pricing` is `None`, it
/// falls back to `pricing` (conservative: reports no routing saving).
pub(crate) fn compute_cost(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
) -> (f64, f64) {
    let Some(pricing) = pricing else {
        return (0.0, 0.0);
    };

    // Token breakdown (no double-counting):
    //   cache_read   = cached_tokens (already a subset of prompt_tokens)
    //   cache_write  = cache_creation_input_tokens (also in prompt_tokens)
    //   fresh_input  = prompt_tokens - cache_read - cache_write
    //
    // Rates:
    //   cache_read  → cached_input_per_million  (or base if absent)
    //   cache_write → cache_write_per_million   (or base if absent; non-Anthropic unchanged)
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
    // Use write-premium rate when available; fall back to base input rate.
    let write_rate = pricing
        .cache_write_per_million
        .unwrap_or(pricing.input_per_million);

    let cost_usd = (fresh_input as f64) * pricing.input_per_million / 1_000_000.0
        + (cache_read as f64) * cached_rate / 1_000_000.0
        + (cache_write as f64) * write_rate / 1_000_000.0
        + (usage.completion_tokens as f64) * pricing.output_per_million / 1_000_000.0;

    // Baseline: full input × input rate + output × output rate (no cache
    // discount), priced against the originally-requested model.
    let baseline_pricing = baseline_pricing.unwrap_or(pricing);
    let baseline_cost_usd = (usage.prompt_tokens as f64) * baseline_pricing.input_per_million
        / 1_000_000.0
        + (usage.completion_tokens as f64) * baseline_pricing.output_per_million / 1_000_000.0;

    // Apply the provider surcharge (e.g. OpenRouter's 5% BYOK fee) to both cost
    // and baseline so saved_usd stays consistent (it scales by the same factor).
    (
        cost_usd * fee_multiplier,
        baseline_cost_usd * fee_multiplier,
    )
}

/// Insert all six required `X-TokenTrimmer-*` response headers.
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

pub(crate) fn attach_cost_headers(
    headers: &mut axum::http::HeaderMap,
    trace_id: Uuid,
    provider_id: &str,
    model_used: &str,
    cost_usd: f64,
    baseline_cost_usd: f64,
    saved_usd: f64,
) {
    let pairs: &[(&str, String)] = &[
        ("x-tokentrimmer-trace-id", trace_id.to_string()),
        ("x-tokentrimmer-provider", provider_id.to_string()),
        ("x-tokentrimmer-model-used", model_used.to_string()),
        ("x-tokentrimmer-cost-usd", format!("{cost_usd:.6}")),
        (
            "x-tokentrimmer-baseline-cost-usd",
            format!("{baseline_cost_usd:.6}"),
        ),
        ("x-tokentrimmer-saved-usd", format!("{saved_usd:.6}")),
    ];

    for (name, value) in pairs {
        if let Ok(v) = value.parse() {
            headers.insert(*name, v);
        }
    }
}

/// Decide which credentials to send to the upstream provider.
///
/// Precedence:
///
/// 1. The credential store (if configured) — production path: per-org
///    upstream key, possibly with `base_url` / `extra_headers`.
/// 2. The raw Bearer token as a fallback — preserves legacy behavior where
///    customers pointed their OpenAI SDK at our gateway with their own
///    upstream key in the `Authorization` header.
///
/// On a store error we log and fall back to the raw Bearer rather than
/// failing the request: cache and credential lookup are best-effort layers.
pub(crate) async fn resolve_credentials(
    state: &AppState,
    org_id: Uuid,
    provider_id: &str,
    raw_bearer: &str,
) -> ProviderCredentials {
    // Source-provider resolution always allows the raw-Bearer fallback (legacy
    // BYO-key passthrough). `expect` is safe: allow_bearer_fallback=true never
    // returns None.
    resolve_credentials_for(state, org_id, provider_id, raw_bearer, true)
        .await
        .expect("bearer fallback yields Some")
}

/// Resolve upstream credentials for `provider_id`.
///
/// With a credential store configured (the hosted per-org model), a store hit
/// wins; on a miss the raw-Bearer fallback applies only when
/// `allow_bearer_fallback` is true (the source provider's key is its own), so a
/// cross-provider target with no stored key returns `None` (fail closed — we
/// must not forward the source key to a different provider).
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
        Ok(None) if allow_bearer_fallback => Some(bearer()),
        Ok(None) => None,
        Err(e) => {
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
    }
}

/// Build the `request_logs` row for an L2 cache hit.
fn request_log_for_l2_hit(
    entry: &CacheEntry,
    ctx: &RequestContext,
    trace_id: Uuid,
    request_started: Instant,
    route_id: Option<Uuid>,
) -> RequestLogRow {
    let baseline = synthetic_baseline_from_entry(entry);
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
        baseline_cost_usd: baseline,
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
    }
}

fn clamp_latency_ms(started: Instant) -> i32 {
    started.elapsed().as_millis().min(i32::MAX as u128) as i32
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
}

/// Look up the org's routing engine (cached ~60s) and evaluate it against
/// the incoming request. On a match, rewrites `req.model` in place and
/// returns the matched route (id + fallbacks) so callers can stamp the id on
/// the request_logs row and fail over across the fallback chain. Returns
/// `None` (and does not modify `req`) when:
///
/// - no routing store is configured (dev / free tier),
/// - the request has no resolvable org (synthetic context),
/// - the backend errors (we log + fall through — never fail user traffic),
/// - or no enabled route matches.
/// A forced route that can't be honored is a `400`; absence of routing is fine
/// for an unforced request.
fn forced_miss(forced: Option<&str>) -> ApiResult<Option<RouteMatch>> {
    match forced {
        Some(name) => Err(ApiError::InvalidRequest(format!("unknown route: {name}"))),
        None => Ok(None),
    }
}

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

    // `m` is `&Route` (inferred from the engine accessors below) regardless of arm.
    let m = match forced_route {
        Some(name) => engine
            .find_by_name(name)
            .ok_or_else(|| ApiError::InvalidRequest(format!("unknown route: {name}")))?,
        None => match engine.evaluate_with_cost(req, ctx, input_tokens, estimated_cost_usd) {
            Some(r) => r,
            None => return Ok(None),
        },
    };
    let route_id = m.id;
    let route_name = m.name.clone();
    let fallbacks = m.then.fallbacks.clone();
    let disable_cache = m.then.disable_cache;
    let max_cost_usd = m.then.max_cost_usd;

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
            },
        }
    }

    // --- Fix A: is_cache_eligible ---

    #[test]
    fn deterministic_request_is_eligible() {
        assert!(is_cache_eligible(&base_req()));
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
mod fee_tests {
    use super::*;

    fn flat_pricing() -> ModelPricing {
        ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
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
        };
        let p = flat_pricing();
        let (cost, base) = compute_cost(&usage, Some(&p), Some(&p), 1.0);
        let (cost_fee, base_fee) = compute_cost(&usage, Some(&p), Some(&p), 1.05);
        // 1M input @ $1/M = $1.00 with no fee.
        assert!((cost - 1.0).abs() < 1e-9, "cost = {cost}");
        // OpenRouter's 5% BYOK fee scales cost and baseline by 1.05.
        assert!((cost_fee - 1.05).abs() < 1e-9, "cost_fee = {cost_fee}");
        assert!(
            (base_fee - base * 1.05).abs() < 1e-12,
            "base_fee = {base_fee}"
        );
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
        };
        let usage_base = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            cache_creation_input_tokens: None, // same tokens, no write bucket
        };
        let (cost_write, _) = compute_cost(&usage_write, Some(&p), Some(&p), 1.0);
        let (cost_base, _) = compute_cost(&usage_base, Some(&p), Some(&p), 1.0);
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
        };
        let (cost, _) = compute_cost(&usage, Some(&p), Some(&p), 1.0);
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
        };
        let (cost, _) = compute_cost(&usage, Some(&p), Some(&p), 1.0);
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
        };
        let (cost, _) = compute_cost(&usage, Some(&p), Some(&p), 1.0);
        let expected = (400_000.0 * 3.0 + 300_000.0 * 0.30 + 300_000.0 * 3.75) / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 1e-9,
            "expected {expected}, got {cost}"
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
