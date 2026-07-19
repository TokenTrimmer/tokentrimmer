//! The Verifiable Compression Receipt (VCR) — a signed, offline-verifiable proof
//! of what a content-aware compression saved.
//!
//! P2a ships a MINIMAL DETERMINISTIC VCR on the existing P1a-d path (NO learned
//! model): for every compression that removed tokens, the gateway signs a
//! `{org_id, trace_id, route, model, token_delta, savings_usd, ts}` receipt with
//! the SAME Ed25519 key it signs audit-chain entries with
//! (`TT_AUDIT_SIGNING_KEY`, already in server state as `audit_signing_key`) and
//! emits it out-of-band (a structured `tt::vcr::receipt` log). The customer
//! verifies it offline with `tt verify-receipt` against the embedded verifying
//! key — competitors can reproduce the savings number but can't sign it.
//!
//! # Why a new signature family (not a reuse of the audit chain)
//! The audit chain is a per-org hash-CHAINED ledger (each entry's hash covers
//! the prior entry's hash → tamper-evident in sequence, but a single receipt
//! can't be verified in isolation without the chain prefix). A VCR is a
//! DETACHED per-event receipt: it must verify standalone, with only the
//! public key + the receipt itself. So the VCR mirrors the attestation +
//! pdf-signature families (`cloud/.../attestation/crypto.rs`,
//! `cloud/.../pdf_signature.rs`): a canonical ASCII string with a disjoint
//! domain-separation prefix (`vcr:v1|`), signed directly (no chain), the
//! signature + the verifying key embedded with the receipt.
//!
//! # Domain separation (the safety property)
//! The three existing families (`att:`/`pdf:v1|`/bare-32B-audit-hash) are
//! verified-disjoint (the `pdf_signature.rs:115-139` test proves it) so a
//! signature from one family can never be mis-validated as another. The VCR
//! picks `vcr:v1|` — disjoint from all three — and the `domain_separation`
//! test below pins that.
//!
//! # Determinism (the attestation lesson)
//! Floating-point field-order breaks signature determinism. The canonical
//! payload is a FIXED-ORDER ASCII string with money rounded to integer micros
//! (`(savings_usd * 1_000_000.0).round() as i64`) — NOT a `serde_json::Value`
//! (field order is not stable across serializers). `0.0034` and `0.0034001`
//! round to the same micros → the same canonical string → the same signature.
//!
//! # Fail-open (the production posture)
//! When `TT_AUDIT_SIGNING_KEY` is unset (the default), `audit_signing_key` is
//! `None` and the gateway emits NO signed receipt (a metrics-only event) —
//! never crashes, never blocks. Mirrors the PDF-report behavior
//! (`pdf_report.rs:238-246`). The receipt is an out-of-band proof surface; it
//! is NEVER returned in the response body (the customer verifies it offline).

use uuid::Uuid;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

/// The domain-separation prefix for VCR v1 receipts. Disjoint from `att:` /
/// `pdf:v1|` / the bare-32B audit hash. Bumped only on a breaking canonical
/// shape change (a v2 would change the prefix token + the field order together).
pub const VCR_PREFIX: &str = "vcr:v1";

/// The VCR schema version. Bumped only on a breaking shape change; a verifier
/// refuses a receipt whose version it does not understand rather than silently
/// mis-reading it (mirrors `SavingsBundle::BUNDLE_SCHEMA_VERSION`).
pub const VCR_SCHEMA_VERSION: u32 = 1;

/// A signed Verifiable Compression Receipt. Serialized as JSON (the structured
/// `tt::vcr::receipt` log carries it); `tt verify-receipt --receipt <json>`
/// deserializes + verifies it offline.
///
/// `signature` is a hex-encoded 64-byte Ed25519 signature over the canonical
/// payload (see [`canonical_payload`]). `verifying_key_hex` is the hex-encoded
/// 32-byte Ed25519 public key the receipt was signed with, embedded so the
/// receipt is self-identifying (the customer does NOT need to look up the key).
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct VcrReceipt {
    /// See [`VCR_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The org whose request was compressed (joins `request_logs.org_id`).
    pub org_id: Uuid,
    /// The request's `trace_id` (joins `request_logs.trace_id` +
    /// `quality_verdicts.request_id` when RUNG 3 gold accumulates — P2c).
    pub trace_id: Uuid,
    /// The matched route's reserved name (e.g. `catalog:openai->gpt-4o-mini`)
    /// or the user route name. Lets the customer attribute the saving to a
    /// route decision.
    pub route: String,
    /// The served model id (post-routing/pin).
    pub model: String,
    /// The pipeline-MEASURED input-token delta: negative = tokens removed
    /// (the compression's effect). Signed so a future audit-neutral transform
    /// round-trips honestly (a positive delta would be a rejected transform →
    /// no receipt is emitted).
    pub token_delta: i64,
    /// The ISOLATED estimated saving in USD (mirrors
    /// `content_compress_saved_est_usd`: an estimate, NOT the invoice-reconciled
    /// headline). Canonicalized to integer micros in the signed payload.
    pub savings_usd: f64,
    /// RFC 3339 timestamp the receipt was produced (informational; explicitly
    /// NOT part of any reproduction check beyond signature validity).
    pub ts: String,
    /// Hex-encoded 32-byte Ed25519 verifying (public) key the receipt was
    /// signed with. Embedded so verification needs no external key lookup.
    pub verifying_key_hex: String,
    /// Hex-encoded 64-byte Ed25519 signature over [`canonical_payload`].
    pub signature: String,
}

/// Build the canonical signed payload string for a receipt. FIXED-ORDER ASCII
/// with a `vcr:v1|` prefix + money rounded to integer micros (determinism — see
/// the module doc). The signature covers EXACTLY these bytes.
///
/// Format: `vcr:v1|<schema_version>|<org_id>|<trace_id>|<route>|<model>|<token_delta>|<savings_micros>|<ts>`
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn canonical_payload(
    schema_version: u32,
    org_id: Uuid,
    trace_id: Uuid,
    route: &str,
    model: &str,
    token_delta: i64,
    savings_usd: f64,
    ts: &str,
) -> String {
    let savings_micros = (savings_usd * 1_000_000.0).round() as i64;
    format!(
        "{VCR_PREFIX}|{schema_version}|{org_id}|{trace_id}|{route}|{model}|{token_delta}|{savings_micros}|{ts}"
    )
}

/// The hex-encoded 32-byte verifying (public) key for a signing key. The
/// gateway embeds this in the receipt so verification needs no key lookup —
/// mirrors `BundleAttestation.verifying_key` + the attestation public response.
#[must_use]
pub fn verifying_key_hex(signing_key: &SigningKey) -> String {
    hex::encode(signing_key.verifying_key().to_bytes())
}

/// Sign a VCR receipt with `key`. Builds the canonical payload, signs it, and
/// returns the self-contained receipt (embedded verifying key + signature).
/// `ts` is the caller's RFC 3339 timestamp (the gateway passes `Utc::now()`; a
/// fixed value in tests → deterministic signatures).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn sign(
    key: &SigningKey,
    org_id: Uuid,
    trace_id: Uuid,
    route: &str,
    model: &str,
    token_delta: i64,
    savings_usd: f64,
    ts: &str,
) -> VcrReceipt {
    let payload = canonical_payload(
        VCR_SCHEMA_VERSION,
        org_id,
        trace_id,
        route,
        model,
        token_delta,
        savings_usd,
        ts,
    );
    let sig = key.sign(payload.as_bytes());
    VcrReceipt {
        schema_version: VCR_SCHEMA_VERSION,
        org_id,
        trace_id,
        route: route.to_string(),
        model: model.to_string(),
        token_delta,
        savings_usd,
        ts: ts.to_string(),
        verifying_key_hex: verifying_key_hex(key),
        signature: hex::encode(sig.to_bytes()),
    }
}

/// Verify a VCR receipt offline against its embedded verifying key. Returns
/// `true` iff the signature is valid over the recomputed canonical payload AND
/// the schema_version is one this verifier understands.
///
/// Reconstructs the canonical string byte-for-byte from the receipt fields
/// (the determinism contract), so ANY field tamper after signing → `false`.
/// Mirrors `attestation::verify_payload_v3`.
#[must_use]
pub fn verify(receipt: &VcrReceipt) -> bool {
    if receipt.schema_version != VCR_SCHEMA_VERSION {
        return false;
    }
    let payload = canonical_payload(
        receipt.schema_version,
        receipt.org_id,
        receipt.trace_id,
        &receipt.route,
        &receipt.model,
        receipt.token_delta,
        receipt.savings_usd,
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

/// Verify a VCR receipt against an EXTERNAL verifying-key hex (not the embedded
/// one). Used by `tt verify-receipt --key-hex <hex>` when the customer supplies
/// the key out-of-band (the stronger trust model: the embedded key could be
/// forged by a compromised gateway; the out-of-band key is what the customer
/// pinned).
#[must_use]
pub fn verify_with_key(verifying_key_hex_: &str, receipt: &VcrReceipt) -> bool {
    if receipt.schema_version != VCR_SCHEMA_VERSION {
        return false;
    }
    let payload = canonical_payload(
        receipt.schema_version,
        receipt.org_id,
        receipt.trace_id,
        &receipt.route,
        &receipt.model,
        receipt.token_delta,
        receipt.savings_usd,
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

    /// A fixed signing key + timestamp so signatures are deterministic across
    /// the round-trip / tamper tests (the gateway passes `Utc::now()`; here we
    /// pin both to assert byte-stable signatures).
    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }
    const FIXED_TS: &str = "2026-07-06T20:00:00Z";

    fn sign_test_receipt() -> VcrReceipt {
        sign(
            &test_key(),
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            "catalog:openai->gpt-4o-mini",
            "gpt-4o-mini",
            -1200,
            0.0034,
            FIXED_TS,
        )
    }

    #[test]
    fn sign_then_verify_round_trips_with_embedded_key() {
        let receipt = sign_test_receipt();
        assert!(
            verify(&receipt),
            "a freshly-signed receipt verifies offline"
        );
        // And against the out-of-band key (the customer's stronger trust model).
        assert!(verify_with_key(&verifying_key_hex(&test_key()), &receipt));
    }

    #[test]
    fn tamper_any_field_fails_verify() {
        let mut r = sign_test_receipt();
        assert!(verify(&r));

        // Each field tamper in isolation → verify must fail.
        r.savings_usd = 0.99;
        assert!(!verify(&r), "tampered savings_usd must fail");

        let mut r = sign_test_receipt();
        r.token_delta = 5;
        assert!(!verify(&r), "tampered token_delta must fail");

        let mut r = sign_test_receipt();
        r.route = "catalog:anthropic->claude-haiku-4-5".into();
        assert!(!verify(&r), "tampered route must fail");

        let mut r = sign_test_receipt();
        r.trace_id = Uuid::from_u128(1000);
        assert!(!verify(&r), "tampered trace_id must fail");

        let mut r = sign_test_receipt();
        r.ts = "2026-08-01T00:00:00Z".into();
        assert!(!verify(&r), "tampered ts must fail");

        let mut r = sign_test_receipt();
        r.model = "gpt-4o".into();
        assert!(!verify(&r), "tampered model must fail");

        let mut r = sign_test_receipt();
        r.org_id = Uuid::from_u128(43);
        assert!(!verify(&r), "tampered org_id must fail");
    }

    #[test]
    fn wrong_key_fails() {
        let receipt = sign_test_receipt();
        let other_vk = verifying_key_hex(&SigningKey::from_bytes(&[9u8; 32]));
        assert!(
            !verify_with_key(&other_vk, &receipt),
            "a receipt signed by key A must not verify against key B's public hex"
        );
    }

    #[test]
    fn garbage_signature_fails() {
        let mut r = sign_test_receipt();
        r.signature = "deadbeef".into();
        assert!(!verify(&r), "a non-64-byte signature must fail");

        let mut r = sign_test_receipt();
        r.signature = "zz".repeat(32); // 64 hex chars but non-hex
        assert!(!verify(&r), "non-hex signature bytes must fail");
    }

    #[test]
    fn garbage_verifying_key_fails() {
        let mut r = sign_test_receipt();
        r.verifying_key_hex = "00".repeat(16); // 32 hex chars = 16 bytes, NOT 32
        assert!(!verify(&r), "a non-32-byte verifying key must fail");
    }

    #[test]
    fn wrong_schema_version_fails() {
        let mut r = sign_test_receipt();
        r.schema_version = 99;
        assert!(
            !verify(&r),
            "an unknown schema version must fail (not silently mis-validated)"
        );
    }

    #[test]
    fn canonical_payload_uses_vcr_prefix_and_is_disjoint_from_other_families() {
        let payload = canonical_payload(
            VCR_SCHEMA_VERSION,
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            "catalog:openai->gpt-4o-mini",
            "gpt-4o-mini",
            -1200,
            0.0034,
            FIXED_TS,
        );
        assert!(
            payload.starts_with("vcr:v1|"),
            "the canonical payload carries the vcr:v1 domain-separation prefix: {payload}"
        );
        assert!(
            !payload.starts_with("att:"),
            "vcr: must be disjoint from att: (no cross-family mis-validation)"
        );
        assert!(
            !payload.starts_with("pdf:v1|"),
            "vcr: must be disjoint from pdf:v1|"
        );
        assert_ne!(
            payload.len(),
            32,
            "vcr: must not collide with a bare 32-byte audit hash"
        );
    }

    #[test]
    fn money_canonicalizes_to_integer_micros_for_determinism() {
        // 0.0034 and 0.0034001 round to the same integer micros (3_400_000 vs
        // 3_400_000.1 → both round to 3_400_000) → the same canonical string →
        // the same signature. (The attestation determinism lesson.)
        let r_a = sign(
            &test_key(),
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            "r",
            "m",
            -1,
            0.0034,
            FIXED_TS,
        );
        let r_b = sign(
            &test_key(),
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            "r",
            "m",
            -1,
            0.0034001,
            FIXED_TS,
        );
        assert_eq!(
            r_a.signature, r_b.signature,
            "sub-micro money differences must NOT change the signature (determinism)"
        );

        // But 0.0034 vs 0.0035 differ at the micros level → different signatures.
        let r_c = sign(
            &test_key(),
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            "r",
            "m",
            -1,
            0.0035,
            FIXED_TS,
        );
        assert_ne!(
            r_a.signature, r_c.signature,
            "a real money difference MUST change the signature (tamper-evidence)"
        );
    }

    #[test]
    fn verifying_key_hex_round_trips() {
        let key = test_key();
        let hex = verifying_key_hex(&key);
        let bytes = hex::decode(&hex).unwrap();
        let arr: [u8; 32] = bytes.try_into().unwrap();
        let vk = VerifyingKey::from_bytes(&arr).unwrap();
        assert_eq!(
            vk,
            key.verifying_key(),
            "the embedded hex decodes back to the signing key's verifying key"
        );
    }
}
