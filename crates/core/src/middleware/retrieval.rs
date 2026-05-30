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
///
/// This path NEVER buffers the request body — it forwards `req` straight to
/// `next` with no `to_bytes` call, so the gateway pays zero body-buffering cost
/// when `TT_RETRIEVAL_STORE` is unset (rv-retrieval-body-buffer, §5.7). Only the
/// enabled path ([`maybe_substitute`]) buffers, and only for POSTs to the chat
/// endpoints.
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
    // Fail-closed: when no ApiKeyContext is present (auth middleware absent or
    // not yet run), skip substitution entirely and forward the request body
    // unchanged. This prevents cross-tenant data leaks if retrieval is ever
    // accidentally layered ahead of auth or used in a passthrough deployment.
    //
    // Set TT_RETRIEVAL_ALLOW_ANON=1 to opt in to the old nil-namespace
    // fallback for local development / integration tests that don't wire the
    // auth middleware. Never set this in production.
    let org_id = match parts.extensions.get::<ApiKeyContext>() {
        Some(ctx) => ctx.org_id,
        None => {
            let allow_anon = std::env::var("TT_RETRIEVAL_ALLOW_ANON")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);

            if allow_anon {
                tracing::debug!(
                    "retrieval: no ApiKeyContext present — \
                     TT_RETRIEVAL_ALLOW_ANON=1, using nil org (dev path)"
                );
                uuid::Uuid::nil()
            } else {
                warn!(
                    "retrieval: no ApiKeyContext present and TT_RETRIEVAL_ALLOW_ANON not set \
                     — skipping substitution (fail-closed)"
                );
                let req = Request::from_parts(parts, Body::from(bytes));
                let mut resp = next.run(req).await;
                resp.headers_mut().insert(
                    "x-tt-retrieval-skipped",
                    HeaderValue::from_static("no-auth"),
                );
                return resp;
            }
        }
    };

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
            if report.low_confidence_skips > 0 {
                if let Ok(v) = HeaderValue::from_str(&report.low_confidence_skips.to_string()) {
                    resp.headers_mut().insert("x-tt-retrieval-low-confidence", v);
                }
            }

            // Track E §10: fire-and-forget an encrypted-prompt audit row of the
            // ORIGINAL request body. Best-effort — spawned so it never blocks
            // the response, and a failure only logs (never fails the request).
            if let Some(audit) = state.audit.clone() {
                // Hold the plaintext prompt clone in a Zeroizing wrapper so the
                // heap buffer is wiped when the spawned task ends — rather than
                // freed (and potentially reused) with the secret still resident
                // (rv-retrieval-body-buffer, §5.7). `record` encrypts via
                // audit.rs; this clone is the only extra plaintext copy.
                let prompt = zeroize::Zeroizing::new(body_str.to_string());
                let substitutions = report.substitutions;
                let tokens_saved = report.tokens_saved_estimate;
                tokio::spawn(async move {
                    if let Err(e) = audit
                        .record(org_id, substitutions, tokens_saved, prompt.as_str())
                        .await
                    {
                        warn!(error = %e, "retrieval audit record failed");
                    }
                    // `prompt` (Zeroizing<String>) drops here, wiping the clone.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware;
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;
    use uuid::Uuid;

    use tt_auth::ApiKeyContext;
    use tt_retrieval::embed::EmbeddingClient;
    use tt_retrieval::store::memory::MemoryStore;

    use super::{maybe_substitute, RetrievalState};

    /// Process-wide lock that serializes tests which read/write
    /// `TT_RETRIEVAL_ALLOW_ANON` so they cannot race each other in the
    /// multi-threaded test runner.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A minimal chat-completions body that contains a `<retrievable` tag so
    /// the middleware won't short-circuit on the quick-scan check.
    fn retrievable_body() -> &'static str {
        r#"{"messages":[{"role":"user","content":"<retrievable corpus=\"test\">hello</retrievable>"}]}"#
    }

    fn build_state() -> RetrievalState {
        RetrievalState {
            store: Arc::new(MemoryStore::new()),
            embedder: Arc::new(EmbeddingClient::openai("test-key")),
            audit: None,
        }
    }

    /// Build a router that runs `maybe_substitute` then a trivial handler.
    fn build_router(state: RetrievalState) -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: axum::body::Bytes| async move {
                    // Echo the body back so tests can inspect what was forwarded.
                    axum::response::Response::builder()
                        .status(200)
                        .body(Body::from(body))
                        .unwrap()
                }),
            )
            .layer(middleware::from_fn_with_state(state.clone(), maybe_substitute))
            .with_state(state)
    }

    fn chat_request(body: &'static str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .expect("request")
    }

    // (a) No ApiKeyContext + dev flag OFF → body forwarded unchanged,
    //     x-tt-retrieval-skipped: no-auth header set, no search runs.
    #[tokio::test]
    async fn fail_closed_no_auth_no_flag() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        // Ensure the env var is NOT set for this test.
        std::env::remove_var("TT_RETRIEVAL_ALLOW_ANON");

        let router = build_router(build_state());
        let resp = router
            .oneshot(chat_request(retrievable_body()))
            .await
            .expect("response");

        assert_eq!(resp.status(), StatusCode::OK);

        // Must emit the skipped header.
        assert_eq!(
            resp.headers().get("x-tt-retrieval-skipped").map(|v| v.as_bytes()),
            Some(b"no-auth".as_slice()),
            "expected x-tt-retrieval-skipped: no-auth when no ApiKeyContext and flag off"
        );

        // Must NOT emit the active/ready retrieval header (no substitution ran).
        assert!(
            resp.headers().get("x-tt-retrieval-enabled").is_none(),
            "x-tt-retrieval-enabled should be absent when substitution is skipped"
        );

        // The body forwarded to the handler must be the original bytes unchanged.
        let forwarded = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body");
        assert_eq!(
            forwarded.as_ref(),
            retrievable_body().as_bytes(),
            "body must be forwarded unchanged when substitution is skipped"
        );
    }

    // (b) No ApiKeyContext + TT_RETRIEVAL_ALLOW_ANON=1 → nil org used,
    //     substitution ATTEMPTED (embedding will fail with test key, but the
    //     middleware does NOT return the skipped header — it reaches the
    //     substitution path).
    #[tokio::test]
    async fn allow_anon_flag_attempts_substitution() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        std::env::set_var("TT_RETRIEVAL_ALLOW_ANON", "1");

        let router = build_router(build_state());
        let resp = router
            .oneshot(chat_request(retrievable_body()))
            .await
            .expect("response");

        // Clean up before any assertions so the lock is still held.
        std::env::remove_var("TT_RETRIEVAL_ALLOW_ANON");

        // The skipped header must NOT be present (we took the anon path).
        assert!(
            resp.headers().get("x-tt-retrieval-skipped").is_none(),
            "x-tt-retrieval-skipped must be absent when TT_RETRIEVAL_ALLOW_ANON=1"
        );

        // Embedding will fail (fake key), so we expect an error header — but
        // crucially we did NOT short-circuit before substitution.
        let has_error = resp.headers().get("x-tt-retrieval-error").is_some();
        let has_active = resp
            .headers()
            .get("x-tt-retrieval-enabled")
            .map(|v| v == "active")
            .unwrap_or(false);
        assert!(
            has_error || has_active,
            "substitution path must have been attempted (error or active header expected)"
        );
    }

    // (c) Valid ApiKeyContext present → org_id used, substitution attempted
    //     against real org (embedding fails with test key, but skipped header
    //     is absent — the authed path was taken).
    #[tokio::test]
    async fn authenticated_path_uses_real_org() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        std::env::remove_var("TT_RETRIEVAL_ALLOW_ANON");

        let org_id = Uuid::new_v4();
        let ctx = ApiKeyContext {
            key_id: Uuid::new_v4(),
            org_id,
            tier: None,
        };

        let req = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .extension(ctx)
            .body(Body::from(retrievable_body()))
            .expect("request");

        let router = build_router(build_state());
        let resp = router.oneshot(req).await.expect("response");

        // Skipped header must NOT be present.
        assert!(
            resp.headers().get("x-tt-retrieval-skipped").is_none(),
            "x-tt-retrieval-skipped must be absent when ApiKeyContext is present"
        );

        // Substitution was attempted (embedding fails w/ test key → error
        // header; OR it somehow succeeds → active header).
        let has_error = resp.headers().get("x-tt-retrieval-error").is_some();
        let has_active = resp
            .headers()
            .get("x-tt-retrieval-enabled")
            .map(|v| v == "active")
            .unwrap_or(false);
        assert!(
            has_error || has_active,
            "authenticated path must attempt substitution"
        );
    }

    #[tokio::test]
    async fn disabled_path_forwards_body_unbuffered() {
        // The disabled middleware must stream the body straight through with no
        // buffering (rv-retrieval-body-buffer). A body larger than the enabled
        // path's MAX_BYTES cap proves it: if the disabled path buffered+capped,
        // this would be truncated/rejected; it must pass through untouched and
        // still carry the `disabled` header.
        let big = vec![b'x'; (1 << 20) + 4096]; // > 1 MiB
        let sent_len = big.len();

        let router = Router::new()
            .route(
                "/v1/chat/completions",
                post(|body: axum::body::Bytes| async move {
                    // Echo the received body so the test can compare lengths.
                    axum::response::Response::builder()
                        .status(StatusCode::OK)
                        .body(Body::from(body))
                        .unwrap()
                }),
            )
            .layer(middleware::from_fn(super::maybe_substitute_disabled));

        let req = HttpRequest::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .body(Body::from(big))
            .unwrap();

        let resp = router.oneshot(req).await.expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("x-tt-retrieval-enabled").map(|v| v.as_bytes()),
            Some(&b"disabled"[..]),
            "disabled middleware must mark the response"
        );
        let echoed = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("collect body");
        assert_eq!(
            echoed.len(),
            sent_len,
            "disabled path must forward the full body untouched (no buffering/cap)"
        );
    }
}
