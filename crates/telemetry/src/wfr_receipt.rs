//! The workflow-run (WFR) receipt — a signed, offline-verifiable proof of a
//! workflow run's cost + savings (+ optional flow-level quality verdict).
//!
//! The sign side lives in the cloud mint endpoint
//! (`POST /v1/admin/workflow-runs/{run_id}/receipt/sign`). This module is
//! the **verify** home shared by `tt verify-receipt` and (eventually) the cloud
//! mint, mirroring `vcr.rs` / `l2_receipt.rs`: a canonical ASCII string with a
//! disjoint domain-separation prefix (`wfr:v1|` through `wfr:v4|`), signed directly
//! (no chain), signature + verifying key embedded with the receipt.
//!
//! # Canonical versions
//! `v1` = cost-only: `wfr:v1|<org>|<workflow_id>|<run_id>|<cost_micros>|
//! <baseline_micros>|<saved_micros>|<status>`. `v2` = v1 + a trailing
//! `|<quality_verdict>` (the flow-level judge's verdict — `equivalent` /
//! `degraded` / `inconclusive`). `v3` signs strict request-delta formula and
//! coverage evidence; `v4` is v3 plus a trailing quality verdict. Incomplete
//! coverage is not mintable. The version tag is part of the signed bytes.
//!
//! # Determinism
//! All money fields are **already integer micro-USD** (`i64`) in the receipt —
//! the verifier does NO float rounding (unlike VCR/L2, which canonicalize f64/f32
//! USD fields to micros). This sidesteps the f32/f64 determinism trap entirely.
//!
//! # Domain separation
//! Every `wfr:vN|` prefix is disjoint from `vcr:v1|` (compressions),
//! `l2:v1|` (semantic-cache hits), `arr:vN|` (top-level agent runs), `att:`
//! (attestations), `pdf:v1|` (PDF reports), and the bare-32B audit hash. A
//! signature from one family can NEVER be mis-validated as another.

use uuid::Uuid;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use tt_shared::{RequestDeltaReceiptError, RequestDeltaReceiptFields};

/// The domain-separation prefix for workflow-run receipts. Disjoint from
/// `vcr:v1|` / `l2:v1|` / `arr:v1|` / `att:` / `pdf:v1|` / the bare-32B audit
/// hash.
pub const WFR_PREFIX: &str = "wfr:";

/// Canonical version tags carried in the payload. `v1`/`v2` are frozen legacy
/// formats; `v3`/`v4` sign strict request-delta evidence, with even versions
/// carrying a trailing `|quality_verdict` field.
pub const CANONICAL_VERSION_V1: &str = "v1";
pub const CANONICAL_VERSION_V2: &str = "v2";
pub const CANONICAL_VERSION_V3: &str = "v3";
pub const CANONICAL_VERSION_V4: &str = "v4";

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
    /// Nonempty terminal status at seal time — canonical-payload field 8.
    pub status: String,
    /// Actual run cost in micro-USD — canonical-payload field 5.
    pub cost_micros: i64,
    /// Baseline cost (without TokenTrimmer) in micro-USD — field 6.
    pub baseline_micros: i64,
    /// Positive-only request delta in micro-USD. For v3/v4 it must equal
    /// `max(signed_request_delta_micros, 0)`.
    pub saved_micros: i64,
    /// Signed v3/v4 request delta. A regression remains negative.
    #[serde(default)]
    pub signed_request_delta_micros: Option<i64>,
    /// Exact formula identifier signed by v3/v4.
    #[serde(default)]
    pub request_delta_formula_version: Option<String>,
    /// Non-truncated requests in the signed run cohort (v3/v4 only).
    #[serde(default)]
    pub request_delta_eligible_requests: Option<i64>,
    /// Strictly measured requests in that cohort (v3/v4 only).
    #[serde(default)]
    pub request_delta_measured_requests: Option<i64>,
    /// Convenience: cost_micros / 1_000_000. NOT part of the signature.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Convenience: baseline_micros / 1_000_000. NOT part of the signature.
    #[serde(default)]
    pub baseline_usd: Option<f64>,
    /// Convenience: saved_micros / 1_000_000. NOT part of the signature.
    #[serde(default)]
    pub saved_usd: Option<f64>,
    /// Convenience signed request delta in USD. NOT part of the signature.
    #[serde(default)]
    pub signed_request_delta_usd: Option<f64>,
    /// Hex-encoded 64-byte Ed25519 signature over [`canonical_payload`].
    pub signature_hex: String,
    /// Hex-encoded 32-byte Ed25519 verifying (public) key. Embedded so
    /// verification needs no external key lookup.
    pub verifying_key_hex: String,
    /// Canonical schema version (`v1`/`v2` legacy or `v3`/`v4` request-delta).
    pub canonical_version: String,
    /// The flow-level quality-gate verdict a v2/v4 receipt was minted with
    /// (`equivalent` / `degraded` / `inconclusive`), or `None` for a v1/v3
    /// (not-sampled) receipt. Part of the v2/v4 canonical payload's trailing
    /// field.
    #[serde(default)]
    pub quality_verdict: Option<String>,
    /// RFC3339 timestamp the receipt was first minted (frozen by DB).
    #[serde(default)]
    pub signed_at: Option<String>,
}

/// Build the canonical signed payload string for a workflow run receipt.
/// Mirrors the cloud run-receipt canonicalizer byte-for-byte.
///
/// Format (v1):
/// `wfr:v1|<org_id>|<workflow_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<status>`
///
/// Format (v2):
/// `wfr:v2|<org_id>|<workflow_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<status>|<quality_verdict>`
///
/// v3/v4 field order after the run ID is:
/// `<cost>|<baseline>|<saved>|<signed>|<formula>|<eligible>|<measured>|<status>`.
/// v4 appends `|<quality_verdict>`.
pub fn canonical_payload(receipt: &WfrReceipt) -> Result<String, WfrReceiptError> {
    if receipt.status.is_empty() {
        return Err(WfrReceiptError::EmptyStatus);
    }
    if receipt.workflow_id.to_string().contains('|')
        || receipt.run_id.to_string().contains('|')
        || receipt.status.contains('|')
    {
        return Err(WfrReceiptError::PipeInField);
    }
    let has_request_delta_fields = receipt.signed_request_delta_micros.is_some()
        || receipt.request_delta_formula_version.is_some()
        || receipt.request_delta_eligible_requests.is_some()
        || receipt.request_delta_measured_requests.is_some()
        || receipt.signed_request_delta_usd.is_some();
    let base = match receipt.canonical_version.as_str() {
        CANONICAL_VERSION_V1 | CANONICAL_VERSION_V2 => {
            if has_request_delta_fields {
                return Err(WfrReceiptError::UnexpectedRequestDeltaEvidence);
            }
            format!(
                "{WFR_PREFIX}{}|{}|{}|{}|{}|{}|{}|{}",
                receipt.canonical_version,
                receipt.org_id,
                receipt.workflow_id,
                receipt.run_id,
                receipt.cost_micros,
                receipt.baseline_micros,
                receipt.saved_micros,
                receipt.status,
            )
        }
        CANONICAL_VERSION_V3 | CANONICAL_VERSION_V4 => {
            let fields = RequestDeltaReceiptFields {
                cost_micros: receipt.cost_micros,
                baseline_micros: receipt.baseline_micros,
                saved_micros: receipt.saved_micros,
                signed_request_delta_micros: receipt
                    .signed_request_delta_micros
                    .ok_or(WfrReceiptError::MissingRequestDeltaEvidence)?,
                formula_version: receipt
                    .request_delta_formula_version
                    .as_deref()
                    .ok_or(WfrReceiptError::MissingRequestDeltaEvidence)?,
                eligible_requests: receipt
                    .request_delta_eligible_requests
                    .ok_or(WfrReceiptError::MissingRequestDeltaEvidence)?,
                measured_requests: receipt
                    .request_delta_measured_requests
                    .ok_or(WfrReceiptError::MissingRequestDeltaEvidence)?,
            };
            let fragment = fields
                .canonical_fragment()
                .map_err(WfrReceiptError::InvalidRequestDeltaEvidence)?;
            format!(
                "{WFR_PREFIX}{}|{}|{}|{}|{fragment}|{}",
                receipt.canonical_version,
                receipt.org_id,
                receipt.workflow_id,
                receipt.run_id,
                receipt.status,
            )
        }
        other => return Err(WfrReceiptError::UnknownVersion(other.to_string())),
    };
    let carries_quality = matches!(
        receipt.canonical_version.as_str(),
        CANONICAL_VERSION_V2 | CANONICAL_VERSION_V4
    );
    if carries_quality {
        let q = receipt.quality_verdict.as_deref().unwrap_or("");
        if q.is_empty() {
            return Err(WfrReceiptError::MissingQualityVerdict);
        }
        if q.contains('|') {
            return Err(WfrReceiptError::PipeInField);
        }
        Ok(format!("{base}|{q}"))
    } else if receipt.quality_verdict.is_some() {
        Err(WfrReceiptError::UnexpectedQualityVerdict)
    } else {
        Ok(base)
    }
}

/// Errors building/verifying a workflow-run receipt.
#[derive(Debug, PartialEq, Eq)]
pub enum WfrReceiptError {
    /// A required canonical status field was empty.
    EmptyStatus,
    /// A string field contained the `|` separator.
    PipeInField,
    /// A v2/v4 receipt lacked its `quality_verdict`.
    MissingQualityVerdict,
    /// A v1/v3 receipt carried a verdict even though that field is not signed
    /// in its canonical payload.
    UnexpectedQualityVerdict,
    /// A v1/v2 receipt carried non-null fields that are not signed by it.
    UnexpectedRequestDeltaEvidence,
    /// A v3/v4 receipt omitted its formula or coverage fields.
    MissingRequestDeltaEvidence,
    /// A v3/v4 receipt's formula, coverage, or money state was inconsistent.
    InvalidRequestDeltaEvidence(RequestDeltaReceiptError),
    /// `canonical_version` was not one of the published versions.
    UnknownVersion(String),
}

impl std::fmt::Display for WfrReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyStatus => {
                f.write_str("receipt status must be non-empty in the canonical payload")
            }
            Self::PipeInField => f.write_str(
                "field contained pipe character '|' which is the payload field separator",
            ),
            Self::MissingQualityVerdict => f.write_str(
                "v2/v4 receipt is missing its quality_verdict (a required canonical-payload field)",
            ),
            Self::UnexpectedQualityVerdict => {
                f.write_str("receipt carries a quality_verdict that is outside its signed payload")
            }
            Self::UnexpectedRequestDeltaEvidence => f.write_str(
                "legacy receipt carries request-delta evidence outside its signed payload",
            ),
            Self::MissingRequestDeltaEvidence => {
                f.write_str("v3/v4 receipt is missing signed formula or coverage evidence")
            }
            Self::InvalidRequestDeltaEvidence(error) => {
                write!(f, "invalid request-delta evidence: {error}")
            }
            Self::UnknownVersion(v) => write!(
                f,
                "unknown canonical_version \"{v}\" (expected v1, v2, v3, or v4)"
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
    //! The canonical strings asserted here mirror the cloud run-receipt
    //! canonicalizer byte-for-byte. If these drift, `tt verify-receipt` and the
    //! cloud mint would disagree.

    use std::collections::BTreeSet;

    use super::*;
    use serde_json::Value;
    use tt_shared::REQUEST_DELTA_ESTIMATE_V1;

    const WFR_STRUCTURAL_SCHEMA: &str =
        include_str!("../../../docs/receipt-spec/wfr-receipt.schema.json");
    const WFR_V1_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v1.golden.json");
    const WFR_V2_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v2.golden.json");
    const WFR_V3_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v3.golden.json");
    const WFR_V4_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v4.golden.json");

    const WFR_V1_GOLDEN_PAYLOAD: &str =
        "wfr:v1|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000b2|00000000-0000-0000-0000-0000000000a1|70000|180000|110000|completed";
    const WFR_V2_GOLDEN_PAYLOAD: &str =
        "wfr:v2|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000b2|00000000-0000-0000-0000-0000000000a1|70000|180000|110000|completed|equivalent";
    const WFR_V3_GOLDEN_PAYLOAD: &str =
        "wfr:v3|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000b2|00000000-0000-0000-0000-0000000000a1|200000|180000|0|-50000|tt.request-delta-estimate.v1|2|2|completed";
    const WFR_V4_GOLDEN_PAYLOAD: &str =
        "wfr:v4|00000000-0000-0000-0000-00000000002a|00000000-0000-0000-0000-0000000000b2|00000000-0000-0000-0000-0000000000a1|70000|180000|100000|100000|tt.request-delta-estimate.v1|3|3|completed|equivalent";

    fn sample_v1() -> WfrReceipt {
        WfrReceipt {
            run_id: Uuid::parse_str("00000000-0000-0000-0000-0000000000a1").unwrap(),
            org_id: Uuid::parse_str("00000000-0000-0000-0000-00000000002a").unwrap(),
            workflow_id: Uuid::parse_str("00000000-0000-0000-0000-0000000000b2").unwrap(),
            status: "completed".to_string(),
            cost_micros: 70_000,
            baseline_micros: 180_000,
            saved_micros: 110_000,
            signed_request_delta_micros: None,
            request_delta_formula_version: None,
            request_delta_eligible_requests: None,
            request_delta_measured_requests: None,
            cost_usd: None,
            baseline_usd: None,
            saved_usd: None,
            signed_request_delta_usd: None,
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
            "urn:tokentrimmer:receipt:wfr:structural-schema:v1-v4"
        );
        assert_eq!(schema["additionalProperties"], Value::Bool(true));
        assert_eq!(
            schema["properties"]["canonical_version"]["enum"],
            serde_json::json!([
                CANONICAL_VERSION_V1,
                CANONICAL_VERSION_V2,
                CANONICAL_VERSION_V3,
                CANONICAL_VERSION_V4
            ])
        );
        assert_eq!(
            schema["properties"]["status"]["minLength"],
            serde_json::json!(1)
        );
        assert_eq!(
            schema["properties"]["status"]["pattern"],
            serde_json::json!("^[^|]+$")
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
        // v1/v3 omit a verdict from their signed bytes; v2/v4 require a
        // nonempty, pipe-free verdict as the trailing signed field.
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
        assert_eq!(
            schema["allOf"][3]["then"]["properties"]["request_delta_formula_version"]["const"],
            REQUEST_DELTA_ESTIMATE_V1
        );
    }

    #[test]
    fn checked_in_golden_vectors_verify_and_pin_canonical_payloads() {
        for (raw, expected_payload) in [
            (WFR_V1_GOLDEN, WFR_V1_GOLDEN_PAYLOAD),
            (WFR_V2_GOLDEN, WFR_V2_GOLDEN_PAYLOAD),
            (WFR_V3_GOLDEN, WFR_V3_GOLDEN_PAYLOAD),
            (WFR_V4_GOLDEN, WFR_V4_GOLDEN_PAYLOAD),
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
    fn canonical_payload_v3_preserves_a_signed_regression_and_coverage() {
        let receipt: WfrReceipt = serde_json::from_str(WFR_V3_GOLDEN).unwrap();
        assert_eq!(canonical_payload(&receipt).unwrap(), WFR_V3_GOLDEN_PAYLOAD);
        assert_eq!(receipt.saved_micros, 0);
        assert_eq!(receipt.signed_request_delta_micros, Some(-50_000));
    }

    #[test]
    fn canonical_payload_v4_appends_quality_to_request_delta_evidence() {
        let receipt: WfrReceipt = serde_json::from_str(WFR_V4_GOLDEN).unwrap();
        assert_eq!(canonical_payload(&receipt).unwrap(), WFR_V4_GOLDEN_PAYLOAD);
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
    fn legacy_versions_reject_unsigned_request_delta_fields() {
        let mut receipt = sample_v1();
        receipt.signed_request_delta_micros = Some(110_000);
        receipt.request_delta_formula_version = Some(REQUEST_DELTA_ESTIMATE_V1.to_string());
        receipt.request_delta_eligible_requests = Some(1);
        receipt.request_delta_measured_requests = Some(1);
        receipt.signed_request_delta_usd = Some(0.11);
        assert_eq!(
            canonical_payload(&receipt),
            Err(WfrReceiptError::UnexpectedRequestDeltaEvidence)
        );
    }

    #[test]
    fn request_delta_versions_reject_incomplete_or_inconsistent_evidence() {
        let mut receipt: WfrReceipt = serde_json::from_str(WFR_V3_GOLDEN).unwrap();
        receipt.request_delta_measured_requests = Some(1);
        assert!(matches!(
            canonical_payload(&receipt),
            Err(WfrReceiptError::InvalidRequestDeltaEvidence(
                RequestDeltaReceiptError::IncompleteCoverage
            ))
        ));

        let mut receipt: WfrReceipt = serde_json::from_str(WFR_V3_GOLDEN).unwrap();
        receipt.saved_micros = 1;
        assert!(matches!(
            canonical_payload(&receipt),
            Err(WfrReceiptError::InvalidRequestDeltaEvidence(
                RequestDeltaReceiptError::InvalidPositiveProjection
            ))
        ));
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
    fn empty_status_is_rejected_by_canonical_payload_and_verification() {
        use ed25519_dalek::{Signer, SigningKey};

        let key = SigningKey::from_bytes(&[7u8; 32]);

        let mut v1 = sample_v1();
        v1.status.clear();
        assert_eq!(canonical_payload(&v1), Err(WfrReceiptError::EmptyStatus));
        let legacy_v1_payload = format!(
            "{WFR_PREFIX}{CANONICAL_VERSION_V1}|{}|{}|{}|{}|{}|{}|",
            v1.org_id,
            v1.workflow_id,
            v1.run_id,
            v1.cost_micros,
            v1.baseline_micros,
            v1.saved_micros,
        );
        v1.signature_hex = hex::encode(key.sign(legacy_v1_payload.as_bytes()).to_bytes());
        v1.verifying_key_hex = hex::encode(key.verifying_key().to_bytes());
        assert!(!verify_with_key(&v1.verifying_key_hex, &v1));

        let mut v2 = sample_v1();
        v2.canonical_version = CANONICAL_VERSION_V2.to_string();
        v2.quality_verdict = Some("equivalent".to_string());
        v2.status.clear();
        assert_eq!(canonical_payload(&v2), Err(WfrReceiptError::EmptyStatus));
        let legacy_v2_payload = format!(
            "{WFR_PREFIX}{CANONICAL_VERSION_V2}|{}|{}|{}|{}|{}|{}||equivalent",
            v2.org_id,
            v2.workflow_id,
            v2.run_id,
            v2.cost_micros,
            v2.baseline_micros,
            v2.saved_micros,
        );
        v2.signature_hex = hex::encode(key.sign(legacy_v2_payload.as_bytes()).to_bytes());
        v2.verifying_key_hex = hex::encode(key.verifying_key().to_bytes());
        assert!(!verify_with_key(&v2.verifying_key_hex, &v2));
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
