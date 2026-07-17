//! The workflow-run (WFR) receipt — a signed, offline-verifiable proof of a
//! workflow run's cost + savings (+ optional flow-level quality verdict).
//!
//! The sign side lives in the cloud mint endpoint
//! (`POST /v1/admin/workflow-runs/{run_id}/receipt/sign`, which calls
//! `cloud::api::workflow_receipt::crypto::sign_receipt[_v2]`). This module is
//! the **verify** home shared by `tt verify-receipt` and (eventually) the cloud
//! mint, mirroring `vcr.rs` / `l2_receipt.rs`: a canonical ASCII string with a
//! disjoint domain-separation prefix (`wfr:v1|` / `wfr:v2|`), signed directly
//! (no chain), signature + verifying key embedded with the receipt.
//!
//! # Two canonical versions
//! `v1` = cost-only: `wfr:v1|<org>|<workflow_id>|<run_id>|<cost_micros>|
//! <baseline_micros>|<saved_micros>|<status>`. `v2` = v1 + a trailing
//! `|<quality_verdict>` (the flow-level judge's verdict — `equivalent` /
//! `degraded` / `inconclusive`; not-sampled runs stay on v1). The tag is part
//! of the signed bytes, so a v1 receipt can never verify as a v2 one and vice
//! versa. Mirrors the canonical builder in
//! `cloud/crates/api/src/workflow_receipt/crypto.rs::canonical_payload_wfr[_v2]`
//! byte-for-byte.
//!
//! # Determinism
//! All money fields are **already integer micro-USD** (`i64`) in the receipt —
//! the verifier does NO float rounding (unlike VCR/L2, which canonicalize f64/f32
//! USD fields to micros). This sidesteps the f32/f64 determinism trap entirely.
//!
//! # Domain separation
//! `wfr:v1|` / `wfr:v2|` is disjoint from `vcr:v1|` (compressions),
//! `l2:v1|` (semantic-cache hits), `att:` (attestations), `pdf:v1|` (PDF
//! reports), and the bare-32B audit hash. A signature from one family can NEVER
//! be mis-validated as another.

use uuid::Uuid;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

/// The domain-separation prefix for workflow-run receipts. Disjoint from
/// `vcr:v1|` / `l2:v1|` / `att:` / `pdf:v1|` / the bare-32B audit hash.
pub const WFR_PREFIX: &str = "wfr:";

/// The canonical version tag carried in the payload. `v1` = the original
/// cost-only receipt; `v2` = v1 + a trailing `|quality_verdict` field.
pub const CANONICAL_VERSION_V1: &str = "v1";
pub const CANONICAL_VERSION_V2: &str = "v2";

/// A signed workflow-run receipt as deserialized from the cloud
/// `VerifyReceiptResponse` JSON shape. Money fields are integer micro-USD
/// (`i64`) — they are the canonical-payload inputs directly (no float
/// canonicalization). The `verifying_key_hex` + `signature_hex` make the
/// receipt offline-verifiable with no key lookup (mirrors `VcrReceipt` /
/// `L2Receipt`). The convenience `*_usd` fields are NOT part of the signature.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct WfrReceipt {
    /// The workflow run ID (also the receipt primary key).
    pub run_id: Uuid,
    /// Org that owns the run — canonical-payload field 2.
    pub org_id: Uuid,
    /// Workflow the run belongs to — canonical-payload field 3.
    pub workflow_id: Uuid,
    /// Terminal status at seal time — canonical-payload field 8.
    pub status: String,
    /// Actual run cost in micro-USD — canonical-payload field 5.
    pub cost_micros: i64,
    /// Baseline cost (without TokenTrimmer) in micro-USD — field 6.
    pub baseline_micros: i64,
    /// Net savings for the run in micro-USD — field 7.
    pub saved_micros: i64,
    /// Convenience: cost_micros / 1_000_000. NOT part of the signature.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Convenience: baseline_micros / 1_000_000. NOT part of the signature.
    #[serde(default)]
    pub baseline_usd: Option<f64>,
    /// Convenience: saved_micros / 1_000_000. NOT part of the signature.
    #[serde(default)]
    pub saved_usd: Option<f64>,
    /// Hex-encoded 64-byte Ed25519 signature over [`canonical_payload`].
    pub signature_hex: String,
    /// Hex-encoded 32-byte Ed25519 verifying (public) key. Embedded so
    /// verification needs no external key lookup.
    pub verifying_key_hex: String,
    /// Canonical schema version (`"v1"` cost-only, or `"v2"` cost + quality
    /// verdict).
    pub canonical_version: String,
    /// The flow-level quality-gate verdict a v2 receipt was minted with
    /// (`equivalent` / `degraded` / `inconclusive`), or `None` for a v1
    /// (not-sampled) receipt. Part of the v2 canonical payload's trailing
    /// field.
    #[serde(default)]
    pub quality_verdict: Option<String>,
    /// RFC3339 timestamp the receipt was first minted (frozen by DB).
    #[serde(default)]
    pub signed_at: Option<String>,
}

/// Build the canonical signed payload string for a workflow run receipt.
/// Mirrors `workflow_receipt::crypto::canonical_payload_wfr[_v2]` byte-for-byte.
///
/// Format (v1):
/// `wfr:v1|<org_id>|<workflow_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<status>`
///
/// Format (v2):
/// `wfr:v2|<org_id>|<workflow_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<status>|<quality_verdict>`
///
/// Returns `Err` if a string field contains `|` (the field separator, an
/// injection guard), if a v2 receipt lacks its `quality_verdict`, or if a v1
/// receipt carries one. A v1 verdict would be an unsigned extra field and is
/// outside the published structural contract.
pub fn canonical_payload(receipt: &WfrReceipt) -> Result<String, WfrReceiptError> {
    if receipt.workflow_id.to_string().contains('|')
        || receipt.run_id.to_string().contains('|')
        || receipt.status.contains('|')
    {
        return Err(WfrReceiptError::PipeInField);
    }
    if receipt.canonical_version == CANONICAL_VERSION_V1 && receipt.quality_verdict.is_some() {
        return Err(WfrReceiptError::UnexpectedQualityVerdict);
    }
    let base = match receipt.canonical_version.as_str() {
        CANONICAL_VERSION_V2 => format!(
            "{WFR_PREFIX}v2|{}|{}|{}|{}|{}|{}|{}",
            receipt.org_id,
            receipt.workflow_id,
            receipt.run_id,
            receipt.cost_micros,
            receipt.baseline_micros,
            receipt.saved_micros,
            receipt.status,
        ),
        CANONICAL_VERSION_V1 => format!(
            "{WFR_PREFIX}v1|{}|{}|{}|{}|{}|{}|{}",
            receipt.org_id,
            receipt.workflow_id,
            receipt.run_id,
            receipt.cost_micros,
            receipt.baseline_micros,
            receipt.saved_micros,
            receipt.status,
        ),
        other => return Err(WfrReceiptError::UnknownVersion(other.to_string())),
    };
    if receipt.canonical_version == CANONICAL_VERSION_V2 {
        let q = receipt.quality_verdict.as_deref().unwrap_or("");
        if q.is_empty() {
            return Err(WfrReceiptError::MissingQualityVerdict);
        }
        if q.contains('|') {
            return Err(WfrReceiptError::PipeInField);
        }
        Ok(format!("{base}|{q}"))
    } else {
        Ok(base)
    }
}

/// Errors building/verifying a workflow-run receipt.
#[derive(Debug, PartialEq, Eq)]
pub enum WfrReceiptError {
    /// A string field contained the `|` separator.
    PipeInField,
    /// A v2 receipt lacked its `quality_verdict`.
    MissingQualityVerdict,
    /// A v1 receipt carried a verdict even though that field is not signed in
    /// the v1 canonical payload.
    UnexpectedQualityVerdict,
    /// `canonical_version` was neither `"v1"` nor `"v2"`.
    UnknownVersion(String),
}

impl std::fmt::Display for WfrReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PipeInField => f.write_str(
                "field contained pipe character '|' which is the payload field separator",
            ),
            Self::MissingQualityVerdict => f.write_str(
                "v2 receipt is missing its quality_verdict (a required canonical-payload field)",
            ),
            Self::UnexpectedQualityVerdict => f.write_str(
                "v1 receipt carries a quality_verdict that is outside its signed payload",
            ),
            Self::UnknownVersion(v) => write!(
                f,
                "unknown canonical_version \"{v}\" (expected \"v1\" or \"v2\")"
            ),
        }
    }
}

impl std::error::Error for WfrReceiptError {}

/// Verify a workflow-run receipt offline against an EXTERNAL verifying-key hex
/// (not the embedded one). Used by `tt verify-receipt --key-hex <hex>` when the
/// customer supplies the key out-of-band (the stronger trust model). Mirrors
/// `vcr::verify_with_key` / `l2_receipt::verify_with_key`.
#[must_use]
pub fn verify_with_key(verifying_key_hex_: &str, receipt: &WfrReceipt) -> bool {
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
    //! Canonical-payload + verify drift gates for the WFR receipt family.
    //!
    //! The canonical strings asserted here mirror
    //! `cloud/crates/api/src/workflow_receipt/crypto.rs::canonical_payload_wfr[_v2]`
    //! byte-for-byte (the sign side). If these drift, `tt verify-receipt` and the
    //! cloud mint would disagree.

    use std::collections::BTreeSet;

    use super::*;
    use serde_json::Value;

    const WFR_STRUCTURAL_SCHEMA: &str =
        include_str!("../../../docs/receipt-spec/wfr-receipt.schema.json");
    const WFR_V1_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v1.golden.json");
    const WFR_V2_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v2.golden.json");

    const WFR_V1_GOLDEN_PAYLOAD: &str =
        "wfr:v1|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000b2|00000000-0000-0000-0000-0000000000a1|70000|180000|110000|completed";
    const WFR_V2_GOLDEN_PAYLOAD: &str =
        "wfr:v2|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000b2|00000000-0000-0000-0000-0000000000a1|70000|180000|110000|completed|equivalent";

    fn sample_v1() -> WfrReceipt {
        WfrReceipt {
            run_id: Uuid::parse_str("00000000-0000-0000-0000-0000000000a1").unwrap(),
            org_id: Uuid::parse_str("00000000-0000-0000-0000-00000000002a").unwrap(),
            workflow_id: Uuid::parse_str("00000000-0000-0000-0000-0000000000b2").unwrap(),
            status: "completed".to_string(),
            cost_micros: 70_000,
            baseline_micros: 180_000,
            saved_micros: 110_000,
            cost_usd: None,
            baseline_usd: None,
            saved_usd: None,
            signature_hex: String::new(),
            verifying_key_hex: String::new(),
            canonical_version: "v1".to_string(),
            quality_verdict: None,
            signed_at: None,
        }
    }

    #[test]
    fn structural_schema_covers_the_current_wfr_wire_contract() {
        let schema: Value =
            serde_json::from_str(WFR_STRUCTURAL_SCHEMA).expect("WFR schema must be valid JSON");
        assert_eq!(
            schema["$id"],
            "urn:tokentrimmer:receipt:wfr:structural-schema:v1-v2"
        );
        assert_eq!(schema["additionalProperties"], Value::Bool(true));
        assert_eq!(
            schema["properties"]["canonical_version"]["enum"],
            serde_json::json!([CANONICAL_VERSION_V1, CANONICAL_VERSION_V2])
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
            "workflow_id",
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
        let serialized = serde_json::to_value(sample_v1()).expect("WFR receipt must serialize");
        for field in serialized
            .as_object()
            .expect("serialized WFR receipt must be an object")
            .keys()
        {
            assert!(
                properties.contains_key(field),
                "machine-readable schema is missing the serialized WFR field {field}"
            );
        }

        // These conditionals mirror the exact canonical-builder distinction:
        // v1 omits a verdict from its signed bytes; v2 requires a nonempty,
        // pipe-free verdict as the trailing signed field.
        assert_eq!(
            schema["allOf"][0]["then"]["properties"]["quality_verdict"]["type"],
            "null"
        );
        assert_eq!(
            schema["allOf"][1]["then"]["required"],
            serde_json::json!(["quality_verdict"])
        );
        assert_eq!(
            schema["allOf"][1]["then"]["properties"]["quality_verdict"]["minLength"],
            1
        );
    }

    #[test]
    fn checked_in_golden_vectors_verify_and_pin_canonical_payloads() {
        for (raw, expected_payload) in [
            (WFR_V1_GOLDEN, WFR_V1_GOLDEN_PAYLOAD),
            (WFR_V2_GOLDEN, WFR_V2_GOLDEN_PAYLOAD),
        ] {
            let receipt: WfrReceipt =
                serde_json::from_str(raw).expect("golden WFR receipt must deserialize");
            assert_eq!(
                canonical_payload(&receipt).expect("golden receipt must build canonical payload"),
                expected_payload
            );
            assert!(
                verify_with_key(&receipt.verifying_key_hex, &receipt),
                "golden receipt must verify with its documented test key"
            );
        }
    }

    #[test]
    fn canonical_payload_v1_matches_cloud_builder() {
        let r = sample_v1();
        let payload = canonical_payload(&r).unwrap();
        assert_eq!(
            payload,
            "wfr:v1|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000b2|00000000-0000-0000-0000-0000000000a1|70000|180000|110000|completed"
        );
    }

    #[test]
    fn canonical_payload_v2_appends_quality_verdict() {
        let mut r = sample_v1();
        r.canonical_version = "v2".to_string();
        r.quality_verdict = Some("equivalent".to_string());
        let payload = canonical_payload(&r).unwrap();
        assert_eq!(
            payload,
            "wfr:v2|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000b2|00000000-0000-0000-0000-0000000000a1|70000|180000|110000|completed|equivalent"
        );
    }

    #[test]
    fn v1_and_v2_payloads_differ() {
        let v1 = canonical_payload(&sample_v1()).unwrap();
        let mut v2 = sample_v1();
        v2.canonical_version = "v2".to_string();
        v2.quality_verdict = Some("equivalent".to_string());
        let v2p = canonical_payload(&v2).unwrap();
        assert_ne!(v1, v2p);
    }

    #[test]
    fn v2_without_quality_verdict_is_rejected() {
        let mut r = sample_v1();
        r.canonical_version = "v2".to_string();
        r.quality_verdict = None;
        assert_eq!(
            canonical_payload(&r),
            Err(WfrReceiptError::MissingQualityVerdict)
        );
    }

    #[test]
    fn v1_with_quality_verdict_is_rejected_by_the_structural_contract() {
        let mut r = sample_v1();
        r.quality_verdict = Some("equivalent".to_string());
        assert_eq!(
            canonical_payload(&r),
            Err(WfrReceiptError::UnexpectedQualityVerdict)
        );
    }

    #[test]
    fn unknown_version_is_rejected() {
        let mut r = sample_v1();
        r.canonical_version = "v9".to_string();
        assert!(matches!(
            canonical_payload(&r),
            Err(WfrReceiptError::UnknownVersion(_))
        ));
    }

    #[test]
    fn pipe_in_status_is_rejected() {
        let mut r = sample_v1();
        r.status = "ok|injected".to_string();
        assert_eq!(canonical_payload(&r), Err(WfrReceiptError::PipeInField));
    }

    #[test]
    fn verify_with_key_round_trips() {
        use ed25519_dalek::{Signature, Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut r = sample_v1();
        // Sign over the canonical payload the way the cloud mint does.
        let payload = canonical_payload(&r).unwrap();
        let sig: Signature = key.sign(payload.as_bytes());
        r.signature_hex = hex::encode(sig.to_bytes());
        r.verifying_key_hex = hex::encode(key.verifying_key().to_bytes());
        // Verify with the embedded key hex (out-of-band pin = the same key).
        assert!(verify_with_key(&r.verifying_key_hex, &r));
    }

    #[test]
    fn verify_with_key_fails_when_saved_micros_tampered() {
        use ed25519_dalek::{Signature, Signer, SigningKey};
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mut r = sample_v1();
        let payload = canonical_payload(&r).unwrap();
        let sig: Signature = key.sign(payload.as_bytes());
        r.signature_hex = hex::encode(sig.to_bytes());
        let key_hex = hex::encode(key.verifying_key().to_bytes());
        // Tamper the savings after signing → canonical changes → verify fails.
        r.saved_micros = 999_999;
        assert!(!verify_with_key(&key_hex, &r));
    }
}
