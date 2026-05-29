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
}

pub async fn run(config: Config) -> Result<(), ListenerError> {
    let log = SessionLog::new(&config.session_log_dir)
        .map_err(|e| ListenerError::SessionLog(e.to_string()))?;
    let log = Arc::new(log);
    let state = anthropic::AppState {
        config: Arc::new(config.clone()),
        http: reqwest::Client::new(),
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
