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
//!   be wasted work. The sandbox path is still subject to the per-IP rate cap
//!   ([`crate::middleware::argon2_cap`]) so an unauthenticated flood of
//!   `tt_test_*` requests cannot pin slots unthrottled (SEC-9).
//! * It does **not** look up provider credentials. The chat handler does
//!   that, because credential lookup is per-provider and the provider isn't
//!   resolved until the model is parsed from the request body.
//!
//! Behaviour matrix:
//!
//! | Header value                | Outcome                                          |
//! | --------------------------- | ------------------------------------------------ |
//! | (none)                      | pass through                                     |
//! | `Bearer tt_test_…`          | per-IP cap → pass through (sandbox downstream) or 429 |
//! | `Bearer tt_live_…` + valid  | `ApiKeyContext` attached as extension; continue  |
//! | `Bearer tt_live_…` + invalid| **401 Unauthorized**                             |
//! | `Bearer <other format>`     | pass through (forward-compat with future schemes)|

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};

use tt_auth::ApiKeyContext;
use uuid::Uuid;

use crate::budget::BudgetDecision;
use crate::middleware::key_cache::CacheLookup;
use crate::tier_resolver::resolve_or_free;
use crate::{ApiError, AppState, DOGFOOD_ORG_ID};

/// Fires the SEC-6 non-loopback dogfood warning at most once per process.
static DOGFOOD_NON_LOOPBACK_WARN: std::sync::Once = std::sync::Once::new();

/// Loud, fire-once warning (SEC-6): dogfood routing is enabled but the gateway
/// is not bound to loopback, so the anonymous dogfood-org stamp is disabled
/// (fail closed). Escalates the wording when a credential store is wired —
/// that is the case where stamping anonymous traffic as the dogfood org could
/// otherwise leak the dogfood org's upstream provider credentials.
fn warn_dogfood_non_loopback_once(state: &AppState) {
    DOGFOOD_NON_LOOPBACK_WARN.call_once(|| {
        let has_credential_store = state.credential_store.is_some();
        let detail = if has_credential_store {
            "A credential store IS wired: without this guard, anonymous callers would be \
             treated as the dogfood org and could spend its upstream provider credentials."
        } else {
            "No credential store is wired, but anonymous callers would still inherit the \
             dogfood org's routes and budget."
        };
        tracing::warn!(
            credential_store = has_credential_store,
            "SEC-6: dogfood routing (TT_DOGFOOD_GROQ_ROUTING) is enabled on a NON-loopback bind \
             — refusing to stamp anonymous requests with the dogfood org (fail closed). {detail} \
             Bind loopback (TT_BIND_ADDR=127.0.0.1) or unset TT_DOGFOOD_GROQ_ROUTING."
        );
    });
}

/// Axum `from_fn_with_state`-compatible middleware function.
pub async fn middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // Identity resolved below (when the request carries one) so the budget
    // gate can run against the org before dispatch. `api_key_id` is tracked
    // alongside `org_id` so the pre-flight check can enforce the per-key cap
    // (P2) against the per-`(org, key)` accumulator.
    let mut org_id: Option<Uuid> = None;
    let mut api_key_id: Option<Uuid> = None;

    if let Some(token) = extract_bearer(req.headers()) {
        // tt_test_* short-circuits to a synthetic sandbox response in the chat
        // handler — no key-store verify and no provider/credential dispatch. It
        // must NOT, however, bypass the per-IP rate cap: an unauthenticated
        // flood of distinct tt_test_* tokens would otherwise pin gateway slots
        // unthrottled (SEC-9). Charge the same per-IP cap used to guard the
        // argon2 cold path BEFORE letting the request through; over the cap we
        // shed with 429 + Retry-After. The synthetic-response behaviour is
        // unchanged for traffic under the cap.
        if token.starts_with("tt_test_") {
            let client_ip = crate::middleware::argon2_cap::client_ip(req.headers());
            if let crate::middleware::argon2_cap::CapDecision::Reject { retry_after_secs } =
                state.argon2_cap.check(client_ip)
            {
                tracing::debug!(
                    ip = %client_ip,
                    "auth: per-IP cap exceeded on sandbox (tt_test_) path, shedding"
                );
                return Ok(verify_cap_response(retry_after_secs));
            }
            return Ok(next.run(req).await);
        }
        // tt_live_* needs verification against the key store.
        if token.starts_with("tt_live_") {
            // No key store wired — dev mode, defer the "no auth configured"
            // decision to the route handler instead of 401-ing the request.
            // SAFE because the gateway fails closed without a key store: boot
            // refuses non-loopback binds (unless explicitly opted in) and
            // wires no credential store, so this unverified pass-through can
            // only forward the caller's own Bearer key upstream — never the
            // operator's env provider keys (P0 #21, `tt gateway` boot).
            let Some(key_store) = state.key_store.as_ref() else {
                return Ok(next.run(req).await);
            };
            // Consult the in-process verify cache first.  The cache is keyed
            // by the blake3 hash of the bearer token so the plaintext is
            // never stored.  On a cache hit we skip the argon2 call entirely.
            let token_hash = crate::middleware::key_cache::hash_token(&token);
            let cache = &state.verify_cache;

            let mut lookup = cache.get(&token_hash);

            // Cross-instance revocation (SEC-4): a positive verify-cache hit is
            // honored only if the key is not marked revoked in the shared
            // registry (the same Redis L1 uses). A revoke on ANY replica SETs
            // the marker; here a marked hit is downgraded to a Miss so the cold
            // path re-runs argon2 against the key store (which rejects the
            // revoked row → 401). This closes the per-replica staleness window
            // that the local-only `evict_key_id` leaves open on every replica
            // that didn't process the revoke. Only positive hits are checked
            // (one Redis GET on the fast path); Failure/Miss skip it. Fails
            // open: no registry wired or a backend error honors the cached
            // positive (degrading to the documented `POSITIVE_TTL_SECS` window).
            if let CacheLookup::Hit(hit_ctx) = &lookup {
                let key_id = hit_ctx.key_id;
                if let Some(reg) = state.revocation.as_ref() {
                    if reg.is_revoked(key_id).await {
                        tracing::debug!(
                            %key_id,
                            "auth: positive cache hit on a revoked key — forcing re-verify"
                        );
                        cache.evict_key_id(key_id);
                        lookup = CacheLookup::Miss;
                    }
                }
            }

            let ctx_opt = match lookup {
                CacheLookup::Hit(cached_ctx) => {
                    tracing::trace!("auth: cache hit, skipping argon2");
                    Some(cached_ctx)
                }
                CacheLookup::Failure => {
                    // Recently-failed token — deny without argon2.
                    tracing::trace!("auth: negative cache hit, denying");
                    return Err(ApiError::Unauthorized);
                }
                CacheLookup::Miss => {
                    // Cold path: about to run the CPU-expensive argon2 verify.
                    //
                    // PRE-AUTH RATE CAP — consult the per-IP cap BEFORE the
                    // hash. A flood of distinct bogus suffixes for a known live
                    // prefix would otherwise miss the cache every time and force
                    // argon2 on each request (a CPU-DoS amplifier). Past the
                    // per-IP threshold we shed with 429 + Retry-After and never
                    // call `tt_auth::verify`, so the expensive work is skipped.
                    // Cache hits (handled above) and non-`tt_live_*` traffic do
                    // not reach here, so authenticated traffic is never capped.
                    let client_ip = crate::middleware::argon2_cap::client_ip(req.headers());
                    if let crate::middleware::argon2_cap::CapDecision::Reject { retry_after_secs } =
                        state.argon2_cap.check(client_ip)
                    {
                        tracing::debug!(
                            ip = %client_ip,
                            "auth: per-IP argon2 verify cap exceeded, shedding pre-hash"
                        );
                        return Ok(verify_cap_response(retry_after_secs));
                    }

                    // Under the cap: run the real argon2 verify.
                    match tt_auth::verify(key_store.as_ref(), &token).await {
                        Ok(ctx) => {
                            cache.insert_hit(token_hash, ctx.clone());
                            Some(ctx)
                        }
                        Err(_) => {
                            cache.insert_failure(token_hash);
                            return Err(ApiError::Unauthorized);
                        }
                    }
                }
            };

            if let Some(mut ctx) = ctx_opt {
                // Fire-and-forget last_used_at update. We never block the
                // request on this — the dashboard's "Last used" column
                // is informational, and Postgres write latency on a
                // cold-start path would burn the gateway's p50 budget.
                let key_store = key_store.clone();
                let key_id = ctx.key_id;
                tokio::spawn(async move {
                    if let Err(e) = key_store.touch_last_used(key_id, chrono::Utc::now()).await {
                        tracing::warn!(error = %e, "touch_last_used failed");
                    }
                });

                // Resolve subscription tier and stamp ApiKeyContext.tier.
                // Fail-open: a DB error returns Free defaults and logs a warn.
                if let Some(resolver) = state.tier_resolver.as_ref() {
                    let resolved = resolve_or_free(resolver.as_ref(), ctx.org_id).await;
                    ctx.tier = Some(resolved.caller_tier);
                }

                org_id = Some(ctx.org_id);
                api_key_id = Some(ctx.key_id);
                req.extensions_mut().insert(ctx);
            }
        }
        // Any other token format passes through unchallenged — forward-compat.
    } else if state.dogfood_enabled {
        // No bearer token + dogfood mode: stamp the fixed dogfood org id so the
        // routing engine can match the pre-seeded dogfood route. This is
        // internal-only; production requests always carry a tt_live_* key.
        //
        // SEC-6 fail-closed guard: stamping a fixed org identity onto anonymous
        // traffic is only safe on a loopback bind. On a shared/public bind it
        // would treat every stranger as the dogfood org (and, with a credential
        // store wired, could spend the dogfood org's upstream credentials), so
        // we stamp ONLY when the bind is loopback. Off loopback the request
        // passes through unstamped (anonymous) and we log a loud one-time warning.
        if state.dogfood_active() {
            req.extensions_mut().insert(ApiKeyContext {
                key_id: Uuid::nil(),
                org_id: DOGFOOD_ORG_ID,
                tier: None,
            });
            org_id = Some(DOGFOOD_ORG_ID);
        } else {
            warn_dogfood_non_loopback_once(&state);
        }
    }

    // Pre-flight budget gate for identified orgs. Anonymous/dev requests
    // (no org_id) and the sandbox path are not metered.
    //
    // When `tier_resolver` is wired, we use per-org resolved limits via
    // `dynamic_budget`; otherwise we fall back to the global `budget`
    // enforcer (if any). This preserves today's dev/no-pool behaviour.
    let mut spend_remaining: Option<f64> = None;
    if let Some(org) = org_id {
        let decision = if let Some(resolver) = state.tier_resolver.as_ref() {
            // Tier-aware path: resolve org limits AND the per-key cap (P2), then
            // check against the per-org + per-key dynamic accumulators. The
            // per-key cap is the tighter-of-org-and-key ceiling, scoped to this
            // key (fail-open on resolver error; the per-key read is fail-soft to
            // an empty cap — a missing cap never adds enforcement).
            let resolved = resolve_or_free(resolver.as_ref(), org).await;
            // Read the per-key cap only when the request carries a real key id
            // (dogfood/anon stamps carry the nil key → org-only).
            let key_cap = match api_key_id {
                Some(key_id) => resolver.resolve_key_cap(org, key_id).await,
                None => crate::budget::KeyBudgetCap::default(),
            };
            state.dynamic_budget.check_with_limits_keyed(
                org,
                api_key_id,
                &resolved.limits,
                &key_cap,
                chrono::Utc::now(),
            )
        } else if let Some(budget) = state.budget.as_ref() {
            // Legacy path: global limits enforcer.
            budget.check(org, chrono::Utc::now())
        } else {
            // No enforcer wired → allow (dev mode).
            BudgetDecision::Allow {
                spend_remaining_usd: None,
            }
        };

        match decision {
            BudgetDecision::Allow {
                spend_remaining_usd,
            } => spend_remaining = spend_remaining_usd,
            BudgetDecision::DenySpend => {
                return Ok(budget_denied_response(
                    "Monthly spend cap reached for this org.",
                    "budget_exceeded",
                    Some(0.0),
                    None,
                ));
            }
            BudgetDecision::DenyMonthlyRequests => {
                return Ok(budget_denied_response(
                    "Monthly request quota reached for this org.",
                    "monthly_quota_exceeded",
                    None,
                    None,
                ));
            }
            BudgetDecision::DenyServed => {
                // P0-3 COGS guard: served-request ceiling (cache hits included)
                // reached. Distinct reason from the billed quota — this is an
                // abuse/infra-cost ceiling, not the advertised included-request
                // limit.
                return Ok(budget_denied_response(
                    "Monthly served-request limit reached for this org.",
                    "served_quota_exceeded",
                    None,
                    None,
                ));
            }
            BudgetDecision::DenyRate { retry_after_secs } => {
                return Ok(budget_denied_response(
                    "Request rate limit exceeded for this org.",
                    "rate_limit_exceeded",
                    None,
                    Some(retry_after_secs),
                ));
            }
            BudgetDecision::DenyKeySpend => {
                // P2: this individual API key reached its per-key monthly spend
                // cap (the org may still have headroom). Key-scoped reason.
                return Ok(budget_denied_response(
                    "Monthly spend cap reached for this API key.",
                    "key_budget_exceeded",
                    Some(0.0),
                    None,
                ));
            }
            BudgetDecision::DenyKeyRequests => {
                // P2: this individual API key reached its per-key monthly
                // request cap. Key-scoped reason.
                return Ok(budget_denied_response(
                    "Monthly request quota reached for this API key.",
                    "key_quota_exceeded",
                    None,
                    None,
                ));
            }
        }
    }

    let mut resp = next.run(req).await;
    // Surface remaining monthly headroom on successful responses too.
    if let Some(rem) = spend_remaining {
        if let Ok(v) = format!("{rem:.6}").parse() {
            resp.headers_mut()
                .insert("x-tokentrimmer-budget-remaining-usd", v);
        }
    }
    Ok(resp)
}

/// Build a `429 Too Many Requests` response for a budget/rate denial, with the
/// `X-TokenTrimmer-Budget-Remaining-Usd` and (for rate limits) `Retry-After` headers.
fn budget_denied_response(
    message: &str,
    kind: &str,
    remaining_usd: Option<f64>,
    retry_after_secs: Option<u64>,
) -> Response {
    let body = serde_json::json!({ "error": { "message": message, "type": kind } });
    let mut resp = (axum::http::StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    let headers = resp.headers_mut();
    if let Some(rem) = remaining_usd {
        if let Ok(v) = format!("{rem:.6}").parse() {
            headers.insert("x-tokentrimmer-budget-remaining-usd", v);
        }
    }
    if let Some(secs) = retry_after_secs {
        if let Ok(v) = secs.to_string().parse() {
            headers.insert(axum::http::header::RETRY_AFTER, v);
        }
    }
    resp
}

/// Build the `429 Too Many Requests` response returned when an IP exceeds the
/// pre-auth per-IP argon2-verify cap. Carries `Retry-After` and an opaque JSON
/// error body. Returned BEFORE any argon2 work so a flood is cheap to reject.
fn verify_cap_response(retry_after_secs: u64) -> Response {
    let body = serde_json::json!({
        "error": {
            "message": "Too many key-verification attempts from this source. Slow down.",
            "type": "rate_limit_exceeded"
        }
    });
    let mut resp = (axum::http::StatusCode::TOO_MANY_REQUESTS, Json(body)).into_response();
    if let Ok(v) = retry_after_secs.max(1).to_string().parse() {
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, v);
    }
    resp
}

/// Pull the credential out of the request headers, preferring
/// `Authorization: Bearer` and falling back to the `x-api-key` header as an
/// alias. Tolerant of casing variations on the scheme name.
///
/// The `x-api-key` alias exists so a naive Claude Code / Anthropic-SDK setup
/// works out of the box: the SDK sends `ANTHROPIC_API_KEY` as `x-api-key`
/// (rather than `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer`). Without
/// this alias, the documented "set `ANTHROPIC_API_KEY`" path never
/// authenticates. We prefer Bearer when both are present (it's the explicit
/// gateway-native scheme); `x-api-key` is purely a convenience alias.
pub(crate) fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    // Prefer Authorization: Bearer <token>.
    if let Some(value) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(value) = value.to_str() {
            let scheme_len = "Bearer ".len();
            if value.len() > scheme_len && value[..scheme_len].eq_ignore_ascii_case("Bearer ") {
                return Some(value[scheme_len..].to_string());
            }
        }
    }
    // Fall back to x-api-key: <token> (Anthropic-SDK alias).
    if let Some(value) = headers.get("x-api-key") {
        if let Ok(token) = value.to_str() {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    //! Integration tests for the auth middleware + verify cache.
    //!
    //! These tests prove that:
    //!
    //! (a) Two requests with the same valid token within TTL invoke argon2
    //!     verify only ONCE (tracked via `find_by_prefix` call count on the
    //!     underlying store, since `tt_auth::verify` calls it exactly once).
    //!
    //! (b) After TTL expiry (simulated by using a fresh empty cache) the
    //!     middleware re-verifies (argon2 runs again).
    //!
    //! (c) An invalid token gets a 401; a subsequent request with a VALID
    //!     token for the same store still works (negative cache doesn't
    //!     poison positive paths for different tokens).
    //!
    //! (d) Within the positive TTL, a revoked key is still accepted
    //!     (documented staleness). After eviction / TTL expiry the key is
    //!     re-verified and the revocation is surfaced as 401.

    use std::collections::HashMap;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    };

    use argon2::{
        password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
        Argon2,
    };
    use async_trait::async_trait;
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
        middleware,
        routing::get,
        Router,
    };
    use chrono::{DateTime, Utc};
    use tower::ServiceExt;
    use tt_auth::{ApiKey, Environment, KeyError, KeyStore};
    use uuid::Uuid;

    use super::extract_bearer;
    use crate::middleware::key_cache::{hash_token, KeyVerifyCache};
    use crate::state::AppState;

    // ------------------------------------------------------------------
    // extract_bearer — Authorization: Bearer preference + x-api-key alias.
    // The alias (P0-5) lets a naive Claude Code / Anthropic-SDK setup
    // authenticate: the SDK sends ANTHROPIC_API_KEY as `x-api-key`.
    // ------------------------------------------------------------------
    #[test]
    fn extract_bearer_prefers_authorization_bearer() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("authorization", "Bearer tt_live_abc".parse().unwrap());
        h.insert("x-api-key", "tt_live_xyz".parse().unwrap());
        assert_eq!(extract_bearer(&h).as_deref(), Some("tt_live_abc"));
    }

    #[test]
    fn extract_bearer_falls_back_to_x_api_key() {
        // No Authorization header → the x-api-key alias is used.
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-api-key", "tt_live_abc".parse().unwrap());
        assert_eq!(extract_bearer(&h).as_deref(), Some("tt_live_abc"));
    }

    #[test]
    fn extract_bearer_accepts_case_insensitive_scheme() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("authorization", "bearer tt_live_abc".parse().unwrap());
        assert_eq!(extract_bearer(&h).as_deref(), Some("tt_live_abc"));
    }

    #[test]
    fn extract_bearer_returns_none_when_absent() {
        let h = axum::http::HeaderMap::new();
        assert_eq!(extract_bearer(&h), None);
    }

    #[test]
    fn extract_bearer_ignores_empty_x_api_key() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("x-api-key", "   ".parse().unwrap());
        assert_eq!(extract_bearer(&h), None);
    }

    #[test]
    fn extract_bearer_ignores_non_bearer_authorization() {
        // A non-Bearer scheme (e.g. "Basic …") must not yield the rest, and if
        // no x-api-key is present, returns None (forward-compat with future
        // schemes — don't accidentally authenticate a Basic auth header).
        let mut h = axum::http::HeaderMap::new();
        h.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        assert_eq!(extract_bearer(&h), None);
    }

    // ------------------------------------------------------------------
    // CountingKeyStore — a KeyStore backed by an interior-mutable HashMap
    // plus a find_by_prefix counter.  `tt_auth::verify` calls find_by_prefix
    // exactly once per cold-path verify, so the counter == argon2 runs.
    // ------------------------------------------------------------------

    struct CountingKeyStore {
        by_prefix: Mutex<HashMap<String, ApiKey>>,
        pub find_count: AtomicU32,
    }

    impl CountingKeyStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                by_prefix: Mutex::new(HashMap::new()),
                find_count: AtomicU32::new(0),
            })
        }
    }

    #[async_trait]
    impl KeyStore for CountingKeyStore {
        async fn insert(&self, key: ApiKey) -> Result<(), KeyError> {
            self.by_prefix
                .lock()
                .map_err(|e| KeyError::Store(e.to_string()))?
                .insert(key.prefix.clone(), key);
            Ok(())
        }

        async fn find_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>, KeyError> {
            self.find_count.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .by_prefix
                .lock()
                .map_err(|e| KeyError::Store(e.to_string()))?
                .get(prefix)
                .cloned())
        }

        async fn revoke(
            &self,
            id: Uuid,
            org_id: Uuid,
            at: DateTime<Utc>,
        ) -> Result<bool, KeyError> {
            let mut g = self
                .by_prefix
                .lock()
                .map_err(|e| KeyError::Store(e.to_string()))?;
            for v in g.values_mut() {
                if v.id == id && v.org_id == org_id {
                    v.revoked_at = Some(at);
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Issue a real argon2-hashed key and insert it into the store.
    async fn seed_key(store: &Arc<CountingKeyStore>, org_id: Uuid, plaintext: &str) -> ApiKey {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(plaintext.as_bytes(), &salt)
            .expect("argon2 hash")
            .to_string();
        let prefix = plaintext[..12].to_string();
        let key = ApiKey {
            id: Uuid::now_v7(),
            org_id,
            prefix,
            hash,
            label: "test".to_string(),
            environment: Environment::Live,
            created_at: Utc::now(),
            revoked_at: None,
        };
        store.insert(key.clone()).await.expect("insert");
        key
    }

    /// Build a minimal `AppState` with a counting store and a FRESH verify cache.
    fn state_with_store(store: Arc<CountingKeyStore>) -> AppState {
        let mut app = AppState::new(crate::registry::ProviderRegistry::new());
        app.verify_cache = Arc::new(KeyVerifyCache::new());
        app.key_store = Some(store);
        app
    }

    /// Build a trivial router that runs the auth middleware and returns 200 on pass.
    fn build_router(state: AppState) -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                super::middleware,
            ))
            .with_state(state)
    }

    fn live_bearer(token: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri("/")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("request")
    }

    /// Like [`live_bearer`] but stamps an `x-forwarded-for` so the per-IP
    /// argon2-verify cap can be exercised with distinct source IPs in-process.
    fn live_bearer_from_ip(token: &str, ip: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri("/")
            .header("Authorization", format!("Bearer {token}"))
            .header("x-forwarded-for", ip)
            .body(Body::empty())
            .expect("request")
    }

    /// `AppState` with a counting store, fresh verify cache, and a per-IP
    /// argon2 cap set to `verify_per_min` (deterministic: burst == per_min, so
    /// the `(verify_per_min + 1)`-th cold-path verify from one IP is shed).
    fn state_with_cap(store: Arc<CountingKeyStore>, verify_per_min: u32) -> AppState {
        use crate::middleware::argon2_cap::{Argon2CapConfig, Argon2VerifyCap};
        let mut app = state_with_store(store);
        app.argon2_cap = Argon2VerifyCap::new(Argon2CapConfig { verify_per_min });
        app
    }

    // ------------------------------------------------------------------
    // (a) Two requests with the same valid token invoke argon2 only ONCE
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn same_valid_token_argon2_once_within_ttl() {
        let org = Uuid::new_v4();
        let plaintext = "tt_live_aabbccdd1234"; // 20 chars, prefix = "tt_live_aabb"

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext).await;

        let state = state_with_store(store.clone());
        let router = build_router(state);

        // First request — cold cache, should hit argon2.
        let resp1 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("resp1");
        assert_eq!(
            resp1.status(),
            StatusCode::OK,
            "first request should succeed"
        );

        // Second request — cache warm, should skip argon2.
        let resp2 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("resp2");
        assert_eq!(
            resp2.status(),
            StatusCode::OK,
            "second request should succeed"
        );

        // find_by_prefix (= argon2 runs) called exactly once.
        assert_eq!(
            store.find_count.load(Ordering::SeqCst),
            1,
            "argon2 / find_by_prefix should only run once within TTL"
        );
    }

    // ------------------------------------------------------------------
    // (b) After TTL expiry the middleware re-verifies.
    //
    // We simulate expiry by using two separate AppState instances that each
    // start with a fresh empty cache — equivalent to the state after expiry.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn re_verifies_after_ttl_expiry() {
        let org = Uuid::new_v4();
        let plaintext = "tt_live_ttlexpiry1234";

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext).await;

        // Router 1 — first verify (cache miss → argon2).
        let state1 = state_with_store(store.clone());
        let router1 = build_router(state1);
        let r1 = router1.oneshot(live_bearer(plaintext)).await.expect("r1");
        assert_eq!(r1.status(), StatusCode::OK);
        assert_eq!(store.find_count.load(Ordering::SeqCst), 1);

        // Router 2 — brand-new empty cache (simulates TTL expiry).
        let state2 = state_with_store(store.clone());
        let router2 = build_router(state2);
        let r2 = router2.oneshot(live_bearer(plaintext)).await.expect("r2");
        assert_eq!(r2.status(), StatusCode::OK);
        assert_eq!(
            store.find_count.load(Ordering::SeqCst),
            2,
            "after TTL expiry (simulated by fresh empty cache) argon2 should run again"
        );
    }

    // ------------------------------------------------------------------
    // (c) Invalid token → 401; valid token for same store still works
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn invalid_token_denied_valid_token_still_works() {
        let org = Uuid::new_v4();
        let plaintext_valid = "tt_live_valid1234567";
        let plaintext_bad = "tt_live_baddd1234567"; // different prefix, not in store

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext_valid).await;

        let state = state_with_store(store.clone());
        let router = build_router(state);

        // Bad token → 401.
        let r_bad = router
            .clone()
            .oneshot(live_bearer(plaintext_bad))
            .await
            .expect("bad resp");
        assert_eq!(r_bad.status(), StatusCode::UNAUTHORIZED);

        // Good token (different prefix) → 200.
        let r_good = router
            .clone()
            .oneshot(live_bearer(plaintext_valid))
            .await
            .expect("good resp");
        assert_eq!(r_good.status(), StatusCode::OK);
    }

    // ------------------------------------------------------------------
    // (d) Revoked key: within positive TTL still accepted (documented staleness);
    //     after cache eviction it is re-verified and rejected.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn revoked_key_accepted_within_positive_ttl_then_rejected_after_eviction() {
        let org = Uuid::new_v4();
        let plaintext = "tt_live_revokeable1234";

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext).await;

        let cache = Arc::new(KeyVerifyCache::new());

        let mut app = AppState::new(crate::registry::ProviderRegistry::new());
        app.verify_cache = cache.clone();
        app.key_store = Some(store.clone());
        let router = build_router(app);

        // First request — key is live, cache miss → argon2 runs → 200.
        let r1 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r1");
        assert_eq!(r1.status(), StatusCode::OK, "pre-revoke should succeed");

        // Now revoke the key in the store (org_id required for org-scoped revoke).
        let (key_id, key_org_id) = {
            let g = store.by_prefix.lock().unwrap();
            let k = g.get(&plaintext[..12]).unwrap();
            (k.id, k.org_id)
        };
        store
            .revoke(key_id, key_org_id, Utc::now())
            .await
            .expect("revoke");

        // Second request — cache still holds the positive entry → 200 (staleness window).
        let r2 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r2");
        assert_eq!(
            r2.status(),
            StatusCode::OK,
            "revoked key still accepted within positive TTL (documented staleness)"
        );
        // argon2 was NOT called again (still 1 find_by_prefix).
        assert_eq!(store.find_count.load(Ordering::SeqCst), 1);

        // Evict the cache entry (simulates TTL expiry or explicit invalidation).
        let token_hash = hash_token(plaintext);
        cache.evict(&token_hash);

        // Third request — cache miss → argon2 runs → key is revoked → 401.
        let r3 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r3");
        assert_eq!(
            r3.status(),
            StatusCode::UNAUTHORIZED,
            "after eviction, revoked key should be rejected"
        );
        // find_by_prefix was called a second time.
        assert_eq!(store.find_count.load(Ordering::SeqCst), 2);
    }

    // ------------------------------------------------------------------
    // (e) Revoking a key and evicting by key_id (the revoke-route path)
    //     causes the next request to re-verify and return 401.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn revoked_key_rejected_after_evict_by_key_id() {
        let org = Uuid::new_v4();
        let plaintext = "tt_live_revoke_by_id99";

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext).await;

        let cache = Arc::new(KeyVerifyCache::new());

        let mut app = AppState::new(crate::registry::ProviderRegistry::new());
        app.verify_cache = cache.clone();
        app.key_store = Some(store.clone());
        let router = build_router(app);

        // First request — cache miss → argon2 → 200; warms the positive entry.
        let r1 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r1");
        assert_eq!(r1.status(), StatusCode::OK, "pre-revoke should succeed");
        assert_eq!(store.find_count.load(Ordering::SeqCst), 1);

        // Revoke in the store + evict the cache BY KEY_ID (what the revoke route does).
        let (key_id, key_org_id) = {
            let g = store.by_prefix.lock().unwrap();
            let k = g.get(&plaintext[..12]).unwrap();
            (k.id, k.org_id)
        };
        store
            .revoke(key_id, key_org_id, Utc::now())
            .await
            .expect("revoke");
        cache.evict_key_id(key_id);

        // Next request — cache miss (evicted) → argon2 re-runs → revoked → 401.
        let r2 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r2");
        assert_eq!(
            r2.status(),
            StatusCode::UNAUTHORIZED,
            "after evict_key_id, revoked key must be rejected"
        );
        assert_eq!(store.find_count.load(Ordering::SeqCst), 2);
    }

    // ------------------------------------------------------------------
    // (e2) SEC-4: a revoke fanned out via the cross-instance RevocationRegistry
    //      forces a re-verify on a PEER replica that still has a warm positive
    //      entry (and never processed the local evict).
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn cross_instance_revocation_marker_forces_reverify() {
        use tt_cache::memory::InMemoryL1Cache;

        let org = Uuid::new_v4();
        let plaintext = "tt_live_xinstance999";

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext).await;

        // A revocation registry backed by a mock Redis (InMemoryL1Cache),
        // shared with the peer that performs the revoke below.
        let registry = Arc::new(crate::middleware::key_cache::RevocationRegistry::new(
            Arc::new(InMemoryL1Cache::new()),
        ));
        let mut app = state_with_store(store.clone());
        app.revocation = Some(registry.clone());
        let router = build_router(app);

        // r1 warms a positive verify-cache entry on THIS replica.
        let r1 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r1");
        assert_eq!(r1.status(), StatusCode::OK, "pre-revoke should succeed");
        assert_eq!(store.find_count.load(Ordering::SeqCst), 1);

        // A PEER replica revokes the key: store row revoked + cross-instance
        // marker SET. THIS replica did not process the revoke locally (no
        // evict_key_id), so its positive entry is still warm — the marker is
        // the only signal it has.
        let (key_id, key_org) = {
            let g = store.by_prefix.lock().unwrap();
            let k = g.get(&plaintext[..12]).unwrap();
            (k.id, k.org_id)
        };
        store
            .revoke(key_id, key_org, Utc::now())
            .await
            .expect("revoke");
        registry.mark_revoked(key_id).await;

        // r2 hits the warm positive entry, but the marker downgrades it to a
        // miss → argon2 re-runs against the now-revoked store row → 401.
        let r2 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r2");
        assert_eq!(
            r2.status(),
            StatusCode::UNAUTHORIZED,
            "cross-instance marker must force a re-verify and reject the revoked key"
        );
        assert_eq!(
            store.find_count.load(Ordering::SeqCst),
            2,
            "argon2 must re-run after the marker downgrades the hit to a miss"
        );
    }

    // ------------------------------------------------------------------
    // (e3) SEC-4 control: WITHOUT a registry, a revoke on a peer leaves this
    //      replica's warm positive entry stale (the documented per-replica
    //      window the registry is designed to close).
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn without_registry_stale_positive_is_honored() {
        let org = Uuid::new_v4();
        let plaintext = "tt_live_noregistry00";

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext).await;

        // state_with_store leaves `revocation` None — no cross-instance channel.
        let state = state_with_store(store.clone());
        let router = build_router(state);

        let r1 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r1");
        assert_eq!(r1.status(), StatusCode::OK);
        assert_eq!(store.find_count.load(Ordering::SeqCst), 1);

        // Peer revokes the store row, but there is no marker and no local evict.
        let (key_id, key_org) = {
            let g = store.by_prefix.lock().unwrap();
            let k = g.get(&plaintext[..12]).unwrap();
            (k.id, k.org_id)
        };
        store
            .revoke(key_id, key_org, Utc::now())
            .await
            .expect("revoke");

        // The warm positive entry is still served — bounded only by the TTL.
        let r2 = router
            .clone()
            .oneshot(live_bearer(plaintext))
            .await
            .expect("r2");
        assert_eq!(
            r2.status(),
            StatusCode::OK,
            "without a registry the stale positive is honored within the TTL window"
        );
        assert_eq!(
            store.find_count.load(Ordering::SeqCst),
            1,
            "no re-verify happens without the cross-instance marker"
        );
    }

    // ------------------------------------------------------------------
    // (f) PRE-AUTH per-IP argon2 cap.
    //
    // A flood of DISTINCT bogus suffixes (each a cache miss → would cost one
    // argon2 hash) from a single IP is capped: past the per-IP threshold the
    // request is shed with 429 + Retry-After and `find_by_prefix` (== argon2
    // run) is NOT invoked on the shed request. We assert the cheap-rejection
    // property via the store's find_count seam.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn argon2_cap_sheds_flood_before_invoking_argon2() {
        let org = Uuid::new_v4();
        // One valid key in the store (not used by the flood — the flood uses
        // distinct never-seeded tokens so every attempt is a real cache miss).
        seed_key(&CountingKeyStore::new(), org, "tt_live_unused0000000").await;

        let store = CountingKeyStore::new();
        // Cap = 2 cold-path verifies per IP (burst == per_min).
        let state = state_with_cap(store.clone(), 2);
        let router = build_router(state);

        let flood_ip = "203.0.113.10";

        // First 2 distinct bogus tokens from this IP are UNDER the cap: each is a
        // cache miss → argon2 runs → key not found → 401.
        for n in 0..2 {
            let token = format!("tt_live_floodaaa{n:04}");
            let r = router
                .clone()
                .oneshot(live_bearer_from_ip(&token, flood_ip))
                .await
                .expect("under-cap resp");
            assert_eq!(
                r.status(),
                StatusCode::UNAUTHORIZED,
                "under-cap bogus token should reach argon2 and 401"
            );
        }
        // argon2 ran exactly twice so far (once per under-cap attempt).
        assert_eq!(
            store.find_count.load(Ordering::SeqCst),
            2,
            "the two under-cap attempts each invoke argon2"
        );

        // Third distinct bogus token from the SAME IP is OVER the cap: shed with
        // 429 + Retry-After, and argon2 must NOT be invoked (find_count stays 2).
        let r3 = router
            .clone()
            .oneshot(live_bearer_from_ip("tt_live_floodaaa9999", flood_ip))
            .await
            .expect("over-cap resp");
        assert_eq!(
            r3.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "over-cap attempt must be shed with 429"
        );
        assert!(
            r3.headers().contains_key(axum::http::header::RETRY_AFTER),
            "429 must carry Retry-After"
        );
        assert_eq!(
            store.find_count.load(Ordering::SeqCst),
            2,
            "over-cap attempt must NOT invoke argon2 (cheap rejection before hash)"
        );
    }

    #[tokio::test]
    async fn argon2_cap_is_per_ip_other_ip_unaffected() {
        let store = CountingKeyStore::new();
        // Cap = 1 cold-path verify per IP.
        let state = state_with_cap(store.clone(), 1);
        let router = build_router(state);

        // IP A burns its single cell on a bogus token (401, argon2 runs once).
        let a1 = router
            .clone()
            .oneshot(live_bearer_from_ip("tt_live_aaaa00000000", "198.51.100.1"))
            .await
            .expect("a1");
        assert_eq!(a1.status(), StatusCode::UNAUTHORIZED);
        // IP A's next attempt is over its cap → 429, no argon2.
        let a2 = router
            .clone()
            .oneshot(live_bearer_from_ip("tt_live_aaaa11111111", "198.51.100.1"))
            .await
            .expect("a2");
        assert_eq!(a2.status(), StatusCode::TOO_MANY_REQUESTS);

        // A DIFFERENT IP is independent: its first attempt still reaches argon2
        // (401 for the bogus token), proving normal traffic is unaffected.
        let b1 = router
            .clone()
            .oneshot(live_bearer_from_ip("tt_live_bbbb00000000", "198.51.100.2"))
            .await
            .expect("b1");
        assert_eq!(
            b1.status(),
            StatusCode::UNAUTHORIZED,
            "a different IP within its own limit is not throttled"
        );

        // argon2 ran twice total: once for IP A's first attempt, once for IP B's.
        // IP A's over-cap attempt did NOT run argon2.
        assert_eq!(store.find_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn argon2_cap_does_not_throttle_cache_hits() {
        // A legitimate caller re-presenting the SAME valid token hits the verify
        // cache on every request after the first, so it never consumes from the
        // per-IP cap — even a cap of 1 must not throttle authenticated traffic.
        let org = Uuid::new_v4();
        let plaintext = "tt_live_cachehit12345";

        let store = CountingKeyStore::new();
        seed_key(&store, org, plaintext).await;

        let state = state_with_cap(store.clone(), 1);
        let router = build_router(state);

        let ip = "192.0.2.50";
        // First request: cold path → consumes the single cap cell, argon2 runs, 200.
        let r1 = router
            .clone()
            .oneshot(live_bearer_from_ip(plaintext, ip))
            .await
            .expect("r1");
        assert_eq!(r1.status(), StatusCode::OK);

        // Many more requests with the same token from the same IP: all cache
        // hits → bypass the cap entirely → all 200, argon2 never runs again.
        for _ in 0..5 {
            let r = router
                .clone()
                .oneshot(live_bearer_from_ip(plaintext, ip))
                .await
                .expect("cache-hit resp");
            assert_eq!(
                r.status(),
                StatusCode::OK,
                "cache-hit (authenticated) traffic must not be capped"
            );
        }
        // argon2 ran exactly once (cold path); the cap was consulted only once.
        assert_eq!(store.find_count.load(Ordering::SeqCst), 1);
    }

    // ------------------------------------------------------------------
    // (g) SEC-9: the `tt_test_*` sandbox path is rate-limited by the same
    //     per-IP cap and NEVER verifies against the key store / dispatches a
    //     credential. A flood of distinct sandbox tokens from one IP is shed
    //     with 429 past the cap, while the credential store is never touched.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn sandbox_key_is_rate_limited_and_never_verifies() {
        let store = CountingKeyStore::new();
        // Cap = 2 sandbox passes per IP (burst == per_min).
        let state = state_with_cap(store.clone(), 2);
        let router = build_router(state);

        let ip = "203.0.113.50";

        // First 2 distinct sandbox tokens from this IP are UNDER the cap: each
        // passes through to the (trivial) downstream handler → 200.
        for n in 0..2 {
            let token = format!("tt_test_sandbox{n:04}");
            let r = router
                .clone()
                .oneshot(live_bearer_from_ip(&token, ip))
                .await
                .expect("under-cap sandbox resp");
            assert_eq!(
                r.status(),
                StatusCode::OK,
                "under-cap sandbox key should pass through"
            );
        }

        // Third distinct sandbox token from the SAME IP is OVER the cap → shed
        // with 429 + Retry-After.
        let r3 = router
            .clone()
            .oneshot(live_bearer_from_ip("tt_test_sandbox9999", ip))
            .await
            .expect("over-cap sandbox resp");
        assert_eq!(
            r3.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "over-cap sandbox key must be shed with 429"
        );
        assert!(
            r3.headers().contains_key(axum::http::header::RETRY_AFTER),
            "sandbox 429 must carry Retry-After"
        );

        // INVARIANT: no sandbox request ever consulted the key store — the
        // sandbox path never verifies a credential nor dispatches to a provider.
        assert_eq!(
            store.find_count.load(Ordering::SeqCst),
            0,
            "sandbox (tt_test_) path must never verify against the key store"
        );
    }

    // ------------------------------------------------------------------
    // (h) SEC-6: anonymous dogfood-org stamping fails CLOSED off loopback.
    //
    //   * loopback bind     → anonymous request is stamped with DOGFOOD_ORG_ID.
    //   * non-loopback bind → anonymous request passes through UNSTAMPED, so a
    //     stranger on a shared deploy is never treated as the dogfood org.
    // ------------------------------------------------------------------

    /// Downstream handler that reports — via the `x-identity` response header —
    /// whether the auth middleware stamped an `ApiKeyContext`, so the dogfood
    /// gate is observable end-to-end through the middleware.
    async fn echo_identity(
        ext: Option<axum::Extension<tt_auth::ApiKeyContext>>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let val = match ext {
            Some(axum::Extension(ctx)) => format!("stamped:{}", ctx.org_id),
            None => "anon".to_string(),
        };
        let mut resp = StatusCode::OK.into_response();
        resp.headers_mut()
            .insert("x-identity", val.parse().expect("header value"));
        resp
    }

    fn build_dogfood_router(state: AppState) -> Router {
        Router::new()
            .route("/", get(echo_identity))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                super::middleware,
            ))
            .with_state(state)
    }

    fn anon_request() -> HttpRequest<Body> {
        HttpRequest::builder()
            .uri("/")
            .body(Body::empty())
            .expect("request")
    }

    #[tokio::test]
    async fn dogfood_loopback_stamps_anonymous_org() {
        let state = AppState::new(crate::registry::ProviderRegistry::new()).with_dogfood_enabled();
        assert!(state.dogfood_active(), "loopback dogfood must be active");

        let router = build_dogfood_router(state);
        let resp = router.oneshot(anon_request()).await.expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-identity")
                .and_then(|v| v.to_str().ok()),
            Some(format!("stamped:{}", crate::DOGFOOD_ORG_ID).as_str()),
            "on a loopback bind, anonymous traffic is stamped with the dogfood org"
        );
    }

    #[tokio::test]
    async fn dogfood_non_loopback_does_not_stamp_anonymous_org() {
        // Dogfood enabled, but the bind is NOT loopback → SEC-6 fail closed.
        let state = AppState::new(crate::registry::ProviderRegistry::new())
            .with_dogfood_enabled_for_bind(false);
        assert!(state.dogfood_enabled, "dogfood is still enabled…");
        assert!(
            !state.dogfood_active(),
            "…but the loopback guard keeps it inactive"
        );

        let router = build_dogfood_router(state);
        let resp = router.oneshot(anon_request()).await.expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-identity")
                .and_then(|v| v.to_str().ok()),
            Some("anon"),
            "off loopback, anonymous traffic must NOT be stamped as the dogfood org"
        );
    }

    #[tokio::test]
    async fn sandbox_cap_is_per_ip_other_ip_unaffected() {
        let store = CountingKeyStore::new();
        // Cap = 1 per IP.
        let state = state_with_cap(store.clone(), 1);
        let router = build_router(state);

        // IP A burns its single cell, then is shed.
        let a1 = router
            .clone()
            .oneshot(live_bearer_from_ip("tt_test_aaaa0000", "198.51.100.1"))
            .await
            .expect("a1");
        assert_eq!(a1.status(), StatusCode::OK);
        let a2 = router
            .clone()
            .oneshot(live_bearer_from_ip("tt_test_aaaa1111", "198.51.100.1"))
            .await
            .expect("a2");
        assert_eq!(a2.status(), StatusCode::TOO_MANY_REQUESTS);

        // A DIFFERENT IP still has its own budget → passes.
        let b1 = router
            .clone()
            .oneshot(live_bearer_from_ip("tt_test_bbbb0000", "198.51.100.2"))
            .await
            .expect("b1");
        assert_eq!(
            b1.status(),
            StatusCode::OK,
            "a different IP within its own sandbox budget is not throttled"
        );

        // Still: the key store was never consulted by any sandbox request.
        assert_eq!(store.find_count.load(Ordering::SeqCst), 0);
    }
}
