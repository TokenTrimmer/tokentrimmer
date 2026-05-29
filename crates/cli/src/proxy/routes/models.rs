//! GET /v1/models — passthrough to upstream/gateway.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::proxy::config::Mode;
use crate::proxy::routes::anthropic::AppState;

pub async fn get_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let upstream = match state.config.mode {
        Mode::Gateway | Mode::Hybrid => format!("{}/v1/models", state.config.gateway_base_url),
        Mode::Bypass => format!("{}/v1/models", state.config.upstream_openai),
    };
    let mut req = state.http.get(&upstream);
    for (k, v) in headers.iter() {
        if matches!(k.as_str(), "host") {
            continue;
        }
        if let Ok(s) = v.to_str() {
            req = req.header(k.as_str(), s);
        }
    }
    if state.config.mode == Mode::Gateway {
        if let Some(k) = &state.config.tt_api_key {
            req = req.bearer_auth(k);
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
