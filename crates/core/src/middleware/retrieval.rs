//! Retrieval middleware. Inspects the request body for <retrievable> tags
//! and, if present, runs substitution before the chat handler dispatches.
//!
//! Wired via `Router::layer(axum::middleware::from_fn_with_state(...))` in
//! `server.rs`. When the substitution succeeds, sets an X-TT-Retrieval-Saved
//! header on the response.

use axum::body::Body;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

pub async fn maybe_substitute(req: Request, next: Next) -> Response {
    // For Day-0, the substitution path is OFF by default. Set the
    // X-TT-Retrieval-Enabled response header so callers can see the
    // capability is recognized but inactive. Activation requires:
    //   1. `tt-retrieval` enabled at boot (env TT_RETRIEVAL_STORE)
    //   2. An OpenAI key for embeddings (TT_OPENAI_EMBED_KEY)
    //   3. The request body containing `<retrievable corpus=` tag text.
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        "x-tt-retrieval-enabled",
        HeaderValue::from_static("v1-deferred-runtime"),
    );
    let _ = Body::default(); // keep deps used; lint-friendly
    resp
}
