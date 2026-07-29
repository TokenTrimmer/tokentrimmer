/// Fields unique to the L2 receipt family. Any one claims L2 ownership before
/// the permissive VCR deserializer can ignore it.
const L2_RECEIPT_MARKERS: [&str; 5] = [
    "matched_entry_id",
    "similarity",
    "verdict",
    "served_cost_usd",
    "baseline_cost_usd",
];

/// The receipt family selected from owned JSON discriminator fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReceiptFamily {
    Bundle,
    Ctdr,
    Wfr,
    Arr,
    L2,
    Vcr,
}

pub(super) fn receipt_family(peek: &serde_json::Value) -> ReceiptFamily {
    let Some(object) = peek.as_object() else {
        return ReceiptFamily::Vcr;
    };

    if object.contains_key("plan_input") || object.contains_key("expected_result") {
        return ReceiptFamily::Bundle;
    }

    // The Chat artifact owns these fields even when their values are malformed
    // or future. It must never fall through to ARR merely because both families
    // carry `canonical_version`, nor to permissive VCR parsing.
    if object.contains_key("evidence_scope") || object.contains_key("cost_receipt_id") {
        return ReceiptFamily::Ctdr;
    }

    if object.contains_key("canonical_version") {
        // WFR owns its workflow relationship and v2/v4 quality field.
        // Everything else in this namespace is routed to ARR.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_then_chat_then_run_namespaces_are_reserved_in_order() {
        assert_eq!(
            receipt_family(&serde_json::json!({
                "plan_input": {},
                "evidence_scope": "tokentrimmer_gateway_accounting",
                "canonical_version": "v1",
            })),
            ReceiptFamily::Bundle,
        );
        for marker in ["evidence_scope", "cost_receipt_id"] {
            let mut value = serde_json::json!({
                "canonical_version": "future",
                "workflow_id": null,
            });
            value
                .as_object_mut()
                .expect("fixture is an object")
                .insert(marker.to_owned(), serde_json::Value::Null);
            assert_eq!(receipt_family(&value), ReceiptFamily::Ctdr,);
        }
        assert_eq!(
            receipt_family(&serde_json::json!({
                "canonical_version": "v1",
                "workflow_id": null,
            })),
            ReceiptFamily::Wfr,
        );
    }
}
