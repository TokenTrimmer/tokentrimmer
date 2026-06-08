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
            guard,
        ));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| McpError::Internal(format!("bind {addr}: {e}")))?;

    tracing::info!(addr = %addr, "MCP SSE server listening (bearer-auth, loopback-only)");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| McpError::Internal(format!("SSE server: {e}")))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Auth + DNS-rebind guard middleware
// ---------------------------------------------------------------------------

/// Reject any request lacking a valid bearer token or arriving with a non-loopback
/// Host/Origin (DNS-rebind defense). Runs before body read + dispatch.
async fn guard(
    axum::extract::State(token): axum::extract::State<std::sync::Arc<str>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    let headers = req.headers();

    if !host_is_local(headers.get(header::HOST)) {
        return (StatusCode::FORBIDDEN, "non-local Host").into_response();
    }
    if !origin_is_local_or_absent(headers.get(header::ORIGIN)) {
        return (StatusCode::FORBIDDEN, "cross-origin").into_response();
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if !presented.is_some_and(|p| ct_eq(p.as_bytes(), token.as_bytes())) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"tt-mcp\"")],
            "missing or invalid bearer token",
        )
            .into_response();
    }
    next.run(req).await
}

/// Constant-time byte compare (length mismatch returns early — token length is
/// not sensitive). Mirrors the cloud `constant_time_eq`.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Host header host-part is loopback (ignoring port). Missing Host → false.
fn host_is_local(h: Option<&axum::http::HeaderValue>) -> bool {
    h.and_then(|v| v.to_str().ok())
        .is_some_and(is_local_authority)
}

/// Origin absent (non-browser MCP clients) or its host is loopback.
fn origin_is_local_or_absent(h: Option<&axum::http::HeaderValue>) -> bool {
    match h.and_then(|v| v.to_str().ok()) {
        None => true,
        // file:// pages and sandboxed iframes send Origin: null. Accepted because
        // the bearer token is still required, so this is not a bypass.
        Some("null") => true,
        Some(s) => s
            .strip_prefix("http://")
            .or_else(|| s.strip_prefix("https://"))
            .is_some_and(is_local_authority),
    }
}

/// Host-part (strip a trailing `:port`) is 127.0.0.1 / localhost / ::1.
/// Note: a bare IPv6 `::1` (no brackets) is rejected per RFC 7230 — use `[::1]`.
fn is_local_authority(authority: &str) -> bool {
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or("") // IPv6 "[::1]:port" → "::1"
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
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

#[cfg(test)]
mod tests {
    use super::{ct_eq, host_is_local, is_local_authority, origin_is_local_or_absent};

    #[test]
    fn ct_eq_matches_and_mismatches() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab")); // length mismatch
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn local_authority_accepts_loopback_rejects_spoofs() {
        assert!(is_local_authority("127.0.0.1"));
        assert!(is_local_authority("127.0.0.1:8080"));
        assert!(is_local_authority("localhost"));
        assert!(is_local_authority("localhost:9000"));
        assert!(is_local_authority("[::1]"));
        assert!(is_local_authority("[::1]:7000"));
        // Spoofs that must be rejected:
        assert!(!is_local_authority("127.0.0.1.evil.com"));
        assert!(!is_local_authority("localhost.evil.com"));
        assert!(!is_local_authority("evil.com"));
        assert!(!is_local_authority("LOCALHOST")); // case-sensitive on purpose
        assert!(!is_local_authority("::1")); // bare ipv6 (unbracketed) rejected
    }

    #[test]
    fn host_and_origin_helpers() {
        use axum::http::HeaderValue;
        assert!(host_is_local(Some(&HeaderValue::from_static(
            "127.0.0.1:8080"
        ))));
        assert!(!host_is_local(None)); // missing Host rejected
        assert!(!host_is_local(Some(&HeaderValue::from_static("evil.com"))));
        assert!(origin_is_local_or_absent(None));
        assert!(origin_is_local_or_absent(Some(&HeaderValue::from_static(
            "null"
        ))));
        assert!(origin_is_local_or_absent(Some(&HeaderValue::from_static(
            "http://localhost:3000"
        ))));
        assert!(!origin_is_local_or_absent(Some(&HeaderValue::from_static(
            "https://evil.com"
        ))));
    }
}
