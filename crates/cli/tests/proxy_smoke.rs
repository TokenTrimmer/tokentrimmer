//! End-to-end: start the proxy, point it at an httpmock upstream, fire a curl.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::response::IntoResponse;
use httpmock::prelude::*;
use tt_cli::proxy::{
    config::{Config, Mode},
    routes::{
        anthropic::{post_messages, AppState},
        openai::post_chat_completions,
    },
    session::SessionLog,
};

#[tokio::test]
async fn anthropic_route_forwards_to_upstream_and_logs() {
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(200)
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-cache", "miss")
                .body("ok");
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        mode: Mode::Bypass,
        tt_api_key: None,
        gateway_base_url: "http://unused".into(),
        upstream_anthropic: upstream.base_url(),
        upstream_openai: "http://unused".into(),
        session_log_dir: tmp.path().to_path_buf(),
        no_tui: true,
        no_preview: true,
    };
    let log = Arc::new(SessionLog::new(&cfg.session_log_dir).unwrap());
    let state = AppState {
        config: Arc::new(cfg),
        http: reqwest::Client::new(),
        log: log.clone(),
    };
    let headers = axum::http::HeaderMap::new();
    let resp = post_messages(
        axum::extract::State(state),
        headers,
        Bytes::from_static(b"req"),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), 200);
    // Give the appender a moment if it spawns.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let snap = log.snapshot();
    assert_eq!(snap.requests, 1);
    assert_eq!(snap.measured_request_deltas, 0);
    assert_eq!(snap.unmeasured_request_deltas, 1);
}

/// The gateway now exposes an Anthropic-native /v1/messages ingress that runs
/// the same routing/cache/failover pipeline as /v1/chat/completions. So
/// Gateway and Hybrid mode forward /v1/messages to the GATEWAY (Gateway injects
/// the TokenTrimmer key; Hybrid passes the client's own credential through).
/// Only Bypass mode forwards direct to the Anthropic upstream, with the
/// client's own credential and no TokenTrimmer key injection.
#[tokio::test]
async fn messages_route_to_gateway_in_gateway_and_hybrid_else_anthropic() {
    // Helper: build a config whose configured upstream points at `upstream` and
    // whose *other* upstream is unroutable, so a mis-routed request 502s.
    for (mode, expect_tt_key_injected) in [
        (Mode::Gateway, true),
        (Mode::Hybrid, false),
        (Mode::Bypass, false),
    ] {
        let routed = MockServer::start_async().await;
        let mock = routed
            .mock_async(move |when, then| {
                when.method(POST)
                    .path("/v1/messages")
                    // Client's own Anthropic credential must always pass through.
                    .header("x-api-key", "client-anthropic-key")
                    // In Gateway mode the proxy injects its TokenTrimmer key on
                    // `authorization`; in Hybrid/Bypass it must NOT (that would
                    // leak it / clobber the client's OAuth on the Anthropic path).
                    .is_true(move |req| {
                        let has_auth = req.headers_vec().iter().any(|(k, v)| {
                            k.eq_ignore_ascii_case("authorization") && v == "Bearer tt-secret"
                        });
                        has_auth == expect_tt_key_injected
                    });
                then.status(200).body("ok");
            })
            .await;

        // The routed upstream is whichever the mode should hit; the other is
        // unroutable so a mis-route fails with a 502 instead of silently passing.
        let (gateway_base_url, upstream_anthropic) = match mode {
            Mode::Gateway | Mode::Hybrid => (routed.base_url(), "http://127.0.0.1:9".to_string()),
            Mode::Bypass => ("http://127.0.0.1:9".to_string(), routed.base_url()),
        };

        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            mode,
            tt_api_key: Some("tt-secret".into()),
            gateway_base_url,
            upstream_anthropic,
            upstream_openai: "http://unused".into(),
            session_log_dir: tmp.path().to_path_buf(),
            no_tui: true,
            no_preview: true,
        };
        let log = Arc::new(SessionLog::new(&cfg.session_log_dir).unwrap());
        let state = AppState {
            config: Arc::new(cfg),
            http: reqwest::Client::new(),
            log,
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-api-key", "client-anthropic-key".parse().unwrap());
        let resp = post_messages(
            axum::extract::State(state),
            headers,
            Bytes::from_static(b"req"),
        )
        .await
        .into_response();
        assert_eq!(
            resp.status(),
            200,
            "mode {mode:?}: /v1/messages must reach the routed upstream"
        );
        assert_eq!(mock.calls_async().await, 1, "mode {mode:?}");
    }
}

#[tokio::test]
async fn anthropic_route_records_signed_request_delta_and_legacy_compatibility_value() {
    // A complete component set is a measured signed delta. The legacy
    // positive-only header remains in the rollup for existing JSONL readers,
    // but it must not hide this regression.
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(200)
                .header("x-tokentrimmer-baseline-cost-usd", "0.0010")
                .header("x-tokentrimmer-cost-usd", "0.0008")
                .header("x-tokentrimmer-provider-cache-saved-usd", "0.0001")
                .header("x-tokentrimmer-cache-bust-usd", "0.0001")
                .header("x-tokentrimmer-summarizer-tax-usd", "0.0002")
                .header("x-tokentrimmer-saved-usd", "0.0000")
                .header("x-tokentrimmer-cache", "hit-l1")
                .body("ok");
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        mode: Mode::Bypass,
        tt_api_key: None,
        gateway_base_url: "http://unused".into(),
        upstream_anthropic: upstream.base_url(),
        upstream_openai: "http://unused".into(),
        session_log_dir: tmp.path().to_path_buf(),
        no_tui: true,
        no_preview: true,
    };
    let log = Arc::new(SessionLog::new(&cfg.session_log_dir).unwrap());
    let state = AppState {
        config: Arc::new(cfg),
        http: reqwest::Client::new(),
        log: log.clone(),
    };
    let resp = post_messages(
        axum::extract::State(state),
        axum::http::HeaderMap::new(),
        Bytes::from_static(b"req"),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), 200);
    tokio::time::sleep(Duration::from_millis(10)).await;

    let snap = log.snapshot();
    assert_eq!(snap.measured_request_deltas, 1);
    assert_eq!(snap.unmeasured_request_deltas, 0);
    assert!(
        (snap.total_signed_request_delta_usd + 0.0002).abs() < 1e-12,
        "signed delta should preserve a regression, got {}",
        snap.total_signed_request_delta_usd
    );
    assert_eq!(snap.total_positive_request_delta_usd, 0.0);
    assert!((snap.total_regression_request_delta_usd - 0.0002).abs() < 1e-12);
    assert!(
        (snap.total_savings_usd - 0.0).abs() < 1e-12,
        "legacy compatibility value should remain independent, got {}",
        snap.total_savings_usd
    );
}

#[tokio::test]
async fn openai_route_records_positive_signed_request_delta_from_all_components() {
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("x-tokentrimmer-baseline-cost-usd", "0.0020")
                .header("x-tokentrimmer-cost-usd", "0.0010")
                .header("x-tokentrimmer-provider-cache-saved-usd", "0.0002")
                .header("x-tokentrimmer-cache-bust-usd", "0.0001")
                .header("x-tokentrimmer-summarizer-tax-usd", "0.0002")
                .header("x-tokentrimmer-saved-usd", "0.0005")
                .body("ok");
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        mode: Mode::Bypass,
        tt_api_key: None,
        gateway_base_url: "http://unused".into(),
        upstream_anthropic: "http://unused".into(),
        upstream_openai: upstream.base_url(),
        session_log_dir: tmp.path().to_path_buf(),
        no_tui: true,
        no_preview: true,
    };
    let log = Arc::new(SessionLog::new(&cfg.session_log_dir).unwrap());
    let state = AppState {
        config: Arc::new(cfg),
        http: reqwest::Client::new(),
        log: log.clone(),
    };
    let resp = post_chat_completions(
        axum::extract::State(state),
        axum::http::HeaderMap::new(),
        Bytes::from_static(b"req"),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), 200);

    let snap = log.snapshot();
    assert_eq!(snap.measured_request_deltas, 1);
    assert_eq!(snap.unmeasured_request_deltas, 0);
    assert!((snap.total_signed_request_delta_usd - 0.0005).abs() < 1e-12);
    assert!((snap.total_positive_request_delta_usd - 0.0005).abs() < 1e-12);
    assert_eq!(snap.total_regression_request_delta_usd, 0.0);
    assert!((snap.total_savings_usd - 0.0005).abs() < 1e-12);
    let body = std::fs::read_to_string(log.path()).unwrap();
    assert!(body.contains("\"state\":\"measured\""));
}

#[tokio::test]
async fn openai_error_response_with_invalid_components_is_unmeasured() {
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(503)
                .header("x-tokentrimmer-baseline-cost-usd", "not-a-number")
                .header("x-tokentrimmer-cost-usd", "0.0010")
                .header("x-tokentrimmer-provider-cache-saved-usd", "0.0002")
                .header("x-tokentrimmer-cache-bust-usd", "0.0001")
                .header("x-tokentrimmer-summarizer-tax-usd", "0.0002")
                .header("x-tokentrimmer-saved-usd", "0.1234")
                .body("gateway error");
        })
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        mode: Mode::Bypass,
        tt_api_key: None,
        gateway_base_url: "http://unused".into(),
        upstream_anthropic: "http://unused".into(),
        upstream_openai: upstream.base_url(),
        session_log_dir: tmp.path().to_path_buf(),
        no_tui: true,
        no_preview: true,
    };
    let log = Arc::new(SessionLog::new(&cfg.session_log_dir).unwrap());
    let state = AppState {
        config: Arc::new(cfg),
        http: reqwest::Client::new(),
        log: log.clone(),
    };
    let resp = post_chat_completions(
        axum::extract::State(state),
        axum::http::HeaderMap::new(),
        Bytes::from_static(b"req"),
    )
    .await
    .into_response();
    assert_eq!(resp.status(), 503);

    let snap = log.snapshot();
    assert_eq!(snap.requests, 1);
    assert_eq!(snap.measured_request_deltas, 0);
    assert_eq!(snap.unmeasured_request_deltas, 1);
    assert_eq!(snap.total_signed_request_delta_usd, 0.0);
    assert!((snap.total_savings_usd - 0.1234).abs() < 1e-12);
    let body = std::fs::read_to_string(log.path()).unwrap();
    assert!(body.contains("\"state\":\"unmeasured\""));
}
