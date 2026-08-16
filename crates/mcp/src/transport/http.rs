//! Axum-based **Streamable HTTP** transport for the MCP server
//! (MCP spec 2025-03-26 — replaces the deprecated HTTP+SSE transport).
//!
//! A single MCP endpoint (`/mcp`) supports three HTTP methods:
//!
//!   POST   /mcp  — the client sends one JSON-RPC message in the body.
//!                  * A *request* (has `id`) is dispatched and the response is
//!                    returned either as a single `application/json` body or,
//!                    when the client's `Accept` prefers `text/event-stream`,
//!                    streamed as one SSE `message` event (then the stream
//!                    closes). The `initialize` response also carries the
//!                    `Mcp-Session-Id` header that the client must echo on all
//!                    subsequent requests.
//!                  * A *notification* / *response* (no `id`) is accepted with
//!                    `202 Accepted` and no body.
//!
//!   GET    /mcp  — opens a server→client SSE stream (`text/event-stream`) for
//!                  out-of-band server messages. Requires `Mcp-Session-Id`.
//!
//!   DELETE /mcp  — explicitly terminates the session named by `Mcp-Session-Id`.
//!
//! Session semantics: a session id is minted on `initialize` and required on
//! every later request. A request that needs a session but omits the header is
//! rejected `400 Bad Request`; an unknown/terminated id is rejected
//! `404 Not Found` (the spec's "session expired" signal).
//!
//! Security (shared with the legacy SSE transport): bearer auth + loopback
//! `Host`/`Origin` guard + a 1 MiB body cap — see [`super::guard`].

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, MethodRouter};
use axum::Json;
use futures::stream::{self, Stream};
use serde_json::Value;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::McpError;
use crate::protocol::{methods, JsonRpcRequest, JsonRpcResponse};
use crate::server::Server;

/// Header carrying the MCP session id (visible-ASCII UUID).
const SESSION_HEADER: &str = "Mcp-Session-Id";

/// 1 MiB cap on request bodies.
const MAX_BODY_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Session registry. A session is a logically-related set of interactions that
// begins at `initialize`. We track the live ids; presence == valid.
// ---------------------------------------------------------------------------

type SessionSet = Arc<Mutex<HashMap<Uuid, ()>>>;

#[derive(Clone)]
struct AppState {
    sessions: SessionSet,
    server: Arc<Server>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Boot the Streamable HTTP MCP server on `addr`. Requires
/// `Authorization: Bearer <auth_token>` and a loopback Host/Origin on every
/// request. Runs until SIGINT/SIGTERM.
pub async fn run(server: Server, addr: SocketAddr, auth_token: String) -> Result<(), McpError> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| McpError::Internal(format!("bind {addr}: {e}")))?;
    run_on_listener(
        server,
        listener,
        auth_token,
        super::guard::shutdown_signal("MCP Streamable HTTP"),
    )
    .await
}

/// Serve on an already-bound listener until `shutdown` resolves.
///
/// Embedders use this to bind port zero without a reserve/drop/rebind race and
/// to tie the loopback broker's lifetime to one local agent run.
pub async fn run_on_listener(
    server: Server,
    listener: tokio::net::TcpListener,
    auth_token: String,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), McpError> {
    let addr = listener
        .local_addr()
        .map_err(|e| McpError::Internal(format!("inspect listener address: {e}")))?;
    let state = AppState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        server: Arc::new(server),
    };
    let app = axum::Router::new()
        .route("/mcp", mcp_endpoint())
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            std::sync::Arc::<str>::from(auth_token.as_str()),
            super::guard::guard,
        ));

    tracing::info!(addr = %addr, "MCP Streamable HTTP server listening (bearer-auth, loopback-only)");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| McpError::Internal(format!("Streamable HTTP server: {e}")))
}

/// Single endpoint multiplexing POST / GET / DELETE on `/mcp`.
fn mcp_endpoint() -> MethodRouter<AppState> {
    any(dispatch_method)
}

async fn dispatch_method(
    State(state): State<AppState>,
    method: axum::http::Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    use axum::http::Method;
    match method {
        Method::POST => handle_post(state, headers, body).await,
        Method::GET => handle_get(state, headers).await,
        Method::DELETE => handle_delete(state, headers).await,
        // The endpoint exists but only POST/GET/DELETE are defined for it.
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /mcp — client→server JSON-RPC
// ---------------------------------------------------------------------------

async fn handle_post(state: AppState, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    // Parse the JSON-RPC envelope. A malformed body is a JSON-RPC parse error.
    let req: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let resp =
                JsonRpcResponse::err(None, McpError::Parse(e.to_string()).code(), e.to_string());
            return (StatusCode::BAD_REQUEST, Json(resp)).into_response();
        }
    };

    let is_initialize = req.method == methods::INITIALIZE;
    let is_request = req.id.is_some();

    // Session check. `initialize` is the one request allowed without a session
    // id (it mints one). Every other request must carry a *live* session id.
    let session_hdr = header_str(&headers, SESSION_HEADER);
    if !is_initialize {
        match session_hdr {
            None => {
                // Spec: a server requiring a session id SHOULD answer 400 to
                // requests (other than initialization) lacking the header.
                return (StatusCode::BAD_REQUEST, "missing Mcp-Session-Id").into_response();
            }
            Some(raw) => {
                let parsed = raw.parse::<Uuid>().ok();
                let live = match parsed {
                    Some(id) => state.sessions.lock().await.contains_key(&id),
                    None => false,
                };
                if !live {
                    // Unknown or terminated session → 404 (spec's expiry signal).
                    return (StatusCode::NOT_FOUND, "unknown or expired session").into_response();
                }
            }
        }
    }

    // Notifications and bare responses (no `id`) do not get a JSON-RPC reply.
    if !is_request {
        return StatusCode::ACCEPTED.into_response();
    }

    // Dispatch the request.
    let resp = state.server.dispatch(req).await;

    // On a successful initialize, mint + register a fresh session id.
    let new_session = if is_initialize && resp.error.is_none() {
        let id = Uuid::new_v4();
        state.sessions.lock().await.insert(id, ());
        Some(id)
    } else {
        None
    };

    let resp_value = match serde_json::to_value(&resp) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "failed to serialize MCP response");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Content negotiation: per the spec a server MAY answer a request with
    // either `application/json` (one object) or `text/event-stream` (an SSE
    // stream) — both are valid, and clients MUST support both. Clients are also
    // required to list *both* in `Accept`, so the mere presence of
    // `text/event-stream` is not a preference signal. We therefore default to
    // JSON and only upgrade to SSE when the client accepts SSE *and not* JSON.
    if prefers_event_stream(&headers) {
        sse_single(resp_value, new_session).into_response()
    } else {
        json_response(resp_value, new_session)
    }
}

/// Build a single-object `application/json` response, attaching `Mcp-Session-Id`
/// when a session was just minted.
fn json_response(value: Value, new_session: Option<Uuid>) -> Response {
    let mut resp = Json(value).into_response();
    if let Some(id) = new_session {
        insert_session_header(resp.headers_mut(), id);
    }
    resp
}

/// Build an SSE response carrying the JSON-RPC `value` as one `message` event,
/// then end the stream (spec: close after all responses sent).
fn sse_single(value: Value, new_session: Option<Uuid>) -> impl IntoResponse {
    let data = serde_json::to_string(&value).unwrap_or_else(|_| "null".into());
    let event = Event::default().event("message").data(data);
    let body: std::vec::IntoIter<Result<Event, std::convert::Infallible>> =
        vec![Ok(event)].into_iter();
    let stream = stream::iter(body);
    let sse = Sse::new(stream);
    // Sse<...> sets Content-Type: text/event-stream; we wrap it so we can also
    // attach the session header on initialize.
    let mut resp = sse.into_response();
    if let Some(id) = new_session {
        insert_session_header(resp.headers_mut(), id);
    }
    resp
}

// ---------------------------------------------------------------------------
// GET /mcp — server→client SSE stream
// ---------------------------------------------------------------------------

async fn handle_get(state: AppState, headers: HeaderMap) -> Response {
    // A GET must target a live session.
    match require_live_session(&state, &headers).await {
        Ok(()) => {}
        Err(resp) => return resp,
    }
    // Open an idle SSE stream. We currently have no server-initiated traffic to
    // push, so the stream simply stays open until the client disconnects; this
    // satisfies the spec's requirement that GET return `text/event-stream`.
    let empty: stream::Pending<Result<Event, std::convert::Infallible>> = stream::pending();
    open_sse(empty).into_response()
}

/// Wrap a stream as an SSE response with a keep-alive comment.
fn open_sse<S>(stream: S) -> Sse<S>
where
    S: Stream<Item = Result<Event, std::convert::Infallible>> + Send + 'static,
{
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

// ---------------------------------------------------------------------------
// DELETE /mcp — terminate a session
// ---------------------------------------------------------------------------

async fn handle_delete(state: AppState, headers: HeaderMap) -> Response {
    let id = match header_str(&headers, SESSION_HEADER).and_then(|s| s.parse::<Uuid>().ok()) {
        Some(id) => id,
        None => return (StatusCode::BAD_REQUEST, "missing Mcp-Session-Id").into_response(),
    };
    let removed = state.sessions.lock().await.remove(&id).is_some();
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Ensure the request carries a live `Mcp-Session-Id`. Returns the error
/// response (400 missing / 404 unknown) to short-circuit on failure.
async fn require_live_session(state: &AppState, headers: &HeaderMap) -> Result<(), Response> {
    match header_str(headers, SESSION_HEADER) {
        None => Err((StatusCode::BAD_REQUEST, "missing Mcp-Session-Id").into_response()),
        Some(raw) => {
            let live = match raw.parse::<Uuid>() {
                Ok(id) => state.sessions.lock().await.contains_key(&id),
                Err(_) => false,
            };
            if live {
                Ok(())
            } else {
                Err((StatusCode::NOT_FOUND, "unknown or expired session").into_response())
            }
        }
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// True when the client wants its response *streamed* over SSE: it accepts
/// `text/event-stream` and does **not** also accept `application/json`. When a
/// client lists both (as the spec requires for general requests) we treat that
/// as "no preference" and answer with JSON; an SSE-only `Accept` is an explicit
/// request for the streamed form.
fn prefers_event_stream(headers: &HeaderMap) -> bool {
    match header_str(headers, header::ACCEPT.as_str()) {
        None => false,
        Some(a) => a.contains("text/event-stream") && !a.contains("application/json"),
    }
}

fn insert_session_header(headers: &mut HeaderMap, id: Uuid) {
    if let Ok(val) = axum::http::HeaderValue::from_str(&id.to_string()) {
        headers.insert(SESSION_HEADER, val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdrs(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn accept_negotiation() {
        // Both listed (spec-mandated for general requests) → no preference → JSON.
        assert!(!prefers_event_stream(&hdrs(&[(
            "accept",
            "application/json, text/event-stream"
        )])));
        // SSE-only → explicit streaming preference.
        assert!(prefers_event_stream(&hdrs(&[(
            "accept",
            "text/event-stream"
        )])));
        // JSON-only → JSON.
        assert!(!prefers_event_stream(&hdrs(&[(
            "accept",
            "application/json"
        )])));
        // Absent → JSON.
        assert!(!prefers_event_stream(&hdrs(&[])));
    }

    #[test]
    fn session_header_round_trips_as_visible_ascii() {
        let id = Uuid::new_v4();
        let mut h = HeaderMap::new();
        insert_session_header(&mut h, id);
        let got = header_str(&h, SESSION_HEADER).unwrap();
        assert_eq!(got, id.to_string());
        assert!(got.bytes().all(|b| (0x21..=0x7e).contains(&b)));
    }
}
