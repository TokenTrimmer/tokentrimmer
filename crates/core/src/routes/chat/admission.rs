//! Request validation and pre-dispatch admission for chat completions.

use axum::http::HeaderMap;
use tt_shared::{context::ProviderCredentials, ModelPricing};
use uuid::Uuid;

use crate::{ApiError, ApiResult, AppState};

/// Pre-flight output-token estimate when a request doesn't set `max_tokens`.
/// Used only to estimate request cost for cost-based route conditions.
const DEFAULT_OUTPUT_TOKENS_ESTIMATE: u32 = 1000;

/// Estimated request cost (USD): input tokens at the input rate + output tokens
/// (from `max_tokens`, else the default) at the output rate.
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

/// Parse `X-TokenTrimmer-Cost-Limit-Usd` as a finite, positive USD ceiling.
pub(crate) fn cost_limit_from_header(headers: &HeaderMap) -> ApiResult<Option<f64>> {
    let Some(value) = headers.get("x-tokentrimmer-cost-limit-usd") else {
        return Ok(None);
    };
    let invalid = || {
        ApiError::InvalidRequest(
            "X-TokenTrimmer-Cost-Limit-Usd must be a finite number greater than zero".into(),
        )
    };
    let value = value.to_str().map_err(|_| invalid())?;
    let limit = value.trim().parse::<f64>().map_err(|_| invalid())?;
    if !limit.is_finite() || limit <= 0.0 {
        return Err(invalid());
    }
    Ok(Some(limit))
}

/// Exact provider id from `X-TokenTrimmer-Provider`, normalized to lowercase.
pub(crate) fn provider_override_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tokentrimmer-provider")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

/// Exact, case-sensitive route name from `X-TokenTrimmer-Route`.
pub(crate) fn route_override_from_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-tokentrimmer-route")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Optional comma-separated route fallback chain.
pub(crate) fn fallback_override_from_header(headers: &HeaderMap) -> Option<Vec<String>> {
    let raw = headers
        .get("x-tokentrimmer-fallback")
        .and_then(|value| value.to_str().ok())?;
    let chain: Vec<String> = raw
        .split(',')
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    (!chain.is_empty()).then_some(chain)
}

/// Per-request upstream timeout in milliseconds, bounded to ten minutes.
pub(crate) fn timeout_ms_from_header(headers: &HeaderMap) -> Option<u64> {
    const MAX_TIMEOUT_MS: u64 = 600_000;
    headers
        .get("x-tokentrimmer-timeout-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|milliseconds| *milliseconds > 0 && *milliseconds <= MAX_TIMEOUT_MS)
}

/// Apply an exact provider pin and bind credentials for that provider.
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
    let credentials = if pinned.id() == source_provider_id {
        super::resolve_credentials(state, org_id, source_provider_id, raw_bearer)
            .await
            .ok_or_else(|| ApiError::MissingProviderCredential {
                provider: source_provider_id.to_string(),
            })?
    } else {
        super::resolve_credentials_for(state, org_id, pinned.id(), raw_bearer, false)
            .await
            .ok_or_else(|| ApiError::MissingProviderCredential {
                provider: pinned.id().to_string(),
            })?
    };
    Ok((pinned, Some(credentials)))
}

/// Enforce a finite request ceiling before dispatch; unknown pricing fails closed.
pub(crate) fn enforce_cost_limit(
    limit: Option<f64>,
    pricing: Option<&ModelPricing>,
    model: &str,
    input_tokens: u32,
    max_tokens: Option<u32>,
) -> ApiResult<()> {
    let Some(limit) = limit else {
        return Ok(());
    };
    let pricing = pricing.ok_or_else(|| ApiError::PriceUnknown {
        model: model.to_string(),
    })?;
    let estimated_usd = estimate_cost_usd(pricing, input_tokens, max_tokens);
    if estimated_usd > limit {
        return Err(ApiError::CostLimitExceeded {
            estimated_usd,
            ceiling_usd: limit,
        });
    }
    Ok(())
}

/// Preserve the public 402 envelope for failover cost-admission failures.
pub(super) fn map_failover_error(error: crate::failover::FailoverError) -> ApiError {
    match error {
        crate::failover::FailoverError::Provider(error) => ApiError::from(error),
        crate::failover::FailoverError::CostLimitExceeded {
            estimated_usd,
            ceiling_usd,
        } => ApiError::CostLimitExceeded {
            estimated_usd,
            ceiling_usd,
        },
        crate::failover::FailoverError::PriceUnknown { model } => ApiError::PriceUnknown { model },
    }
}
