//! API key authentication middleware.
//!
//! Extracts the `Authorization: Bearer ...` header and, for a TokenTrimmer
//! live key (`tt_live_*`), verifies it against the configured key store and
//! attaches the resulting [`ApiKeyContext`] as a request extension. The chat
//! handler then reads that extension to look up the per-org upstream
//! credentials it needs to authenticate to the provider.
//!
//! ## What this middleware deliberately does NOT do
//!
//! * It does **not** require a Bearer token. Requests without one pass
//!   through so `/health` and `/v1/models` keep working without auth, and so
//!   the chat handler retains the option to construct a synthetic context
//!   for tests that don't wire auth.
//! * It does **not** verify `tt_test_*` sandbox keys against the key store —
//!   the chat handler short-circuits sandbox traffic to a deterministic
//!   synthetic response before touching any provider, so verification would
//!   be wasted work.
//! * It does **not** look up provider credentials. The chat handler does
//!   that, because credential lookup is per-provider and the provider isn't
//!   resolved until the model is parsed from the request body.
//!
//! Behaviour matrix:
//!
//! | Header value                | Outcome                                          |
//! | --------------------------- | ------------------------------------------------ |
//! | (none)                      | pass through                                     |
//! | `Bearer tt_test_…`          | pass through (sandbox handled downstream)        |
//! | `Bearer tt_live_…` + valid  | `ApiKeyContext` attached as extension; continue  |
//! | `Bearer tt_live_…` + invalid| **401 Unauthorized**                             |
//! | `Bearer <other format>`     | pass through (forward-compat with future schemes)|

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};

use crate::{ApiError, AppState};

/// Axum `from_fn_with_state`-compatible middleware function.
pub async fn middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(token) = extract_bearer(&req) {
        // tt_test_* short-circuits to sandbox in the chat handler. No verify needed.
        if token.starts_with("tt_test_") {
            return Ok(next.run(req).await);
        }
        // tt_live_* needs verification against the key store.
        if token.starts_with("tt_live_") {
            // No key store wired — dev mode, defer the "no auth configured"
            // decision to the route handler instead of 401-ing the request.
            let Some(key_store) = state.key_store.as_ref() else {
                return Ok(next.run(req).await);
            };
            match tt_auth::verify(key_store.as_ref(), &token).await {
                Ok(ctx) => {
                    // Fire-and-forget last_used_at update. We never block the
                    // request on this — the dashboard's "Last used" column
                    // is informational, and Postgres write latency on a
                    // cold-start path would burn the gateway's p50 budget.
                    let key_store = key_store.clone();
                    let key_id = ctx.key_id;
                    tokio::spawn(async move {
                        if let Err(e) = key_store.touch_last_used(key_id, chrono::Utc::now()).await
                        {
                            tracing::warn!(error = %e, "touch_last_used failed");
                        }
                    });
                    req.extensions_mut().insert(ctx);
                }
                Err(_) => return Err(ApiError::Unauthorized),
            }
        }
        // Any other token format passes through unchallenged — forward-compat.
    }
    Ok(next.run(req).await)
}

/// Pull the bearer string out of `Authorization: Bearer <token>` if present.
/// Tolerant of casing variations on the scheme name.
fn extract_bearer(req: &Request) -> Option<String> {
    let value = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let scheme_len = "Bearer ".len();
    if value.len() <= scheme_len {
        return None;
    }
    if !value[..scheme_len].eq_ignore_ascii_case("Bearer ") {
        return None;
    }
    Some(value[scheme_len..].to_string())
}
