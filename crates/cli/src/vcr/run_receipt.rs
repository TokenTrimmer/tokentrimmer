//! Versioned workflow- and agent-run receipt CLI presentation.

use anyhow::Context;

/// Verify a WFR receipt, including v3/v4 request-delta evidence.
pub(super) fn verify_wfr_receipt(
    raw: &str,
    _peek: &serde_json::Value,
    key_hex: &str,
) -> anyhow::Result<()> {
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
        "cost_micros: {}, baseline_micros: {}, saved_micros: {}{}{}",
        receipt.cost_micros,
        receipt.baseline_micros,
        receipt.saved_micros,
        receipt
            .signed_request_delta_micros
            .map(|signed| format!(
                ", signed_request_delta_micros: {signed}, formula: {}, coverage: {}/{}",
                receipt
                    .request_delta_formula_version
                    .as_deref()
                    .unwrap_or("missing"),
                receipt.request_delta_measured_requests.unwrap_or(-1),
                receipt.request_delta_eligible_requests.unwrap_or(-1),
            ))
            .unwrap_or_default(),
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
        crate::ui::error("FAIL: signature does not verify (tampered receipt, wrong key, unknown canonical version, invalid request-delta evidence, an empty/pipe-containing status, or a v2/v4 receipt missing its quality_verdict)");
        anyhow::bail!(
            "WFR-receipt verification failed for run_id={}",
            receipt.run_id
        );
    }
}

/// Verify an ARR receipt, including v2 request-delta evidence.
pub(super) fn verify_arr_receipt(
    raw: &str,
    _peek: &serde_json::Value,
    key_hex: &str,
) -> anyhow::Result<()> {
    let receipt: tt_telemetry::arr_receipt::AgentRunReceipt =
        serde_json::from_str(raw).context("parse ARR receipt JSON")?;
    crate::ui::note(&format!(
        "ARR-receipt {} for org {} agent run {} (status {})",
        receipt.canonical_version, receipt.org_id, receipt.run_id, receipt.status,
    ));
    crate::ui::note(&format!(
        "cost_micros: {}, baseline_micros: {}, saved_micros: {}{}",
        receipt.cost_micros,
        receipt.baseline_micros,
        receipt.saved_micros,
        receipt
            .signed_request_delta_micros
            .map(|signed| format!(
                ", signed_request_delta_micros: {signed}, formula: {}, coverage: {}/{}",
                receipt
                    .request_delta_formula_version
                    .as_deref()
                    .unwrap_or("missing"),
                receipt.request_delta_measured_requests.unwrap_or(-1),
                receipt.request_delta_eligible_requests.unwrap_or(-1),
            ))
            .unwrap_or_default(),
    ));

    if tt_telemetry::arr_receipt::verify_with_key(key_hex, &receipt) {
        crate::ui::ok("PASS: signature verifies against the supplied verifying key");
        Ok(())
    } else {
        crate::ui::error("FAIL: signature does not verify (tampered receipt, wrong key, unknown canonical version, or invalid request-delta evidence)");
        anyhow::bail!(
            "ARR-receipt verification failed for run_id={}",
            receipt.run_id
        );
    }
}
