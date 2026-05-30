//! Retrieval middleware. Inspects the request body for <retrievable> tags
//! and, if present, runs substitution before the chat handler dispatches.
//!
//! Wired via `Router::layer(axum::middleware::from_fn_with_state(...))` in
//! `server.rs`. When the substitution succeeds, sets an X-TT-Retrieval-*
//! response header on the response.
//!
//! # Activation
//! Set `TT_RETRIEVAL_STORE=memory|postgres` and `TT_OPENAI_EMBED_KEY=<key>` at
//! Gateway boot (the `postgres` store also needs `DATABASE_URL`). When the
//! required vars are absent, retrieval is disabled and
//! `x-tt-retrieval-enabled: disabled` is returned on every response.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;
use tracing::{debug, warn};

use tt_auth::ApiKeyContext;
use tt_retrieval::embed::EmbeddingClient;
use tt_retrieval::audit::RetrievalAuditLog;
use tt_retrieval::store::memory::MemoryStore;
use tt_retrieval::store::postgres::PostgresStore;
use tt_retrieval::store::RetrievalStore;
use tt_retrieval::substitute_in_messages;

/// Maximum body size we'll buffer for retrieval inspection: 1 MiB.
const MAX_BYTES: usize = 1 << 20;

/// Shared state passed to the retrieval middleware when it is active.
#[derive(Clone)]
pub struct RetrievalState {
    pub store: Arc<dyn RetrievalStore + Send + Sync>,
    pub embedder: Arc<EmbeddingClient>,
    /// Encrypted-prompt audit log (Track E §10). `Some` only with the Postgres
    /// store + `TT_MASTER_KEY`; writes are best-effort and never fail a request.
    pub audit: Option<Arc<RetrievalAuditLog>>,
}

/// Build `RetrievalState` from environment variables.
/// Returns `None` (with a log message) when env vars are missing or invalid.
pub fn build_retrieval_state() -> Option<RetrievalState> {
    let store_kind = match std::env::var("TT_RETRIEVAL_STORE") {
        Ok(v) => v,
        Err(_) => {
            debug!("TT_RETRIEVAL_STORE not set — retrieval middleware disabled");
            return None;
        }
    };

    let embed_key = match std::env::var("TT_OPENAI_EMBED_KEY") {
        Ok(v) => v,
        Err(_) => {
            warn!(
                "TT_RETRIEVAL_STORE is set but TT_OPENAI_EMBED_KEY is missing — retrieval middleware disabled"
            );
            return None;
        }
    };

    // Optional encrypted-prompt audit log (Track E §10) — built alongside the
    // Postgres store when TT_MASTER_KEY is present.
    let mut audit: Option<Arc<RetrievalAuditLog>> = None;
    let store: Arc<dyn RetrievalStore + Send + Sync> = match store_kind.as_str() {
        "memory" => Arc::new(MemoryStore::new()),
        "postgres" => {
            let url = match std::env::var("DATABASE_URL") {
                Ok(u) => u,
                Err(_) => {
                    warn!(
                        "TT_RETRIEVAL_STORE=postgres but DATABASE_URL is missing — retrieval middleware disabled"
                    );
                    return None;
                }
            };
            // Lazy pool: connects on first use, so this stays sync and never
            // blocks boot on a cold Neon start (mirrors the L2 cache wiring).
            match sqlx::PgPool::connect_lazy(&url) {
                Ok(pool) => {
                    // The audit log shares the pool; enabled only when
                    // TT_MASTER_KEY is set (otherwise it's silently off).
                    audit = RetrievalAuditLog::from_env(pool.clone()).map(Arc::new);
                    Arc::new(PostgresStore::new(pool))
                }
                Err(e) => {
                    warn!(error = %e, "failed to build Postgres retrieval store — retrieval middleware disabled");
                    return None;
                }
            }
        }
        other => {
            warn!(
                store_kind = %other,
                "TT_RETRIEVAL_STORE has unknown value — retrieval middleware disabled"
            );
            return None;
        }
    };

    let embedder = Arc::new(EmbeddingClient::openai(embed_key));
    Some(RetrievalState {
        store,
        embedder,
        audit,
    })
}

/// Middleware entry point when retrieval is **disabled** (no env vars or
/// invalid config). Adds `x-tt-retrieval-enabled: disabled` to every response.
pub async fn maybe_substitute_disabled(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        "x-tt-retrieval-enabled",
        HeaderValue::from_static("disabled"),
    );
    resp
}

/// Middleware entry point when retrieval is **enabled**. Buffers the request
/// body, looks for `<retrievable` tags, calls `substitute_in_messages` when
/// found, then forwards the (possibly modified) body downstream.
pub async fn maybe_substitute(
    State(state): State<RetrievalState>,
    req: Request,
    next: Next,
) -> Response {
    let (parts, body) = req.into_parts();

    // Only intercept POST to chat/completions paths.
    let is_chat_path =
        parts.uri.path() == "/v1/chat/completions" || parts.uri.path() == "/v1/messages";
    if !is_chat_path || parts.method != axum::http::Method::POST {
        let req = Request::from_parts(parts, body);
        let mut resp = next.run(req).await;
        resp.headers_mut()
            .insert("x-tt-retrieval-enabled", HeaderValue::from_static("ready"));
        return resp;
    }

    // Buffer body (limited to MAX_BYTES).
    let bytes = match axum::body::to_bytes(body, MAX_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "retrieval: body too large or unreadable — forwarding unchanged");
            let req = Request::from_parts(parts, Body::empty());
            let mut resp = next.run(req).await;
            resp.headers_mut().insert(
                "x-tt-retrieval-error",
                HeaderValue::from_static("body-too-large"),
            );
            return resp;
        }
    };

    // Parse JSON body.
    let mut body_json: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "retrieval: JSON parse failed — forwarding unchanged");
            let req = Request::from_parts(parts, Body::from(bytes));
            let mut resp = next.run(req).await;
            resp.headers_mut().insert(
                "x-tt-retrieval-error",
                HeaderValue::from_static("json-parse-failed"),
            );
            return resp;
        }
    };

    // Quick scan: skip substitution if no `<retrievable` substring anywhere.
    let body_str = std::str::from_utf8(&bytes).unwrap_or("");
    if !body_str.contains("<retrievable") {
        let req = Request::from_parts(parts, Body::from(bytes));
        let mut resp = next.run(req).await;
        resp.headers_mut()
            .insert("x-tt-retrieval-enabled", HeaderValue::from_static("ready"));
        return resp;
    }

    // Extract the messages array.
    let messages = match body_json.get_mut("messages") {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => {
            // No messages array — forward unchanged.
            let req = Request::from_parts(parts, Body::from(bytes));
            let mut resp = next.run(req).await;
            resp.headers_mut()
                .insert("x-tt-retrieval-enabled", HeaderValue::from_static("ready"));
            return resp;
        }
    };

    // Read org_id from the ApiKeyContext extension set by the auth middleware.
    // Authenticated requests use the caller's real org_id, guaranteeing
    // per-tenant isolation in the retrieval store: org A's stored documents
    // are never surfaced to org B and vice versa.
    //
    // When no ApiKeyContext is present (unauthenticated or dev-mode requests
    // without a key store wired), fall back to Uuid::nil() — the shared
    // unauthenticated namespace — and emit a debug trace. This preserves
    // the legacy behaviour for local development and integration tests that
    // don't wire the auth middleware. Authenticated production traffic never
    // reaches this branch.
    let org_id = parts
        .extensions
        .get::<ApiKeyContext>()
        .map(|ctx| ctx.org_id)
        .unwrap_or_else(|| {
            tracing::debug!(
                "retrieval: no ApiKeyContext present — \
                 using nil org (unauthenticated / dev path)"
            );
            uuid::Uuid::nil()
        });

    // Run substitution.
    match substitute_in_messages(messages, org_id, state.store.as_ref(), &state.embedder).await {
        Ok(report) => {
            // Re-serialize modified body.
            let new_bytes = match serde_json::to_vec(&body_json) {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "retrieval: re-serialize failed — forwarding original");
                    let req = Request::from_parts(parts, Body::from(bytes));
                    let mut resp = next.run(req).await;
                    resp.headers_mut().insert(
                        "x-tt-retrieval-error",
                        HeaderValue::from_static("serialize-failed"),
                    );
                    return resp;
                }
            };

            let req = Request::from_parts(parts, Body::from(new_bytes));
            let mut resp = next.run(req).await;

            resp.headers_mut()
                .insert("x-tt-retrieval-enabled", HeaderValue::from_static("active"));
            if let Ok(v) = HeaderValue::from_str(&report.substitutions.to_string()) {
                resp.headers_mut().insert("x-tt-retrieval-substitutions", v);
            }
            if let Ok(v) = HeaderValue::from_str(&report.tokens_saved_estimate.to_string()) {
                resp.headers_mut().insert("x-tt-retrieval-tokens-saved", v);
            }

            // Track E §10: fire-and-forget an encrypted-prompt audit row of the
            // ORIGINAL request body. Best-effort — spawned so it never blocks
            // the response, and a failure only logs (never fails the request).
            if let Some(audit) = state.audit.clone() {
                let prompt = body_str.to_string();
                let substitutions = report.substitutions;
                let tokens_saved = report.tokens_saved_estimate;
                tokio::spawn(async move {
                    if let Err(e) = audit
                        .record(org_id, substitutions, tokens_saved, &prompt)
                        .await
                    {
                        warn!(error = %e, "retrieval audit record failed");
                    }
                });
            }
            resp
        }
        Err(e) => {
            warn!(error = %e, "retrieval: substitution failed — forwarding original body");
            let req = Request::from_parts(parts, Body::from(bytes));
            let mut resp = next.run(req).await;
            let kind = match &e {
                tt_retrieval::RetrievalError::Embedding(_) => "embedding-error",
                tt_retrieval::RetrievalError::Store(_) => "store-error",
                tt_retrieval::RetrievalError::Tag(_) => "tag-parse-error",
                tt_retrieval::RetrievalError::Malformed(_) => "malformed",
            };
            if let Ok(v) = HeaderValue::from_str(kind) {
                resp.headers_mut().insert("x-tt-retrieval-error", v);
            }
            resp
        }
    }
}
