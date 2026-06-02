//! End-to-end: start the proxy, point it at an httpmock upstream, fire a curl.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::response::IntoResponse;
use httpmock::prelude::*;
use tt_cli::proxy::{
    config::{Config, Mode},
    routes::anthropic::{post_messages, AppState},
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
}

#[tokio::test]
async fn anthropic_route_records_realized_savings_from_header() {
    // The gateway reports realized savings on `x-tokentrimmer-saved-usd`. The
    // proxy must read it into the session rollup so the Ctrl-C banner shows a
    // real "you saved $X" figure instead of $0.0000.
    let upstream = MockServer::start_async().await;
    let _m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(200)
                .header("x-tokentrimmer-cost-usd", "0.0001")
                .header("x-tokentrimmer-saved-usd", "0.0009")
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
    assert!(
        (snap.total_savings_usd - 0.0009).abs() < 1e-9,
        "rollup should record realized savings from the header, got {}",
        snap.total_savings_usd
    );
}
