//! Postgres-backed [`AuditWriter`].
//!
//! The chain is per-org; each append must atomically read the most recent
//! `(hash, seq)` for the org and insert a row whose `prev_hash` matches.
//! Concurrent writes for the same org could otherwise race and produce two
//! entries with the same `prev_hash`, breaking [`super::verify_chain`].
//!
//! Concurrency strategy: a SERIALIZABLE transaction with an explicit
//! `SELECT ... FOR UPDATE` against the last row. The `FOR UPDATE` lock
//! holds across the INSERT so a parallel append on the same org blocks
//! until we commit. Other orgs are unaffected.

use async_trait::async_trait;
use chrono::{DateTime, Timelike, Utc};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use sqlx::PgPool;
use uuid::Uuid;

use super::{compute_hash, Actor, AuditEntry, AuditError, AuditWriter, PayloadFields};

/// Normalize a wall-clock timestamp to PostgreSQL's microsecond precision
/// before it enters the signed payload. PostgreSQL discards sub-microsecond
/// digits on insert; hashing the higher-precision value would make a freshly
/// persisted entry fail verification after it is read back.
fn postgres_timestamp(now: DateTime<Utc>) -> DateTime<Utc> {
    let nanoseconds = now.nanosecond();
    now.with_nanosecond(nanoseconds - (nanoseconds % 1_000))
        .expect("microsecond-normalized nanoseconds are always in range")
}

/// Postgres-backed audit writer.
///
/// Cheap to clone — the inner [`PgPool`] is reference-counted by sqlx, and
/// the [`SigningKey`] is `Clone`-able (it's a wrapper around 32 bytes of
/// secret material).
pub struct PostgresAuditWriter {
    pool: PgPool,
    signing_key: SigningKey,
}

impl std::fmt::Debug for PostgresAuditWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresAuditWriter")
            .field("pool", &"PgPool { .. }")
            .field("signing_key", &"<redacted>")
            .finish()
    }
}

impl PostgresAuditWriter {
    /// Construct with a supplied signing key. Production callers should
    /// load the key from a secret store (e.g. Fly secret
    /// `TT_AUDIT_SIGNING_KEY`) so chains stay verifiable across restarts.
    pub fn new(pool: PgPool, signing_key: SigningKey) -> Self {
        Self { pool, signing_key }
    }

    /// Public verification key for [`super::verify_chain`].
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }
}

#[async_trait]
impl AuditWriter for PostgresAuditWriter {
    async fn write(
        &self,
        org_id: Uuid,
        actor: Actor,
        event: String,
        payload: serde_json::Value,
    ) -> Result<AuditEntry, AuditError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AuditError::Storage(e.to_string()))?;

        // Lock the most recent row for this org so concurrent appends
        // block until we commit. Without FOR UPDATE two writers could
        // observe the same prev_hash and break the chain.
        let last: Option<(String, i64)> = sqlx::query_as(
            "SELECT hash, seq FROM audit_entries \
             WHERE org_id = $1 \
             ORDER BY seq DESC \
             LIMIT 1 \
             FOR UPDATE",
        )
        .bind(org_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| AuditError::Storage(e.to_string()))?;

        let (prev_hash_str, prev_hash_bytes, next_seq): (String, [u8; 32], i64) = match last {
            Some((hash, seq)) => {
                let decoded = hex::decode(&hash).map_err(|e| AuditError::Storage(e.to_string()))?;
                if decoded.len() != 32 {
                    return Err(AuditError::Storage(format!(
                        "stored hash decoded to {} bytes, expected 32",
                        decoded.len()
                    )));
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&decoded);
                (hash, arr, seq + 1)
            }
            None => {
                let zeroes = [0u8; 32];
                (hex::encode(zeroes), zeroes, 0)
            }
        };

        let id = Uuid::new_v4();
        let timestamp = postgres_timestamp(Utc::now());

        let fields = PayloadFields {
            id,
            org_id,
            timestamp,
            actor: &actor,
            event: &event,
            payload: &payload,
            seq: next_seq,
        };

        let hash = compute_hash(&prev_hash_bytes, &fields)?;
        let hash_hex = hash.to_hex().to_string();

        let signature = self
            .signing_key
            .try_sign(hash.as_bytes())
            .map_err(|e| AuditError::Signing(e.to_string()))?;
        let signature_hex = hex::encode(signature.to_bytes());

        let actor_json = serde_json::to_value(&actor)?;

        sqlx::query(
            "INSERT INTO audit_entries \
             (id, org_id, ts, actor, event, payload, prev_hash, hash, signature, seq) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(id)
        .bind(org_id)
        .bind(timestamp)
        .bind(actor_json)
        .bind(&event)
        .bind(&payload)
        .bind(&prev_hash_str)
        .bind(&hash_hex)
        .bind(&signature_hex)
        .bind(next_seq)
        .execute(&mut *tx)
        .await
        .map_err(|e| AuditError::Storage(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| AuditError::Storage(e.to_string()))?;

        // Emit the new chain tip on a dedicated target so operators can route it
        // to an append-only, off-box sink. This out-of-band anchor is what makes
        // tail-truncation / whole-chain deletion detectable later via
        // `tt audit verify --expected-tip` (the DB alone cannot reveal it).
        // Per-write is fine: audit writes happen only on privileged actions.
        tracing::info!(
            target: "tt::audit::tip",
            org_id = %org_id,
            seq = next_seq,
            tip_hash = %hash_hex,
            ts = %timestamp.to_rfc3339(),
            "audit chain tip advanced"
        );

        Ok(AuditEntry {
            id,
            org_id,
            seq: next_seq,
            timestamp,
            actor,
            event,
            payload,
            prev_hash: prev_hash_str,
            hash: hash_hex,
            signature: signature_hex,
        })
    }

    async fn list(&self, org_id: Uuid) -> Result<Vec<AuditEntry>, AuditError> {
        let rows: Vec<AuditRow> = sqlx::query_as::<_, AuditRow>(
            "SELECT id, org_id, ts, actor, event, payload, prev_hash, hash, signature, seq \
             FROM audit_entries \
             WHERE org_id = $1 \
             ORDER BY seq ASC",
        )
        .bind(org_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AuditError::Storage(e.to_string()))?;

        rows.into_iter().map(AuditRow::into_entry).collect()
    }
}

#[derive(sqlx::FromRow)]
struct AuditRow {
    id: Uuid,
    org_id: Uuid,
    ts: chrono::DateTime<chrono::Utc>,
    actor: sqlx::types::Json<serde_json::Value>,
    event: String,
    payload: sqlx::types::Json<serde_json::Value>,
    prev_hash: String,
    hash: String,
    signature: String,
    seq: i64,
}

impl AuditRow {
    fn into_entry(self) -> Result<AuditEntry, AuditError> {
        let actor: Actor = serde_json::from_value(self.actor.0)?;
        Ok(AuditEntry {
            id: self.id,
            org_id: self.org_id,
            seq: self.seq,
            timestamp: self.ts,
            actor,
            event: self.event,
            payload: self.payload.0,
            prev_hash: self.prev_hash,
            hash: self.hash,
            signature: self.signature,
        })
    }
}

/// Parse a 32-byte Ed25519 signing key from a hex string. Used by tt-api's
/// `main.rs` to load the key from `TT_AUDIT_SIGNING_KEY` at boot.
pub fn signing_key_from_hex(hex_str: &str) -> Result<SigningKey, String> {
    let bytes = hex::decode(hex_str.trim()).map_err(|e| format!("invalid hex: {e}"))?;
    if bytes.len() != 32 {
        return Err(format!(
            "expected 32 bytes (64 hex chars), got {} bytes",
            bytes.len()
        ));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&arr))
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Timelike};

    use super::*;

    #[test]
    fn signed_timestamp_is_stable_after_postgres_round_trip() {
        let higher_precision = DateTime::parse_from_rfc3339("2026-08-02T12:34:56.123456789Z")
            .expect("fixed timestamp")
            .to_utc();

        let normalized = postgres_timestamp(higher_precision);
        assert_eq!(normalized.nanosecond(), 123_456_000);
        assert_eq!(postgres_timestamp(normalized), normalized);
    }

    #[test]
    fn signing_key_from_hex_rejects_short() {
        assert!(signing_key_from_hex("abcd").is_err());
    }

    #[test]
    fn signing_key_from_hex_rejects_non_hex() {
        assert!(signing_key_from_hex("ZZZZ").is_err());
    }

    #[test]
    fn signing_key_from_hex_round_trips() {
        let bytes = [7u8; 32];
        let hex_str = hex::encode(bytes);
        let key = signing_key_from_hex(&hex_str).unwrap();
        assert_eq!(key.to_bytes(), bytes);
    }
}
