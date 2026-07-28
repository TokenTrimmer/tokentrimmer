//! `tt verify-receipt` — offline verification of a TokenTrimmer signed receipt.
//!
//! Originally the verify side of the Verifiable Compression Receipt (VCR); now
//! dispatches across receipt families by field-presence so the one CLI verifies
//! any signed TokenTrimmer receipt:
//!   - a **VCR** (`vcr:v1|…` canonical payload) — compression savings.
//!   - an **L2 receipt** (`l2:v1|…` canonical payload) — semantic-cache hit
//!     provenance (`matched_entry_id` / `similarity` / `verdict`).
//!   - a **WFR receipt** (`wfr:v1|…` through `wfr:v4|…`) — workflow-run cost,
//!     request-delta evidence, and optional quality verdict.
//!   - an **ARR receipt** (`arr:v1|…` / `arr:v2|…`) — top-level agent-run cost
//!     and request-delta evidence.
//!
//! The sign side of each lives in the cloud mint-on-demand endpoint
//! (`POST /v1/admin/requests/{trace_id}/{compression,l2}-receipt/sign`, which
//! calls `tt_telemetry::vcr::sign` / `tt_telemetry::l2_receipt::sign`; and
//! `POST /v1/admin/workflow-runs/{run_id}/receipt/sign` for wfr; and
//! `POST /v1/admin/agent-runs/{run_id}/receipt/sign` for arr). This CLI takes
//! a receipt JSON + the customer's out-of-band verifying-key hex + asserts the
//! Ed25519 signature is valid over the canonical payload — proving the figure
//! was attested by the key holder, offline, with no network or DB.
//! Mirrors `tt verify-bundle` (`crates/cli/src/bundle.rs:366-415`) +
//! `tt audit verify` (`crates/cli/src/audit.rs:53-146`).
//!
//! Exit code is non-zero on any failure (tampered receipt, wrong key, malformed
//! JSON, unknown schema version) so it drops straight into a CI gate or a
//! "download the receipt → verify it" customer flow.

use std::path::Path;

use anyhow::Context;

use tt_telemetry::vcr::VcrReceipt;

mod run_receipt;

use run_receipt::{verify_arr_receipt, verify_wfr_receipt};

/// Fields unique to the L2 receipt family. Any one of these claims L2 family
/// ownership before the permissive VCR deserializer gets a chance to ignore it.
const L2_RECEIPT_MARKERS: [&str; 5] = [
    "matched_entry_id",
    "similarity",
    "verdict",
    "served_cost_usd",
    "baseline_cost_usd",
];

/// The receipt family selected from owned JSON discriminator fields. Keep the
/// run-receipt variants separate: both ARR and WFR carry `canonical_version`,
/// but only WFR carries a `workflow_id` (and its v2/v4 `quality_verdict`).
/// Every canonical-version-bearing object is reserved by one of these two
/// paths so serde's permissive VCR parser can never silently ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptFamily {
    Bundle,
    Wfr,
    Arr,
    L2,
    Vcr,
}

fn receipt_family(peek: &serde_json::Value) -> ReceiptFamily {
    let Some(object) = peek.as_object() else {
        return ReceiptFamily::Vcr;
    };

    if object.contains_key("plan_input") || object.contains_key("expected_result") {
        return ReceiptFamily::Bundle;
    }

    if object.contains_key("canonical_version") {
        // WFR owns its workflow relationship and its v2/v4 field even when
        // those values are malformed. Everything else in the canonical-version
        // namespace is routed to ARR. Both parsers fail closed on malformed or
        // future shapes; neither can fall through to VCR.
        if object.contains_key("workflow_id") || object.contains_key("quality_verdict") {
            return ReceiptFamily::Wfr;
        }
        return ReceiptFamily::Arr;
    }

    if L2_RECEIPT_MARKERS
        .iter()
        .any(|field| object.contains_key(*field))
    {
        return ReceiptFamily::L2;
    }

    ReceiptFamily::Vcr
}

/// `tt verify-receipt --receipt <path> --key-hex <hex>` entry point. Reads the
/// receipt JSON, dispatches to the right family by field-presence,
/// reconstructs the canonical payload, verifies the signature, and prints
/// PASS/FAIL with the receipt fields. Exits non-zero on failure.
///
/// # Errors
/// Returns an error — so the process exits non-zero — when the receipt file
/// cannot be read/parsed, the key hex is malformed, or the signature does not
/// verify (tampered fields, wrong key, unknown schema version).
pub fn run_verify_receipt(receipt_path: &str, key_hex: &str) -> anyhow::Result<()> {
    use anyhow::Context;

    let raw = std::fs::read_to_string(receipt_path)
        .with_context(|| format!("read receipt {receipt_path}"))?;

    // Family dispatch by field-presence (the JSON fields are the dispatch key;
    // the signed canonical payload's prefix — `vcr:v1|` / `l2:v1|` / `wfr:v1|`
    // / `arr:v1|` — is the disjointness guarantee). Order matters:
    //   - A savings bundle carries `plan_input` or `expected_result`; it belongs
    //     to `tt verify-bundle`, never this receipt verifier.
    //   - The run-receipt namespace reserves any own `canonical_version` field.
    //     WFR claims `workflow_id` / `quality_verdict`; ARR claims every other
    //     canonical-version-bearing object. Malformed or future variants never
    //     reach VCR's permissive deserializer.
    //   - L2 reserves every L2-only field (checked before VCR, since L2 shares
    //     the `signature`/`verifying_key_hex` names). A partial/malformed L2
    //     shape must fail its own parser, never become a verified VCR because
    //     serde permissively ignores its unknown field.
    //   - VCR carries `signature` + `verifying_key_hex` (or `route`+`schema`).
    // Peek as a Value so a mismatch is a clean "unknown receipt type" error,
    // not a deserialization panic. This keeps the one CLI verifying every
    // receipt family without a `--kind` flag.
    let peek: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse receipt JSON {receipt_path}"))?;
    match receipt_family(&peek) {
        ReceiptFamily::Bundle => anyhow::bail!(
            "receipt appears to be a savings bundle; verify it with `tt verify-bundle {receipt_path}`"
        ),
        ReceiptFamily::Wfr => verify_wfr_receipt(&raw, &peek, key_hex),
        ReceiptFamily::Arr => verify_arr_receipt(&raw, &peek, key_hex),
        ReceiptFamily::L2 => verify_l2_receipt(&raw, &peek, key_hex),
        ReceiptFamily::Vcr => verify_vcr_receipt(&raw, &peek, key_hex),
    }
}

/// Verify a VCR (compression-savings) receipt.
fn verify_vcr_receipt(raw: &str, _peek: &serde_json::Value, key_hex: &str) -> anyhow::Result<()> {
    let receipt: VcrReceipt = serde_json::from_str(raw).context("parse VCR receipt JSON")?;
    crate::ui::note(&format!(
        "VCR v{} for org {} trace {} (route {}, model {})",
        receipt.schema_version, receipt.org_id, receipt.trace_id, receipt.route, receipt.model
    ));
    crate::ui::note(&format!(
        "token_delta: {}, savings_usd: {:.6}, ts: {}",
        receipt.token_delta, receipt.savings_usd, receipt.ts
    ));

    if tt_telemetry::vcr::verify_with_key(key_hex, &receipt) {
        crate::ui::ok("PASS: signature verifies against the supplied verifying key");
        Ok(())
    } else {
        crate::ui::error("FAIL: signature does not verify (tampered receipt, wrong key, or unknown schema version)");
        anyhow::bail!("VCR verification failed for trace_id={}", receipt.trace_id);
    }
}

/// Verify an L2 (semantic-cache-hit provenance) receipt.
fn verify_l2_receipt(raw: &str, _peek: &serde_json::Value, key_hex: &str) -> anyhow::Result<()> {
    let receipt: tt_telemetry::l2_receipt::L2Receipt =
        serde_json::from_str(raw).context("parse L2 receipt JSON")?;
    crate::ui::note(&format!(
        "L2-receipt v{} for org {} trace {} (matched entry {}, verdict {})",
        receipt.schema_version,
        receipt.org_id,
        receipt.trace_id,
        receipt.matched_entry_id,
        receipt.verdict,
    ));
    crate::ui::note(&format!(
        "similarity: {:.4}, served_cost_usd: {:.6}, baseline_cost_usd: {:.6}, ts: {}",
        receipt.similarity, receipt.served_cost_usd, receipt.baseline_cost_usd, receipt.ts,
    ));

    if tt_telemetry::l2_receipt::verify_with_key(key_hex, &receipt) {
        crate::ui::ok("PASS: signature verifies against the supplied verifying key");
        Ok(())
    } else {
        crate::ui::error("FAIL: signature does not verify (tampered receipt, wrong key, or unknown schema version)");
        anyhow::bail!(
            "L2-receipt verification failed for trace_id={}",
            receipt.trace_id
        );
    }
}

/// Read a receipt from a path (the `--receipt <path>` surface). Kept as a
/// separate helper so a future `--receipt-stdin` variant can share the verify
/// path.
#[allow(dead_code)]
fn read_receipt(path: &Path) -> anyhow::Result<VcrReceipt> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read receipt {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse receipt JSON {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_telemetry::vcr::{sign, verifying_key_hex, VcrReceipt};
    use uuid::Uuid;

    const WFR_V1_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v1.golden.json");
    const WFR_V2_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v2.golden.json");
    const WFR_V3_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v3.golden.json");
    const WFR_V4_GOLDEN: &str = include_str!("../../../docs/receipt-spec/wfr-v4.golden.json");
    const ARR_V1_GOLDEN: &str = include_str!("../../../docs/receipt-spec/arr-v1.golden.json");
    const ARR_V2_GOLDEN: &str = include_str!("../../../docs/receipt-spec/arr-v2.golden.json");

    /// Write a receipt to a temp file + return the path + the signing key's
    /// verifying-key hex (the customer's out-of-band key).
    fn write_test_receipt(
        dir: &std::path::Path,
        name: &str,
        receipt: &VcrReceipt,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(receipt).unwrap()).unwrap();
        path
    }

    fn write_test_json(
        dir: &std::path::Path,
        name: &str,
        value: &serde_json::Value,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
        path
    }

    fn key_hex() -> String {
        verifying_key_hex(&ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]))
    }

    fn sign_receipt() -> VcrReceipt {
        sign(
            &ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            "catalog:openai->gpt-4o-mini",
            "gpt-4o-mini",
            -1200,
            0.0034,
            "2026-07-06T20:00:00Z",
        )
    }

    #[test]
    fn verify_receipt_passes_for_a_valid_signature() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_receipt(dir.path(), "receipt.json", &sign_receipt());
        // run_verify_receipt prints + returns Ok on PASS.
        run_verify_receipt(path.to_str().unwrap(), &key_hex()).expect("a valid receipt verifies");
    }

    #[test]
    fn verify_receipt_fails_for_a_tampered_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut receipt = sign_receipt();
        receipt.savings_usd = 0.99; // tamper after signing
        let path = write_test_receipt(dir.path(), "tampered.json", &receipt);
        let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect_err("a tampered receipt must fail");
        assert!(
            format!("{err:#}").contains("VCR verification failed"),
            "the error names the failure: {err:#}"
        );
    }

    #[test]
    fn verify_receipt_fails_with_wrong_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_receipt(dir.path(), "receipt.json", &sign_receipt());
        let wrong_key = verifying_key_hex(&ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]));
        run_verify_receipt(path.to_str().unwrap(), &wrong_key)
            .expect_err("a receipt signed by key A must not verify against key B");
    }

    #[test]
    fn verify_receipt_errors_on_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not json").unwrap();
        run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect_err("malformed JSON must error");
    }

    #[test]
    fn verify_receipt_errors_on_missing_file() {
        run_verify_receipt("/nonexistent/receipt.json", &key_hex())
            .expect_err("a missing file must error");
    }

    #[test]
    fn verify_receipt_errors_on_malformed_key_hex() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_receipt(dir.path(), "receipt.json", &sign_receipt());
        run_verify_receipt(path.to_str().unwrap(), "not-hex")
            .expect_err("a malformed key hex must error (verify returns false → bail)");
    }

    #[test]
    fn canonical_version_namespace_is_reserved_for_arr_or_wfr_before_vcr() {
        let dir = tempfile::tempdir().unwrap();
        for (name, canonical_version, wfr_marker, expected_parser) in [
            (
                "arr-current",
                serde_json::json!("v1"),
                None,
                "parse ARR receipt JSON",
            ),
            (
                "arr-future",
                serde_json::json!("v999"),
                None,
                "parse ARR receipt JSON",
            ),
            (
                "arr-malformed",
                serde_json::json!({ "version": "v1" }),
                None,
                "parse ARR receipt JSON",
            ),
            (
                "wfr-current",
                serde_json::json!("v1"),
                Some(serde_json::json!(Uuid::from_u128(0xb2))),
                "parse WFR receipt JSON",
            ),
            (
                "wfr-future",
                serde_json::json!("v999"),
                Some(serde_json::json!(Uuid::from_u128(0xb2))),
                "parse WFR receipt JSON",
            ),
        ] {
            // VcrReceipt ignores unknown JSON fields when deserializing. Without
            // namespace reservation, this otherwise-valid VCR would verify.
            let mut receipt = serde_json::to_value(sign_receipt()).unwrap();
            receipt["canonical_version"] = canonical_version;
            if let Some(workflow_id) = wfr_marker {
                receipt["workflow_id"] = workflow_id;
            }
            let path = write_test_json(dir.path(), &format!("vcr-run-{name}.json"), &receipt);

            let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
                .expect_err("a run-receipt-marked VCR must not use legacy VCR verification");
            assert!(
                format!("{err:#}").contains(expected_parser),
                "{name} run-receipt discriminator must take its family parser: {err:#}",
            );
        }
    }

    #[test]
    fn quality_verdict_reserves_wfr_before_arr_or_vcr() {
        let mut receipt = serde_json::to_value(sign_receipt()).unwrap();
        receipt["canonical_version"] = serde_json::json!("v1");
        receipt["quality_verdict"] = serde_json::json!("equivalent");
        assert_eq!(receipt_family(&receipt), ReceiptFamily::Wfr);

        let dir = tempfile::tempdir().unwrap();
        let path = write_test_json(dir.path(), "wfr-quality-reservation.json", &receipt);
        let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect_err("a WFR-only field must not become an ARR or VCR");
        assert!(format!("{err:#}").contains("parse WFR receipt JSON"));
    }

    #[test]
    fn vcr_with_bundle_discriminator_is_directed_to_verify_bundle() {
        let dir = tempfile::tempdir().unwrap();
        for (field, value_kind, value) in [
            ("plan_input", "object", serde_json::json!({})),
            ("plan_input", "null", serde_json::Value::Null),
            ("expected_result", "object", serde_json::json!({})),
            ("expected_result", "empty string", serde_json::json!("")),
        ] {
            // These are otherwise-valid VCRs, so this specifically proves an
            // own bundle discriminator cannot be ignored by VCR deserialization.
            let mut receipt = serde_json::to_value(sign_receipt()).unwrap();
            receipt[field] = value;
            let path = write_test_json(
                dir.path(),
                &format!("vcr-bundle-{field}-{value_kind}.json"),
                &receipt,
            );

            let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
                .expect_err("a bundle-marked VCR must not use VCR verification");
            assert!(
                format!("{err:#}").contains("tt verify-bundle"),
                "{field} ({value_kind}) must direct the user to the bundle verifier: {err:#}",
            );
        }
    }

    #[test]
    fn vcr_with_any_l2_marker_never_falls_through_to_vcr() {
        let dir = tempfile::tempdir().unwrap();
        for (field, value) in [
            ("matched_entry_id", serde_json::json!(Uuid::from_u128(7))),
            ("similarity", serde_json::json!(0.9312)),
            ("verdict", serde_json::json!("verified")),
            ("served_cost_usd", serde_json::json!(0.0)),
            ("baseline_cost_usd", serde_json::json!(0.0117)),
        ] {
            // VcrReceipt ignores unknown JSON fields when deserializing. Each
            // lone L2 marker must therefore claim L2 ownership before that
            // fallback can verify the original VCR signature.
            let mut receipt = serde_json::to_value(sign_receipt()).unwrap();
            receipt[field] = value;
            let path = write_test_json(dir.path(), &format!("vcr-l2-{field}.json"), &receipt);

            let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
                .expect_err("an L2-marked VCR must not use VCR verification");
            assert!(
                format!("{err:#}").contains("parse L2 receipt JSON"),
                "{field} must take the L2 parser: {err:#}",
            );
        }
    }

    #[test]
    fn bundle_and_run_receipts_remain_ahead_of_l2_markers() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = serde_json::to_value(sign_receipt()).unwrap();
        bundle["plan_input"] = serde_json::json!({});
        bundle["matched_entry_id"] = serde_json::json!(Uuid::from_u128(7));
        let bundle_path = write_test_json(dir.path(), "bundle-before-l2.json", &bundle);
        let bundle_err = run_verify_receipt(bundle_path.to_str().unwrap(), &key_hex())
            .expect_err("a bundle must retain priority over an L2 marker");
        assert!(format!("{bundle_err:#}").contains("tt verify-bundle"));

        let mut wfr = serde_json::to_value(sign_receipt()).unwrap();
        wfr["canonical_version"] = serde_json::json!("v1");
        wfr["workflow_id"] = serde_json::json!(Uuid::from_u128(0xb2));
        wfr["matched_entry_id"] = serde_json::json!(Uuid::from_u128(7));
        let wfr_path = write_test_json(dir.path(), "wfr-before-l2.json", &wfr);
        let wfr_err = run_verify_receipt(wfr_path.to_str().unwrap(), &key_hex())
            .expect_err("a WFR marker must retain priority over an L2 marker");
        assert!(format!("{wfr_err:#}").contains("parse WFR receipt JSON"));

        let mut arr = serde_json::to_value(sign_receipt()).unwrap();
        arr["canonical_version"] = serde_json::json!("v1");
        arr["matched_entry_id"] = serde_json::json!(Uuid::from_u128(7));
        let arr_path = write_test_json(dir.path(), "arr-before-l2.json", &arr);
        let arr_err = run_verify_receipt(arr_path.to_str().unwrap(), &key_hex())
            .expect_err("an ARR marker must retain priority over an L2 marker");
        assert!(format!("{arr_err:#}").contains("parse ARR receipt JSON"));
    }

    // ── L2 (semantic-cache-hit) receipt dispatch + verify ────────────────────
    // The same `tt verify-receipt` CLI verifies a complete L2 receipt while
    // reserving every L2-only field before permissive VCR deserialization.

    use tt_telemetry::l2_receipt::{
        sign as sign_l2, L2Receipt, L2_SCHEMA_VERSION, VERDICT_VERIFIED,
    };

    fn sign_l2_receipt() -> L2Receipt {
        sign_l2(
            &ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
            Uuid::from_u128(42),
            Uuid::from_u128(99),
            Uuid::from_u128(7),
            0.9312,
            VERDICT_VERIFIED,
            0.0,
            0.0117,
            "2026-07-08T12:00:00Z",
        )
    }

    fn write_test_l2_receipt(
        dir: &std::path::Path,
        name: &str,
        receipt: &L2Receipt,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(receipt).unwrap()).unwrap();
        path
    }

    #[test]
    fn l2_receipt_verifies_via_dispatch() {
        // The same run_verify_receipt entry point routes an L2 receipt to the
        // L2 verify path (dispatched by field-presence) + verifies it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_l2_receipt(dir.path(), "l2.json", &sign_l2_receipt());
        run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect("a valid L2 receipt verifies via the dispatch path");
    }

    #[test]
    fn l2_receipt_fails_when_tampered() {
        let dir = tempfile::tempdir().unwrap();
        let mut receipt = sign_l2_receipt();
        receipt.similarity = 0.9900; // tamper after signing
        let path = write_test_l2_receipt(dir.path(), "l2-tampered.json", &receipt);
        let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect_err("a tampered L2 receipt must fail");
        assert!(
            format!("{err:#}").contains("L2-receipt verification failed"),
            "the error names the L2 failure: {err:#}"
        );
    }

    #[test]
    fn l2_receipt_carrying_unknown_schema_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut receipt = sign_l2_receipt();
        receipt.schema_version = L2_SCHEMA_VERSION + 999;
        let path = write_test_l2_receipt(dir.path(), "l2-unknown.json", &receipt);
        run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect_err("an unknown L2 schema version must fail");
    }

    #[test]
    fn vcr_and_l2_receipts_dispatch_independently() {
        // A VCR receipt + an L2 receipt at the same key both verify through
        // the one entry point — the dispatch is by field-presence, not a flag.
        let dir = tempfile::tempdir().unwrap();
        let vcr_path = write_test_receipt(dir.path(), "vcr.json", &sign_receipt());
        let l2_path = write_test_l2_receipt(dir.path(), "l2.json", &sign_l2_receipt());
        run_verify_receipt(vcr_path.to_str().unwrap(), &key_hex())
            .expect("VCR dispatches to the VCR path");
        run_verify_receipt(l2_path.to_str().unwrap(), &key_hex())
            .expect("L2 dispatches to the L2 path");
    }

    // ── WFR (workflow-run) receipt dispatch + verify ─────────────────────────
    // The same `tt verify-receipt` CLI verifies a WFR receipt (dispatched by
    // field-presence: canonical_version + signature_hex). The cloud mint signs
    // these; here we sign over the public canonical_payload to produce a
    // verifiable fixture (the verify side is what the CLI owns).

    use tt_telemetry::wfr_receipt::{canonical_payload as wfr_canonical, WfrReceipt};

    fn sample_wfr_receipt(canonical_version: &str, quality_verdict: Option<&str>) -> WfrReceipt {
        use ed25519_dalek::Signer as _;
        // Sign over the canonical payload the way the cloud mint does, using the
        // same fixed test key as the other families so key_hex() verifies it.
        let request_delta = matches!(canonical_version, "v3" | "v4");
        let mut receipt = WfrReceipt {
            run_id: Uuid::from_u128(0xa1),
            org_id: Uuid::from_u128(42),
            workflow_id: Uuid::from_u128(0xb2),
            status: "completed".to_string(),
            cost_micros: 70_000,
            baseline_micros: 180_000,
            saved_micros: 110_000,
            signed_request_delta_micros: request_delta.then_some(110_000),
            request_delta_formula_version: request_delta
                .then(|| tt_shared::REQUEST_DELTA_ESTIMATE_V1.to_string()),
            request_delta_eligible_requests: request_delta.then_some(2),
            request_delta_measured_requests: request_delta.then_some(2),
            cost_usd: None,
            baseline_usd: None,
            saved_usd: None,
            signed_request_delta_usd: None,
            signature_hex: String::new(),
            verifying_key_hex: String::new(),
            canonical_version: canonical_version.to_string(),
            quality_verdict: quality_verdict.map(str::to_string),
            signed_at: None,
        };
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let payload = wfr_canonical(&receipt).expect("canonical payload builds");
        let sig = key.sign(payload.as_bytes());
        receipt.signature_hex = hex::encode(sig.to_bytes());
        receipt.verifying_key_hex = hex::encode(key.verifying_key().to_bytes());
        receipt
    }

    fn write_test_wfr_receipt(
        dir: &std::path::Path,
        name: &str,
        receipt: &WfrReceipt,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(receipt).unwrap()).unwrap();
        path
    }

    #[test]
    fn wfr_v1_receipt_verifies_via_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let path =
            write_test_wfr_receipt(dir.path(), "wfr-v1.json", &sample_wfr_receipt("v1", None));
        run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect("a valid WFR v1 receipt verifies via the dispatch path");
    }

    #[test]
    fn wfr_v2_receipt_verifies_via_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_wfr_receipt(
            dir.path(),
            "wfr-v2.json",
            &sample_wfr_receipt("v2", Some("equivalent")),
        );
        run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect("a valid WFR v2 receipt verifies via the dispatch path");
    }

    #[test]
    fn wfr_v3_and_v4_receipts_verify_via_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        for (name, receipt) in [
            ("wfr-v3.json", sample_wfr_receipt("v3", None)),
            ("wfr-v4.json", sample_wfr_receipt("v4", Some("equivalent"))),
        ] {
            let path = write_test_wfr_receipt(dir.path(), name, &receipt);
            run_verify_receipt(path.to_str().unwrap(), &key_hex())
                .expect("a request-delta WFR verifies through CLI dispatch");
        }
    }

    #[test]
    fn checked_in_wfr_golden_vectors_verify_via_cli_dispatch() {
        // These are static cross-language vectors, not test-time signatures.
        // They pin the documented JSON wire shape, canonical payload bytes,
        // Ed25519 encoding, and field-presence dispatch together.
        let dir = tempfile::tempdir().unwrap();
        for (name, raw) in [
            ("wfr-v1.golden.json", WFR_V1_GOLDEN),
            ("wfr-v2.golden.json", WFR_V2_GOLDEN),
            ("wfr-v3.golden.json", WFR_V3_GOLDEN),
            ("wfr-v4.golden.json", WFR_V4_GOLDEN),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, raw).expect("write checked-in WFR vector");
            run_verify_receipt(path.to_str().unwrap(), &key_hex())
                .expect("checked-in WFR vector must verify through CLI dispatch");
        }
    }

    #[test]
    fn wfr_receipt_fails_when_tampered() {
        let dir = tempfile::tempdir().unwrap();
        let mut receipt = sample_wfr_receipt("v2", Some("equivalent"));
        receipt.saved_micros = 999_999; // tamper after signing
        let path = write_test_wfr_receipt(dir.path(), "wfr-tampered.json", &receipt);
        let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect_err("a tampered WFR receipt must fail");
        assert!(
            format!("{err:#}").contains("WFR-receipt verification failed"),
            "the error names the WFR failure: {err:#}"
        );
    }

    #[test]
    fn wfr_receipt_fails_when_a_legacy_empty_status_payload_is_signed() {
        use ed25519_dalek::Signer as _;

        let dir = tempfile::tempdir().unwrap();
        let mut receipt = sample_wfr_receipt("v2", Some("equivalent"));
        receipt.status.clear();

        // Sign the malformed legacy bytes directly. The current canonicalizer
        // must reject this receipt before its otherwise-valid Ed25519 signature
        // can produce a CLI PASS.
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let legacy_payload = format!(
            "wfr:v2|{}|{}|{}|{}|{}|{}||equivalent",
            receipt.org_id,
            receipt.workflow_id,
            receipt.run_id,
            receipt.cost_micros,
            receipt.baseline_micros,
            receipt.saved_micros,
        );
        receipt.signature_hex = hex::encode(key.sign(legacy_payload.as_bytes()).to_bytes());
        receipt.verifying_key_hex = hex::encode(key.verifying_key().to_bytes());

        let path = write_test_wfr_receipt(dir.path(), "wfr-empty-status.json", &receipt);
        let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect_err("an empty-status WFR must fail despite a legacy-valid signature");
        assert!(format!("{err:#}").contains("WFR-receipt verification failed"));
    }

    #[test]
    fn arr_and_wfr_future_versions_remain_in_their_own_verifiers() {
        let dir = tempfile::tempdir().unwrap();

        let mut arr = sample_arr_receipt();
        arr.canonical_version = "v999".to_string();
        let arr_path = write_test_arr_receipt(dir.path(), "arr-future.json", &arr);
        let arr_err = run_verify_receipt(arr_path.to_str().unwrap(), &key_hex())
            .expect_err("a future ARR version must not fall through to VCR");
        assert!(format!("{arr_err:#}").contains("ARR-receipt verification failed"));

        let mut wfr = sample_wfr_receipt("v1", None);
        wfr.canonical_version = "v999".to_string();
        let wfr_path = write_test_wfr_receipt(dir.path(), "wfr-future.json", &wfr);
        let wfr_err = run_verify_receipt(wfr_path.to_str().unwrap(), &key_hex())
            .expect_err("a future WFR version must not fall through to VCR");
        assert!(format!("{wfr_err:#}").contains("WFR-receipt verification failed"));
    }

    // ── ARR (agent-run) receipt dispatch + verify ───────────────────────────
    // ARR intentionally has no workflow_id. The `canonical_version` namespace
    // therefore routes to ARR unless a WFR-only field claims WFR ownership.

    use tt_telemetry::arr_receipt::{canonical_payload as arr_canonical, AgentRunReceipt};

    fn sample_arr_receipt() -> AgentRunReceipt {
        use ed25519_dalek::Signer as _;

        let mut receipt = AgentRunReceipt {
            run_id: Uuid::from_u128(0xa1),
            org_id: Uuid::from_u128(42),
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
            signed_at: None,
        };
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let payload = arr_canonical(&receipt).expect("canonical ARR payload builds");
        let signature = key.sign(payload.as_bytes());
        receipt.signature_hex = hex::encode(signature.to_bytes());
        receipt.verifying_key_hex = hex::encode(key.verifying_key().to_bytes());
        receipt
    }

    fn write_test_arr_receipt(
        dir: &std::path::Path,
        name: &str,
        receipt: &AgentRunReceipt,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_string_pretty(receipt).unwrap()).unwrap();
        path
    }

    #[test]
    fn arr_v1_receipt_verifies_via_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_arr_receipt(dir.path(), "arr-v1.json", &sample_arr_receipt());
        run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect("a valid ARR v1 receipt verifies via the dispatch path");
    }

    #[test]
    fn checked_in_arr_golden_vector_verifies_via_cli_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        for (name, raw) in [
            ("arr-v1.golden.json", ARR_V1_GOLDEN),
            ("arr-v2.golden.json", ARR_V2_GOLDEN),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, raw).expect("write checked-in ARR vector");
            run_verify_receipt(path.to_str().unwrap(), &key_hex())
                .expect("checked-in ARR vector must verify through CLI dispatch");
        }
    }

    #[test]
    fn request_delta_formula_or_signed_money_tampering_fails_cli_verification() {
        let dir = tempfile::tempdir().unwrap();
        for (name, raw, field, replacement) in [
            (
                "wfr-v3-formula-tampered.json",
                WFR_V3_GOLDEN,
                "request_delta_formula_version",
                serde_json::json!("tt.request-delta-estimate.v2"),
            ),
            (
                "arr-v2-delta-tampered.json",
                ARR_V2_GOLDEN,
                "signed_request_delta_micros",
                serde_json::json!(-49_999),
            ),
        ] {
            let mut receipt_json: serde_json::Value = serde_json::from_str(raw).unwrap();
            receipt_json[field] = replacement;
            let path = write_test_json(dir.path(), name, &receipt_json);
            run_verify_receipt(path.to_str().unwrap(), &key_hex())
                .expect_err("tampered request-delta evidence must not verify");
        }
    }

    #[test]
    fn arr_receipt_fails_when_tampered() {
        let dir = tempfile::tempdir().unwrap();
        let mut receipt = sample_arr_receipt();
        receipt.saved_micros = 999_999;
        let path = write_test_arr_receipt(dir.path(), "arr-tampered.json", &receipt);
        let err = run_verify_receipt(path.to_str().unwrap(), &key_hex())
            .expect_err("a tampered ARR receipt must fail");
        assert!(
            format!("{err:#}").contains("ARR-receipt verification failed"),
            "the error names the failure: {err:#}"
        );
    }

    #[test]
    fn vcr_l2_wfr_and_arr_receipts_dispatch_independently() {
        // All four families at the same key verify through the one entry point.
        let dir = tempfile::tempdir().unwrap();
        let vcr_path = write_test_receipt(dir.path(), "vcr.json", &sign_receipt());
        let l2_path = write_test_l2_receipt(dir.path(), "l2.json", &sign_l2_receipt());
        let wfr_path = write_test_wfr_receipt(
            dir.path(),
            "wfr.json",
            &sample_wfr_receipt("v2", Some("equivalent")),
        );
        let arr_path = write_test_arr_receipt(dir.path(), "arr.json", &sample_arr_receipt());
        run_verify_receipt(vcr_path.to_str().unwrap(), &key_hex())
            .expect("VCR dispatches to the VCR path");
        run_verify_receipt(l2_path.to_str().unwrap(), &key_hex())
            .expect("L2 dispatches to the L2 path");
        run_verify_receipt(wfr_path.to_str().unwrap(), &key_hex())
            .expect("WFR dispatches to the WFR path");
        run_verify_receipt(arr_path.to_str().unwrap(), &key_hex())
            .expect("ARR dispatches to the ARR path");
    }
}
