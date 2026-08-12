//! POST /v1/messages — accept Anthropic native, forward per mode.
//!
//! The hosted gateway now exposes an Anthropic-native `/v1/messages` ingress
//! that multiplexes through the SAME routing/cache/failover pipeline as
//! `/v1/chat/completions` (see `tt-core`'s `routes::messages` → `chat::handler`).
//! So in Gateway and Hybrid mode we forward `/v1/messages` to the GATEWAY — just
//! like the OpenAI-wire route — so Anthropic-wire clients (Claude Code, Cursor)
//! actually get caching + routing + failover, not logging-only. Only Bypass mode
//! (no gateway) forwards direct to the Anthropic upstream.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::proxy::config::{Config, ForwardTarget, Mode};
use crate::proxy::forward;
use crate::proxy::preview;
use crate::proxy::session::{gateway_accounting_from_headers, LogLine, SessionLog};

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
    let headers = match forward::prepare_forward_headers(
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
    // Best-effort cost preview. Bounded by PREVIEW_TIMEOUT_MS (500 ms) so we
    // never delay the user request by more than that even when the gateway
    // is slow. Failure → None → no header injection.
    let preview_headers = preview::fetch(&state.http, &state.config, &body).await;
    match forward::forward_post(&state.http, &upstream, headers, body).await {
        Ok(resp) => {
            let accounting = gateway_accounting_from_headers(&resp.headers);
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
                cost_usd: accounting.cost_usd,
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
                realized_savings_usd: accounting.realized_savings_usd,
                request_delta_estimate: accounting.request_delta_estimate,
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

/// Pick the upstream for an Anthropic-wire path per mode. Gateway and Hybrid
/// route through the gateway (which now has an Anthropic `/v1/messages` ingress
/// that runs the full routing/cache/failover pipeline); only Bypass forwards
/// direct to the Anthropic upstream.
fn upstream_url(cfg: &Config, path: &str) -> String {
    match cfg.mode.contract().target {
        ForwardTarget::Gateway => format!("{}{}", cfg.gateway_base_url, path),
        ForwardTarget::Provider => format!("{}{}", cfg.upstream_anthropic, path),
    }
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

    /// The gateway now exposes an Anthropic-native /v1/messages ingress that
    /// multiplexes through the same routing/cache/failover pipeline as
    /// /v1/chat/completions. So Gateway and Hybrid mode must forward
    /// /v1/messages to the GATEWAY (to get those features), exactly like the
    /// OpenAI-wire route does — only Bypass (no gateway) goes direct to
    /// Anthropic.
    #[test]
    fn messages_route_to_gateway_in_gateway_and_hybrid_modes() {
        for mode in [Mode::Gateway, Mode::Hybrid] {
            let cfg = cfg_with_mode(mode);
            assert_eq!(
                upstream_url(&cfg, "/v1/messages"),
                "http://gateway.test/v1/messages",
                "mode {mode:?} must route /v1/messages through the gateway"
            );
        }
    }

    /// Bypass mode has no gateway, so /v1/messages goes direct to Anthropic.
    #[test]
    fn messages_route_to_anthropic_upstream_in_bypass_mode() {
        let cfg = cfg_with_mode(Mode::Bypass);
        assert_eq!(
            upstream_url(&cfg, "/v1/messages"),
            "http://anthropic.test/v1/messages",
            "bypass mode must forward /v1/messages direct to Anthropic"
        );
    }

    /// End-to-end through the handler: in Gateway/Hybrid the POST must land at
    /// the gateway base URL's /v1/messages (not api.anthropic.com), and in
    /// Bypass it must land at the Anthropic upstream. Mirrors the httpmock
    /// style of `forward.rs`.
    #[tokio::test]
    async fn handler_forwards_messages_to_gateway_then_anthropic_per_mode() {
        use crate::proxy::session::SessionLog;
        use axum::extract::State;
        use axum::http::HeaderMap;
        use httpmock::prelude::*;
        use std::sync::Arc;

        let gateway = MockServer::start_async().await;
        let anthropic = MockServer::start_async().await;

        let gw_mock = gateway
            .mock_async(|when, then| {
                when.method(POST).path("/v1/messages");
                then.status(200).body("gateway-resp");
            })
            .await;
        let anthropic_mock = anthropic
            .mock_async(|when, then| {
                when.method(POST).path("/v1/messages");
                then.status(200).body("anthropic-resp");
            })
            .await;

        let tmp = std::env::temp_dir().join(format!("tt-proxy-test-{}", std::process::id()));
        let log = Arc::new(SessionLog::new(&tmp).unwrap());

        let make_state = |mode: Mode| AppState {
            config: Arc::new(Config {
                bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                mode,
                tt_api_key: Some("tt-secret".into()),
                gateway_base_url: gateway.base_url(),
                upstream_anthropic: anthropic.base_url(),
                upstream_openai: "http://openai.unused".into(),
                session_log_dir: tmp.clone(),
                no_tui: true,
                no_preview: true,
            }),
            http: reqwest::Client::new(),
            log: log.clone(),
        };

        // Gateway + Hybrid → gateway ingress.
        for mode in [Mode::Gateway, Mode::Hybrid] {
            let resp = post_messages(
                State(make_state(mode)),
                HeaderMap::new(),
                bytes::Bytes::from_static(b"{}"),
            )
            .await;
            assert_eq!(resp.status(), 200, "mode {mode:?} status");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                &body[..],
                b"gateway-resp",
                "mode {mode:?} must hit the gateway, not Anthropic"
            );
        }

        // Bypass → Anthropic upstream.
        let resp = post_messages(
            State(make_state(Mode::Bypass)),
            HeaderMap::new(),
            bytes::Bytes::from_static(b"{}"),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"anthropic-resp", "bypass must hit Anthropic");

        gw_mock.assert_calls(2); // Gateway + Hybrid
        anthropic_mock.assert_calls(1); // Bypass only
    }
}
