//! Axum server with SIGTERM/SIGINT graceful shutdown.

use std::sync::Arc;

use axum::{routing::get, Router};
use thiserror::Error;
use tokio::signal;

use crate::proxy::config::Config;
use crate::proxy::routes::{anthropic, models, openai};
use crate::proxy::session::SessionLog;

#[derive(Debug, Error)]
pub enum ListenerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session log: {0}")]
    SessionLog(String),
    #[error("http client: {0}")]
    HttpClient(#[source] reqwest::Error),
}

pub async fn run(config: Config) -> Result<(), ListenerError> {
    let log = SessionLog::new(&config.session_log_dir)
        .map_err(|e| ListenerError::SessionLog(e.to_string()))?;
    let log = Arc::new(log);
    let state = anthropic::AppState {
        config: Arc::new(config.clone()),
        http: build_http_client()?,
        log: log.clone(),
    };

    let app = Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(anthropic::post_messages),
        )
        .route(
            "/v1/chat/completions",
            axum::routing::post(openai::post_chat_completions),
        )
        .route("/v1/models", get(models::get_models))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    tracing::info!(addr=%config.bind, "tt proxy listening");
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_then_banner(log, config.no_tui))
        .await?;
    Ok(())
}

/// Model API redirects are unnecessary and unsafe for a credential-forwarding
/// proxy: an `x-api-key` header is not a standard redirect-sensitive header.
/// Refusing redirects keeps the validated upstream origin as the only recipient.
fn build_http_client() -> Result<reqwest::Client, ListenerError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(ListenerError::HttpClient)
}

async fn shutdown_then_banner(log: Arc<SessionLog>, no_tui: bool) {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    if !no_tui {
        crate::proxy::tui::print_summary(&log.snapshot(), log.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::forward;
    use axum::http::HeaderMap;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn proxy_client_does_not_follow_credential_bearing_redirects() {
        let target = MockServer::start_async().await;
        let target_request = target
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/capture")
                    .header("x-api-key", "provider-secret");
                then.status(200);
            })
            .await;

        let origin = MockServer::start_async().await;
        let redirect = origin
            .mock_async(|when, then| {
                when.method(POST).path("/v1/messages");
                then.status(307)
                    .header("location", format!("{}/capture", target.base_url()));
            })
            .await;

        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "provider-secret".parse().unwrap());
        let response = forward::forward_post(
            &build_http_client().unwrap(),
            &format!("{}/v1/messages", origin.base_url()),
            headers,
            bytes::Bytes::new(),
        )
        .await
        .unwrap();

        assert_eq!(response.status, 307);
        redirect.assert_calls(1);
        target_request.assert_calls(0);
    }
}
