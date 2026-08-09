//! Customer co-signed audit checkpoint
//! (`tokentrimmer.customer-audit-checkpoint.v1`).
//!
//! An operator who wants an OUT-OF-BAND, customer-controlled anchor of a
//! completed TokenTrimmer audit chain has this artifact created: a token that
//! the customer signs with THEIR OWN Ed25519 key, binding the exact
//! organization, the TokenTrimmer audit verifying key, the monotonic sequence,
//! the lowercase BLAKE3 tip hash, a whole-second UTC time, and a SHA-256
//! identity for the customer's own public key.
//!
//! ## Power boundary
//!
//! A verified checkpoint proves TOKEN TRIMMER's chain is intact up to a tip the
//! CUSTOMER signed — it is not third-party timestamping, transparency-log
//! publication, or authority over what the payload means. The customer public
//! key is deliberately absent from the artifact and must arrive out of band;
//! the token never contains a Decrypted seed or any sensitive audit values.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{canonical_bytes, canonical_json};

pub const CHECKPOINT_SCHEMA: &str = "tokentrimmer.customer-audit-checkpoint.v1";

/// Domain-separates the checkpoint signature from any other Ed25519 use of the
/// same customer key (e.g. the same key signing some unrelated file).
const DOMAIN_PREFIX: &[u8] = b"tokentrimmer.customer-audit-checkpoint.v1\n";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CheckpointError {
    #[error("checkpoint schema must be {CHECKPOINT_SCHEMA}")]
    WrongSchema,
    #[error("checkpoint {field} must be canonical lower-case hex (64 chars)")]
    BadCanonicalHex { field: &'static str },
    #[error("checkpoint sequence must be a non-negative integer")]
    BadSequence,
    #[error("checkpoint timestamp must be a whole-second UTC instant (YYYY-MM-DDTHH:MM:SSZ)")]
    BadTimestamp,
    #[error("customer key identity mismatch: {expected} != {actual}")]
    KeyIdentityMismatch { expected: String, actual: String },
    #[error("checkpoint signature verification failed")]
    BadSignature,
    #[error("checkpoint must contain exactly the schema, six bound fields, and a signature")]
    MalformedShape,
    #[error("checkpoint serialization failed")]
    Serialization,
}

/// The six signable fields a customer's key binds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPayload {
    /// One canonical organization UUID (lowercase).
    pub org: String,
    /// Canonical 64-lowercase-hex TokenTrimmer Ed25519 audit verifying key.
    pub verifying_key_hex: String,
    /// Non-negative monotonic chain sequence of the signed tip.
    pub sequence: i64,
    /// Canonical lowercase BLAKE3 tip hash (64 hex) of the signed tip.
    pub tip_hash: String,
    /// Whole-second UTC instant when the checkpoint was created.
    pub timestamp: String,
    /// SHA-256 (hex) identity of the customer's own Ed25519 public key.
    pub customer_key_identity: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointArtifact {
    schema: String,
    #[serde(flatten)]
    payload: CheckpointPayload,
    signature: String,
}

fn is_canonical_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|b| b.is_ascii_hexdigit()) && value == value.to_lowercase()
}

/// SHA-256 identity of an Ed25519 public key (hex), as bound by the payload.
pub fn customer_key_identity(verifying_key: &VerifyingKey) -> String {
    hex::encode(Sha256::digest(verifying_key.to_bytes()))
}

/// Canonical, domain-separated bytes the customer signs.
pub fn signable_bytes(payload: &CheckpointPayload) -> Result<Vec<u8>, CheckpointError> {
    let value = serde_json::to_value(payload).map_err(|_| CheckpointError::Serialization)?;
    let canonical = canonical_json(value);
    let body = canonical_bytes(&canonical).map_err(|_| CheckpointError::Serialization)?;
    let mut bytes = Vec::with_capacity(DOMAIN_PREFIX.len() + body.len());
    bytes.extend_from_slice(DOMAIN_PREFIX);
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

/// Build a checkpoint artifact signed by the customer's private key.
pub fn build_checkpoint(
    payload: &CheckpointPayload,
    signing_key: &SigningKey,
) -> Result<serde_json::Value, CheckpointError> {
    validate_payload(payload)?;
    if customer_key_identity(&signing_key.verifying_key()) != payload.customer_key_identity {
        return Err(CheckpointError::KeyIdentityMismatch {
            expected: payload.customer_key_identity.clone(),
            actual: customer_key_identity(&signing_key.verifying_key()),
        });
    }
    let signature = signing_key.sign(&signable_bytes(payload)?).to_bytes();
    let artifact = CheckpointArtifact {
        schema: CHECKPOINT_SCHEMA.to_string(),
        payload: payload.clone(),
        signature: hex::encode(signature),
    };
    serde_json::to_value(artifact).map_err(|_| CheckpointError::Serialization)
}

fn validate_payload(payload: &CheckpointPayload) -> Result<(), CheckpointError> {
    if payload.sequence < 0 {
        return Err(CheckpointError::BadSequence);
    }
    if !is_canonical_hex(&payload.verifying_key_hex, 64) {
        return Err(CheckpointError::BadCanonicalHex { field: "verifying_key_hex" });
    }
    if !is_canonical_hex(&payload.tip_hash, 64) {
        return Err(CheckpointError::BadCanonicalHex { field: "tip_hash" });
    }
    if !is_canonical_hex(&payload.customer_key_identity, 64) {
        return Err(CheckpointError::BadCanonicalHex { field: "customer_key_identity" });
    }
    // Whole-second UTC instant only: exactly `YYYY-MM-DDTHH:MM:SSZ` with
    // canonical zero-padding (no sub-second, no offset).
    if !whole_second_utc(&payload.timestamp) {
        return Err(CheckpointError::BadTimestamp);
    }
    Ok(())
}

fn whole_second_utc(value: &str) -> bool {
    // Strict shape `YYYY-MM-DDTHH:MM:SSZ` with zero sub-second/offset.
    let Ok(then) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ") else {
        return false;
    };
    // Round-trip must be byte-identical (canonical zero-padding).
    then.format("%Y-%m-%dT%H:%M:%SZ").to_string() == value
}

/// Strictly verify a checkpoint artifact under the customer's public key and
/// return the bound payload. Unknown fields, a wrong schema, a wrong customer
/// key, or a tampered payload all fail closed.
pub fn verify_checkpoint(
    value: &serde_json::Value,
    customer_verifying_key: &VerifyingKey,
) -> Result<CheckpointPayload, CheckpointError> {
    let artifact: CheckpointArtifact =
        serde_json::from_value(value.clone()).map_err(|_| CheckpointError::MalformedShape)?;
    if artifact.schema != CHECKPOINT_SCHEMA {
        return Err(CheckpointError::WrongSchema);
    }
    validate_payload(&artifact.payload)?;
    let signature_bytes =
        hex::decode(&artifact.signature).map_err(|_| CheckpointError::BadSignature)?;
    let signature_array: &[u8; 64] = signature_bytes
        .as_slice()
        .try_into()
        .map_err(|_| CheckpointError::BadSignature)?;
    let signature = ed25519_dalek::Signature::from_bytes(signature_array);
    customer_verifying_key
        .verify(&signable_bytes(&artifact.payload)?, &signature)
        .map_err(|_| CheckpointError::BadSignature)?;
    let expected_identity = customer_key_identity(customer_verifying_key);
    if artifact.payload.customer_key_identity != expected_identity {
        return Err(CheckpointError::KeyIdentityMismatch {
            expected: expected_identity,
            actual: artifact.payload.customer_key_identity.clone(),
        });
    }
    Ok(artifact.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn sample_payload(identity: &str) -> CheckpointPayload {
        CheckpointPayload {
            org: "11111111-1111-1111-1111-111111111111".to_string(),
            verifying_key_hex: "a".repeat(64),
            sequence: 41,
            tip_hash: "b".repeat(64),
            timestamp: "2026-07-28T12:00:00Z".to_string(),
            customer_key_identity: identity.to_string(),
        }
    }

    #[test]
    fn roundtrip_build_verify() {
        let customer = SigningKey::from_bytes(&[7u8; 32]);
        let identity = customer_key_identity(&customer.verifying_key());
        let artifact = build_checkpoint(&sample_payload(&identity), &customer).unwrap();
        let payload = verify_checkpoint(&artifact, &customer.verifying_key()).unwrap();
        assert_eq!(payload.sequence, 41);
        assert_eq!(payload.org, "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn wrong_customer_key_and_tamper_fail_closed() {
        let customer = SigningKey::from_bytes(&[7u8; 32]);
        let identity = customer_key_identity(&customer.verifying_key());
        let artifact = build_checkpoint(&sample_payload(&identity), &customer).unwrap();

        let wrong = SigningKey::from_bytes(&[9u8; 32]);
        assert_eq!(
            verify_checkpoint(&artifact, &wrong.verifying_key()),
            Err(CheckpointError::BadSignature)
        );

        let mut tampered = artifact.clone();
        tampered["tip_hash"] = serde_json::json!("c".repeat(64));
        assert_eq!(
            verify_checkpoint(&tampered, &customer.verifying_key()),
            Err(CheckpointError::BadSignature)
        );
    }

    #[test]
    fn rejects_unknown_fields_and_bad_shapes() {
        let customer = SigningKey::from_bytes(&[7u8; 32]);
        let identity = customer_key_identity(&customer.verifying_key());
        let artifact = build_checkpoint(&sample_payload(&identity), &customer).unwrap();
        let mut extra = artifact.clone();
        extra["sneak"] = serde_json::json!(1);
        assert_eq!(
            verify_checkpoint(&extra, &customer.verifying_key()),
            Err(CheckpointError::MalformedShape)
        );

        let bad_seq = sample_payload(&identity);
        let bad_seq = CheckpointPayload { sequence: -1, ..bad_seq };
        assert_eq!(build_checkpoint(&bad_seq, &customer), Err(CheckpointError::BadSequence));
    }
}
