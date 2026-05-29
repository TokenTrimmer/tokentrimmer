//! Helper for the proxy's pre-forward call to `POST /v1/preview` on the
//! TokenTrimmer Gateway.
//!
//! The proxy uses the result purely as decoration — it sets
//! `X-TT-Preview-Cost-Usd` and `X-TT-Suggested-Route` headers on the
//! response so the calling IDE can render a cost overlay. Failure to
//! fetch the preview (timeout, network error, upstream 5xx, no_preview
//! flag, bypass mode) is silently swallowed; the proxy continues to
//! forward the user request unchanged.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::proxy::config::{Config, Mode};

/// Hard cap on how long we'll wait for the preview before forwarding the
/// real request unannotated. 500ms keeps the proxy latency budget tight.
const PREVIEW_TIMEOUT_MS: u64 = 500;

/// Decoded fields of `PreviewResponse` that we actually use as response
/// headers. Everything else from the preview body is ignored.
#[derive(Debug, Clone, Default)]
pub struct PreviewHeaders {
    pub cost_usd: Option<f64>,
    pub suggested_route: Option<String>,
    pub suggested_savings_usd: Option<f64>,
}

/// Fetch a cost preview for the JSON body that the proxy is about to forward.
///
/// Returns `None` when:
/// - the proxy is in bypass mode (no gateway to ask),
/// - `--no-preview` is set,
/// - the body can't be parsed as a chat-shape JSON,
/// - the gateway returns a non-2xx,
/// - the request times out (`PREVIEW_TIMEOUT_MS`),
/// - any other transport error.
pub async fn fetch(http: &reqwest::Client, cfg: &Config, body: &[u8]) -> Option<PreviewHeaders> {
    if cfg.no_preview {
        return None;
    }
    if cfg.mode == Mode::Bypass {
        return None;
    }

    // The preview accepts a chat-shape body (model, messages, max_tokens).
    // Forward the same bytes the user sent — for both OpenAI and Anthropic
    // native shapes, this works because the preview reads only those fields.
    let parsed: Value = serde_json::from_slice(body).ok()?;
    // Require at least a `model` field. Anthropic and OpenAI both have one.
    if !parsed.get("model").is_some_and(|v| v.is_string()) {
        return None;
    }

    let url = format!("{}/v1/preview", cfg.gateway_base_url);
    let mut req = http
        .post(&url)
        .json(&parsed)
        .timeout(Duration::from_millis(PREVIEW_TIMEOUT_MS));
    if let Some(k) = &cfg.tt_api_key {
        req = req.bearer_auth(k);
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status=%resp.status(), "preview returned non-2xx; skipping header injection");
        return None;
    }
    let body: PreviewBody = resp.json().await.ok()?;

    Some(PreviewHeaders {
        cost_usd: body.current.as_ref().map(|c| c.cost_usd),
        suggested_route: body
            .route_suggestions
            .as_ref()
            .and_then(|v| v.first().map(|s| s.route.clone())),
        suggested_savings_usd: body
            .route_suggestions
            .as_ref()
            .and_then(|v| v.first().map(|s| s.savings_usd)),
    })
}

/// Minimal subset of `tt_preview::PreviewResponse` we need to render headers.
/// We don't depend on the `tt-preview` crate here so the proxy can ship even
/// when tt-preview isn't in the workspace.
#[derive(Debug, Deserialize, Default)]
struct PreviewBody {
    current: Option<Current>,
    route_suggestions: Option<Vec<RouteSuggestion>>,
}

#[derive(Debug, Deserialize)]
struct Current {
    cost_usd: f64,
}

#[derive(Debug, Deserialize)]
struct RouteSuggestion {
    route: String,
    savings_usd: f64,
}

/// Decorate an existing `HeaderMap` with the preview values. Safe to call on
/// any response — does nothing when `headers` is `None`.
pub fn decorate_headers(map: &mut axum::http::HeaderMap, headers: Option<&PreviewHeaders>) {
    let Some(h) = headers else {
        return;
    };
    if let Some(cost) = h.cost_usd {
        if let Ok(v) = format!("{cost:.6}").parse() {
            map.insert("x-tt-preview-cost-usd", v);
        }
    }
    if let Some(route) = &h.suggested_route {
        if let Ok(v) = route.parse() {
            map.insert("x-tt-suggested-route", v);
        }
    }
    if let Some(savings) = h.suggested_savings_usd {
        if let Ok(v) = format!("{savings:.6}").parse() {
            map.insert("x-tt-suggested-savings-usd", v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::config::{Config, Mode};
    use httpmock::prelude::*;
    use serde_json::json;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;

    fn make_cfg(gateway_base_url: String, mode: Mode, no_preview: bool) -> Config {
        Config {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            mode,
            tt_api_key: Some("test-key".into()),
            gateway_base_url,
            upstream_anthropic: "http://unused".into(),
            upstream_openai: "http://unused".into(),
            session_log_dir: PathBuf::from("/tmp/unused"),
            no_tui: true,
            no_preview,
        }
    }

    #[tokio::test]
    async fn fetch_returns_none_in_bypass_mode() {
        let cfg = make_cfg("http://nope".into(), Mode::Bypass, false);
        let body = json!({"model": "x", "messages": []}).to_string();
        assert!(fetch(&reqwest::Client::new(), &cfg, body.as_bytes()).await.is_none());
    }

    #[tokio::test]
    async fn fetch_returns_none_when_no_preview_flag() {
        let cfg = make_cfg("http://nope".into(), Mode::Gateway, true);
        let body = json!({"model": "x", "messages": []}).to_string();
        assert!(fetch(&reqwest::Client::new(), &cfg, body.as_bytes()).await.is_none());
    }

    #[tokio::test]
    async fn fetch_returns_none_when_body_not_chat_shape() {
        let cfg = make_cfg("http://nope".into(), Mode::Gateway, false);
        // Missing `model` field.
        let body = json!({"messages": []}).to_string();
        assert!(fetch(&reqwest::Client::new(), &cfg, body.as_bytes()).await.is_none());
    }

    #[tokio::test]
    async fn fetch_returns_some_on_2xx_with_current_block() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/preview");
                then.status(200).json_body(json!({
                    "current": { "cost_usd": 0.000189 },
                    "route_suggestions": [
                        { "route": "swap-to-haiku-4-5", "savings_usd": 0.000166 }
                    ]
                }));
            })
            .await;
        let cfg = make_cfg(server.base_url(), Mode::Gateway, false);
        let body = json!({"model": "claude-sonnet-4-6", "messages": []}).to_string();
        let result = fetch(&reqwest::Client::new(), &cfg, body.as_bytes()).await.unwrap();
        assert_eq!(result.cost_usd, Some(0.000189));
        assert_eq!(result.suggested_route.as_deref(), Some("swap-to-haiku-4-5"));
    }

    #[tokio::test]
    async fn fetch_returns_none_on_5xx() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/preview");
                then.status(500).body("boom");
            })
            .await;
        let cfg = make_cfg(server.base_url(), Mode::Gateway, false);
        let body = json!({"model": "x", "messages": []}).to_string();
        assert!(fetch(&reqwest::Client::new(), &cfg, body.as_bytes()).await.is_none());
    }

    #[test]
    fn decorate_noop_on_none() {
        let mut h = axum::http::HeaderMap::new();
        decorate_headers(&mut h, None);
        assert!(h.is_empty());
    }

    #[test]
    fn decorate_sets_all_three_headers_when_present() {
        let mut h = axum::http::HeaderMap::new();
        decorate_headers(
            &mut h,
            Some(&PreviewHeaders {
                cost_usd: Some(0.000189),
                suggested_route: Some("swap-to-haiku-4-5".into()),
                suggested_savings_usd: Some(0.000166),
            }),
        );
        assert!(h.contains_key("x-tt-preview-cost-usd"));
        assert!(h.contains_key("x-tt-suggested-route"));
        assert!(h.contains_key("x-tt-suggested-savings-usd"));
    }
}
