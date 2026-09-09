//! Retrieval activation and request-local deferral.
//!
//! Middleware never embeds, searches or persists prompt bodies. It carries a
//! trusted tenant-bound intent into chat preparation, which resolves routing
//! BEFORE any retrieval I/O and guards embedding queries with that policy.
//! Native Messages ingress forwards the same intent to the canonical handler.
//!
//! Activation: TT_RETRIEVAL_STORE=memory|postgres + TT_OPENAI_EMBED_KEY;
//! postgres also requires DATABASE_URL. Disabled retrieval does not buffer.

use axum::{
    body::Body,
    extract::{Request, State},
    http::HeaderValue,
    middleware::Next,
    response::Response,
};
use std::sync::Arc;
use tracing::{debug, warn};
use tt_auth::ApiKeyContext;
use tt_retrieval::{
    audit::RetrievalAuditLog,
    embed::EmbeddingClient,
    store::{memory::MemoryStore, postgres::PostgresStore, RetrievalStore},
};

mod deferred;
pub use deferred::DeferredRetrieval;

const MAX_BYTES: usize = 1 << 20;

#[derive(Clone)]
pub struct RetrievalState {
    pub store: Arc<dyn RetrievalStore + Send + Sync>,
    pub embedder: Arc<EmbeddingClient>,
    /// Explicit encrypted-prompt audit, skipped for redact routes. Otherwise
    /// retains the canonical pre-substitution body; best-effort, not a receipt.
    pub audit: Option<Arc<RetrievalAuditLog>>,
}

/// Completed retrieval accounting only. Intent is deliberately a different
/// type so an unexecuted request cannot advertise performed substitutions.
#[derive(Clone, Copy, Debug, Default)]
pub struct RetrievalTelemetry {
    pub substitutions: u32,
    pub tokens_saved: i64,
}

pub fn build_retrieval_state() -> Option<RetrievalState> {
    let store_kind = match std::env::var("TT_RETRIEVAL_STORE") {
        Ok(v) => v,
        Err(_) => {
            debug!("TT_RETRIEVAL_STORE not set — retrieval disabled");
            return None;
        }
    };
    let embed_key = match std::env::var("TT_OPENAI_EMBED_KEY") {
        Ok(v) => v,
        Err(_) => {
            warn!(
                "TT_RETRIEVAL_STORE is set but TT_OPENAI_EMBED_KEY is missing — retrieval disabled"
            );
            return None;
        }
    };
    let mut audit: Option<Arc<RetrievalAuditLog>> = None;
    let store: Arc<dyn RetrievalStore + Send + Sync> = match store_kind.as_str() {
        "memory" => Arc::new(MemoryStore::new()),
        "postgres" => {
            let url = match std::env::var("DATABASE_URL") {
                Ok(u) => u,
                Err(_) => {
                    warn!("TT_RETRIEVAL_STORE=postgres but DATABASE_URL is missing — retrieval disabled");
                    return None;
                }
            };
            match sqlx::PgPool::connect_lazy(&url) {
                Ok(pool) => {
                    audit = RetrievalAuditLog::from_env(pool.clone()).map(Arc::new);
                    Arc::new(PostgresStore::new(pool))
                }
                Err(e) => {
                    warn!(error = %e, "failed to build Postgres retrieval store — retrieval disabled");
                    return None;
                }
            }
        }
        other => {
            warn!(store_kind = %other, "TT_RETRIEVAL_STORE has unknown value — retrieval disabled");
            return None;
        }
    };
    Some(RetrievalState {
        store,
        embedder: Arc::new(EmbeddingClient::openai(embed_key)),
        audit,
    })
}

/// Never buffers the body or performs I/O.
pub async fn maybe_substitute_disabled(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        "x-tokentrimmer-retrieval-enabled",
        HeaderValue::from_static("disabled"),
    );
    resp
}

/// Detect a retrieval intent without executing it. Only the policy-aware
/// preparation stage may consume this intent. Results flow back through a
/// bounded, prompt-free outcome so headers work for cache hits and streams too.
pub async fn maybe_substitute(
    State(state): State<RetrievalState>,
    req: Request,
    next: Next,
) -> Response {
    let (parts, body) = req.into_parts();
    let is_chat = parts.uri.path() == "/v1/chat/completions" || parts.uri.path() == "/v1/messages";
    if !is_chat || parts.method != axum::http::Method::POST {
        return ready(next.run(Request::from_parts(parts, body)).await);
    }
    let bytes = match axum::body::to_bytes(body, MAX_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            // Preserve the existing native-handler error path on unreadable or
            // oversized bodies. No retrieval I/O can occur on this branch.
            let mut resp = next.run(Request::from_parts(parts, Body::empty())).await;
            resp.headers_mut().insert(
                "x-tokentrimmer-retrieval-error",
                HeaderValue::from_static("body-too-large"),
            );
            return resp;
        }
    };
    if !std::str::from_utf8(&bytes)
        .unwrap_or("")
        .contains("<retrievable")
    {
        return ready(
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await,
        );
    }
    let org_id = match parts.extensions.get::<ApiKeyContext>() {
        Some(ctx) => ctx.org_id,
        None if std::env::var("TT_RETRIEVAL_ALLOW_ANON")
            .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) =>
        {
            uuid::Uuid::nil()
        }
        None => {
            let mut resp = next
                .run(Request::from_parts(parts, Body::from(bytes)))
                .await;
            resp.headers_mut().insert(
                "x-tokentrimmer-retrieval-skipped",
                HeaderValue::from_static("no-auth"),
            );
            return resp;
        }
    };
    let deferred = DeferredRetrieval::new(state, org_id);
    let mut req = Request::from_parts(parts, Body::from(bytes));
    req.extensions_mut().insert(deferred.clone());
    let mut response = next.run(req).await;
    deferred.attach_headers(&mut response).await;
    response
}

fn ready(mut response: Response) -> Response {
    response.headers_mut().insert(
        "x-tokentrimmer-retrieval-enabled",
        HeaderValue::from_static("ready"),
    );
    response
}

#[cfg(test)]
mod tests;
