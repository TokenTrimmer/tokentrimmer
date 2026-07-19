//! The agent-run (ARR) receipt — a signed, offline-verifiable proof of a
//! top-level agent run's catalog-priced cost + savings estimate.
//!
//! The sign side lives in the cloud mint endpoint
//! (`POST /v1/admin/agent-runs/{run_id}/receipt/sign`, which calls
//! `cloud::api::agent_receipt::crypto::sign_receipt`). This module is the
//! public **verify** home shared by `tt verify-receipt` and other offline
//! consumers. It mirrors `wfr_receipt.rs`: a canonical ASCII string with a
//! disjoint domain-separation prefix (`arr:v1|`), signed directly (no chain),
//! with the signature and verifying key embedded in the share response.
//!
//! An agent run is top-level, not a workflow child. Its canonical payload has
//! no `workflow_id`; that distinction is part of both the wire contract and
//! the signed bytes.
//!
//! # Canonical payload
//!
//! `arr:v1|<org_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<status>`
//!
//! Money values are already integer micro-USD (`i64`), so verification never
//! round-trips convenience floats.
//!
//! # Domain separation
//!
//! `arr:v1|` is disjoint from `vcr:v1|` (compressions), `l2:v1|`
//! (semantic-cache hits), `wfr:v1|` / `wfr:v2|` (workflow runs), `att:`
//! (attestations), `pdf:v1|` (PDF reports), and the bare-32B audit hash. A
//! signature from one family can never validate as another.

use uuid::Uuid;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// The domain-separation prefix for agent-run receipts.
pub const ARR_PREFIX: &str = "arr:";

/// The only currently supported agent-run canonical payload version.
pub const CANONICAL_VERSION_V1: &str = "v1";

/// A signed top-level agent-run receipt as deserialized from the cloud
/// `VerifyReceiptResponse` JSON shape. The raw micro-USD fields are the
/// canonical payload inputs; convenience `*_usd` values and `signed_at` are
/// intentionally not signed.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct AgentRunReceipt {
    /// Agent run UUID (also the receipt primary key).
    pub run_id: Uuid,
    /// Organization UUID — canonical payload field 2.
    pub org_id: Uuid,
    /// Terminal agent-run status — canonical payload field 7.
    pub status: String,
    /// Recorded run cost in integer micro-USD — canonical payload field 4.
    pub cost_micros: i64,
    /// Catalog-priced baseline estimate in integer micro-USD — field 5.
    pub baseline_micros: i64,
    /// Net savings estimate in integer micro-USD — field 6.
    pub saved_micros: i64,
    /// Convenience display value only; not part of the signature.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Convenience display value only; not part of the signature.
    #[serde(default)]
    pub baseline_usd: Option<f64>,
    /// Convenience display value only; not part of the signature.
    #[serde(default)]
    pub saved_usd: Option<f64>,
    /// Hex-encoded 64-byte Ed25519 signature over [`canonical_payload`].
    pub signature_hex: String,
    /// Hex-encoded 32-byte Ed25519 public key. Trust this key out of band.
    pub verifying_key_hex: String,
    /// Canonical schema version; currently only `"v1"`.
    pub canonical_version: String,
    /// RFC3339 timestamp when the receipt was first minted; not signed.
    #[serde(default)]
    pub signed_at: Option<String>,
}

/// Build the canonical signed payload for an agent-run receipt.
///
/// Uses the same field order and bytes as
/// `cloud/crates/api/src/agent_receipt/crypto.rs::canonical_payload_arr` for
/// every structurally valid cloud-minted receipt:
/// `arr:v1|<org_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<status>`.
///
/// A pipe in a string field would alter the unambiguous field boundary and is
/// rejected. Unknown versions are rejected rather than silently treated as v1.
/// The public share response is minted only for a terminal run, so an empty
/// status is structurally invalid here. The verifier deliberately does not
/// hard-code today's terminal-status enum: any nonempty, separator-safe future
/// status remains verifiable if that issuer-side allowlist evolves.
pub fn canonical_payload(receipt: &AgentRunReceipt) -> Result<String, AgentRunReceiptError> {
    if receipt.status.is_empty() {
        return Err(AgentRunReceiptError::EmptyStatus);
    }
    if receipt.run_id.to_string().contains('|') || receipt.status.contains('|') {
        return Err(AgentRunReceiptError::PipeInField);
    }
    if receipt.canonical_version != CANONICAL_VERSION_V1 {
        return Err(AgentRunReceiptError::UnknownVersion(
            receipt.canonical_version.clone(),
        ));
    }

    Ok(format!(
        "{ARR_PREFIX}v1|{}|{}|{}|{}|{}|{}",
        receipt.org_id,
        receipt.run_id,
        receipt.cost_micros,
        receipt.baseline_micros,
        receipt.saved_micros,
        receipt.status,
    ))
}

/// Errors building or verifying an agent-run receipt payload.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentRunReceiptError {
    /// The terminal status was empty, outside the public receipt wire contract.
    EmptyStatus,
    /// A string field contained the `|` payload-field separator.
    PipeInField,
    /// The payload version is not supported by this verifier.
    UnknownVersion(String),
}

impl std::fmt::Display for AgentRunReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyStatus => f.write_str("status must be nonempty"),
            Self::PipeInField => f.write_str(
                "field contained pipe character '|' which is the payload field separator",
            ),
            Self::UnknownVersion(version) => write!(
                f,
                "unknown canonical_version \"{version}\" (expected \"{CANONICAL_VERSION_V1}\")"
            ),
        }
    }
}

impl std::error::Error for AgentRunReceiptError {}

/// Verify an agent-run receipt offline against an external, trusted
/// verifying-key hex string. The embedded key is a convenience value; callers
/// should supply a pinned or otherwise out-of-band trusted key when issuer
/// identity matters.
#[must_use]
pub fn verify_with_key(verifying_key_hex_: &str, receipt: &AgentRunReceipt) -> bool {
    let Ok(payload) = canonical_payload(receipt) else {
        return false;
    };
    let Ok(vk_bytes) = hex::decode(verifying_key_hex_) else {
        return false;
    };
    let Ok(vk_array): Result<[u8; 32], _> = vk_bytes.try_into() else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&vk_array) else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(&receipt.signature_hex) else {
        return false;
    };
    let Ok(sig_array): Result<[u8; 64], _> = sig_bytes.try_into() else {
        return false;
    };
    let signature = Signature::from_bytes(&sig_array);
    verifying_key.verify(payload.as_bytes(), &signature).is_ok()
}

#[cfg(test)]
mod tests {
    //! Canonical-payload + verify drift gates for the ARR receipt family.
    //!
    //! The canonical string is sourced from
    //! `cloud/crates/api/src/agent_receipt/crypto.rs::canonical_payload_arr`.
    //! If either side drifts, a cloud-minted receipt will stop verifying in
    //! `tt verify-receipt`.

    use std::collections::BTreeSet;

    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::Value;

    use super::*;

    const ARR_STRUCTURAL_SCHEMA: &str =
        include_str!("../../../docs/receipt-spec/arr-receipt.schema.json");
    const ARR_V1_GOLDEN: &str = include_str!("../../../docs/receipt-spec/arr-v1.golden.json");
    const ARR_V1_GOLDEN_PAYLOAD: &str =
        "arr:v1|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000a1|70000|180000|110000|completed";

    fn sample_v1() -> AgentRunReceipt {
        AgentRunReceipt {
            run_id: Uuid::parse_str("00000000-0000-0000-0000-0000000000a1").unwrap(),
            org_id: Uuid::parse_str("00000000-0000-0000-0000-00000000002a").unwrap(),
            status: "completed".to_string(),
            cost_micros: 70_000,
            baseline_micros: 180_000,
            saved_micros: 110_000,
            cost_usd: None,
            baseline_usd: None,
            saved_usd: None,
            signature_hex: String::new(),
            verifying_key_hex: String::new(),
            canonical_version: CANONICAL_VERSION_V1.to_string(),
            signed_at: None,
        }
    }

    #[test]
    fn structural_schema_covers_the_current_arr_wire_contract() {
        let schema: Value =
            serde_json::from_str(ARR_STRUCTURAL_SCHEMA).expect("ARR schema must be valid JSON");
        assert_eq!(
            schema["$id"],
            "urn:tokentrimmer:receipt:arr:structural-schema:v1"
        );
        assert_eq!(schema["additionalProperties"], Value::Bool(true));
        assert_eq!(
            schema["properties"]["canonical_version"]["enum"],
            serde_json::json!([CANONICAL_VERSION_V1])
        );
        assert_eq!(
            schema["properties"]["status"]["minLength"],
            serde_json::json!(1)
        );

        let required: BTreeSet<_> = schema["required"]
            .as_array()
            .expect("schema required must be an array")
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .expect("schema required names must be strings")
            })
            .collect();
        let expected_required: BTreeSet<_> = [
            "run_id",
            "org_id",
            "status",
            "cost_micros",
            "baseline_micros",
            "saved_micros",
            "signature_hex",
            "verifying_key_hex",
            "canonical_version",
        ]
        .into_iter()
        .collect();
        assert_eq!(required, expected_required);

        let properties = schema["properties"]
            .as_object()
            .expect("schema properties must be an object");
        let serialized = serde_json::to_value(sample_v1()).expect("ARR receipt must serialize");
        let serialized_fields = serialized
            .as_object()
            .expect("serialized ARR receipt must be an object");
        assert!(
            !serialized_fields.contains_key("workflow_id"),
            "ARR serialization must remain top-level rather than inherit WFR's workflow_id"
        );
        for field in serialized_fields.keys() {
            assert!(
                properties.contains_key(field),
                "machine-readable schema is missing the serialized ARR field {field}"
            );
        }
        assert!(
            !properties.contains_key("workflow_id"),
            "ARR must remain top-level rather than inherit WFR's workflow_id"
        );
    }

    #[test]
    fn checked_in_golden_vector_verifies_and_pins_canonical_payload() {
        let receipt: AgentRunReceipt =
            serde_json::from_str(ARR_V1_GOLDEN).expect("golden ARR receipt must deserialize");
        assert_eq!(
            canonical_payload(&receipt).expect("golden receipt must build canonical payload"),
            ARR_V1_GOLDEN_PAYLOAD
        );
        assert!(
            verify_with_key(&receipt.verifying_key_hex, &receipt),
            "golden receipt must verify with its documented test key"
        );
    }

    #[test]
    fn canonical_payload_matches_cloud_builder() {
        assert_eq!(
            canonical_payload(&sample_v1()).unwrap(),
            ARR_V1_GOLDEN_PAYLOAD
        );
    }

    #[test]
    fn unknown_version_is_rejected() {
        let mut receipt = sample_v1();
        receipt.canonical_version = "v9".to_string();
        assert!(matches!(
            canonical_payload(&receipt),
            Err(AgentRunReceiptError::UnknownVersion(_))
        ));
    }

    #[test]
    fn pipe_in_status_is_rejected() {
        let mut receipt = sample_v1();
        receipt.status = "completed|forged".to_string();
        assert_eq!(
            canonical_payload(&receipt),
            Err(AgentRunReceiptError::PipeInField)
        );
    }

    #[test]
    fn empty_status_is_rejected_by_the_public_wire_contract() {
        let mut receipt = sample_v1();
        receipt.status.clear();
        assert_eq!(
            canonical_payload(&receipt),
            Err(AgentRunReceiptError::EmptyStatus)
        );
    }

    #[test]
    fn verifier_is_format_compatible_with_a_future_nonempty_separator_safe_status() {
        // Current cloud mint eligibility is an issuer-side terminal-status
        // allowlist. The canonical builder deliberately does not encode that
        // exact enum, so a future nonempty terminal status signed by the cloud
        // remains independently verifiable rather than being treated as
        // malformed.
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut receipt = sample_v1();
        receipt.status = "cancelled".to_string();
        let signature = key.sign(canonical_payload(&receipt).unwrap().as_bytes());
        receipt.signature_hex = hex::encode(signature.to_bytes());
        let key_hex = hex::encode(key.verifying_key().to_bytes());
        assert!(verify_with_key(&key_hex, &receipt));
    }

    #[test]
    fn verify_with_key_round_trips() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut receipt = sample_v1();
        let signature = key.sign(canonical_payload(&receipt).unwrap().as_bytes());
        receipt.signature_hex = hex::encode(signature.to_bytes());
        receipt.verifying_key_hex = hex::encode(key.verifying_key().to_bytes());
        assert!(verify_with_key(&receipt.verifying_key_hex, &receipt));
    }

    #[test]
    fn verify_with_key_fails_when_saved_micros_is_tampered() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut receipt = sample_v1();
        let signature = key.sign(canonical_payload(&receipt).unwrap().as_bytes());
        receipt.signature_hex = hex::encode(signature.to_bytes());
        receipt.saved_micros = 999_999;
        let key_hex = hex::encode(key.verifying_key().to_bytes());
        assert!(!verify_with_key(&key_hex, &receipt));
    }
}
