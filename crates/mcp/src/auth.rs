//! `tt_live_*` / `tt_test_*` API-key handling for the MCP server.
//!
//! Two distinct checks live here:
//!
//! 1. [`validate_api_key`] — a **format** check (presence + known prefix). Used
//!    at the CLI boundary (`tt login`, `tt mcp` boot) where the *operator's own*
//!    key is being read from the environment to forward upstream, and there is
//!    no key store to verify against.
//!
//! 2. [`verify_api_key`] — **real** verification of a *presented* key against a
//!    [`KeyStore`], exactly the way the Gateway does it: it delegates to
//!    [`tt_auth::verify`], which looks the key up by prefix and runs the
//!    constant-time argon2 password verify. On success it returns the bound
//!    [`ApiKeyContext`] (org + key id) so the server can act on the right
//!    tenant; on any failure (absent/unknown/wrong-secret/revoked/malformed) it
//!    returns [`McpError::Unauthorized`] (JSON-RPC `-32001`).
//!
//! [`Authenticator`] is the seam that puts (2) on the **live request path**: the
//! [`crate::server::Server`] holds one (when a key store is wired) and calls it
//! before dispatching `tools/call` / `resources/read`. It implements the design
//! §8 flow — *validate on first tool/resource call, cache the `org_id` for the
//! process lifetime* — so the verified tenant is bound once and reused, and an
//! invalid/absent key fails the call closed with `-32001`.
//!
//! We deliberately do **not** reimplement any crypto here — all hashing and the
//! constant-time compare are owned by `tt_auth`, so the MCP server inherits the
//! same verify path (and the same no-timing-oracle guarantees) as the Gateway.

use std::sync::Arc;

use tokio::sync::OnceCell;
use tt_auth::{ApiKeyContext, KeyError, KeyStore};

use crate::error::McpError;

/// Validate the *format* of an API key: it must be present and carry a known
/// environment prefix (`tt_live_` / `tt_test_`). This is a cheap shape check, not
/// a verification against the key store — see [`verify_api_key`] for that.
pub fn validate_api_key(env_var: Option<String>) -> Result<String, McpError> {
    let k = env_var.ok_or_else(|| McpError::Unauthorized("TT_API_KEY missing".into()))?;
    if !k.starts_with("tt_live_") && !k.starts_with("tt_test_") {
        return Err(McpError::Unauthorized("invalid TT_API_KEY prefix".into()));
    }
    Ok(k)
}

/// Verify a *presented* API key against `store`, the same way the Gateway does.
///
/// Delegates to [`tt_auth::verify`] (lookup-by-prefix + constant-time argon2
/// password verify) and, on success, returns the [`ApiKeyContext`] carrying the
/// verified `org_id` / `key_id` so callers can bind the right tenant into the
/// request context.
///
/// Any verification failure collapses to [`McpError::Unauthorized`]
/// (JSON-RPC `-32001`), matching the MCP design (§8): the server must reject
/// unauthenticated/invalid-key requests with the `unauthorized` error and must
/// not leak *why* a key was rejected (key-exists-but-wrong-secret,
/// unknown-prefix, and revoked all map to the same response — no enumeration
/// oracle).
///
/// An absent key (`None`) is treated as unauthorized: the MCP server fails
/// closed. This is the verify primitive [`Authenticator`] drives on the first
/// tool/resource call.
pub async fn verify_api_key<S: KeyStore + ?Sized>(
    store: &S,
    presented: Option<&str>,
) -> Result<ApiKeyContext, McpError> {
    let presented = presented.ok_or_else(|| McpError::Unauthorized("missing API key".into()))?;
    tt_auth::verify(store, presented)
        .await
        .map_err(map_key_error)
}

/// Process-lifetime authenticator wired into the [`crate::server::Server`].
///
/// Realises the MCP design (§8): *"Server validates on first tool/resource
/// call; caches the `org_id` for the process lifetime."* The operator's own
/// presented key (the same key the loopback transport requires as its bearer
/// token) is verified against a [`KeyStore`] **on the first tool/resource
/// dispatch**, and the resulting [`ApiKeyContext`] is cached so every later
/// call binds the same verified tenant without re-running argon2.
///
/// This is the seam through which [`verify_api_key`] enters the live request
/// path: the server holds an `Authenticator` and calls
/// [`Authenticator::context`] before dispatching `tools/call` / `resources/read`
/// so tools act on the right tenant. When no store is wired (the local dev
/// boot, where the transport bearer guard is the gate and there is no key store
/// to verify against), the server simply holds no `Authenticator` and dispatch
/// proceeds unchanged.
pub struct Authenticator {
    store: Arc<dyn KeyStore>,
    presented: String,
    /// First-call verification result, cached for the process lifetime. Holds
    /// the bound [`ApiKeyContext`] on success so the org is resolved exactly
    /// once; the verify failure is *not* cached (a transient store fault must
    /// not permanently wedge the server).
    cached: OnceCell<ApiKeyContext>,
}

impl Authenticator {
    /// Build an authenticator that will verify `presented` against `store` on
    /// first use. No verification happens here — it is deferred to the first
    /// tool/resource call, per §8.
    pub fn new(store: Arc<dyn KeyStore>, presented: impl Into<String>) -> Self {
        Self {
            store,
            presented: presented.into(),
            cached: OnceCell::new(),
        }
    }

    /// Return the verified [`ApiKeyContext`] for the operator key, verifying
    /// against the key store on the first call and returning the cached result
    /// thereafter. Fails closed with [`McpError::Unauthorized`] (`-32001`) on an
    /// invalid/absent/revoked key — exactly the same verify path and error
    /// mapping as [`verify_api_key`], so no timing oracle is introduced and the
    /// rejection never reveals *why* a key failed.
    pub async fn context(&self) -> Result<&ApiKeyContext, McpError> {
        self.cached
            .get_or_try_init(|| verify_api_key(self.store.as_ref(), Some(&self.presented)))
            .await
    }
}

/// Collapse a [`KeyError`] into the MCP `unauthorized` error. Every variant maps
/// to the same JSON-RPC `-32001` so the server never reveals *which* check
/// failed (no key-enumeration oracle). The variant name is kept in the message
/// purely for server-side logs/tests — it is generic enough not to leak tenant
/// state.
fn map_key_error(err: KeyError) -> McpError {
    let reason = match err {
        KeyError::InvalidFormat => "invalid key format",
        KeyError::NotFound => "unknown or invalid key",
        KeyError::Revoked => "revoked key",
        // Store/hash/audit faults are internal failures, but for an auth path we
        // still fail closed (unauthorized) rather than surface internals to the
        // client. They are logged so operators can distinguish them.
        KeyError::Hash(_) | KeyError::Store(_) | KeyError::Audit(_) => {
            tracing::warn!(error = %err, "MCP key verification: internal store/hash fault, failing closed");
            "verification failed"
        }
        // Issuance-only variants; not reachable from `verify`, but handled
        // exhaustively so future variants force a compile error here.
        KeyError::PrefixCollision | KeyError::PrefixCollisionExhausted(_) => "verification failed",
    };
    McpError::Unauthorized(reason.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tt_auth::{issue, Environment, InMemoryKeyStore};
    use tt_telemetry::audit::{Actor, InMemoryAuditWriter};
    use uuid::Uuid;

    // ── validate_api_key (format-only) ──────────────────────────────────────

    #[test]
    fn rejects_missing_key() {
        assert!(matches!(
            validate_api_key(None).unwrap_err(),
            McpError::Unauthorized(_)
        ));
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!(matches!(
            validate_api_key(Some("nope".into())).unwrap_err(),
            McpError::Unauthorized(_)
        ));
    }

    #[test]
    fn accepts_valid_prefix() {
        assert!(validate_api_key(Some("tt_live_abc".into())).is_ok());
        assert!(validate_api_key(Some("tt_test_xyz".into())).is_ok());
    }

    // ── verify_api_key (real store-backed verification) ─────────────────────

    /// Mint a real argon2-hashed key for `org` into the store and return its
    /// plaintext. Uses `tt_auth::issue` so the test exercises the genuine
    /// hashing path (no crypto reimplemented in the test).
    async fn seed_key(store: &InMemoryKeyStore, org: Uuid) -> String {
        let audit = InMemoryAuditWriter::default();
        let issued = issue(
            store,
            &audit,
            org,
            "mcp-test",
            Environment::Live,
            Actor::System,
        )
        .await
        .expect("issue key");
        issued.plaintext
    }

    /// A valid presented key is authorized and the correct org is bound.
    #[tokio::test]
    async fn valid_key_authorized_and_binds_org() {
        let store = InMemoryKeyStore::new();
        let org = Uuid::now_v7();
        let plaintext = seed_key(&store, org).await;

        let ctx = verify_api_key(&store, Some(&plaintext))
            .await
            .expect("valid key should be authorized");

        assert_eq!(
            ctx.org_id, org,
            "verified context must carry the issuing org"
        );
    }

    /// An absent key is rejected (fail-closed) with the unauthorized error.
    #[tokio::test]
    async fn absent_key_rejected() {
        let store = InMemoryKeyStore::new();
        let err = verify_api_key(&store, None).await.unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
        // MCP/JSON-RPC unauthorized code.
        assert_eq!(err.code(), -32001);
    }

    /// A well-formed but unknown key (not in the store) is rejected.
    #[tokio::test]
    async fn unknown_key_rejected() {
        let store = InMemoryKeyStore::new();
        // Correct shape, never issued.
        let err = verify_api_key(&store, Some("tt_live_deadbeef0000"))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
        assert_eq!(err.code(), -32001);
    }

    /// A key with the right prefix but the wrong secret is rejected, and the
    /// rejection is indistinguishable from "unknown key" (no enumeration oracle:
    /// both are `Unauthorized`, neither names the org).
    #[tokio::test]
    async fn wrong_secret_rejected_without_oracle() {
        let store = InMemoryKeyStore::new();
        let org = Uuid::now_v7();
        let plaintext = seed_key(&store, org).await;

        // Same 12-char prefix, different secret tail → prefix hit, hash mismatch.
        let prefix = &plaintext[..12];
        let forged = format!("{prefix}{}", "f".repeat(plaintext.len() - 12));
        assert_ne!(
            forged, plaintext,
            "forged key must differ from the real one"
        );

        let err = verify_api_key(&store, Some(&forged)).await.unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
        // Must not leak the org id in the error message.
        assert!(
            !err.to_string().contains(&org.to_string()),
            "error must not leak the org id"
        );
    }

    /// A malformed key (wrong prefix / too short) is rejected as unauthorized.
    #[tokio::test]
    async fn malformed_key_rejected() {
        let store = InMemoryKeyStore::new();
        let err = verify_api_key(&store, Some("not-a-key")).await.unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
    }

    /// A revoked key is rejected even though it was once valid.
    #[tokio::test]
    async fn revoked_key_rejected() {
        let store = InMemoryKeyStore::new();
        let org = Uuid::now_v7();
        let audit = InMemoryAuditWriter::default();

        let issued = issue(
            &store,
            &audit,
            org,
            "mcp-revoke-test",
            Environment::Live,
            Actor::System,
        )
        .await
        .expect("issue key");

        // Sanity: valid before revocation.
        verify_api_key(&store, Some(&issued.plaintext))
            .await
            .expect("key valid before revoke");

        // Revoke it (org-scoped) directly in the store.
        let revoked = store
            .revoke(issued.record.id, org, chrono::Utc::now())
            .await
            .expect("revoke");
        assert!(revoked, "revoke should report the key as just-revoked");

        let err = verify_api_key(&store, Some(&issued.plaintext))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
    }
}
