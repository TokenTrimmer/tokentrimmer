//! The L2 (semantic-cache) hit provenance receipt — a signed, offline-verifiable
//! proof of what a cache hit matched + saved.
//!
//! The L2 cache serves a near-duplicate query from a stored entry for $0 (no
//! provider call). The saving is booked as `baseline_cost_usd` on a
//! `cache_layer = "l2"` request_logs row, but that's a dashboard number the
//! customer trusts on faith. This receipt makes the hit *provable*: a signed
//! `{org_id, trace_id, matched_entry_id, similarity, verdict, served_cost_usd,
//! baseline_cost_usd, ts}` record the customer verifies offline, mirroring the
//! VCR (`crates/telemetry/src/vcr.rs`) for compressions.
//!
//! # Why a new family (not a reuse of the VCR or the audit chain)
//! Same reasoning as the VCR (see `vcr.rs` "Why a new signature family"): the
//! audit chain is a per-org hash-CHAINED ledger (a single receipt can't verify
//! in isolation without the chain prefix). A cache-hit receipt is a DETACHED
//! per-event receipt — it must verify standalone, with only the public key +
//! the receipt itself. So it mirrors the VCR/attestation/pdf-signature
//! families: a canonical ASCII string with a disjoint domain-separation prefix
//! (`l2:v1|`), signed directly (no chain), the signature + the verifying key
//! embedded with the receipt.
//!
//! # Domain separation (the safety property)
//! `l2:v1|` is disjoint from `vcr:v1|` (compressions), `att:` (attestations),
//! `pdf:v1|` (PDF reports), and the bare-32B audit hash. A signature from one
//! family can NEVER be mis-validated as another — the `domain_separation` test
//! pins that (a `l2:v1` receipt must not verify as a `vcr:v1` receipt and
//! vice versa, because their payloads begin with different prefixes).
//!
//! # Determinism (the attestation/VCR lesson)
//! Floating-point field-order breaks signature determinism. The canonical
//! payload is a FIXED-ORDER ASCII string with similarity + money rounded to
//! integer micros (`(x * 1_000_000.0).round() as i64`) — NOT a `serde_json`
//! value (field order is not stable across serializers). Two hits at
//! similarity 0.9312 round to the same micros → the same canonical string →
//! the same signature.
//!
//! # Verdict codes (no free-form strings in the signed payload)
//! The four `L2VerifyDecision` variants map to fixed stable codes so the
//! canonical payload is byte-stable (a free-form string would invite
//! whitespace/encoding drift → signature instability). `confident` /
//! `verified` / `unverifiable` / `rejected`.
//!
//! # Fail-open (the production posture)
//! Mirrors the VCR: when `TT_AUDIT_SIGNING_KEY` is unset, the gateway emits NO
//! signed receipt (a metrics-only event) — never crashes, never blocks the
//! cache hit. The receipt is an out-of-band proof surface; it is NEVER
//! returned in the response body (the customer mints + verifies it offline).

use uuid::Uuid;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// The domain-separation prefix for L2-hit provenance receipts. Disjoint from
/// `vcr:v1|` / `att:` / `pdf:v1|` / the bare-32B audit hash. Bumped only on a
/// breaking canonical-shape change.
pub const L2_PREFIX: &str = "l2:v1";

/// The L2-receipt schema version. Bumped only on a breaking shape change; a
/// verifier refuses a receipt whose version it does not understand rather than
/// silently mis-reading it (mirrors `VcrReceipt::VCR_SCHEMA_VERSION`).
pub const L2_SCHEMA_VERSION: u32 = 1;

/// The stable string code a verdict serializes to in the canonical payload.
/// These are part of the SIGNED bytes — do NOT change them (change the version
/// instead); doing so invalidates every prior receipt's signature.
pub const VERDICT_CONFIDENT: &str = "confident";
pub const VERDICT_VERIFIED: &str = "verified";
pub const VERDICT_UNVERIFIABLE: &str = "unverifiable";
pub const VERDICT_REJECTED: &str = "rejected";

/// A signed L2-hit provenance receipt. Serialized as JSON; `tt verify-receipt`
/// deserializes + verifies it offline (dispatching on the `l2:v1` prefix in
/// the canonical payload — the CLI routes by family).
///
/// `signature` is a hex-encoded 64-byte Ed25519 signature over the canonical
/// payload. `verifying_key_hex` is the hex-encoded 32-byte public key the
/// receipt was signed with, embedded so verification needs no key lookup
/// (mirrors `VcrReceipt`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct L2Receipt {
    /// See [`L2_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The org whose cache was served (joins `request_logs.org_id`).
    pub org_id: Uuid,
    /// The hit request's `trace_id` (joins `request_logs.trace_id`).
    pub trace_id: Uuid,
    /// The cache entry the query matched (`cache_entries.id`). Lets the
    /// customer attribute the hit to a specific prior request's answer.
    pub matched_entry_id: Uuid,
    /// The cosine similarity of the match (0.0–1.0). Canonicalized to integer
    /// micros in the signed payload.
    pub similarity: f32,
    /// The verify-gate verdict code (`confident` / `verified` / `unverifiable`
    /// / `rejected`). NOT the served decision's raw f32 agreement — that's a
    /// telemetry detail, not part of the provenance contract.
    pub verdict: String,
    /// The served cost (USD) — $0 for a pure cache hit. Canonicalized to
    /// integer micros.
    pub served_cost_usd: f64,
    /// The baseline cost (USD) the hit saved — what the request would have
    /// cost at the served model's list price. Canonicalized to micros.
    pub baseline_cost_usd: f64,
    /// RFC 3339 timestamp the receipt was produced (informational; explicitly
    /// NOT part of any reproduction check beyond signature validity).
    pub ts: String,
    /// Hex-encoded 32-byte Ed25519 verifying (public) key the receipt was
    /// signed with. Embedded so verification needs no external key lookup.
    pub verifying_key_hex: String,
    /// Hex-encoded 64-byte Ed25519 signature over [`canonical_payload`].
    pub signature: String,
}

/// Build the canonical signed payload string for an L2 hit. FIXED-ORDER ASCII
/// with similarity + money rounded to integer micros (determinism — see the
/// module doc). The signature covers EXACTLY these bytes.
///
/// Format:
/// `l2:v1|<schema_version>|<org_id>|<trace_id>|<matched_entry_id>|<similarity_micros>|<verdict>|<served_cost_micros>|<baseline_cost_micros>|<ts>`
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn canonical_payload(
    schema_version: u32,
    org_id: Uuid,
    trace_id: Uuid,
    matched_entry_id: Uuid,
    similarity: f32,
    verdict: &str,
    served_cost_usd: f64,
    baseline_cost_usd: f64,
    ts: &str,
) -> String {
    let similarity_micros = (similarity * 1_000_000.0_f32).round() as i64;
    let served_micros = (served_cost_usd * 1_000_000.0).round() as i64;
    let baseline_micros = (baseline_cost_usd * 1_000_000.0).round() as i64;
    format!(
        "{L2_PREFIX}|{schema_version}|{org_id}|{trace_id}|{matched_entry_id}|{similarity_micros}|{verdict}|{served_micros}|{baseline_micros}|{ts}",
    )
}

/// The hex-encoded 32-byte verifying (public) key for a signing key. Embedded
/// in the receipt so verification needs no key lookup (mirrors
/// `vcr::verifying_key_hex`).
#[must_use]
pub fn verifying_key_hex(signing_key: &SigningKey) -> String {
    hex::encode(signing_key.verifying_key().to_bytes())
}

/// Sign an L2-hit receipt with `key`. Builds the canonical payload, signs it,
/// and returns the self-contained receipt (embedded verifying key +
/// signature). `ts` is the caller's RFC 3339 timestamp (the gateway passes
/// `Utc::now()`; a fixed value in tests → deterministic signatures).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn sign(
    key: &SigningKey,
    org_id: Uuid,
    trace_id: Uuid,
    matched_entry_id: Uuid,
    similarity: f32,
    verdict: &str,
    served_cost_usd: f64,
    baseline_cost_usd: f64,
    ts: &str,
) -> L2Receipt {
    let payload = canonical_payload(
        L2_SCHEMA_VERSION,
        org_id,
        trace_id,
        matched_entry_id,
        similarity,
        verdict,
        served_cost_usd,
        baseline_cost_usd,
        ts,
    );
    let sig = key.sign(payload.as_bytes());
    L2Receipt {
        schema_version: L2_SCHEMA_VERSION,
        org_id,
        trace_id,
        matched_entry_id,
        similarity,
        verdict: verdict.to_string(),
        served_cost_usd,
        baseline_cost_usd,
        ts: ts.to_string(),
        verifying_key_hex: verifying_key_hex(key),
        signature: hex::encode(sig.to_bytes()),
    }
}

/// Verify an L2-hit receipt offline against its embedded verifying key.
/// Returns `true` iff the signature is valid over the recomputed canonical
/// payload AND the schema_version is one this verifier understands.
///
/// Mirrors `vcr::verify`: reconstructs the canonical string byte-for-byte
/// from the receipt fields (the determinism contract), so ANY field tamper
/// after signing → `false`.
#[must_use]
pub fn verify(receipt: &L2Receipt) -> bool {
    if receipt.schema_version != L2_SCHEMA_VERSION {
        return false;
    }
    let payload = canonical_payload(
        receipt.schema_version,
        receipt.org_id,
        receipt.trace_id,
        receipt.matched_entry_id,
        receipt.similarity,
        &receipt.verdict,
        receipt.served_cost_usd,
        receipt.baseline_cost_usd,
        &receipt.ts,
    );
    let Ok(vk_bytes) = hex::decode(&receipt.verifying_key_hex) else {
        return false;
    };
    let Ok(vk_array): Result<[u8; 32], _> = vk_bytes.try_into() else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&vk_array) else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(&receipt.signature) else {
        return false;
    };
    let Ok(sig_array): Result<[u8; 64], _> = sig_bytes.try_into() else {
        return false;
    };
    let signature = Signature::from_bytes(&sig_array);
    verifying_key.verify(payload.as_bytes(), &signature).is_ok()
}

/// Verify an L2-hit receipt against an EXTERNAL verifying-key hex (not the
/// embedded one). Used by `tt verify-receipt --key-hex <hex>` when the
/// customer supplies the key out-of-band (the stronger trust model).
/// Mirrors `vcr::verify_with_key`.
#[must_use]
pub fn verify_with_key(verifying_key_hex_: &str, receipt: &L2Receipt) -> bool {
    if receipt.schema_version != L2_SCHEMA_VERSION {
        return false;
    }
    let payload = canonical_payload(
        receipt.schema_version,
        receipt.org_id,
        receipt.trace_id,
        receipt.matched_entry_id,
        receipt.similarity,
        &receipt.verdict,
        receipt.served_cost_usd,
        receipt.baseline_cost_usd,
        &receipt.ts,
    );
    let Ok(vk_bytes) = hex::decode(verifying_key_hex_) else {
        return false;
    };
    let Ok(vk_array): Result<[u8; 32], _> = vk_bytes.try_into() else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&vk_array) else {
        return false;
    };
    let Ok(sig_bytes) = hex::decode(&receipt.signature) else {
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
    use super::*;
    use crate::vcr;

    /// A fixed signing key + timestamp so signatures are deterministic across
    /// the round-trip / tamper tests (the gateway passes `Utc::now()`; here we
    /// pin both to assert byte-stable signatures). Mirrors `vcr`'s test_key.
    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }
    const FIXED_TS: &str = "2026-07-08T12:00:00Z";

    fn sign_test_receipt() -> L2Receipt {
        sign(
            &test_key(),
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            Uuid::from_u128(7),
            0.931_2,
            VERDICT_VERIFIED,
            0.0,
            0.011_7,
            FIXED_TS,
        )
    }

    #[test]
    fn sign_then_verify_round_trips_with_embedded_key() {
        let receipt = sign_test_receipt();
        assert!(
            verify(&receipt),
            "a receipt signed + verified with its embedded key must PASS"
        );
    }

    #[test]
    fn verify_with_external_pinned_key_passes() {
        let receipt = sign_test_receipt();
        let key_hex = verifying_key_hex(&test_key());
        assert!(verify_with_key(&key_hex, &receipt));
    }

    #[test]
    fn verify_with_wrong_external_key_fails() {
        let receipt = sign_test_receipt();
        let wrong = SigningKey::from_bytes(&[99u8; 32]);
        let wrong_hex = verifying_key_hex(&wrong);
        assert!(!verify_with_key(&wrong_hex, &receipt));
    }

    #[test]
    fn tamper_similarity_breaks_signature() {
        let mut receipt = sign_test_receipt();
        // Re-signing is NOT done — the signature still covers the original
        // similarity, so the recomputed canonical payload differs → FAIL.
        receipt.similarity = 0.9900;
        assert!(!verify(&receipt));
    }

    #[test]
    fn tamper_baseline_cost_breaks_signature() {
        let mut receipt = sign_test_receipt();
        receipt.baseline_cost_usd = 0.9900;
        assert!(!verify(&receipt));
    }

    #[test]
    fn tamper_verdict_breaks_signature() {
        let mut receipt = sign_test_receipt();
        receipt.verdict = VERDICT_CONFIDENT.to_string();
        assert!(!verify(&receipt));
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let mut receipt = sign_test_receipt();
        receipt.schema_version = 999;
        assert!(!verify(&receipt));
    }

    #[test]
    fn canonical_payload_uses_l2_prefix_and_is_disjoint_from_vcr() {
        let payload = canonical_payload(
            L2_SCHEMA_VERSION,
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            Uuid::from_u128(7),
            0.931_2,
            VERDICT_VERIFIED,
            0.0,
            0.011_7,
            FIXED_TS,
        );
        assert!(
            payload.starts_with("l2:v1|"),
            "the canonical payload carries the l2:v1 domain-separation prefix: {payload}"
        );
        // Disjoint from the VCR family — a `l2:v1` payload can never be a
        // `vcr:v1` payload (different prefix). The two canonical payloads are
        // not interchangeable: a signature over a `vcr:v1|…` string must NOT
        // verify as a `l2:v1|…` receipt and vice versa.
        let vcr_payload = vcr::canonical_payload(
            vcr::VCR_SCHEMA_VERSION,
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            "catalog:openai->gpt-4o-mini",
            "gpt-4o-mini",
            -1200,
            0.0034,
            FIXED_TS,
        );
        assert_ne!(
            payload, vcr_payload,
            "l2:v1 and vcr:v1 canonical payloads must be disjoint strings"
        );
        assert!(
            !vcr_payload.starts_with("l2:v1|"),
            "vcr:v1 must be disjoint from l2:v1"
        );
    }

    #[test]
    fn similarity_rounds_to_integer_micros() {
        let payload = canonical_payload(
            L2_SCHEMA_VERSION,
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            Uuid::from_u128(7),
            0.931_2,
            VERDICT_VERIFIED,
            0.0,
            0.011_7,
            FIXED_TS,
        );
        // 0.9312 * 1_000_000 = 931200.0 → 931200.
        assert!(payload.contains("|931200|"));
    }

    #[test]
    fn money_rounds_to_integer_micros() {
        let payload = canonical_payload(
            L2_SCHEMA_VERSION,
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            Uuid::from_u128(7),
            0.931_2,
            VERDICT_VERIFIED,
            0.0,
            0.011_7,
            FIXED_TS,
        );
        // 0.0117 * 1_000_000 = 11700.0 → 11700.
        assert!(payload.contains("|0|11700|"));
    }
}
