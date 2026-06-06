//! Audit log entries — hash-chained + Ed25519-signed.
//!
//! Schema: each entry hashes `prev_hash_bytes || canonical_json(payload)` with
//! BLAKE3, then signs the 32-byte hash with an Ed25519 key. The genesis entry
//! uses 32 zero bytes as `prev_hash`.
//!
//! ## Public surface
//!
//! - [`AuditEntry`] / [`Actor`] — wire types (stable shape; do not change without migration).
//! - [`AuditWriter`] — async trait implemented by storage backends.
//! - [`InMemoryAuditWriter`] — in-process writer for tests and CLI demos.
//! - [`verify_chain`] — standalone verifier; used by `tt audit verify`.
//! - [`AuditError`] / [`VerifyError`] — error enums.

#[cfg(feature = "postgres")]
pub mod postgres;
pub mod writer;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

pub use writer::{AuditWriter, InMemoryAuditWriter};

// ─── Schema types ────────────────────────────────────────────────────────────

/// A single entry in the hash-chained audit log.
///
/// Fields are stable: the hash chain covers a canonical serialization of a
/// subset of them (see [`canonical_payload_bytes`]). Do not reorder or rename
/// without updating the chain computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unique entry identifier (UUIDv4).
    pub id: Uuid,
    /// Organization this entry belongs to.
    pub org_id: Uuid,
    /// Monotonic per-org sequence number (0-based, gap-free).
    pub seq: i64,
    /// Wall-clock time the entry was written (UTC, RFC 3339).
    pub timestamp: DateTime<Utc>,
    /// Actor that caused the event.
    pub actor: Actor,
    /// Free-form event name (e.g. `"api_key.created"`).
    pub event: String,
    /// Arbitrary structured payload; covered by the hash chain.
    pub payload: serde_json::Value,
    /// Hex-encoded BLAKE3 hash of the previous entry (64 × `0` for genesis).
    pub prev_hash: String,
    /// Hex-encoded BLAKE3 hash of `prev_hash_bytes || canonical_json(payload_to_sign)`.
    pub hash: String,
    /// Hex-encoded Ed25519 signature over the 32-byte raw hash.
    pub signature: String,
}

/// Who caused an audit event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Actor {
    /// A human user identified by UUID.
    User { user_id: Uuid },
    /// An API key identified by UUID.
    ApiKey { key_id: Uuid },
    /// An internal system process.
    System,
    /// An unauthenticated caller.
    Anonymous,
}

// ─── Error types ─────────────────────────────────────────────────────────────

/// Errors returned by [`AuditWriter`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// JSON serialization failed.
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
    /// Storage layer error (e.g. Postgres, S3).
    #[error("storage: {0}")]
    Storage(String),
    /// Ed25519 signing failed.
    #[error("signing: {0}")]
    Signing(String),
}

/// Errors returned by [`verify_chain`].
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// An entry's `prev_hash` does not match the previous entry's `hash`.
    #[error("entry {index} has wrong prev_hash (expected {expected}, got {got})")]
    BrokenChain {
        /// Zero-based index of the offending entry.
        index: usize,
        /// Hash value that was expected.
        expected: String,
        /// Hash value that was actually stored.
        got: String,
    },
    /// An entry's stored `hash` does not match the recomputed hash.
    #[error("entry {index} hash mismatch")]
    HashMismatch {
        /// Zero-based index of the offending entry.
        index: usize,
    },
    /// An entry's `seq` is not the expected gap-free successor (0 for genesis,
    /// `prev.seq + 1` otherwise) — a gap signals a deleted/reordered entry.
    #[error("entry {index} has wrong seq (expected {expected}, got {got})")]
    SeqGap {
        /// Zero-based index of the offending entry.
        index: usize,
        /// The seq value that was expected.
        expected: i64,
        /// The seq value actually stored.
        got: i64,
    },
    /// An entry's Ed25519 signature is invalid.
    #[error("entry {0} signature invalid: {1}")]
    BadSignature(usize, String),
    /// Hex decoding failed for a stored hash or signature.
    #[error("hex decode failed at entry {0}: {1}")]
    Hex(usize, String),
    /// The chain does not reach the expected anchor tip — its last entry's
    /// seq/hash differs, or it is shorter than `anchor.seq + 1` entries
    /// (tail-truncation or whole-chain deletion).
    #[error("chain does not reach anchor tip: expected len {expected_len}, got {got_len}")]
    TruncatedChain {
        /// Entries the anchor implies (`anchor.seq + 1`).
        expected_len: i64,
        /// Entries actually present.
        got_len: i64,
    },
}

// ─── Canonical serialization helpers ─────────────────────────────────────────

/// Recursively sort all JSON object keys so serialization is deterministic.
///
/// Arrays are left in-order (order is meaningful for arrays). Objects at every
/// level of nesting are converted to a [`BTreeMap`]-backed [`Value::Object`].
pub fn canonical_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .into_iter()
                .map(|(k, v)| (k, canonical_json(v)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(canonical_json).collect()),
        other => other,
    }
}

/// Serialize `value` to canonical JSON bytes (objects sorted by key).
pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    let sorted = canonical_json(value.clone());
    serde_json::to_vec(&sorted)
}

/// Build the JSON object that is covered by the hash chain for `entry`.
///
/// Keys are a fixed set; the output is then passed through [`canonical_bytes`]
/// before being hashed.
pub fn canonical_payload_bytes(
    entry_fields: &PayloadFields<'_>,
) -> Result<Vec<u8>, serde_json::Error> {
    let obj = serde_json::json!({
        "id": entry_fields.id.to_string(),
        "org_id": entry_fields.org_id.to_string(),
        "timestamp": entry_fields.timestamp.to_rfc3339(),
        "actor": entry_fields.actor,
        "event": entry_fields.event,
        "payload": entry_fields.payload,
        "seq": entry_fields.seq,
    });
    canonical_bytes(&obj)
}

/// Fields used to build the canonical payload for hashing.
///
/// Kept as a plain struct so callers (writer + verifier) share the same logic.
pub struct PayloadFields<'a> {
    /// Entry UUID.
    pub id: Uuid,
    /// Organization UUID.
    pub org_id: Uuid,
    /// Entry timestamp.
    pub timestamp: DateTime<Utc>,
    /// Actor reference.
    pub actor: &'a Actor,
    /// Event name.
    pub event: &'a str,
    /// Arbitrary payload.
    pub payload: &'a serde_json::Value,
    /// Monotonic per-org sequence number (0-based). Bound into the hash so an
    /// entry's position cannot be changed without breaking its signature.
    pub seq: i64,
}

/// Compute the BLAKE3 hash for an entry.
///
/// Covers `prev_hash_bytes (32) || canonical_payload_bytes`.
pub fn compute_hash(
    prev_hash_bytes: &[u8; 32],
    fields: &PayloadFields<'_>,
) -> Result<blake3::Hash, AuditError> {
    let payload_bytes = canonical_payload_bytes(fields)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(prev_hash_bytes.as_ref());
    hasher.update(&payload_bytes);
    Ok(hasher.finalize())
}

// ─── verify_chain ─────────────────────────────────────────────────────────────

/// Verify the integrity of a hash chain against `verifying_key`.
///
/// For each entry this checks:
/// 1. `prev_hash` matches the previous entry's `hash` (or all-zeros for genesis).
/// 2. The stored `hash` matches the recomputed BLAKE3 hash.
/// 3. The Ed25519 `signature` is valid over the raw 32-byte hash.
///
/// Returns `Ok(())` if and only if all entries pass all three checks.
pub fn verify_chain(
    entries: &[AuditEntry],
    verifying_key: &ed25519_dalek::VerifyingKey,
) -> Result<(), VerifyError> {
    use ed25519_dalek::Verifier;

    let genesis_prev = "0".repeat(64);

    for (i, entry) in entries.iter().enumerate() {
        // ── 0. seq must be gap-free and monotonic ─────────────────────────────
        let expected_seq = if i == 0 { 0 } else { entries[i - 1].seq + 1 };
        if entry.seq != expected_seq {
            return Err(VerifyError::SeqGap {
                index: i,
                expected: expected_seq,
                got: entry.seq,
            });
        }

        // ── 1. prev_hash linkage ──────────────────────────────────────────────
        let expected_prev = if i == 0 {
            genesis_prev.clone()
        } else {
            entries[i - 1].hash.clone()
        };

        if entry.prev_hash != expected_prev {
            return Err(VerifyError::BrokenChain {
                index: i,
                expected: expected_prev,
                got: entry.prev_hash.clone(),
            });
        }

        // ── 2. Hash recomputation ─────────────────────────────────────────────
        let prev_hex =
            hex::decode(&entry.prev_hash).map_err(|e| VerifyError::Hex(i, e.to_string()))?;
        let mut prev_bytes = [0u8; 32];
        if prev_hex.len() != 32 {
            return Err(VerifyError::Hex(
                i,
                format!("prev_hash decoded to {} bytes, expected 32", prev_hex.len()),
            ));
        }
        prev_bytes.copy_from_slice(&prev_hex);

        let fields = PayloadFields {
            id: entry.id,
            org_id: entry.org_id,
            timestamp: entry.timestamp,
            actor: &entry.actor,
            event: &entry.event,
            payload: &entry.payload,
            seq: entry.seq,
        };

        let computed = compute_hash(&prev_bytes, &fields)
            .map_err(|_| VerifyError::HashMismatch { index: i })?;
        let computed_hex = computed.to_hex().to_string();

        if computed_hex != entry.hash {
            return Err(VerifyError::HashMismatch { index: i });
        }

        // ── 3. Signature check ────────────────────────────────────────────────
        let sig_bytes =
            hex::decode(&entry.signature).map_err(|e| VerifyError::Hex(i, e.to_string()))?;
        let sig_array: [u8; 64] = sig_bytes
            .try_into()
            .map_err(|_| VerifyError::BadSignature(i, "signature is not 64 bytes".to_string()))?;
        let signature = ed25519_dalek::Signature::from_bytes(&sig_array);

        verifying_key
            .verify(computed.as_bytes(), &signature)
            .map_err(|e| VerifyError::BadSignature(i, e.to_string()))?;
    }

    Ok(())
}

/// A captured "tip" of an audit chain — the last entry's `seq` + `hash`.
///
/// Capture one after a write (via [`TipAnchor::from_entry`]), anchor it where an
/// attacker with DB write access cannot also roll it back (an append-only / WORM
/// sink — see the deferred `post-scale-s3-object-lock` work), and later pass it
/// to [`verify_chain_with_anchor`] to detect tail-truncation / whole-chain
/// deletion (which plain [`verify_chain`] cannot see).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipAnchor {
    /// `seq` of the expected last entry.
    pub seq: i64,
    /// Hex-encoded BLAKE3 `hash` of the expected last entry.
    pub hash: String,
}

impl TipAnchor {
    /// Capture the current tip from the chain's last entry.
    #[must_use]
    pub fn from_entry(entry: &AuditEntry) -> Self {
        Self {
            seq: entry.seq,
            hash: entry.hash.clone(),
        }
    }
}

/// Verify a chain AND assert it still reaches `anchor`'s tip with no missing
/// tail. Runs [`verify_chain`] first, then checks the last entry's `(seq, hash)`
/// equals the anchor and that exactly `anchor.seq + 1` entries are present.
///
/// This is what makes tail-truncation and whole-chain deletion detectable, given
/// a trustworthy `anchor` captured earlier. An empty/short chain, or a tip that
/// does not match, returns [`VerifyError::TruncatedChain`].
pub fn verify_chain_with_anchor(
    entries: &[AuditEntry],
    verifying_key: &ed25519_dalek::VerifyingKey,
    anchor: &TipAnchor,
) -> Result<(), VerifyError> {
    let expected_len = anchor.seq + 1;
    let got_len = entries.len() as i64;
    let tip_ok = entries
        .last()
        .is_some_and(|last| last.seq == anchor.seq && last.hash == anchor.hash);
    if !tip_ok || got_len != expected_len {
        return Err(VerifyError::TruncatedChain {
            expected_len,
            got_len,
        });
    }
    verify_chain(entries, verifying_key)
}
