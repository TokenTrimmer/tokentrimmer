//! POST /v1/chat/completions — accept OpenAI native, forward per mode.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::proxy::config::Mode;
use crate::proxy::forward;
use crate::proxy::preview;
use crate::proxy::routes::anthropic::AppState;
use crate::proxy::session::LogLine;

pub async fn post_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let upstream = match state.config.mode {
        Mode::Gateway | Mode::Hybrid => {
            format!("{}/v1/chat/completions", state.config.gateway_base_url)
        }
        Mode::Bypass => format!("{}/v1/chat/completions", state.config.upstream_openai),
    };
    let mut h = headers.clone();
    if state.config.mode == Mode::Gateway {
        if let Some(k) = &state.config.tt_api_key {
            h.insert("authorization", format!("Bearer {k}").parse().unwrap());
        }
    }
    // Best-effort cost preview (bounded by PREVIEW_TIMEOUT_MS). On failure
    // the proxy forwards unannotated — never block the user request.
    let preview_headers = preview::fetch(&state.http, &state.config, &body).await;
    match forward::forward_post(&state.http, &upstream, h, body).await {
        Ok(resp) => {
            let _ = state.log.append(&LogLine {
                timestamp: chrono::Utc::now().to_rfc3339(),
                mode: match state.config.mode {
                    Mode::Gateway => "gateway",
                    Mode::Bypass => "bypass",
                    Mode::Hybrid => "hybrid",
                },
                route: "POST /v1/chat/completions",
                model: None,
                input_tokens: None,
                output_tokens: None,
                cost_usd: resp
                    .headers
                    .get("x-tokentrimmer-cost-usd")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok()),
                preview_cost_usd: None,
                cache_layer: resp
                    .headers
                    .get("x-tokentrimmer-cache")
                    .and_then(|v| v.to_str().ok()),
                suggested_route: None,
                suggested_savings_usd: None,
                trace_id: resp
                    .headers
                    .get("x-tokentrimmer-trace-id")
                    .and_then(|v| v.to_str().ok()),
            });
            let mut response_headers = resp.headers.clone();
            preview::decorate_headers(&mut response_headers, preview_headers.as_ref());
            let body = forward::into_axum_body(resp.body);
            (resp.status, response_headers, body).into_response()
        }
        Err(e) => {
            let mut h = HeaderMap::new();
            h.insert("x-tt-proxy-down", "true".parse().unwrap());
            (
                axum::http::StatusCode::BAD_GATEWAY,
                h,
                format!("proxy upstream error: {e}"),
            )
                .into_response()
        }
    }
}
