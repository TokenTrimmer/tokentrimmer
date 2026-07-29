use anyhow::{bail, Context};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const EVIDENCE_SCOPE: &str = "tokentrimmer_gateway_accounting";
const FORMULA_V1: &str = "tt.request-delta-estimate.v1";
const KEY_ID_PREFIX: &str = "ed25519-sha256:";
const MAX_MONEY_E8: u64 = 100_000_000_000_000;
const MAX_TOKEN_COUNT: u64 = 2_147_483_647;
const MAX_TRACE_BYTES: usize = 512;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatDispatchReceipt {
    schema_version: u64,
    canonical_version: String,
    evidence_scope: String,
    issuer_key_id: String,
    org_id: Uuid,
    session_id: Uuid,
    turn_id: Uuid,
    cost_receipt_id: Uuid,
    dispatch_ordinal: u64,
    receipt_source: String,
    gateway_trace_id: Option<String>,
    cost_e8: u64,
    baseline_cost_e8: Option<u64>,
    saved_e8: Option<u64>,
    provider_cache_saved_e8: Option<u64>,
    cache_bust_penalty_e8: Option<u64>,
    summarizer_tax_e8: Option<u64>,
    request_delta_formula_version: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
    verifying_key_hex: String,
    signature_hex: String,
}

fn is_lower_hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn decode_array<const N: usize>(value: &str, label: &str) -> anyhow::Result<[u8; N]> {
    if !is_lower_hex(value, N) {
        bail!("{label} must be exactly {} lowercase hex characters", N * 2);
    }
    hex::decode(value)
        .with_context(|| label.to_owned())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("{label} must decode to exactly {N} bytes"))
}

fn optional_integer(value: Option<u64>) -> String {
    value.map_or_else(|| "n".to_owned(), |value| format!("v{value}"))
}

fn optional_text(value: Option<&str>) -> String {
    value.map_or_else(
        || "n".to_owned(),
        |value| format!("h{}", hex::encode(value)),
    )
}

fn canonical_payload(receipt: &ChatDispatchReceipt) -> String {
    [
        "ctdr:v1".to_owned(),
        receipt.issuer_key_id.clone(),
        receipt.org_id.to_string(),
        receipt.session_id.to_string(),
        receipt.turn_id.to_string(),
        receipt.cost_receipt_id.to_string(),
        receipt.dispatch_ordinal.to_string(),
        receipt.receipt_source.clone(),
        optional_text(receipt.gateway_trace_id.as_deref()),
        receipt.cost_e8.to_string(),
        optional_integer(receipt.baseline_cost_e8),
        optional_integer(receipt.saved_e8),
        optional_integer(receipt.provider_cache_saved_e8),
        optional_integer(receipt.cache_bust_penalty_e8),
        optional_integer(receipt.summarizer_tax_e8),
        optional_text(receipt.request_delta_formula_version.as_deref()),
        optional_integer(receipt.input_tokens),
        optional_integer(receipt.output_tokens),
        optional_integer(receipt.cached_tokens),
    ]
    .join("|")
}

fn validate_shape(receipt: &ChatDispatchReceipt) -> anyhow::Result<()> {
    if receipt.schema_version != 1
        || receipt.canonical_version != "v1"
        || receipt.evidence_scope != EVIDENCE_SCOPE
    {
        bail!("unsupported Chat dispatch receipt schema, canonical version, or evidence scope");
    }
    if !(1..=64).contains(&receipt.dispatch_ordinal)
        || !matches!(
            receipt.receipt_source.as_str(),
            "sse_terminal" | "response_header"
        )
        || receipt
            .gateway_trace_id
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TRACE_BYTES)
    {
        bail!("invalid Chat dispatch ordinal, source, or trace");
    }
    let money = [
        Some(receipt.cost_e8),
        receipt.baseline_cost_e8,
        receipt.saved_e8,
        receipt.provider_cache_saved_e8,
        receipt.cache_bust_penalty_e8,
        receipt.summarizer_tax_e8,
    ];
    if money
        .into_iter()
        .flatten()
        .any(|value| value > MAX_MONEY_E8)
    {
        bail!("Chat dispatch money exceeds the supported bound");
    }
    if receipt.baseline_cost_e8.is_some() != receipt.saved_e8.is_some() {
        bail!("Chat dispatch baseline and saved fields must be all present or all null");
    }
    let components = [
        receipt.provider_cache_saved_e8,
        receipt.cache_bust_penalty_e8,
        receipt.summarizer_tax_e8,
    ];
    let component_count = components.iter().filter(|value| value.is_some()).count();
    if component_count != 0 && component_count != components.len() {
        bail!("Chat dispatch request-delta components must be all present or all null");
    }
    if component_count == 0 {
        if receipt.request_delta_formula_version.is_some() {
            bail!("component-free Chat dispatch receipt cannot name a formula");
        }
    } else if receipt.request_delta_formula_version.as_deref() != Some(FORMULA_V1)
        || receipt.baseline_cost_e8.is_none()
    {
        bail!("measured Chat dispatch receipt needs the supported formula and baseline");
    }
    let tokens = [
        receipt.input_tokens,
        receipt.output_tokens,
        receipt.cached_tokens,
    ];
    if tokens
        .into_iter()
        .flatten()
        .any(|value| value > MAX_TOKEN_COUNT)
        || (receipt.receipt_source == "sse_terminal" && tokens.iter().any(Option::is_none))
        || (receipt.receipt_source == "response_header" && tokens.iter().any(Option::is_some))
    {
        bail!("Chat dispatch token tuple does not match its evidence source");
    }
    Ok(())
}

fn derived_key_id(key_bytes: &[u8; 32]) -> String {
    format!("{KEY_ID_PREFIX}{}", hex::encode(Sha256::digest(key_bytes)))
}

pub(super) fn verify_chat_dispatch_receipt(raw: &str, key_hex: &str) -> anyhow::Result<()> {
    let receipt: ChatDispatchReceipt =
        serde_json::from_str(raw).context("parse Chat dispatch receipt JSON")?;
    validate_shape(&receipt)?;

    let supplied_key_bytes = decode_array::<32>(key_hex, "supplied verifying key")?;
    if receipt.verifying_key_hex != key_hex {
        bail!("embedded Chat dispatch verifying key does not match the supplied out-of-band key");
    }
    if receipt.issuer_key_id != derived_key_id(&supplied_key_bytes) {
        bail!("Chat dispatch issuer key ID does not match the supplied out-of-band key");
    }
    let verifying_key = VerifyingKey::from_bytes(&supplied_key_bytes)
        .context("supplied Chat dispatch verifying key is invalid")?;
    let signature = Signature::from_bytes(&decode_array::<64>(
        &receipt.signature_hex,
        "Chat dispatch signature",
    )?);
    verifying_key
        .verify(canonical_payload(&receipt).as_bytes(), &signature)
        .context("Chat dispatch signature does not verify")?;

    crate::ui::note(&format!(
        "Chat dispatch receipt v1 for org {} session {} turn {} dispatch {}",
        receipt.org_id, receipt.session_id, receipt.turn_id, receipt.dispatch_ordinal,
    ));
    if let (Some(baseline), Some(provider_cache), Some(cache_bust), Some(summarizer_tax)) = (
        receipt.baseline_cost_e8,
        receipt.provider_cache_saved_e8,
        receipt.cache_bust_penalty_e8,
        receipt.summarizer_tax_e8,
    ) {
        let delta = i128::from(baseline)
            - i128::from(receipt.cost_e8)
            - i128::from(provider_cache)
            - i128::from(cache_bust)
            - i128::from(summarizer_tax);
        crate::ui::note(&format!(
            "cost_e8: {}, signed_request_delta_e8: {delta} ({FORMULA_V1})",
            receipt.cost_e8,
        ));
    } else {
        crate::ui::note(&format!(
            "cost_e8: {}, signed request delta: not measured",
            receipt.cost_e8,
        ));
    }
    crate::ui::ok(
        "PASS: signature verifies against the supplied key (gateway accounting; not provider or invoice proof)",
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("../../../../docs/receipt-spec/ctdr-v1.golden.json");
    const KEY_HEX: &str = "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";

    #[test]
    fn cloud_golden_verifies_with_the_out_of_band_key() {
        verify_chat_dispatch_receipt(GOLDEN, KEY_HEX).expect("Cloud golden must verify");
    }

    #[test]
    fn exact_canonical_payload_matches_the_cloud_and_browser_vector() {
        let receipt: ChatDispatchReceipt = serde_json::from_str(GOLDEN).unwrap();
        assert_eq!(
            canonical_payload(&receipt),
            concat!(
                "ctdr:v1|",
                "ed25519-sha256:fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff552263889|",
                "00000000-0000-0000-0000-000000000001|",
                "00000000-0000-0000-0000-000000000002|",
                "00000000-0000-0000-0000-000000000003|",
                "00000000-0000-0000-0000-000000000004|",
                "2|sse_terminal|h74726163657c757466382de29c93|",
                "7000000|v18000000|v10000000|v100000|v200000|v700000|",
                "h74742e726571756573742d64656c74612d657374696d6174652e7631|",
                "v100|v20|v30"
            )
        );
    }

    #[test]
    fn tamper_wrong_key_and_unknown_field_fail_closed() {
        let mut value: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        value["summarizer_tax_e8"] = serde_json::json!(700_001);
        let tampered = serde_json::to_string(&value).unwrap();
        assert!(verify_chat_dispatch_receipt(&tampered, KEY_HEX).is_err());

        let wrong_key = "00".repeat(32);
        assert!(verify_chat_dispatch_receipt(GOLDEN, &wrong_key).is_err());

        value = serde_json::from_str(GOLDEN).unwrap();
        value["future_claim"] = serde_json::json!(true);
        assert!(format!(
            "{:#}",
            verify_chat_dispatch_receipt(&serde_json::to_string(&value).unwrap(), KEY_HEX,)
                .unwrap_err()
        )
        .contains("unknown field"));
    }
}
