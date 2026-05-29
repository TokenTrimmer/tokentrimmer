//! POST /v1/messages — accept Anthropic native, forward to gateway or
//! upstream per the active mode.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::proxy::config::{Config, Mode};
use crate::proxy::forward;
use crate::proxy::preview;
use crate::proxy::session::{LogLine, SessionLog};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub log: Arc<SessionLog>,
}

pub async fn post_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let upstream = upstream_url(&state.config, "/v1/messages");
    let mut h = headers.clone();
    if state.config.mode == Mode::Gateway {
        if let Some(k) = &state.config.tt_api_key {
            h.insert("authorization", format!("Bearer {k}").parse().unwrap());
        }
    }
    // Best-effort cost preview. Bounded by PREVIEW_TIMEOUT_MS (500 ms) so we
    // never delay the user request by more than that even when the gateway
    // is slow. Failure → None → no header injection.
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
                route: "POST /v1/messages",
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
            response_headers.insert(
                "x-tt-proxy-mode",
                match state.config.mode {
                    Mode::Gateway => "gateway".parse().unwrap(),
                    Mode::Bypass => "bypass".parse().unwrap(),
                    Mode::Hybrid => "hybrid".parse().unwrap(),
                },
            );
            preview::decorate_headers(&mut response_headers, preview_headers.as_ref());
            let body = forward::into_axum_body(resp.body);
            (resp.status, response_headers, body).into_response()
        }
        Err(e) => {
            tracing::warn!(error=%e, "anthropic forward failed");
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

fn upstream_url(cfg: &Config, path: &str) -> String {
    match cfg.mode {
        Mode::Gateway | Mode::Hybrid => format!("{}{}", cfg.gateway_base_url, path),
        Mode::Bypass => format!("{}{}", cfg.upstream_anthropic, path),
    }
}
