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

use std::collections::{BTreeSet, HashMap};

use chacha20poly1305::{
    aead::{Aead, KeyInit, OsRng, Payload},
    AeadCore, XChaCha20Poly1305, XNonce,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tt_shared::context::SecretString;
use uuid::Uuid;

use super::types::{NodeKind, WorkflowDefinition};

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

const LIST_SECRET_ROWS_SQL: &str = "\
SELECT name, secret_enc, created_at, rotated_at \
FROM workflow_secrets \
WHERE org_id = $1 \
ORDER BY name ASC \
LIMIT $2";

/// Internal encrypted row used to derive safe picker metadata. The ciphertext
/// is deliberately private to this module and is never serialized.
pub(crate) struct WorkflowSecretRow {
    pub(crate) name: String,
    secret_enc: Vec<u8>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) rotated_at: Option<DateTime<Utc>>,
}

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

/// Return `true` when `name` matches `^[A-Z0-9_]{1,64}$` — the charset used
/// in `{{secrets.NAME}}` template references in Http nodes.
pub(crate) fn is_valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
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

/// Read a deterministic, bounded page of encrypted rows for the safe secret
/// inventory. Callers may derive decryptability, but must never expose the
/// ciphertext or plaintext.
pub(crate) async fn list_secret_rows(
    pool: &PgPool,
    org_id: Uuid,
    limit: i64,
) -> Result<Vec<WorkflowSecretRow>, sqlx::Error> {
    use sqlx::Row as _;

    let rows = sqlx::query(LIST_SECRET_ROWS_SQL)
        .bind(org_id)
        .bind(limit)
        .fetch_all(pool)
        .await?;

    rows.into_iter()
        .map(|row| {
            Ok(WorkflowSecretRow {
                name: row.try_get("name")?,
                secret_enc: row.try_get("secret_enc")?,
                created_at: row.try_get("created_at")?,
                rotated_at: row.try_get("rotated_at")?,
            })
        })
        .collect()
}

impl WorkflowSecretRow {
    /// Test whether this row decrypts for its original org/name binding. The
    /// plaintext is immediately dropped and never leaves this module.
    pub(crate) fn is_decryptable(&self, master: &[u8; 32], org_id: Uuid) -> bool {
        decrypt_secret(master, org_id, &self.name, &self.secret_enc).is_some()
    }
}

/// Collect every exact `{{secrets.NAME}}` reference used by Http wire fields.
/// Invalid or unclosed references are rejected without echoing their contents,
/// because definitions can themselves contain sensitive user input.
pub(crate) fn required_secret_names(
    def: &WorkflowDefinition,
) -> Result<BTreeSet<String>, Vec<String>> {
    let mut names = BTreeSet::new();
    let mut errors = Vec::new();

    for node in &def.nodes {
        let NodeKind::Http {
            url, headers, body, ..
        } = &node.kind
        else {
            continue;
        };

        scan_secret_references(url, &node.id, "url", &mut names, &mut errors);
        for (_, value) in headers {
            scan_secret_references(value, &node.id, "header", &mut names, &mut errors);
        }
        if let Some(body) = body {
            scan_secret_references(body, &node.id, "body", &mut names, &mut errors);
        }
    }

    if errors.is_empty() {
        Ok(names)
    } else {
        Err(errors)
    }
}

fn scan_secret_references(
    value: &str,
    node_id: &str,
    field: &str,
    names: &mut BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    const PREFIX: &str = "{{secrets.";
    let mut remaining = value;

    while let Some(start) = remaining.find(PREFIX) {
        let after_prefix = &remaining[start + PREFIX.len()..];
        let Some(end) = after_prefix.find("}}") else {
            errors.push(format!(
                "node \"{node_id}\": Http {field} contains an unclosed \
                 {{{{secrets.NAME}}}} reference"
            ));
            return;
        };
        let name = &after_prefix[..end];
        if is_valid_secret_name(name) {
            names.insert(name.to_string());
        } else {
            errors.push(format!(
                "node \"{node_id}\": Http {field} contains an invalid secret reference; \
                 names must match ^[A-Z0-9_]{{1,64}}$"
            ));
        }
        remaining = &after_prefix[end + 2..];
    }
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

    /// `is_valid_secret_name` rejects names with lowercase letters, spaces,
    /// hyphens, empty strings, and names longer than 64 characters.
    #[test]
    fn secret_name_rejects_bad_names() {
        assert!(!is_valid_secret_name("lowercase"));
        assert!(!is_valid_secret_name("HAS SPACE"));
        assert!(!is_valid_secret_name("MY-KEY"));
        assert!(!is_valid_secret_name(""));
        assert!(!is_valid_secret_name(&"A".repeat(65)));
    }

    /// `is_valid_secret_name` accepts uppercase-letter / digit / underscore
    /// names up to 64 characters.
    #[test]
    fn secret_name_accepts_valid_names() {
        assert!(is_valid_secret_name("MY_API_KEY"));
        assert!(is_valid_secret_name("A"));
        assert!(is_valid_secret_name("KEY_123"));
        assert!(is_valid_secret_name(&"A".repeat(64)));
    }

    #[test]
    fn secret_inventory_query_is_scoped_ordered_and_bounded() {
        assert!(LIST_SECRET_ROWS_SQL.contains("WHERE org_id = $1"));
        assert!(LIST_SECRET_ROWS_SQL.contains("ORDER BY name ASC"));
        assert!(LIST_SECRET_ROWS_SQL.contains("LIMIT $2"));
        assert!(!LIST_SECRET_ROWS_SQL.contains("SELECT *"));
    }

    #[test]
    fn required_secret_names_are_exact_and_deduplicated() {
        use super::super::types::{BudgetPolicy, Node};

        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "refs".into(),
            nodes: vec![Node {
                id: "http".into(),
                kind: NodeKind::Http {
                    method: "POST".into(),
                    url: "https://api.example.com/{{secrets.PATH_KEY}}".into(),
                    headers: vec![
                        ("authorization".into(), "Bearer {{secrets.API_KEY}}".into()),
                        ("x-repeat".into(), "{{secrets.API_KEY}}".into()),
                    ],
                    body: Some("{{secrets.BODY_KEY}}".into()),
                    max_response_bytes: None,
                },
            }],
            edges: vec![],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec!["api.example.com".into()],
            metadata: serde_json::Value::Null,
            triggers: vec![],
        };

        assert_eq!(
            required_secret_names(&def)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["API_KEY", "BODY_KEY", "PATH_KEY"]
        );
    }

    #[test]
    fn malformed_secret_references_do_not_echo_definition_contents() {
        use super::super::types::{BudgetPolicy, Node};

        let sensitive_malformed_name = "bad-private-value";
        let def = WorkflowDefinition {
            id: Uuid::nil(),
            version: 1,
            name: "bad refs".into(),
            nodes: vec![Node {
                id: "http".into(),
                kind: NodeKind::Http {
                    method: "POST".into(),
                    url: "https://api.example.com".into(),
                    headers: vec![(
                        "authorization".into(),
                        format!("{{{{secrets.{sensitive_malformed_name}}}}}"),
                    )],
                    body: Some("{{secrets.UNCLOSED".into()),
                    max_response_bytes: None,
                },
            }],
            edges: vec![],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec!["api.example.com".into()],
            metadata: serde_json::Value::Null,
            triggers: vec![],
        };

        let errors = required_secret_names(&def).unwrap_err();
        assert_eq!(errors.len(), 2);
        assert!(errors
            .iter()
            .all(|error| !error.contains(sensitive_malformed_name)));
        assert!(errors
            .iter()
            .any(|error| error.contains("invalid secret reference")));
        assert!(errors.iter().any(|error| error.contains("unclosed")));
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
