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

    #[error("unauthorized")]
    Unauthorized,

    #[error("payment required")]
    PaymentRequired,

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("model not found: {model}")]
    ModelNotFound { model: String },

    #[error("rate limited (retry after {retry_after_ms} ms)")]
    RateLimited { retry_after_ms: u64 },

    #[error("upstream provider: {0}")]
    Provider(#[from] ProviderError),

    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    message: String,
    #[serde(rename = "type")]
    type_: &'a str,
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    param: Option<&'a str>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, type_, code, message) = match &self {
            ApiError::InvalidRequest(m) => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_request",
                m.clone(),
            ),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authentication_error",
                "invalid_api_key",
                "Invalid or missing API key".into(),
            ),
            ApiError::PaymentRequired => (
                StatusCode::PAYMENT_REQUIRED,
                "billing_error",
                "subscription_required",
                "Subscription required".into(),
            ),
            ApiError::Forbidden(m) => (
                StatusCode::FORBIDDEN,
                "permission_error",
                "operation_not_permitted",
                m.clone(),
            ),
            ApiError::ModelNotFound { model } => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                "model_not_found",
                format!("Model '{model}' is not registered with any configured provider"),
            ),
            ApiError::RateLimited { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "rate_limit_exceeded",
                self.to_string(),
            ),
            ApiError::Provider(err) => map_provider_error(err),
            ApiError::Internal(m) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "internal_error",
                m.clone(),
            ),
        };

        let body = ErrorEnvelope {
            error: ErrorBody {
                message,
                type_,
                code,
                param: None,
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
