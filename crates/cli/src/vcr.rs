//! `tt verify-receipt` — offline verification of a TokenTrimmer Verifiable
//! Compression Receipt (VCR).
//!
//! P2a step 3: the verify side of the VCR (the sign side is the cloud
//! mint-on-demand endpoint `POST /v1/admin/requests/{trace_id}/compression-
//! receipt/sign`, which calls `tt_telemetry::vcr::sign`). This CLI takes a
//! receipt JSON + the customer's out-of-band verifying-key hex + asserts the
//! Ed25519 signature is valid over the canonical `vcr:v1|…` payload — proving
//! the savings figure was attested by the key holder, offline, with no network
//! or DB. Mirrors `tt verify-bundle` (`crates/cli/src/bundle.rs:366-415`) +
//! `tt audit verify` (`crates/cli/src/audit.rs:53-146`).
//!
//! Exit code is non-zero on any failure (tampered receipt, wrong key, malformed
//! JSON, unknown schema version) so it drops straight into a CI gate or a
//! "download the receipt → verify it" customer flow.

use std::path::Path;

use anyhow::Context;

use tt_telemetry::vcr::VcrReceipt;

/// `tt verify-receipt --receipt <path> --key-hex <hex>` entry point. Reads the
/// receipt JSON, reconstructs the canonical payload, verifies the signature, and
/// prints PASS/FAIL with the receipt fields. Exits non-zero on failure.
///
/// # Errors
/// Returns an error — so the process exits non-zero — when the receipt file
/// cannot be read/parsed, the key hex is malformed, or the signature does not
/// verify (tampered fields, wrong key, unknown schema version).
pub fn run_verify_receipt(receipt_path: &str, key_hex: &str) -> anyhow::Result<()> {
    use anyhow::Context;

    let raw = std::fs::read_to_string(receipt_path)
        .with_context(|| format!("read receipt {receipt_path}"))?;
    let receipt: VcrReceipt =
        serde_json::from_str(&raw).with_context(|| format!("parse receipt JSON {receipt_path}"))?;

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
}
