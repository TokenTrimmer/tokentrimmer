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
/// Matches environment prefix "tt_live_" (8 chars) + first 4 hex chars = 12.
const PREFIX_DISPLAY_LEN: usize = 12;

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
pub async fn issue<S: KeyStore + ?Sized, A: AuditWriter + ?Sized>(
    store: &S,
    audit_writer: &A,
    org_id: Uuid,
    label: impl Into<String>,
    environment: Environment,
    actor: Actor,
) -> Result<IssuedKey, KeyError> {
    let label = label.into();

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
    store.insert(key.clone()).await?;

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

    Ok(IssuedKey {
        record: key,
        plaintext,
    })
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
