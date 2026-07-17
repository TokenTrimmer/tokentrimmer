//! `tt verify-receipt` — offline verification of a TokenTrimmer signed receipt.
//!
//! Originally the verify side of the Verifiable Compression Receipt (VCR); now
//! dispatches across receipt families by field-presence so the one CLI verifies
//! any signed TokenTrimmer receipt:
//!   - a **VCR** (`vcr:v1|…` canonical payload) — compression savings.
//!   - an **L2 receipt** (`l2:v1|…` canonical payload) — semantic-cache hit
//!     provenance (`matched_entry_id` / `similarity` / `verdict`).
//!   - a **WFR receipt** (`wfr:v1|…` / `wfr:v2|…` canonical payload) — workflow
//!     run cost + savings (+ optional quality verdict).
//!
//! The sign side of each lives in the cloud mint-on-demand endpoint
//! (`POST /v1/admin/requests/{trace_id}/{compression,l2}-receipt/sign`, which
//! calls `tt_telemetry::vcr::sign` / `tt_telemetry::l2_receipt::sign`; and
//! `POST /v1/admin/workflow-runs/{run_id}/receipt/sign` for wfr). This
//! CLI takes a receipt JSON + the customer's out-of-band verifying-key hex +
//! asserts the Ed25519 signature is valid over the canonical payload — proving
//! the figure was attested by the key holder, offline, with no network or DB.
//! Mirrors `tt verify-bundle` (`crates/cli/src/bundle.rs:366-415`) +
//! `tt audit verify` (`crates/cli/src/audit.rs:53-146`).
//!
//! Exit code is non-zero on any failure (tampered receipt, wrong key, malformed
//! JSON, unknown schema version) so it drops straight into a CI gate or a
//! "download the receipt → verify it" customer flow.

use std::path::Path;

use anyhow::Context;

use tt_telemetry::vcr::VcrReceipt;

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
    // the signed canonical payload's prefix — `vcr:v1|` / `l2:v1|` / `wfr:v1|` —
    // is the disjointness guarantee). Order matters:
    //   - WFR carries `canonical_version` + `signature_hex` (the discriminator —
    //     checked before VCR, since a WFR also carries a `verifying_key_hex`).
    //   - L2 carries `matched_entry_id` + `verdict` (checked before VCR, since
    //     L2 shares the `signature`/`verifying_key_hex` names).
    //   - VCR carries `signature` + `verifying_key_hex` (or `route`+`schema`).
    // Peek as a Value so a mismatch is a clean "unknown receipt type" error,
    // not a deserialization panic. This keeps the one CLI verifying every
    // receipt family without a `--kind` flag.
    let peek: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse receipt JSON {receipt_path}"))?;
    let is_wfr = (peek.get("canonical_version").and_then(|v| v.as_str()) == Some("v1")
        || peek.get("canonical_version").and_then(|v| v.as_str()) == Some("v2"))
        && peek.get("signature_hex").is_some();
    let is_l2 = peek.get("matched_entry_id").is_some() && peek.get("verdict").is_some();

    if is_wfr {
        verify_wfr_receipt(&raw, &peek, key_hex)
    } else if is_l2 {
        verify_l2_receipt(&raw, &peek, key_hex)
    } else {
        verify_vcr_receipt(&raw, &peek, key_hex)
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

/// Verify a WFR (workflow-run) receipt (v1 cost-only or v2 cost + quality verdict).
/// Money fields are already integer micro-USD, so there's no float
/// canonicalization (unlike VCR/L2) — the canonical payload is built from the
/// `*_micros` integers directly.
fn verify_wfr_receipt(raw: &str, _peek: &serde_json::Value, key_hex: &str) -> anyhow::Result<()> {
    let receipt: tt_telemetry::wfr_receipt::WfrReceipt =
        serde_json::from_str(raw).context("parse WFR receipt JSON")?;
    crate::ui::note(&format!(
        "WFR-receipt {} for org {} workflow {} run {} (status {})",
        receipt.canonical_version,
        receipt.org_id,
        receipt.workflow_id,
        receipt.run_id,
        receipt.status,
    ));
    crate::ui::note(&format!(
        "cost_micros: {}, baseline_micros: {}, saved_micros: {}{}",
        receipt.cost_micros,
        receipt.baseline_micros,
        receipt.saved_micros,
        receipt
            .quality_verdict
            .as_deref()
            .map(|q| format!(", quality_verdict: {q}"))
            .unwrap_or_default(),
    ));

    if tt_telemetry::wfr_receipt::verify_with_key(key_hex, &receipt) {
        crate::ui::ok("PASS: signature verifies against the supplied verifying key");
        Ok(())
    } else {
        crate::ui::error("FAIL: signature does not verify (tampered receipt, wrong key, unknown canonical version, or a v2 receipt missing its quality_verdict)");
        anyhow::bail!(
            "WFR-receipt verification failed for run_id={}",
            receipt.run_id
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

    // ── L2 (semantic-cache-hit) receipt dispatch + verify ────────────────────
    // The same `tt verify-receipt` CLI verifies an L2 receipt (dispatched by
    // field-presence: matched_entry_id + verdict). Mirrors the VCR tests.

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
        let mut receipt = WfrReceipt {
            run_id: Uuid::from_u128(0xa1),
            org_id: Uuid::from_u128(42),
            workflow_id: Uuid::from_u128(0xb2),
            status: "completed".to_string(),
            cost_micros: 70_000,
            baseline_micros: 180_000,
            saved_micros: 110_000,
            cost_usd: None,
            baseline_usd: None,
            saved_usd: None,
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
    fn checked_in_wfr_golden_vectors_verify_via_cli_dispatch() {
        // These are static cross-language vectors, not test-time signatures.
        // They pin the documented JSON wire shape, canonical payload bytes,
        // Ed25519 encoding, and field-presence dispatch together.
        let dir = tempfile::tempdir().unwrap();
        for (name, raw) in [
            ("wfr-v1.golden.json", WFR_V1_GOLDEN),
            ("wfr-v2.golden.json", WFR_V2_GOLDEN),
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
    fn vcr_l2_and_wfr_receipts_dispatch_independently() {
        // All three families at the same key verify through the one entry point.
        let dir = tempfile::tempdir().unwrap();
        let vcr_path = write_test_receipt(dir.path(), "vcr.json", &sign_receipt());
        let l2_path = write_test_l2_receipt(dir.path(), "l2.json", &sign_l2_receipt());
        let wfr_path = write_test_wfr_receipt(
            dir.path(),
            "wfr.json",
            &sample_wfr_receipt("v2", Some("equivalent")),
        );
        run_verify_receipt(vcr_path.to_str().unwrap(), &key_hex())
            .expect("VCR dispatches to the VCR path");
        run_verify_receipt(l2_path.to_str().unwrap(), &key_hex())
            .expect("L2 dispatches to the L2 path");
        run_verify_receipt(wfr_path.to_str().unwrap(), &key_hex())
            .expect("WFR dispatches to the WFR path");
    }
}
