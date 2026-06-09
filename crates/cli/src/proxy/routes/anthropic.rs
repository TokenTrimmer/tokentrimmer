//! POST /v1/messages — accept Anthropic native, forward direct to the
//! Anthropic upstream (bypass path) in every mode: the gateway exposes no
//! /v1/messages ingress yet, so routing it there is a guaranteed 404.

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
    // Anthropic traffic always takes the bypass path (see `upstream_url`),
    // so the client's own credentials pass through untouched — never inject
    // the TokenTrimmer key: it would leak to Anthropic and clobber OAuth.
    //
    // Best-effort cost preview. Bounded by PREVIEW_TIMEOUT_MS (500 ms) so we
    // never delay the user request by more than that even when the gateway
    // is slow. Failure → None → no header injection.
    let preview_headers = preview::fetch(&state.http, &state.config, &body).await;
    match forward::forward_post(&state.http, &upstream, headers, body).await {
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
                suggested_route: preview_headers
                    .as_ref()
                    .and_then(|p| p.suggested_route.as_deref()),
                suggested_savings_usd: preview_headers
                    .as_ref()
                    .and_then(|p| p.suggested_savings_usd),
                realized_savings_usd: resp
                    .headers
                    .get("x-tokentrimmer-saved-usd")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok()),
                trace_id: resp
                    .headers
                    .get("x-tokentrimmer-trace-id")
                    .and_then(|v| v.to_str().ok()),
            });
            if !state.config.no_tui {
                crate::proxy::tui::print_live_line(&state.log.snapshot());
            }
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

/// Anthropic wire endpoints always go direct to the Anthropic upstream
/// (the bypass path), regardless of mode: the gateway exposes no
/// /v1/messages ingress yet, so forwarding there would 404. Revisit once
/// the gateway grows an Anthropic-native route.
fn upstream_url(cfg: &Config, path: &str) -> String {
    format!("{}{}", cfg.upstream_anthropic, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;

    fn cfg_with_mode(mode: Mode) -> Config {
        Config {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            mode,
            tt_api_key: Some("tt-secret".into()),
            gateway_base_url: "http://gateway.test".into(),
            upstream_anthropic: "http://anthropic.test".into(),
            upstream_openai: "http://openai.test".into(),
            session_log_dir: PathBuf::from("/tmp/unused"),
            no_tui: true,
            no_preview: true,
        }
    }

    /// The gateway exposes no /v1/messages ingress, so Anthropic wire
    /// traffic must take the bypass path in every mode — routing it to the
    /// gateway in Gateway/Hybrid is a guaranteed 404.
    #[test]
    fn messages_route_to_anthropic_upstream_in_all_modes() {
        for mode in [Mode::Gateway, Mode::Bypass, Mode::Hybrid] {
            let cfg = cfg_with_mode(mode);
            assert_eq!(
                upstream_url(&cfg, "/v1/messages"),
                "http://anthropic.test/v1/messages",
                "mode {mode:?} must bypass the gateway for /v1/messages"
            );
        }
    }
}
