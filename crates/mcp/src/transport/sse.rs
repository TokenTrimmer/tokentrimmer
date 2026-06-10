//! Axum-based SSE transport for the MCP server.
//!
//! **DEPRECATED.** This is the legacy HTTP+SSE transport (MCP spec 2024-11-05),
//! superseded by the Streamable HTTP transport in [`super::http`] (MCP spec
//! 2025-03-26). It is retained only for backward compatibility with older MCP
//! clients; new deployments should use `--transport http`.
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

/// 1 MiB cap on POST /messages bodies (GET /sse has none).
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Boot the SSE MCP server on `addr`. Requires `Authorization: Bearer <auth_token>`
/// and a loopback Host/Origin on every request. Runs until SIGINT/SIGTERM.
pub async fn run(server: Server, addr: SocketAddr, auth_token: String) -> Result<(), McpError> {
    let state = AppState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        server: Arc::new(server),
    };

    let app = axum::Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::<str>::from(auth_token.as_str()),
            super::guard::guard,
        ));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| McpError::Internal(format!("bind {addr}: {e}")))?;

    tracing::info!(addr = %addr, "MCP SSE server listening (bearer-auth, loopback-only)");

    axum::serve(listener, app)
        .with_graceful_shutdown(super::guard::shutdown_signal("MCP SSE"))
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
        let session_id = self.session_id;
        // Fast path: if the lock is free, remove synchronously — no task, no
        // leak on runtime shutdown.
        if let Ok(mut guard) = self.sessions.try_lock() {
            guard.remove(&session_id);
            tracing::debug!(session_id = %session_id, "SSE session cleaned up (sync)");
            return;
        }
        // Contended: only spawn if a runtime is actually live, else the task
        // would silently leak. The session map self-heals (stale senders error
        // on next POST), so dropping cleanup here is safe.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let sessions = self.sessions.clone();
            handle.spawn(async move {
                sessions.lock().await.remove(&session_id);
                tracing::debug!(session_id = %session_id, "SSE session cleaned up (async)");
            });
        } else {
            tracing::debug!(session_id = %session_id, "SSE session cleanup skipped — no runtime");
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cleanup_stream_removes_session_synchronously_on_drop() {
        use super::{CleanupStream, SessionMap};
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::sync::{mpsc, Mutex};
        use tokio_stream::wrappers::UnboundedReceiverStream;
        use uuid::Uuid;

        let sessions: SessionMap = Arc::new(Mutex::new(HashMap::new()));
        let id = Uuid::from_u128(1);
        let (tx, rx) = mpsc::unbounded_channel();
        sessions.try_lock().unwrap().insert(id, tx);

        let cleanup = CleanupStream {
            inner: UnboundedReceiverStream::new(rx),
            sessions: sessions.clone(),
            session_id: id,
        };
        assert!(sessions.try_lock().unwrap().contains_key(&id));

        // Lock is uncontended → Drop removes synchronously, no spawned task.
        drop(cleanup);
        assert!(
            !sessions.try_lock().unwrap().contains_key(&id),
            "Drop should remove the session synchronously via try_lock"
        );
    }
}
