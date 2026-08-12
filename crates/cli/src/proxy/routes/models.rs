//! GET /v1/models — passthrough to upstream/gateway.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::proxy::config::ForwardTarget;
use crate::proxy::routes::anthropic::AppState;

pub async fn get_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let upstream = match state.config.mode.contract().target {
        ForwardTarget::Gateway => format!("{}/v1/models", state.config.gateway_base_url),
        ForwardTarget::Provider => format!("{}/v1/models", state.config.upstream_openai),
    };
    let headers = match crate::proxy::forward::prepare_forward_headers(
        state.config.mode,
        headers,
        state.config.tt_api_key.as_deref(),
    ) {
        Ok(headers) => headers,
        Err(error) => {
            tracing::error!(%error, "proxy credential contract failed closed");
            return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut req = state.http.get(&upstream);
    for (key, value) in &headers {
        if !matches!(key.as_str(), "host") {
            req = req.header(key, value);
        }
    }
    match req.send().await {
        Ok(r) => {
            let status = axum::http::StatusCode::from_u16(r.status().as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let body = r.bytes().await.unwrap_or_default();
            (status, body).into_response()
        }
        Err(_) => axum::http::StatusCode::BAD_GATEWAY.into_response(),
    }
}
