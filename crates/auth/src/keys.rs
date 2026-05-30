//! API key issuance + verification.
//!
//! Keys are issued once and never recoverable from storage — only the argon2
//! hash is persisted. Verification: presented key → recompute hash → compare.
//!
//! See `docs/04-gateway-api-reference.md` for the `Authorization: Bearer tt_live_*`
//! header contract and ADR-002 for the open-core rationale.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use argon2::{
    password_hash::{rand_core::OsRng as ArgonRng, PasswordHasher, PasswordVerifier, SaltString},
    Argon2, PasswordHash,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tt_telemetry::audit::{Actor, AuditError, AuditWriter};
use uuid::Uuid;

use crate::ApiKeyContext;

/// Number of random bytes behind the key (post-prefix). 16 bytes = 128 bits.
const KEY_RANDOM_BYTES: usize = 16;

/// Length of the displayable prefix used to look up the row in storage.
/// Matches environment prefix "tt_live_" (8 chars) + first 4 hex chars = 12,
/// giving 4 hex digits = 16 bits of prefix entropy.
///
/// # Scaling note
///
/// The global prefix space is 2^16 = 65 536 unique prefixes (shared across all
/// environments and orgs). The birthday paradox gives ~1% collision probability
/// at ~37 total live keys and ~50% at ~256. The retry loop in [`issue`] handles
/// collisions transparently for current scales, but a future migration that
/// widens the prefix (e.g. to 8 hex chars = 32 bits = ~4 billion slots) will be
/// required before the system approaches thousands of concurrently-active keys.
/// Widening requires a schema migration of the `prefix` column and `find_by_prefix`
/// changes to accept both old and new lengths during a dual-write transition window.
const PREFIX_DISPLAY_LEN: usize = 12;

/// Maximum number of attempts [`issue`] will make before giving up when every
/// attempt produces a prefix collision. In practice, collisions at modest key
/// counts are vanishingly rare (see [`PREFIX_DISPLAY_LEN`] scaling note).
const ISSUE_MAX_ATTEMPTS: u32 = 5;

/// Whether a key was issued for the live production environment or a test environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    /// Production/live traffic.
    Live,
    /// Test/sandbox traffic — will not be charged.
    Test,
}

impl Environment {
    /// Returns the string prefix for keys in this environment.
    fn prefix(self) -> &'static str {
        match self {
            Environment::Live => "tt_live_",
            Environment::Test => "tt_test_",
        }
    }
}

/// A persisted API key row — what the database stores. Never contains the plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    /// Unique key identifier (UUIDv7).
    pub id: Uuid,
    /// Organization this key belongs to.
    pub org_id: Uuid,
    /// First 12 chars of the key, e.g. `"tt_live_abcd"`. Used to look up the row.
    pub prefix: String,
    /// Argon2 PHC string. Never returned to the user.
    pub hash: String,
    /// Human-readable label for the key.
    pub label: String,
    /// Which environment this key is valid for.
    pub environment: Environment,
    /// When the key was created (UTC).
    pub created_at: DateTime<Utc>,
    /// When the key was revoked, if ever (UTC).
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The result of [`issue()`] — contains the full plaintext key, which the caller
/// MUST show to the user exactly once and then discard.
#[derive(Debug, Clone)]
pub struct IssuedKey {
    /// The persisted record (without plaintext).
    pub record: ApiKey,
    /// Plaintext key, e.g. `"tt_live_abcdef0123..."`. Show ONCE.
    pub plaintext: String,
}

/// Errors that can occur during key issuance or verification.
#[derive(Debug, Error)]
pub enum KeyError {
    /// Argon2 hashing or hash parsing failed.
    #[error("argon2: {0}")]
    Hash(String),
    /// The presented key does not match the expected format.
    #[error("invalid key format")]
    InvalidFormat,
    /// No key was found matching the presented prefix.
    #[error("not found")]
    NotFound,
    /// The key has been revoked.
    #[error("revoked")]
    Revoked,
    /// Audit logging failed.
    #[error("audit: {0}")]
    Audit(#[from] AuditError),
    /// Key store operation failed.
    #[error("store: {0}")]
    Store(String),
    /// A key with the same display prefix already exists in the store.
    ///
    /// The caller (e.g. [`issue`]) should retry with freshly-generated random
    /// bytes. The 16-bit prefix space (`tt_live_` + 4 hex chars) has a ~1%
    /// birthday collision probability at ~37 live keys and ~50% at ~256 keys
    /// globally. Retrying up to [`ISSUE_MAX_ATTEMPTS`] times keeps issuance
    /// correct at current scales; a future migration to a wider prefix is
    /// needed if the global key count approaches the thousands.
    #[error("prefix collision — retry")]
    PrefixCollision,
    /// Key issuance failed because all [`ISSUE_MAX_ATTEMPTS`] attempts
    /// encountered a prefix collision. This is extremely unlikely in normal
    /// operation; it signals a near-full prefix space or a broken CSPRNG.
    #[error("key issuance failed after {0} attempts due to repeated prefix collisions; prefix space may be exhausted")]
    PrefixCollisionExhausted(u32),
}

/// A secret-free view of an API key, for listing endpoints. Never carries the
/// argon2 hash or the plaintext — safe to return over the admin API.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKeySummary {
    pub id: Uuid,
    pub org_id: Uuid,
    pub prefix: String,
    pub label: String,
    pub environment: Environment,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
}

impl From<&ApiKey> for ApiKeySummary {
    fn from(k: &ApiKey) -> Self {
        Self {
            id: k.id,
            org_id: k.org_id,
            prefix: k.prefix.clone(),
            label: k.label.clone(),
            environment: k.environment,
            created_at: k.created_at,
            // `ApiKey` doesn't carry last_used_at; stores that track it (Postgres)
            // populate this directly in their `list_active`.
            last_used_at: None,
        }
    }
}

/// Persistence contract for API key storage.
///
/// Implementations: [`InMemoryKeyStore`] (this file),
/// `PostgresKeyStore` (lands when DB pool wiring is done).
#[async_trait]
pub trait KeyStore: Send + Sync {
    /// Insert a new key row.
    async fn insert(&self, key: ApiKey) -> Result<(), KeyError>;

    /// List active (non-revoked) keys for `org_id`, newest first, WITHOUT any
    /// secret material. Default impl returns empty so stores that don't support
    /// listing opt out trivially; [`InMemoryKeyStore`] and `PostgresKeyStore`
    /// override it.
    async fn list_active(&self, _org_id: Uuid) -> Result<Vec<ApiKeySummary>, KeyError> {
        Ok(Vec::new())
    }

    /// Look up by prefix (the [`PREFIX_DISPLAY_LEN`]-char display prefix).
    async fn find_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>, KeyError>;

    /// Mark the key with the given `id` as revoked at `at`.
    ///
    /// Returns `true` if found and updated, `false` if not found.
    async fn revoke(&self, id: Uuid, at: DateTime<Utc>) -> Result<bool, KeyError>;

    /// Stamp `last_used_at = at` on the row with the given `id`. Default
    /// impl is a no-op so stores that don't track usage (e.g., simple
    /// in-memory test stores that don't care) can opt out trivially.
    /// The Postgres impl overrides to `UPDATE … SET last_used_at = $1`.
    async fn touch_last_used(&self, _id: Uuid, _at: DateTime<Utc>) -> Result<(), KeyError> {
        Ok(())
    }
}

/// In-memory key store. Suitable for tests and CLI demos; production uses Postgres.
pub struct InMemoryKeyStore {
    by_prefix: Arc<Mutex<HashMap<String, ApiKey>>>,
}

impl InMemoryKeyStore {
    /// Create a new, empty in-memory store.
    pub fn new() -> Self {
        Self {
            by_prefix: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl KeyStore for InMemoryKeyStore {
    async fn insert(&self, key: ApiKey) -> Result<(), KeyError> {
        let mut g = self
            .by_prefix
            .lock()
            .map_err(|e| KeyError::Store(e.to_string()))?;
        if g.contains_key(&key.prefix) {
            return Err(KeyError::PrefixCollision);
        }
        g.insert(key.prefix.clone(), key);
        Ok(())
    }

    async fn find_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>, KeyError> {
        let g = self
            .by_prefix
            .lock()
            .map_err(|e| KeyError::Store(e.to_string()))?;
        Ok(g.get(prefix).cloned())
    }

    async fn list_active(&self, org_id: Uuid) -> Result<Vec<ApiKeySummary>, KeyError> {
        let g = self
            .by_prefix
            .lock()
            .map_err(|e| KeyError::Store(e.to_string()))?;
        let mut out: Vec<ApiKeySummary> = g
            .values()
            .filter(|k| k.org_id == org_id && k.revoked_at.is_none())
            .map(ApiKeySummary::from)
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at)); // newest first
        Ok(out)
    }

    async fn revoke(&self, id: Uuid, at: DateTime<Utc>) -> Result<bool, KeyError> {
        let mut g = self
            .by_prefix
            .lock()
            .map_err(|e| KeyError::Store(e.to_string()))?;
        for v in g.values_mut() {
            if v.id == id {
                if v.revoked_at.is_some() {
                    // Idempotent: revoking an already-revoked key is a
                    // no-op. Matches the Postgres impl's `WHERE revoked_at
                    // IS NULL` semantics so callers see the same
                    // was-already-active signal regardless of store.
                    return Ok(false);
                }
                v.revoked_at = Some(at);
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Issue a new API key for `org_id`.
///
/// Writes the key to `store`, emits an `apikey.issued` audit row via
/// `audit_writer`, and returns an [`IssuedKey`] containing the full plaintext
/// (which the caller MUST show exactly once and then discard).
///
/// The audit payload includes the key `id`, `prefix`, `label`, and
/// `environment` — it deliberately excludes the plaintext.
///
/// # Prefix-collision retry
///
/// If the store signals a prefix collision ([`KeyError::PrefixCollision`]), the
/// function re-generates the random bytes and retries up to
/// [`ISSUE_MAX_ATTEMPTS`] times total. If all attempts collide, it returns
/// [`KeyError::PrefixCollisionExhausted`] — a distinct, retryable error that
/// the caller can surface clearly without hiding behind a generic `Store` error.
pub async fn issue<S: KeyStore + ?Sized, A: AuditWriter + ?Sized>(
    store: &S,
    audit_writer: &A,
    org_id: Uuid,
    label: impl Into<String>,
    environment: Environment,
    actor: Actor,
) -> Result<IssuedKey, KeyError> {
    let label = label.into();

    let mut attempt = 0u32;
    loop {
        attempt += 1;

        // Generate 16 random bytes via the OS CSPRNG.
        let mut bytes = [0u8; KEY_RANDOM_BYTES];
        OsRng.fill_bytes(&mut bytes);
        let random_hex = hex::encode(bytes);
        let plaintext = format!("{}{}", environment.prefix(), random_hex);
        let prefix = plaintext[..PREFIX_DISPLAY_LEN].to_string();

        // Hash the plaintext with argon2.
        let salt = SaltString::generate(&mut ArgonRng);
        let argon = Argon2::default();
        let hash = argon
            .hash_password(plaintext.as_bytes(), &salt)
            .map_err(|e| KeyError::Hash(e.to_string()))?
            .to_string();

        let id = Uuid::now_v7();
        let now = Utc::now();
        let key = ApiKey {
            id,
            org_id,
            prefix: prefix.clone(),
            hash,
            label: label.clone(),
            environment,
            created_at: now,
            revoked_at: None,
        };

        match store.insert(key.clone()).await {
            Ok(()) => {
                // Emit audit row. Payload deliberately excludes plaintext.
                let payload = serde_json::json!({
                    "key_id": id,
                    "prefix": prefix,
                    "label": label,
                    "environment": environment,
                });
                audit_writer
                    .write(org_id, actor, "apikey.issued".to_string(), payload)
                    .await?;

                return Ok(IssuedKey {
                    record: key,
                    plaintext,
                });
            }
            Err(KeyError::PrefixCollision) if attempt < ISSUE_MAX_ATTEMPTS => {
                tracing::warn!(
                    attempt,
                    max = ISSUE_MAX_ATTEMPTS,
                    %prefix,
                    "api key prefix collision — retrying with fresh random bytes"
                );
                // Loop and regenerate.
                continue;
            }
            Err(KeyError::PrefixCollision) => {
                tracing::error!(
                    attempts = attempt,
                    "api key issuance exhausted all attempts due to prefix collisions"
                );
                return Err(KeyError::PrefixCollisionExhausted(attempt));
            }
            Err(other) => return Err(other),
        }
    }
}

/// Verify a presented key string.
///
/// Looks up the stored row by prefix, checks it is not revoked, then performs
/// an argon2 password verify. Returns the [`ApiKeyContext`] on success.
///
/// Argon2 verification is constant-time for keys that exist. For unknown
/// prefixes the rejection is faster (no hash compare), but that does not leak
/// which org owns a key — only that the prefix is unregistered.
///
/// # Errors
///
/// - [`KeyError::InvalidFormat`] — key does not start with `tt_live_` or
///   `tt_test_`, or is shorter than [`PREFIX_DISPLAY_LEN`].
/// - [`KeyError::NotFound`] — no row matches the prefix, or argon2 verify
///   failed (collapsed to `NotFound` to avoid leaking "key exists but wrong").
/// - [`KeyError::Revoked`] — key has been revoked.
pub async fn verify<S: KeyStore + ?Sized>(
    store: &S,
    presented: &str,
) -> Result<ApiKeyContext, KeyError> {
    if presented.len() < PREFIX_DISPLAY_LEN
        || !(presented.starts_with("tt_live_") || presented.starts_with("tt_test_"))
    {
        return Err(KeyError::InvalidFormat);
    }
    let prefix = &presented[..PREFIX_DISPLAY_LEN];
    let key = store
        .find_by_prefix(prefix)
        .await?
        .ok_or(KeyError::NotFound)?;
    if key.revoked_at.is_some() {
        return Err(KeyError::Revoked);
    }

    let parsed = PasswordHash::new(&key.hash).map_err(|e| KeyError::Hash(e.to_string()))?;
    Argon2::default()
        .verify_password(presented.as_bytes(), &parsed)
        // Collapse to NotFound to avoid leaking "key exists but hash mismatch".
        .map_err(|_| KeyError::NotFound)?;

    Ok(ApiKeyContext {
        key_id: key.id,
        org_id: key.org_id,
        // `tier` is injected by the cloud tier-resolution layer
        // (rv-tier-limits-enforcement). Until that layer is wired, `None`
        // causes the gateway to fall back to the 24h conservative default.
        tier: None,
    })
}

/// Revoke an API key and emit an `apikey.revoked` audit row.
///
/// Wraps the bare [`KeyStore::revoke`] mutation with the audit-emission step
/// so callers don't accidentally revoke without leaving a chain entry — the
/// chain is the tamper-evident record of every privileged action.
///
/// # Errors
///
/// - [`KeyError::NotFound`] — no key matches `key_id` in the store
/// - [`KeyError::Audit`] — store update succeeded but the audit row failed
///   to write. The key IS revoked when this occurs; the caller should
///   re-attempt the audit emission out-of-band.
/// - [`KeyError::Store`] — underlying store error.
pub async fn revoke_key<S: KeyStore + ?Sized, A: AuditWriter + ?Sized>(
    store: &S,
    audit_writer: &A,
    org_id: Uuid,
    key_id: Uuid,
    actor: Actor,
) -> Result<(), KeyError> {
    let now = Utc::now();
    let updated = store.revoke(key_id, now).await?;
    if !updated {
        return Err(KeyError::NotFound);
    }
    let payload = serde_json::json!({
        "key_id": key_id,
        "revoked_at": now.to_rfc3339(),
    });
    audit_writer
        .write(org_id, actor, "apikey.revoked".to_string(), payload)
        .await?;
    Ok(())
}

#[cfg(test)]
mod prefix_collision_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tt_telemetry::audit::InMemoryAuditWriter;

    // ── A store wrapper that returns PrefixCollision for the first N inserts ─
    struct FailNTimesStore {
        inner: InMemoryKeyStore,
        /// How many more inserts should return PrefixCollision before delegating.
        remaining_failures: AtomicU32,
    }

    impl FailNTimesStore {
        fn new(fail_count: u32) -> Self {
            Self {
                inner: InMemoryKeyStore::new(),
                remaining_failures: AtomicU32::new(fail_count),
            }
        }
    }

    #[async_trait]
    impl KeyStore for FailNTimesStore {
        async fn insert(&self, key: ApiKey) -> Result<(), KeyError> {
            let remaining = self.remaining_failures.fetch_update(
                Ordering::SeqCst,
                Ordering::SeqCst,
                |v| if v > 0 { Some(v - 1) } else { None },
            );
            if remaining.is_ok() {
                return Err(KeyError::PrefixCollision);
            }
            self.inner.insert(key).await
        }

        async fn find_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>, KeyError> {
            self.inner.find_by_prefix(prefix).await
        }

        async fn revoke(&self, id: Uuid, at: DateTime<Utc>) -> Result<bool, KeyError> {
            self.inner.revoke(id, at).await
        }
    }

    // (a) InMemoryKeyStore::insert returns PrefixCollision on duplicate prefix
    //     and does NOT overwrite the existing key.
    #[tokio::test]
    async fn in_memory_insert_duplicate_prefix_returns_collision_and_no_overwrite() {
        let store = InMemoryKeyStore::new();
        let org_a = Uuid::now_v7();
        let org_b = Uuid::now_v7();

        let key_a = ApiKey {
            id: Uuid::now_v7(),
            org_id: org_a,
            prefix: "tt_live_aaaa".to_string(),
            hash: "hash-a".to_string(),
            label: "first".to_string(),
            environment: Environment::Live,
            created_at: Utc::now(),
            revoked_at: None,
        };
        let key_b = ApiKey {
            id: Uuid::now_v7(),
            org_id: org_b,
            prefix: "tt_live_aaaa".to_string(), // same prefix
            hash: "hash-b".to_string(),
            label: "second".to_string(),
            environment: Environment::Live,
            created_at: Utc::now(),
            revoked_at: None,
        };

        store.insert(key_a.clone()).await.unwrap();

        let err = store.insert(key_b).await.unwrap_err();
        assert!(
            matches!(err, KeyError::PrefixCollision),
            "expected PrefixCollision, got: {err:?}"
        );

        // The original key must still be in the store (no overwrite).
        let found = store
            .find_by_prefix("tt_live_aaaa")
            .await
            .unwrap()
            .expect("original key must still be present");
        assert_eq!(found.org_id, org_a, "key_a must not be overwritten by key_b");
        assert_eq!(found.hash, "hash-a", "hash must not be overwritten");
    }

    // (b) issue() retries past a single forced collision and succeeds.
    //     The issued key is stored correctly under its prefix.
    #[tokio::test]
    async fn issue_retries_past_one_forced_collision() {
        let store = FailNTimesStore::new(1); // fail first insert, succeed on second
        let audit = InMemoryAuditWriter::default();
        let org = Uuid::now_v7();

        let issued = issue(
            &store,
            &audit,
            org,
            "retry-test",
            Environment::Live,
            Actor::System,
        )
        .await
        .expect("issue should succeed after retry");

        // The issued key must be stored under its prefix.
        let found = store
            .find_by_prefix(&issued.record.prefix)
            .await
            .unwrap()
            .expect("issued key must be retrievable by prefix");
        assert_eq!(found.id, issued.record.id);

        // Plaintext must start with the correct env prefix.
        assert!(
            issued.plaintext.starts_with("tt_live_"),
            "plaintext must start with tt_live_"
        );
    }

    // (c) Exhausting all retries surfaces PrefixCollisionExhausted, not Store.
    #[tokio::test]
    async fn issue_exhausted_retries_returns_collision_exhausted_error() {
        // Fail every insert unconditionally (fail_count > ISSUE_MAX_ATTEMPTS).
        let store = FailNTimesStore::new(ISSUE_MAX_ATTEMPTS + 1);
        let audit = InMemoryAuditWriter::default();
        let org = Uuid::now_v7();

        let err = issue(
            &store,
            &audit,
            org,
            "exhausted-test",
            Environment::Live,
            Actor::System,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, KeyError::PrefixCollisionExhausted(n) if n == ISSUE_MAX_ATTEMPTS),
            "expected PrefixCollisionExhausted({ISSUE_MAX_ATTEMPTS}), got: {err:?}"
        );

        // The error message must not look like an opaque Store error.
        let msg = err.to_string();
        assert!(
            msg.contains("prefix"),
            "error message should mention prefix, got: {msg}"
        );
    }
}

#[cfg(test)]
mod list_active_tests {
    use super::*;

    fn sample(org: Uuid, prefix: &str, revoked: bool) -> ApiKey {
        ApiKey {
            id: Uuid::now_v7(),
            org_id: org,
            prefix: prefix.to_string(),
            hash: "argon2-phc-string".to_string(),
            label: "test key".to_string(),
            environment: Environment::Live,
            created_at: Utc::now(),
            revoked_at: if revoked { Some(Utc::now()) } else { None },
        }
    }

    #[tokio::test]
    async fn list_active_excludes_revoked_other_orgs_and_secrets() {
        let store = InMemoryKeyStore::new();
        let org = Uuid::now_v7();
        let other = Uuid::now_v7();
        store
            .insert(sample(org, "tt_live_aaaa", false))
            .await
            .unwrap();
        store
            .insert(sample(org, "tt_live_bbbb", true))
            .await
            .unwrap(); // revoked
        store
            .insert(sample(other, "tt_live_cccc", false))
            .await
            .unwrap(); // other org

        let active = store.list_active(org).await.unwrap();
        assert_eq!(active.len(), 1, "only the org's non-revoked key");
        assert_eq!(active[0].prefix, "tt_live_aaaa");
        // The secret-free summary must never serialize the argon2 hash.
        let json = serde_json::to_string(&active[0]).unwrap();
        assert!(!json.contains("argon2"), "summary leaked the hash: {json}");
    }

    #[tokio::test]
    async fn list_active_empty_for_unknown_org() {
        let store = InMemoryKeyStore::new();
        assert!(store.list_active(Uuid::now_v7()).await.unwrap().is_empty());
    }
}
