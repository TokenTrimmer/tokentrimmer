//! Encrypted-prompt audit log for retrieval substitutions (Track E spec §10).
//!
//! Records the **original** prompt (before `<retrievable>` substitution),
//! encrypted with a key derived from `TT_MASTER_KEY`, so a customer can run an
//! offline quality audit of what a substitution replaced — while a DB leak (or
//! TokenTrimmer staff) yields only ciphertext. Mirrors the provider-credential
//! crypto in `tt_auth`: XChaCha20-Poly1305, per-row key = SHA-256(master ‖
//! domain ‖ org_id), AAD bound to org_id. Schema:
//! `crates/core/migrations/0006_retrieval_audit_log`.
//!
//! Writes are best-effort: the gateway records fire-and-forget so an audit
//! failure never blocks or fails the user's request.

use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng, Payload},
    AeadCore, XChaCha20Poly1305, XNonce,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::RetrievalError;

const NONCE_LEN: usize = 24;
const AAD_MAGIC: &[u8] = b"tt-retrieval:audit_log:v1";

/// Postgres-backed encrypted-prompt audit log.
pub struct RetrievalAuditLog {
    pool: sqlx::PgPool,
    master_key: [u8; 32],
}

impl std::fmt::Debug for RetrievalAuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetrievalAuditLog")
            .field("pool", &"PgPool { .. }")
            .field("master_key", &"[REDACTED]")
            .finish()
    }
}

impl RetrievalAuditLog {
    pub fn new(pool: sqlx::PgPool, master_key: [u8; 32]) -> Self {
        Self { pool, master_key }
    }

    /// Build from `TT_MASTER_KEY` (hex, 64 chars = 32 bytes). Returns `None`
    /// when the var is missing or malformed — the caller treats audit as
    /// disabled in that case rather than failing retrieval.
    pub fn from_env(pool: sqlx::PgPool) -> Option<Self> {
        let hex_key = std::env::var("TT_MASTER_KEY").ok()?;
        let bytes = hex::decode(hex_key.trim()).ok()?;
        let master_key: [u8; 32] = bytes.try_into().ok()?;
        Some(Self { pool, master_key })
    }

    /// Encrypt the original prompt and insert an audit row. Best-effort — the
    /// caller spawns this and logs (does not propagate) errors.
    pub async fn record(
        &self,
        org_id: Uuid,
        substitutions: u32,
        tokens_saved: i64,
        original_prompt: &str,
    ) -> Result<(), RetrievalError> {
        let blob = encrypt_prompt(&self.master_key, org_id, original_prompt)?;
        sqlx::query(
            r#"INSERT INTO retrieval_audit_log
                 (id, org_id, prompt_enc, substitutions, tokens_saved)
               VALUES ($1, $2, $3, $4, $5)"#,
        )
        .bind(Uuid::new_v4())
        .bind(org_id)
        .bind(&blob)
        .bind(i32::try_from(substitutions).unwrap_or(i32::MAX))
        .bind(tokens_saved)
        .execute(&self.pool)
        .await
        .map_err(|e| RetrievalError::Store(format!("audit insert: {e}")))?;
        Ok(())
    }

    /// Decrypt a stored audit blob — used by the offline-audit read path.
    pub fn decrypt(&self, org_id: Uuid, blob: &[u8]) -> Result<String, RetrievalError> {
        decrypt_prompt(&self.master_key, org_id, blob)
    }
}

/// Per-row audit key, domain-separated from the credential KDF.
fn derive_audit_key(master: &[u8; 32], org_id: Uuid) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(master);
    h.update(b"|tt-retrieval:audit-key:v1|");
    h.update(org_id.as_bytes());
    h.finalize().into()
}

fn aad(org_id: Uuid) -> Vec<u8> {
    let mut buf = Vec::with_capacity(AAD_MAGIC.len() + 16);
    buf.extend_from_slice(AAD_MAGIC);
    buf.extend_from_slice(org_id.as_bytes());
    buf
}

fn encrypt_prompt(master: &[u8; 32], org_id: Uuid, plain: &str) -> Result<Vec<u8>, RetrievalError> {
    let key = derive_audit_key(master, org_id);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ad = aad(org_id);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plain.as_bytes(),
                aad: &ad,
            },
        )
        .map_err(|_| RetrievalError::Store("audit encrypt failed".into()))?;
    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&ciphertext);
    Ok(blob)
}

fn decrypt_prompt(master: &[u8; 32], org_id: Uuid, blob: &[u8]) -> Result<String, RetrievalError> {
    if blob.len() < NONCE_LEN {
        return Err(RetrievalError::Malformed("audit blob too short".into()));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce = XNonce::from_slice(nonce_bytes);
    let key = derive_audit_key(master, org_id);
    let cipher = XChaCha20Poly1305::new((&key).into());
    let ad = aad(org_id);
    let plain = cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: &ad,
            },
        )
        .map_err(|_| RetrievalError::Store("audit decrypt failed".into()))?;
    String::from_utf8(plain).map_err(|_| RetrievalError::Malformed("audit utf8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let master = [5u8; 32];
        let org = Uuid::from_u128(7);
        let prompt = r#"{"messages":[{"role":"user","content":"secret prompt"}]}"#;
        let blob = encrypt_prompt(&master, org, prompt).unwrap();
        assert_eq!(decrypt_prompt(&master, org, &blob).unwrap(), prompt);
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext() {
        let master = [1u8; 32];
        let org = Uuid::from_u128(1);
        let blob = encrypt_prompt(&master, org, "api_key=sk-leakme").unwrap();
        assert!(!String::from_utf8_lossy(&blob).contains("sk-leakme"));
    }

    #[test]
    fn wrong_org_or_master_fails_to_decrypt() {
        let master = [2u8; 32];
        let org = Uuid::from_u128(2);
        let blob = encrypt_prompt(&master, org, "hello").unwrap();
        // Wrong org (AAD + per-row key both differ).
        assert!(decrypt_prompt(&master, Uuid::from_u128(3), &blob).is_err());
        // Wrong master key.
        assert!(decrypt_prompt(&[9u8; 32], org, &blob).is_err());
    }

    #[test]
    fn truncated_blob_is_rejected() {
        assert!(decrypt_prompt(&[0u8; 32], Uuid::nil(), &[0u8; 5]).is_err());
    }
}
