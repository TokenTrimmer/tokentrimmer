//! Postgres-backed [`ProviderCredentialStore`].
//!
//! Each row in `provider_credentials` holds a single upstream API key
//! encrypted with XChaCha20-Poly1305. The key used for encryption is
//! derived per-row via SHA-256 from a `TT_MASTER_KEY` root and the
//! `(org_id, provider)` pair — so a leaked row's ciphertext can't be
//! decrypted with anything other than that org's derived key, and a
//! single root rotation doesn't require re-encrypting every row in one
//! transaction.
//!
//! On-disk layout of `secret_enc`: `nonce (24 bytes) || ciphertext (rest)`.
//! XChaCha20-Poly1305 nonces are 24 bytes (random per encryption); the
//! AEAD tag is appended to the ciphertext by the underlying crate.
//!
//! AAD on every encryption includes a magic prefix plus the canonical
//! `(org_id, provider)` pair, so an attacker who swaps two rows in
//! Postgres cannot decrypt them — the AAD mismatch produces a decrypt
//! failure rather than a successful decode under the wrong identity.
//!
//! Gated behind the `postgres` feature. OSS distributions that don't ship
//! credential storage (self-hosters using `EnvProviderCredentialStore`
//! instead) skip the sqlx + chacha20poly1305 deps entirely.

use async_trait::async_trait;
use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng},
    AeadCore, XChaCha20Poly1305, XNonce,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;
use tt_shared::context::{ProviderCredentials, SecretString};
use uuid::Uuid;

use crate::{CredentialError, ProviderCredentialStore};

/// Length of the XChaCha20-Poly1305 nonce (extended-nonce ChaCha20).
const NONCE_LEN: usize = 24;

/// Magic prefix mixed into AAD so a ciphertext from a different system
/// path (or a future schema with overlapping bytes) cannot accidentally
/// decrypt successfully.
const AAD_MAGIC: &[u8] = b"tt-auth:provider_credentials:v1";

/// Errors surfaced by the Postgres credential store.
#[derive(Debug, Error)]
pub enum CredentialStoreError {
    /// `TT_MASTER_KEY` was malformed (must be 32 hex-encoded bytes = 64 chars).
    #[error("invalid TT_MASTER_KEY: {0}")]
    BadMasterKey(String),

    /// Stored ciphertext is shorter than the nonce length.
    #[error("ciphertext too short: {len} bytes (need >= {NONCE_LEN})")]
    CiphertextTooShort {
        /// Actual length of the stored bytes.
        len: usize,
    },

    /// AEAD decrypt failed (tampered ciphertext, wrong AAD, or stale key).
    #[error("decrypt failed")]
    Decrypt,

    /// AEAD encrypt failed (extremely rare — only on internal RustCrypto errors).
    #[error("encrypt failed")]
    Encrypt,

    /// Postgres query failed.
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),

    /// Stored row's `extra_headers` JSON was malformed.
    #[error("extra_headers parse: {0}")]
    ExtraHeaders(#[from] serde_json::Error),
}

impl From<CredentialStoreError> for CredentialError {
    fn from(value: CredentialStoreError) -> Self {
        CredentialError::Store(value.to_string())
    }
}

/// Postgres-backed [`ProviderCredentialStore`].
///
/// Construct via [`PostgresProviderCredentialStore::from_env`] (production)
/// or [`PostgresProviderCredentialStore::new`] (tests / explicit master key).
#[derive(Clone)]
pub struct PostgresProviderCredentialStore {
    pool: PgPool,
    /// 32-byte root key from `TT_MASTER_KEY`. Per-row keys are derived via a
    /// SHA-256 one-shot KDF (see `derive_key`), not HKDF.
    master_key: [u8; 32],
}

impl std::fmt::Debug for PostgresProviderCredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresProviderCredentialStore")
            .field("pool", &"PgPool { .. }")
            .field("master_key", &"[REDACTED]")
            .finish()
    }
}

impl PostgresProviderCredentialStore {
    /// Build a store from an explicit master key and pool. Tests / wiring.
    pub fn new(pool: PgPool, master_key: [u8; 32]) -> Self {
        Self { pool, master_key }
    }

    /// Build a store using `TT_MASTER_KEY` from process env. The env var is
    /// hex-encoded (64 chars = 32 bytes). Returns [`CredentialStoreError::BadMasterKey`]
    /// when the env var is missing or malformed.
    pub fn from_env(pool: PgPool) -> Result<Self, CredentialStoreError> {
        let hex = std::env::var("TT_MASTER_KEY").map_err(|_| {
            CredentialStoreError::BadMasterKey("TT_MASTER_KEY not set in environment".into())
        })?;
        let bytes = hex::decode(hex.trim())
            .map_err(|e| CredentialStoreError::BadMasterKey(format!("hex decode: {e}")))?;
        let master_key: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            CredentialStoreError::BadMasterKey(format!(
                "expected 32 bytes (64 hex chars); got {}",
                v.len()
            ))
        })?;
        Ok(Self { pool, master_key })
    }

    /// Encrypt + insert/upsert a new credential row.
    ///
    /// Used by the dashboard's credential-entry UI (when that lands) and by
    /// the integration tests below.
    pub async fn put(
        &self,
        org_id: Uuid,
        provider: &str,
        label: &str,
        upstream_api_key: &str,
        base_url: Option<&str>,
        extra_headers: &[(String, String)],
    ) -> Result<Uuid, CredentialStoreError> {
        let derived = self.derive_key(org_id, provider);
        let cipher = XChaCha20Poly1305::new((&derived).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = Self::aad(org_id, provider);
        let payload = chacha20poly1305::aead::Payload {
            msg: upstream_api_key.as_bytes(),
            aad: &aad,
        };
        let ciphertext = cipher
            .encrypt(&nonce, payload)
            .map_err(|_| CredentialStoreError::Encrypt)?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);

        let extra_json = serde_json::to_value(extra_headers)?;
        let id = sqlx::query_scalar::<_, Uuid>(
            r#"INSERT INTO provider_credentials
                 (org_id, provider, label, secret_enc, base_url, extra_headers)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (org_id, provider) DO UPDATE SET
                 label = EXCLUDED.label,
                 secret_enc = EXCLUDED.secret_enc,
                 base_url = EXCLUDED.base_url,
                 extra_headers = EXCLUDED.extra_headers,
                 rotated_at = now()
               RETURNING id"#,
        )
        .bind(org_id)
        .bind(provider)
        .bind(label)
        .bind(&blob)
        .bind(base_url)
        .bind(&extra_json)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Derive a per-row encryption key.
    ///
    /// We use a simple SHA-256-based one-shot KDF rather than full HKDF here
    /// because the master key is already cryptographically random and the
    /// info contains a unique salt (`(org_id, provider)`). This keeps the
    /// dep tree thinner — no `hkdf` crate — without giving up the property
    /// that a leak of one row's ciphertext does not help decrypt another.
    fn derive_key(&self, org_id: Uuid, provider: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.master_key);
        h.update(b"|tt-auth:cred-key:v1|");
        h.update(org_id.as_bytes());
        h.update(b"|");
        h.update(provider.as_bytes());
        h.finalize().into()
    }

    /// Build the AAD bytes for an encrypt/decrypt operation.
    fn aad(org_id: Uuid, provider: &str) -> Vec<u8> {
        let mut buf = Vec::with_capacity(AAD_MAGIC.len() + 16 + 1 + provider.len());
        buf.extend_from_slice(AAD_MAGIC);
        buf.extend_from_slice(org_id.as_bytes());
        buf.push(b':');
        buf.extend_from_slice(provider.as_bytes());
        buf
    }

    /// Decrypt a stored blob (nonce || ciphertext) under the per-row derived key.
    fn decrypt(
        &self,
        org_id: Uuid,
        provider: &str,
        blob: &[u8],
    ) -> Result<String, CredentialStoreError> {
        if blob.len() < NONCE_LEN {
            return Err(CredentialStoreError::CiphertextTooShort { len: blob.len() });
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = XNonce::from_slice(nonce_bytes);
        let derived = self.derive_key(org_id, provider);
        let cipher = XChaCha20Poly1305::new((&derived).into());
        let aad = Self::aad(org_id, provider);
        let payload = chacha20poly1305::aead::Payload {
            msg: ciphertext,
            aad: &aad,
        };
        let plain = cipher
            .decrypt(nonce, payload)
            .map_err(|_| CredentialStoreError::Decrypt)?;
        String::from_utf8(plain).map_err(|_| CredentialStoreError::Decrypt)
    }
}

#[async_trait]
impl ProviderCredentialStore for PostgresProviderCredentialStore {
    async fn put(
        &self,
        org_id: Uuid,
        provider_id: &str,
        label: &str,
        credentials: ProviderCredentials,
    ) -> Result<(), CredentialError> {
        let extra: Vec<(String, String)> = credentials.extra_headers.clone();
        self.put(
            org_id,
            provider_id,
            label,
            credentials.api_key.expose(),
            credentials.base_url.as_deref(),
            &extra,
        )
        .await
        .map(|_| ())
        .map_err(CredentialError::from)
    }

    async fn get(
        &self,
        org_id: Uuid,
        provider_id: &str,
    ) -> Result<Option<ProviderCredentials>, CredentialError> {
        let row: Option<(Vec<u8>, Option<String>, serde_json::Value)> = sqlx::query_as(
            r#"SELECT secret_enc, base_url, extra_headers
               FROM provider_credentials
               WHERE org_id = $1 AND provider = $2"#,
        )
        .bind(org_id)
        .bind(provider_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(CredentialStoreError::Sql)?;

        let Some((blob, base_url, extra_json)) = row else {
            return Ok(None);
        };

        let plaintext = self
            .decrypt(org_id, provider_id, &blob)
            .map_err(CredentialError::from)?;
        let extra_headers: Vec<(String, String)> =
            serde_json::from_value(extra_json).map_err(CredentialStoreError::ExtraHeaders)?;
        Ok(Some(ProviderCredentials {
            api_key: SecretString::new(plaintext),
            base_url,
            extra_headers,
        }))
    }
}

// ─── Postgres-backed API key store ─────────────────────────────────────────
//
// Pairs with the `api_keys` table from the cloud-schema-0001 migration
// (`crates/api/migrations/0001_cloud_schema.up.sql` in the cloud repo).
// The argon2 hash is computed by `tt_auth::keys::issue` and verified by
// `tt_auth::keys::verify`; this store is the thin Postgres adapter.

use crate::keys::{ApiKey, Environment, KeyError, KeyStore};
use chrono::{DateTime, Utc};

/// Postgres-backed implementation of [`KeyStore`].
///
/// Cheap to clone — the inner `PgPool` is reference-counted by sqlx.
#[derive(Clone)]
pub struct PostgresKeyStore {
    pool: PgPool,
}

impl PostgresKeyStore {
    /// Construct from an existing pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl std::fmt::Debug for PostgresKeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresKeyStore")
            .field("pool", &"PgPool { .. }")
            .finish()
    }
}

fn env_from_str(s: &str) -> Result<Environment, KeyError> {
    match s {
        "live" => Ok(Environment::Live),
        "test" => Ok(Environment::Test),
        other => Err(KeyError::Store(format!(
            "unknown environment column value: {other}"
        ))),
    }
}

fn env_to_str(env: Environment) -> &'static str {
    match env {
        Environment::Live => "live",
        Environment::Test => "test",
    }
}

#[async_trait]
impl KeyStore for PostgresKeyStore {
    async fn insert(&self, key: ApiKey) -> Result<(), KeyError> {
        sqlx::query(
            r#"INSERT INTO api_keys
                 (id, org_id, label, prefix, secret_hash, environment, created_at, last_used_at, revoked_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"#,
        )
        .bind(key.id)
        .bind(key.org_id)
        .bind(&key.label)
        .bind(&key.prefix)
        .bind(&key.hash)
        .bind(env_to_str(key.environment))
        .bind(key.created_at)
        .bind(Option::<DateTime<Utc>>::None) // last_used_at — not tracked at insert time
        .bind(key.revoked_at)
        .execute(&self.pool)
        .await
        .map_err(|e| KeyError::Store(e.to_string()))?;
        Ok(())
    }

    async fn find_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>, KeyError> {
        let row: Option<(
            Uuid,
            Uuid,
            String,
            String,
            String,
            String,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
        )> = sqlx::query_as(
            r#"SELECT id, org_id, label, prefix, secret_hash, environment, created_at, revoked_at
               FROM api_keys
               WHERE prefix = $1"#,
        )
        .bind(prefix)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| KeyError::Store(e.to_string()))?;
        let Some((id, org_id, label, prefix, hash, env_str, created_at, revoked_at)) = row else {
            return Ok(None);
        };
        Ok(Some(ApiKey {
            id,
            org_id,
            prefix,
            hash,
            label,
            environment: env_from_str(&env_str)?,
            created_at,
            revoked_at,
        }))
    }

    async fn revoke(&self, id: Uuid, at: DateTime<Utc>) -> Result<bool, KeyError> {
        let rows = sqlx::query(
            r#"UPDATE api_keys SET revoked_at = $1
               WHERE id = $2 AND revoked_at IS NULL"#,
        )
        .bind(at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| KeyError::Store(e.to_string()))?;
        Ok(rows.rows_affected() == 1)
    }

    async fn touch_last_used(&self, id: Uuid, at: DateTime<Utc>) -> Result<(), KeyError> {
        // Single-row update. `last_used_at` is informational only — readers
        // only consult it for the dashboard's "Last used" column.
        sqlx::query(r#"UPDATE api_keys SET last_used_at = $1 WHERE id = $2"#)
            .bind(at)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| KeyError::Store(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod key_store_tests {
    use super::*;

    #[tokio::test]
    async fn env_str_round_trips() {
        assert_eq!(env_to_str(Environment::Live), "live");
        assert_eq!(env_to_str(Environment::Test), "test");
        assert_eq!(env_from_str("live").unwrap(), Environment::Live);
        assert_eq!(env_from_str("test").unwrap(), Environment::Test);
    }

    #[tokio::test]
    async fn env_from_str_rejects_garbage() {
        let err = env_from_str("garbage").unwrap_err();
        assert!(matches!(err, KeyError::Store(_)));
    }

    #[tokio::test]
    async fn debug_redacts_pool() {
        let store = PostgresKeyStore::new(panic_pool());
        let s = format!("{store:?}");
        assert!(s.contains("PostgresKeyStore"));
        assert!(s.contains("PgPool { .. }"));
    }

    fn panic_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .expect("lazy connect")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: encrypt under one key, decrypt under the same key.
    #[tokio::test]
    async fn aad_changes_with_org_or_provider() {
        let a = PostgresProviderCredentialStore::aad(Uuid::nil(), "openai");
        let b = PostgresProviderCredentialStore::aad(Uuid::from_u128(1), "openai");
        let c = PostgresProviderCredentialStore::aad(Uuid::nil(), "anthropic");
        assert_ne!(a, b, "AAD must vary by org_id");
        assert_ne!(a, c, "AAD must vary by provider");
    }

    #[tokio::test]
    async fn derive_key_is_deterministic_per_inputs_and_distinct_otherwise() {
        let store = PostgresProviderCredentialStore {
            pool: panic_pool(),
            master_key: [1u8; 32],
        };
        let org = Uuid::from_u128(42);
        let k1 = store.derive_key(org, "openai");
        let k2 = store.derive_key(org, "openai");
        assert_eq!(k1, k2, "same inputs must produce same key");
        let k3 = store.derive_key(org, "anthropic");
        assert_ne!(k1, k3, "different provider must produce different key");
        let k4 = store.derive_key(Uuid::from_u128(43), "openai");
        assert_ne!(k1, k4, "different org must produce different key");

        let other_master = PostgresProviderCredentialStore {
            pool: panic_pool(),
            master_key: [2u8; 32],
        };
        let k5 = other_master.derive_key(org, "openai");
        assert_ne!(k1, k5, "different master key must produce different key");
    }

    /// Build a `PgPool` that panics on first use. We construct the store
    /// without ever calling Postgres, just to exercise the in-memory parts.
    fn panic_pool() -> PgPool {
        // Lazy pool construction with an obviously-bogus URL — the tests
        // that touch the pool would fail with a connection error, but none
        // of the offline tests above do touch it.
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .expect("lazy connect")
    }

    /// Combined into one test because the three cases share global process
    /// environment and would otherwise race when cargo runs tests in parallel.
    #[tokio::test]
    async fn from_env_validation() {
        // Case 1: var unset → BadMasterKey.
        std::env::remove_var("TT_MASTER_KEY");
        let err = PostgresProviderCredentialStore::from_env(panic_pool()).unwrap_err();
        assert!(matches!(err, CredentialStoreError::BadMasterKey(_)));

        // Case 2: too-short hex → BadMasterKey.
        std::env::set_var("TT_MASTER_KEY", "deadbeef");
        let err = PostgresProviderCredentialStore::from_env(panic_pool()).unwrap_err();
        assert!(matches!(err, CredentialStoreError::BadMasterKey(_)));

        // Case 3: 32-byte hex → success, master_key matches.
        let key = hex::encode([7u8; 32]);
        std::env::set_var("TT_MASTER_KEY", &key);
        let store =
            PostgresProviderCredentialStore::from_env(panic_pool()).expect("valid hex master key");
        assert_eq!(store.master_key, [7u8; 32]);

        std::env::remove_var("TT_MASTER_KEY");
    }

    /// Round-trip a single ciphertext via the same store, no Postgres.
    #[tokio::test]
    async fn encrypt_decrypt_round_trip_without_db() {
        let store = PostgresProviderCredentialStore {
            pool: panic_pool(),
            master_key: [9u8; 32],
        };
        let org = Uuid::from_u128(7);
        let provider = "openai";
        let plain = "sk-test-very-secret";

        // Encrypt inline (mirrors what `put` does up to the SQL insert).
        let derived = store.derive_key(org, provider);
        let cipher = XChaCha20Poly1305::new((&derived).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = PostgresProviderCredentialStore::aad(org, provider);
        let ct = cipher
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: plain.as_bytes(),
                    aad: &aad,
                },
            )
            .unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct);

        let recovered = store.decrypt(org, provider, &blob).unwrap();
        assert_eq!(recovered, plain);
    }

    /// Decrypt under the *wrong* AAD (different org_id) must fail.
    #[tokio::test]
    async fn decrypt_rejects_wrong_aad() {
        let store = PostgresProviderCredentialStore {
            pool: panic_pool(),
            master_key: [3u8; 32],
        };
        let org_a = Uuid::from_u128(1);
        let org_b = Uuid::from_u128(2);
        let provider = "openai";
        let plain = "sk-test";

        let derived = store.derive_key(org_a, provider);
        let cipher = XChaCha20Poly1305::new((&derived).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad_a = PostgresProviderCredentialStore::aad(org_a, provider);
        let ct = cipher
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: plain.as_bytes(),
                    aad: &aad_a,
                },
            )
            .unwrap();
        let mut blob = Vec::new();
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct);

        // Try to decrypt as org_b — must fail.
        let err = store.decrypt(org_b, provider, &blob).unwrap_err();
        assert!(matches!(err, CredentialStoreError::Decrypt));
    }

    #[tokio::test]
    async fn decrypt_rejects_truncated_blob() {
        let store = PostgresProviderCredentialStore {
            pool: panic_pool(),
            master_key: [0u8; 32],
        };
        let err = store.decrypt(Uuid::nil(), "openai", &[0u8; 5]).unwrap_err();
        assert!(matches!(
            err,
            CredentialStoreError::CiphertextTooShort { len: 5 }
        ));
    }
}
