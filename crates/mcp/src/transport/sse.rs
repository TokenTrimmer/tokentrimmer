//! Axum-based SSE transport for the MCP server.
//!
//! Protocol (MCP spec 2024-11-05):
//!   GET  /sse              — opens a persistent SSE stream; emits `event: endpoint`
//!                            with `data: /messages?sessionId=<uuid>` so the client
//!                            knows where to POST.
//!   POST /messages?sessionId=<uuid> — client sends a JSON-RPC request; server dispatches
//!                            via `Server::dispatch`, pushes the response as
//!                            `event: message` back over the SSE stream; returns 202.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use futures::stream::Stream;
use serde::Deserialize;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;
use uuid::Uuid;

use crate::error::McpError;
use crate::protocol::JsonRpcRequest;
use crate::server::Server;

// ---------------------------------------------------------------------------
// Shared session map: session_id → sender end of unbounded channel
// ---------------------------------------------------------------------------

type SessionMap = Arc<Mutex<HashMap<Uuid, mpsc::UnboundedSender<Result<Event, McpError>>>>>;

#[derive(Clone)]
struct AppState {
    sessions: SessionMap,
    server: Arc<Server>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Boot the SSE MCP server on `addr`. Runs until shutdown signal (SIGINT/SIGTERM).
pub async fn run(server: Server, addr: SocketAddr) -> Result<(), McpError> {
    let state = AppState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        server: Arc::new(server),
    };

    let app = axum::Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| McpError::Internal(format!("bind {addr}: {e}")))?;

    tracing::info!(addr = %addr, "MCP SSE server listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| McpError::Internal(format!("SSE server: {e}")))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// GET /sse
// ---------------------------------------------------------------------------

async fn sse_handler(State(state): State<AppState>) -> impl IntoResponse {
    let session_id = Uuid::new_v4();
    let (tx, rx) = mpsc::unbounded_channel::<Result<Event, McpError>>();

    // Register session
    state.sessions.lock().await.insert(session_id, tx.clone());

    // The one-time endpoint event tells the client where to POST.
    let endpoint_event = Event::default()
        .event("endpoint")
        .data(format!("/messages?sessionId={session_id}"));

    // Seed the channel with the endpoint event so the stream emits it first.
    let _ = tx.send(Ok(endpoint_event));

    // Wrap receiver in a tokio-stream adapter.
    let base_stream = UnboundedReceiverStream::new(rx);

    // Clone Arc so the cleanup closure can remove the session on drop.
    let sessions = state.sessions.clone();
    let cleanup_stream = CleanupStream {
        inner: base_stream,
        sessions,
        session_id,
    };

    Sse::new(cleanup_stream)
}

// ---------------------------------------------------------------------------
// POST /messages?sessionId=<uuid>
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SessionQuery {
    #[serde(rename = "sessionId")]
    session_id: Uuid,
}

async fn messages_handler(
    State(state): State<AppState>,
    Query(q): Query<SessionQuery>,
    Json(req): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let resp = state.server.dispatch(req).await;

    let json_str = match serde_json::to_string(&resp) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize MCP response");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let event = Event::default().event("message").data(json_str);

    let sessions = state.sessions.lock().await;
    match sessions.get(&q.session_id) {
        Some(tx) => {
            if tx.send(Ok(event)).is_err() {
                tracing::warn!(session_id = %q.session_id, "SSE sender closed");
                return StatusCode::GONE.into_response();
            }
            StatusCode::ACCEPTED.into_response()
        }
        None => {
            tracing::warn!(session_id = %q.session_id, "session not found");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// CleanupStream — removes the session entry when the stream is dropped
// (client disconnect).
// ---------------------------------------------------------------------------

use std::pin::Pin;
use std::task::{Context, Poll};

struct CleanupStream {
    inner: UnboundedReceiverStream<Result<Event, McpError>>,
    sessions: SessionMap,
    session_id: Uuid,
}

impl Stream for CleanupStream {
    type Item = Result<Event, McpError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl Drop for CleanupStream {
    fn drop(&mut self) {
        let sessions = self.sessions.clone();
        let session_id = self.session_id;
        // Spawn a task to remove the session asynchronously (can't await in drop).
        tokio::spawn(async move {
            sessions.lock().await.remove(&session_id);
            tracing::debug!(session_id = %session_id, "SSE session cleaned up");
        });
    }
}

// ---------------------------------------------------------------------------
// Graceful shutdown
// ---------------------------------------------------------------------------

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("MCP SSE shutdown: SIGINT"),
        _ = terminate => tracing::info!("MCP SSE shutdown: SIGTERM"),
    }
}
