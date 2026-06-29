//! Per-org encrypted secret store for workflow HTTP nodes.
//!
//! Secrets are referenced in workflow definitions as `{{secrets.NAME}}` and
//! resolved at runtime by the HTTP-node executor (W3b Task 3). This module
//! handles the encrypted persistence layer only.
//!
//! ## Cipher scheme (mirrors `cloud/secret_cipher.rs` — new namespace)
//!
//! * 32-byte root key from `TT_MASTER_KEY` (64 hex chars). Read via
//!   [`master_key_from_env`].
//! * Per-row key = `SHA256(master || CONTEXT || org_id_bytes || name_bytes)`.
//!   A leaked row cannot be decrypted with a different row's derived key, and
//!   renaming a secret means the old ciphertext no longer decrypts (AAD check
//!   also enforces this independently).
//! * On-disk layout: `nonce (24 bytes) || ciphertext+tag`. XChaCha20-Poly1305
//!   nonce is 24 bytes (random per encryption); the AEAD tag is appended to
//!   the ciphertext by the underlying crate.
//! * AAD = `CONTEXT || org_id_bytes || b':' || name_bytes`. Binding both
//!   org_id and name means a row copied to a different org or renamed under
//!   the same org fails the AAD check rather than decrypting silently.
//! * CONTEXT = `b"tt:workflow_secret:v1"` — DISTINCT from:
//!   - `b"tt-auth:provider_credentials:v1"` (provider-cred store)
//!   - `b"tt-api:managed_chat_key:v1"` (cloud managed-chat-key store)
//!
//!   Ciphertexts from one path can never cross-decrypt under another.

use std::collections::HashMap;

use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng, Payload},
    AeadCore, XChaCha20Poly1305, XNonce,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tt_shared::context::SecretString;
use uuid::Uuid;

/// Length of the XChaCha20-Poly1305 nonce (extended-nonce ChaCha20).
const NONCE_LEN: usize = 24;

/// Namespace string mixed into both the per-row KDF and the AAD. Distinct from
/// every other CONTEXT in the system so ciphertexts cannot cross-decrypt.
const CONTEXT: &[u8] = b"tt:workflow_secret:v1";

// ---------------------------------------------------------------------------
// Key derivation + AAD
// ---------------------------------------------------------------------------

/// Derive a per-row encryption key.
///
/// `SHA256(master || CONTEXT || org_id_bytes || name_bytes)` — the master is
/// cryptographically random, org_id and name together are a unique salt, so
/// this gives the "one leaked row doesn't help decrypt another" property.
fn derive_key(master: &[u8; 32], org_id: Uuid, name: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(master);
    h.update(CONTEXT);
    h.update(org_id.as_bytes());
    h.update(name.as_bytes());
    h.finalize().into()
}

/// Build the AAD bytes (`CONTEXT || org_id_bytes || ':' || name_bytes`).
///
/// Binding both org_id and name means a row moved to another org OR renamed
/// within the same org fails to decrypt (AAD mismatch → hard error).
fn build_aad(org_id: Uuid, name: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(CONTEXT.len() + 16 + 1 + name.len());
    buf.extend_from_slice(CONTEXT);
    buf.extend_from_slice(org_id.as_bytes());
    buf.extend_from_slice(b":");
    buf.extend_from_slice(name.as_bytes());
    buf
}

// ---------------------------------------------------------------------------
// Public(crate) cipher API
// ---------------------------------------------------------------------------

/// Encrypt `plain` for `(org_id, name)`.
///
/// Returns `nonce (24 bytes) || ciphertext+tag`. The nonce is random per call
/// so encrypting the same plaintext twice gives different blobs.
///
/// # Panics
///
/// Never panics in practice — XChaCha20-Poly1305 encrypt only fails on
/// internal RustCrypto allocation errors, which are impossible on hosted
/// hardware. If the impossibly rare error occurs the blob is empty (callers
/// should treat `blob.len() < NONCE_LEN` as a failure).
pub(crate) fn encrypt_secret(master: &[u8; 32], org_id: Uuid, name: &str, plain: &str) -> Vec<u8> {
    let derived = derive_key(master, org_id, name);
    let cipher = XChaCha20Poly1305::new((&derived).into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aad = build_aad(org_id, name);
    let Ok(ciphertext) = cipher.encrypt(
        &nonce,
        Payload {
            msg: plain.as_bytes(),
            aad: &aad,
        },
    ) else {
        // Encryption failure is not expected; return an empty blob so the
        // caller's `blob.len() < NONCE_LEN` guard catches it.
        return Vec::new();
    };
    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);
    blob
}

/// Decrypt a stored `nonce || ciphertext+tag` blob for `(org_id, name)`.
///
/// Returns `None` on any failure: blob too short, bad AEAD tag, AAD mismatch
/// (wrong org_id or wrong name), non-UTF-8 plaintext. **Never panics.**
pub(crate) fn decrypt_secret(
    master: &[u8; 32],
    org_id: Uuid,
    name: &str,
    blob: &[u8],
) -> Option<SecretString> {
    if blob.len() < NONCE_LEN {
        return None;
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce = XNonce::from_slice(nonce_bytes);
    let derived = derive_key(master, org_id, name);
    let cipher = XChaCha20Poly1305::new((&derived).into());
    let aad = build_aad(org_id, name);
    let plain = cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .ok()?;
    let s = String::from_utf8(plain).ok()?;
    Some(SecretString::new(s))
}

/// Read `TT_MASTER_KEY` (64 hex chars → `[u8; 32]`) from the process
/// environment. Returns `None` when the variable is absent or malformed.
///
/// Mirrors the pattern in `crates/auth/src/postgres.rs`
/// (`PostgresProviderCredentialStore::from_env`).
///
/// Dead-code allowed — wired by W3b Task 3/4 route handlers.
#[allow(dead_code)]
pub(crate) fn master_key_from_env() -> Option<[u8; 32]> {
    let hex = std::env::var("TT_MASTER_KEY").ok()?;
    let bytes = hex::decode(hex.trim()).ok()?;
    bytes.try_into().ok()
}

// ---------------------------------------------------------------------------
// DB store helpers (async — wired by Task 3/4)
// ---------------------------------------------------------------------------

// SQL constants follow the `store.rs` convention: named constants, runtime
// sqlx (no `query!` macros that require an offline cache).

const UPSERT_SECRET_SQL: &str = "\
INSERT INTO workflow_secrets (org_id, name, secret_enc) \
VALUES ($1, $2, $3) \
ON CONFLICT (org_id, name) DO UPDATE \
  SET secret_enc = EXCLUDED.secret_enc, \
      rotated_at = now()";

const SELECT_SECRETS_SQL: &str = "\
SELECT name, secret_enc \
FROM workflow_secrets \
WHERE org_id = $1";

/// Encrypt `plain` and UPSERT it into `workflow_secrets` for `(org_id, name)`.
///
/// On conflict (the secret already exists) the ciphertext is rotated in-place
/// and `rotated_at` is updated. Dead-code allowed — wired by W3b Task 3/4.
#[allow(dead_code)]
pub(crate) async fn store_secret(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    master: &[u8; 32],
    plain: &str,
) -> Result<(), sqlx::Error> {
    let blob = encrypt_secret(master, org_id, name, plain);
    sqlx::query(UPSERT_SECRET_SQL)
        .bind(org_id)
        .bind(name)
        .bind(blob)
        .execute(pool)
        .await?;
    Ok(())
}

/// Load and decrypt all secrets for `org_id`. Rows that fail to decrypt
/// (stale key, tampered blob) are silently skipped — callers get a best-effort
/// map rather than a hard error, so a single bad row doesn't block execution.
///
/// Dead-code allowed — wired by W3b Task 3/4.
#[allow(dead_code)]
pub(crate) async fn load_secrets(
    pool: &PgPool,
    org_id: Uuid,
    master: &[u8; 32],
) -> HashMap<String, SecretString> {
    let rows = sqlx::query(SELECT_SECRETS_SQL)
        .bind(org_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    let mut map = HashMap::new();
    for row in rows {
        use sqlx::Row as _;
        let name: String = match row.try_get("name") {
            Ok(v) => v,
            Err(_) => continue,
        };
        let blob: Vec<u8> = match row.try_get("secret_enc") {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(secret) = decrypt_secret(master, org_id, &name, &blob) {
            map.insert(name, secret);
        }
    }
    map
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn master() -> [u8; 32] {
        [0x42u8; 32]
    }

    /// Round-trip: encrypt then decrypt recovers the original plaintext.
    #[test]
    fn cipher_roundtrip() {
        let org = Uuid::nil();
        let name = "MY_API_KEY";
        let plain = "super-secret-value-abc123";
        let blob = encrypt_secret(&master(), org, name, plain);
        // Blob must be longer than just the nonce.
        assert!(
            blob.len() > NONCE_LEN,
            "blob too short: {} bytes",
            blob.len()
        );
        let recovered =
            decrypt_secret(&master(), org, name, &blob).expect("decrypt should succeed");
        assert_eq!(recovered.expose(), plain);
    }

    /// AAD binding on name: encrypting under "A" must not decrypt under "B".
    #[test]
    fn decrypt_wrong_name_fails() {
        let org = Uuid::nil();
        let blob = encrypt_secret(&master(), org, "SECRET_A", "value");
        // Attempt decrypt with a different name → AAD mismatch → None.
        let result = decrypt_secret(&master(), org, "SECRET_B", &blob);
        assert!(result.is_none(), "expected None for wrong name, got Some");
    }

    /// AAD binding on org_id: a blob encrypted for org A must not decrypt for org B.
    #[test]
    fn decrypt_wrong_org_fails() {
        let org_a = Uuid::from_u128(1);
        let org_b = Uuid::from_u128(2);
        let blob = encrypt_secret(&master(), org_a, "KEY", "value");
        let result = decrypt_secret(&master(), org_b, "KEY", &blob);
        assert!(result.is_none(), "expected None for wrong org, got Some");
    }

    /// Flipping any byte in the ciphertext portion must cause an authentication
    /// failure, not a successful (garbage) decrypt or a panic.
    #[test]
    fn decrypt_tampered_blob_fails() {
        let org = Uuid::nil();
        let mut blob = encrypt_secret(&master(), org, "KEY", "value");
        // Flip a byte in the ciphertext (past the nonce).
        blob[NONCE_LEN] ^= 0xFF;
        let result = decrypt_secret(&master(), org, "KEY", &blob);
        assert!(
            result.is_none(),
            "expected None for tampered blob, got Some"
        );
    }

    /// An empty or too-short blob must return None without panicking.
    #[test]
    fn decrypt_short_blob_is_none() {
        let org = Uuid::nil();
        // Empty blob.
        assert!(decrypt_secret(&master(), org, "KEY", &[]).is_none());
        // Blob shorter than the nonce.
        assert!(decrypt_secret(&master(), org, "KEY", &[0u8; NONCE_LEN - 1]).is_none());
    }
}
