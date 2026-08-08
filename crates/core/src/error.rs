//! Axum-shaped errors that serialize to the OpenAI-compatible error envelope
//! documented in `docs/04-gateway-api-reference.md`.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;
use tt_shared::ProviderError;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// A route definition failed strict canonical validation. Unlike a generic
    /// 400, this is deliberately field-addressable so API clients and the
    /// dashboard can attach the failure to the exact control that would have
    /// been silently ignored or changed at runtime.
    #[error("route definition validation failed")]
    RouteValidation {
        issues: Vec<tt_routing::RouteValidationIssue>,
    },

    #[error("unauthorized")]
    Unauthorized,

    #[error("payment required")]
    PaymentRequired,

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("model not found: {model}")]
    ModelNotFound { model: String },

    #[error("no upstream credential for provider {provider}")]
    MissingProviderCredential { provider: String },

    #[error(
        "estimated cost ${estimated_usd:.4} exceeds the ${ceiling_usd:.4} per-request ceiling"
    )]
    CostLimitExceeded {
        estimated_usd: f64,
        ceiling_usd: f64,
    },

    #[error("rate limited (retry after {retry_after_ms} ms)")]
    RateLimited { retry_after_ms: u64 },

    /// A caller-set per-request deadline (`X-TokenTrimmer-Timeout-Ms`) elapsed.
    #[error("request timed out after {ms} ms")]
    RequestTimeout { ms: u64 },

    #[error("upstream provider: {0}")]
    Provider(#[from] ProviderError),

    #[error("internal: {0}")]
    Internal(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// 409 — the request conflicts with the resource's current state (e.g. a
    /// run that is not awaiting tool outputs, or already being resumed).
    #[error("conflict: {0}")]
    Conflict(String),

    /// The panel feature is disabled (kill-switch). Explicit — never a silent
    /// fallback to single-model, so a panel caller is not surprised by
    /// single-model billing.
    #[error("Fusion panel is disabled")]
    PanelDisabled,

    /// Too few legs survived to arbitrate.
    #[error("panel quorum unmet: {met} of {required} legs succeeded")]
    PanelQuorumUnmet { required: usize, met: usize },

    /// The request-local resolved credentials cannot start the configured
    /// panel before any upstream member or arbiter dispatch.
    #[error(
        "panel credential preflight failed: {credentialed} credentialed member legs for quorum {required} (arbiter credential missing: {missing_arbiter})"
    )]
    PanelCredentialPreflight {
        required: usize,
        credentialed: usize,
        missing_arbiter: bool,
    },

    /// A panel strategy requested but not implemented in this build.
    #[error("panel strategy not supported: {strategy}")]
    PanelStrategyUnsupported { strategy: String },

    /// A Fusion panel leg model is present in the runtime catalog but cannot
    /// satisfy the request's required capabilities before any fan-out. This
    /// is distinct from [`ApiError::ModelNotFound`]: the model IS cataloged,
    /// it just cannot serve the requested modality (vision/tools/json-mode).
    #[error(
        "panel {role} model {model} cannot satisfy required capabilities: {reasons:?}"
    )]
    PanelModelCapabilityUnavailable {
        /// `"member"` or `"arbiter"`.
        role: &'static str,
        /// The model id that cannot satisfy the request's required capabilities.
        model: String,
        /// Human-readable missing-capability reasons (e.g. `tools_not_supported`).
        reasons: Vec<&'static str>,
    },
}

#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    message: String,
    #[serde(rename = "type")]
    type_: &'static str,
    code: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issues: Option<Vec<tt_routing::RouteValidationIssue>>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, type_, code, message, param, issues) = match &self {
            ApiError::InvalidRequest(m) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_request",
                m.clone(),
                None,
                None,
            ),
            ApiError::RouteValidation { issues } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request_error",
                "route_validation_failed",
                "Route definition failed canonical validation.".into(),
                issues.first().map(|issue| issue.field.clone()),
                Some(issues.clone()),
            ),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "invalid_api_key",
                "Invalid or missing API key".into(),
                None,
                None,
            ),
            ApiError::PaymentRequired => (
                StatusCode::PAYMENT_REQUIRED,
                "billing_error",
                "subscription_required",
                "Subscription required".into(),
                None,
                None,
            ),
            ApiError::Forbidden(m) => (
                StatusCode::FORBIDDEN,
                "permission_error",
                "operation_not_permitted",
                m.clone(),
                None,
                None,
            ),
            ApiError::ModelNotFound { model } => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_found",
                format!("Model '{model}' is not registered with any configured provider"),
                None,
                None,
            ),
            ApiError::MissingProviderCredential { provider } => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "missing_provider_credential",
                format!(
                    "No upstream credential configured for provider '{provider}'. Add your org's '{provider}' API key in the TokenTrimmer dashboard (Credentials) before sending, routing, or pinning requests to this provider."
                ),
                None,
                None,
            ),
            ApiError::CostLimitExceeded {
                estimated_usd,
                ceiling_usd,
            } => (
                StatusCode::PAYMENT_REQUIRED,
                "billing_error",
                "cost_limit_exceeded",
                format!(
                    "Estimated request cost ${estimated_usd:.4} exceeds the configured ${ceiling_usd:.4} per-request ceiling."
                ),
                None,
                None,
            ),
            ApiError::RateLimited { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "rate_limit_exceeded",
                self.to_string(),
                None,
                None,
            ),
            ApiError::RequestTimeout { ms } => (
                StatusCode::REQUEST_TIMEOUT,
                "timeout_error",
                "request_timeout",
                format!("Request exceeded the {ms} ms X-TokenTrimmer-Timeout-Ms deadline."),
                None,
                None,
            ),
            ApiError::Provider(err) => {
                let (status, type_, code, message) = map_provider_error(err);
                (status, type_, code, message, None, None)
            }
            ApiError::Internal(m) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "internal_error",
                m.clone(),
                None,
                None,
            ),
            ApiError::NotFound(m) => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "not_found",
                m.clone(),
                None,
                None,
            ),
            ApiError::ServiceUnavailable(m) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "service_unavailable",
                m.clone(),
                None,
                None,
            ),
            ApiError::Conflict(m) => (
                StatusCode::CONFLICT,
                "invalid_request_error",
                "conflict",
                m.clone(),
                None,
                None,
            ),
            ApiError::PanelDisabled => (
                StatusCode::FORBIDDEN,
                "permission_error",
                "panel_disabled",
                "The Fusion panel is not enabled on this gateway.".into(),
                None,
                None,
            ),
            ApiError::PanelQuorumUnmet { required, met } => (
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "panel_quorum_unmet",
                format!("Fusion panel could not reach quorum: {met} of {required} legs succeeded."),
                None,
                None,
            ),
            ApiError::PanelCredentialPreflight {
                required,
                credentialed,
                missing_arbiter,
            } => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "panel_credentials_unavailable",
                format!(
                    "Fusion panel cannot start: {credentialed} of {required} required member legs have configured provider credentials.{}",
                    if *missing_arbiter {
                        " A configured credential is also required for the selected arbiter provider."
                    } else {
                        ""
                    }
                ),
                None,
                None,
            ),
            ApiError::PanelStrategyUnsupported { strategy } => (
                StatusCode::NOT_IMPLEMENTED,
                "invalid_request_error",
                "panel_strategy_unsupported",
                format!("Fusion panel strategy '{strategy}' is not supported yet."),
                None,
                None,
            ),
            ApiError::PanelModelCapabilityUnavailable {
                role,
                model,
                reasons,
            } => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "panel_model_capability_unavailable",
                format!(
                    "Fusion panel {role} model '{model}' cannot satisfy the required capabilities: {}.",
                    reasons.join(", ")
                ),
                None,
                None,
            ),
        };

        let body = ErrorEnvelope {
            error: ErrorBody {
                message,
                type_,
                code,
                param,
                issues,
            },
        };
        (status, Json(body)).into_response()
    }
}

fn map_provider_error(err: &ProviderError) -> (StatusCode, &'static str, &'static str, String) {
    match err {
        ProviderError::Unauthorized(_) => (
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "upstream_unauthorized",
            err.to_string(),
        ),
        ProviderError::RateLimited { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            "upstream_rate_limited",
            err.to_string(),
        ),
        ProviderError::ModelNotFound { .. } => (
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            err.to_string(),
        ),
        ProviderError::InvalidRequest(_) => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "upstream_invalid_request",
            err.to_string(),
        ),
        ProviderError::Timeout { .. } => (
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_error",
            "upstream_timeout",
            err.to_string(),
        ),
        ProviderError::ProviderUpstream { status, .. } if *status >= 500 => (
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "upstream_server_error",
            err.to_string(),
        ),
        ProviderError::ProviderUpstream { .. } | ProviderError::Network(_) => (
            StatusCode::BAD_GATEWAY,
            "upstream_error",
            "upstream_unavailable",
            err.to_string(),
        ),
        ProviderError::Deserialize(_) | ProviderError::Internal(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "internal_error",
            err.to_string(),
        ),
        ProviderError::Unsupported(_) => (
            StatusCode::NOT_IMPLEMENTED,
            "invalid_request_error",
            "unsupported_operation",
            err.to_string(),
        ),
    }
}
