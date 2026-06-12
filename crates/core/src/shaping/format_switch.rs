//! Format switch (research Phase 3.3): opt-in CSV / bare-value emission.
//!
//! Planner ([`plan_format_switch`]) — PURE eligibility decision driven by the
//! request's `response_format` SCHEMA SHAPE (the only honest signal of what
//! the caller expects back). "When unsure, no-op" is enforced in code: no
//! schema, `json_object` (shape unverifiable), strict `json_schema` (grammar
//! lock), nested/non-scalar leaves, streaming, tools, and n>1 all Skip with a
//! bounded reason token.
//!
//! Mutator ([`apply_format_switch_request`]) — drops `response_format` (the
//! switched emission is deliberately NOT JSON) and appends the deterministic
//! instruction as a TRAILING System message. The instruction is identical for
//! a given route+schema, so the dispatched prefix stays byte-stable across
//! turns (DeterministicOnIngress — no provider prefix-cache bust to book).
//!
//! Strip-validator ([`validate_switched_body`]) — the in-code equivalent of a
//! stop-sequence guard (provider stop params can only truncate generation,
//! not prevent a prose preamble, and OpenAI caps stop slots the caller may
//! already use): drops markdown fences and any prose preamble, then validates
//! the emission against the expected shape. Any mismatch ⇒ the caller fails
//! OPEN (body passes through untouched, `format_switch_failed:<label>`).
//!
//! Estimator ([`estimate_saved_tokens`]) — tokenizer-grounded ESTIMATE only:
//! tokens(JSON-equivalent reconstruction) − tokens(emitted body). `None` when
//! the reconstruction is not computable; the caller then books $0 and meters
//! `tt_format_switch_unestimated_total` (rule: when in doubt, book $0).

use tt_shared::messages::{Message, MessageContent};
use tt_shared::ChatCompletionRequest;

use super::ShapeDecision;

/// Instruction prefix for the CSV switch; the full instruction appends the
/// expected header (`columns.join(",")`). Byte-exact contract — tests assert
/// on it and the strip-validator's header detection pairs with it.
pub(crate) const CSV_INSTRUCTION_PREFIX: &str = "Respond ONLY with CSV data. \
Output the header line below exactly, then one line per record. Quote any \
field containing a comma, double quote, or newline with double quotes \
(RFC 4180). No prose, no markdown fences, no JSON. Header: ";

/// Instruction for the bare-value switch. Byte-exact contract.
pub(crate) const BARE_INSTRUCTION: &str = "Respond ONLY with the answer value \
itself on a single line. No prose, no labels, no markdown fences, no JSON, \
no quotation marks around the value.";

/// The switched emission format, derived from the caller's schema shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SwitchFormat {
    /// Flat-uniform records: array of all-scalar objects → CSV with this
    /// fixed, deterministic column order.
    Csv { columns: Vec<String> },
    /// Single value: `key` is the wrapping property name when the schema was
    /// a single-property object (used for the JSON-equivalent estimate),
    /// `None` for a bare root scalar (estimate not computable ⇒ $0 + meter).
    Bare { key: Option<String> },
}

/// A validated decision to switch this request's emission format.
#[derive(Debug, Clone)]
pub(crate) struct FormatSwitchPlan {
    pub format: SwitchFormat,
    /// `"csv"` | `"bare"` — warnings-token + metric label + request_logs value.
    pub label: &'static str,
}

/// PURE planner. `None` when the route did not request a switch (the default
/// path does zero work). `Some(Skip(reason))` pushes a
/// `format_switch_skipped:<reason>` token at the call site; every gate below
/// is enforced in code, in this order (first failure wins).
pub(crate) fn plan_format_switch(
    req: &ChatCompletionRequest,
    requested: Option<&str>,
) -> Option<ShapeDecision<FormatSwitchPlan>> {
    let requested = requested?;
    // 1. Unknown wire value (route created by a newer producer): fail-open
    //    forward-compat — no-op with a warning, never an error.
    if requested != "csv" && requested != "bare" {
        return Some(ShapeDecision::Skip("unknown_format"));
    }
    // 2-4. Hard ineligibility: streaming (would need full-stream buffering),
    //      tool use (the response may not be text), n>1 (multi-choice).
    if req.stream {
        return Some(ShapeDecision::Skip("streaming"));
    }
    if !req.tools.is_empty() || req.tool_choice.is_some() {
        return Some(ShapeDecision::Skip("tools"));
    }
    if req.n.is_some_and(|n| n > 1) {
        return Some(ShapeDecision::Skip("n"));
    }
    // 5-6. Prose protection: no response_format (free-form prose ask) and
    //      json_object (shape unverifiable) both no-op — when unsure, no-op.
    let Some(rf) = req.response_format.as_ref() else {
        return Some(ShapeDecision::Skip("no_schema"));
    };
    if rf.r#type != "json_schema" {
        return Some(ShapeDecision::Skip("no_schema"));
    }
    let Some(raw) = rf.json_schema.as_ref() else {
        return Some(ShapeDecision::Skip("no_schema"));
    };
    // 7. Grammar lock: strict structured output means the caller demanded
    //    schema-enforced JSON — format switching is DISABLED.
    let (schema, strict) = unwrap_schema_envelope(raw);
    if strict {
        return Some(ShapeDecision::Skip("strict_schema"));
    }
    // 8. Schema-shape analysis.
    let decision = match requested {
        "csv" => match csv_columns_from_schema(schema) {
            Some(columns) => ShapeDecision::Apply(FormatSwitchPlan {
                format: SwitchFormat::Csv { columns },
                label: "csv",
            }),
            None => ShapeDecision::Skip("nested_schema"),
        },
        // Only "bare" remains (gate 1 rejected everything else).
        _ => match bare_key_from_schema(schema) {
            Some(key) => ShapeDecision::Apply(FormatSwitchPlan {
                format: SwitchFormat::Bare { key },
                label: "bare",
            }),
            None => ShapeDecision::Skip("not_single_value"),
        },
    };
    Some(decision)
}

/// Unwrap the OpenAI `response_format.json_schema` envelope
/// (`{"name", "strict", "schema"}`) — accepting a bare schema too. Returns
/// the schema value and whether `strict: true` was requested.
fn unwrap_schema_envelope(raw: &serde_json::Value) -> (&serde_json::Value, bool) {
    if let Some(obj) = raw.as_object() {
        if let Some(inner) = obj.get("schema").filter(|s| s.is_object()) {
            let strict = obj.get("strict").and_then(|s| s.as_bool()).unwrap_or(false);
            return (inner, strict);
        }
    }
    (raw, false)
}

/// JSON-schema scalar leaf types eligible for a CSV cell / bare value.
fn is_scalar_type(t: &str) -> bool {
    matches!(t, "string" | "number" | "integer" | "boolean")
}

fn schema_type(schema: &serde_json::Value) -> Option<&str> {
    schema.get("type").and_then(|t| t.as_str())
}

/// CSV eligibility: root `{"type":"array","items":{object, all-scalar
/// props}}` OR a root object with EXACTLY ONE property that is such an array.
/// Returns the deterministic column order: the items schema's `required`
/// order when it lists exactly the property set, else lexicographic (stable
/// regardless of producer key order). `None` ⇒ nested/non-flat ⇒ no-op.
fn csv_columns_from_schema(schema: &serde_json::Value) -> Option<Vec<String>> {
    let items = match schema_type(schema)? {
        "array" => schema.get("items")?,
        "object" => {
            let props = schema.get("properties")?.as_object()?;
            if props.len() != 1 {
                return None;
            }
            let only = props.values().next()?;
            if schema_type(only)? != "array" {
                return None;
            }
            only.get("items")?
        }
        _ => return None,
    };
    if schema_type(items)? != "object" {
        return None;
    }
    let props = items.get("properties")?.as_object()?;
    if props.is_empty() {
        return None;
    }
    // Every leaf must be scalar — any nested object/array/union ⇒ ineligible.
    for prop_schema in props.values() {
        if !schema_type(prop_schema).is_some_and(is_scalar_type) {
            return None;
        }
    }
    let mut columns: Vec<String> = props.keys().cloned().collect();
    columns.sort_unstable(); // serde_json maps are BTree, but be explicit.
    if let Some(required) = items.get("required").and_then(|r| r.as_array()) {
        let req_cols: Vec<String> = required
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let mut sorted_req = req_cols.clone();
        sorted_req.sort_unstable();
        if sorted_req == columns {
            // `required` is a permutation of the property set — the caller's
            // declared order wins.
            return Some(req_cols);
        }
    }
    Some(columns)
}

/// Bare eligibility: a root object with EXACTLY ONE scalar property (key =
/// its name) or a root scalar type (key = `None`). `None` ⇒ not a
/// single-value ask ⇒ no-op.
#[allow(clippy::option_option)]
fn bare_key_from_schema(schema: &serde_json::Value) -> Option<Option<String>> {
    match schema_type(schema)? {
        t if is_scalar_type(t) => Some(None),
        "object" => {
            let props = schema.get("properties")?.as_object()?;
            if props.len() != 1 {
                return None;
            }
            let (key, prop_schema) = props.iter().next()?;
            if schema_type(prop_schema).is_some_and(is_scalar_type) {
                Some(Some(key.clone()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The exact instruction text for a plan — deterministic per route+schema.
pub(crate) fn instruction_for(plan: &FormatSwitchPlan) -> String {
    match &plan.format {
        SwitchFormat::Csv { columns } => {
            format!("{CSV_INSTRUCTION_PREFIX}{}", columns.join(","))
        }
        SwitchFormat::Bare { .. } => BARE_INSTRUCTION.to_string(),
    }
}

/// Mutate the request for dispatch: drop `response_format` (the switched
/// emission is deliberately not JSON — leaving a json_schema in place would
/// fight the instruction) and append the deterministic instruction as a
/// TRAILING System message (the Anthropic adapter folds all System messages
/// into the top-level system param; identical bytes per route+schema ⇒ no
/// provider prefix-cache bust). Touches NOTHING else.
pub(crate) fn apply_format_switch_request(
    req: &mut ChatCompletionRequest,
    plan: &FormatSwitchPlan,
) {
    req.response_format = None;
    req.messages.push(Message::System {
        content: MessageContent::Text(instruction_for(plan)),
    });
}

/// Strip-validate the switched emission (the in-code stop-sequence-guard
/// equivalent). `Ok(stripped body)` ⇒ serve the stripped body and advertise
/// the switch; `Err(reason)` ⇒ the caller fails OPEN (untouched body,
/// `format_switch_failed:<label>`, no saving booked, response never cached
/// under the switched key).
pub(crate) fn validate_switched_body(
    body: &str,
    format: &SwitchFormat,
) -> Result<String, &'static str> {
    match format {
        SwitchFormat::Csv { columns } => validate_csv(body, columns),
        SwitchFormat::Bare { .. } => validate_bare(body),
    }
}

fn is_fence_line(line: &str) -> bool {
    line.trim().starts_with("```")
}

fn validate_csv(body: &str, columns: &[String]) -> Result<String, &'static str> {
    let expected_header = columns.join(",");
    let mut lines = body
        .lines()
        .filter(|l| !is_fence_line(l))
        .map(str::trim_end);
    // Skip prose preamble until the header line (case-insensitive,
    // whitespace-trimmed). Missing header ⇒ the model did not comply.
    let found_header = lines
        .by_ref()
        .any(|l| l.trim().eq_ignore_ascii_case(&expected_header));
    if !found_header {
        return Err("header");
    }
    let mut out = vec![expected_header.clone()];
    let mut pending_blanks = 0usize;
    for line in lines {
        if line.trim().is_empty() {
            // Blank lines are tolerated only as trailing padding; interior
            // blanks before another record are fine too (they separate
            // nothing) — they are simply dropped from the stripped body.
            pending_blanks += 1;
            continue;
        }
        let Some(fields) = csv_field_count(line) else {
            return Err("body"); // unterminated quote
        };
        if fields != columns.len() {
            return Err("body");
        }
        let _ = pending_blanks;
        pending_blanks = 0;
        out.push(line.to_string());
    }
    Ok(out.join("\n"))
}

fn validate_bare(body: &str) -> Result<String, &'static str> {
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| !is_fence_line(l))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match lines.as_slice() {
        [single] => Ok((*single).to_string()),
        _ => Err("not_bare"),
    }
}

/// Minimal RFC-4180 field counter: counts comma-separated fields honoring
/// double-quoted fields (with `""` escapes). `None` on an unterminated quote.
fn csv_field_count(line: &str) -> Option<usize> {
    csv_fields(line).map(|f| f.len())
}

/// Minimal RFC-4180 field splitter (shared by the validator and the
/// JSON-equivalent reconstruction). `None` on an unterminated quote.
fn csv_fields(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    current.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                current.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => fields.push(std::mem::take(&mut current)),
                _ => current.push(c),
            }
        }
    }
    if in_quotes {
        return None;
    }
    fields.push(current);
    Some(fields)
}

/// Parse a CSV cell / bare value the way the JSON answer would have carried
/// it: integer, float, boolean, else string.
fn cell_to_json(cell: &str) -> serde_json::Value {
    let trimmed = cell.trim();
    if let Ok(i) = trimmed.parse::<i64>() {
        return serde_json::Value::from(i);
    }
    if let Ok(f) = trimmed.parse::<f64>() {
        if f.is_finite() {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return serde_json::Value::Number(n);
            }
        }
    }
    if trimmed == "true" || trimmed == "false" {
        return serde_json::Value::Bool(trimmed == "true");
    }
    serde_json::Value::String(cell.to_string())
}

/// Tokenizer-grounded ESTIMATE of the tokens the switch saved:
/// `tokens(JSON-equivalent reconstruction) − tokens(stripped body)`, floored
/// at 0. `None` ⇒ not computable ⇒ the caller books $0 and meters
/// `tt_format_switch_unestimated_total`. This figure is a labeled estimate —
/// it never folds into invoice-reconciled cost/baseline/saved figures.
pub(crate) fn estimate_saved_tokens(
    provider_id: &str,
    model: &str,
    stripped: &str,
    format: &SwitchFormat,
) -> Option<u32> {
    let json_equivalent = match format {
        SwitchFormat::Csv { columns } => {
            let mut records = Vec::new();
            for line in stripped.lines().skip(1) {
                let fields = csv_fields(line)?;
                if fields.len() != columns.len() {
                    return None;
                }
                let mut obj = serde_json::Map::new();
                for (col, cell) in columns.iter().zip(fields.iter()) {
                    obj.insert(col.clone(), cell_to_json(cell));
                }
                records.push(serde_json::Value::Object(obj));
            }
            serde_json::to_string(&serde_json::Value::Array(records)).ok()?
        }
        SwitchFormat::Bare { key: Some(key) } => {
            let mut obj = serde_json::Map::new();
            obj.insert(key.clone(), cell_to_json(stripped));
            serde_json::to_string(&serde_json::Value::Object(obj)).ok()?
        }
        // Root-scalar schema: there is no defensible JSON wrapper to price
        // against — book $0 and meter instead of inventing one.
        SwitchFormat::Bare { key: None } => return None,
    };
    let json_tokens = tt_tokenize::estimate_tokens_for_model(provider_id, model, &json_equivalent);
    let body_tokens = tt_tokenize::estimate_tokens_for_model(provider_id, model, stripped);
    Some(json_tokens.saturating_sub(body_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::ResponseFormat;

    fn flat_records_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "qty": {"type": "integer"},
                    "price": {"type": "number"}
                },
                "required": ["name", "qty", "price"]
            }
        })
    }

    fn req_with_schema(schema: serde_json::Value) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::User {
                content: MessageContent::Text("list the items".into()),
                name: None,
            }],
            response_format: Some(ResponseFormat {
                r#type: "json_schema".into(),
                json_schema: Some(serde_json::json!({
                    "name": "out", "strict": false, "schema": schema
                })),
            }),
            ..Default::default()
        }
    }

    fn skip_reason(d: Option<ShapeDecision<FormatSwitchPlan>>) -> &'static str {
        match d {
            Some(ShapeDecision::Skip(r)) => r,
            Some(ShapeDecision::Apply(_)) => panic!("expected Skip, got Apply"),
            None => panic!("expected Skip, got None"),
        }
    }

    fn applied(d: Option<ShapeDecision<FormatSwitchPlan>>) -> FormatSwitchPlan {
        match d {
            Some(ShapeDecision::Apply(p)) => p,
            Some(ShapeDecision::Skip(r)) => panic!("expected Apply, got Skip({r})"),
            None => panic!("expected Apply, got None"),
        }
    }

    /// No route request → None (zero work on the default path).
    #[test]
    fn no_request_is_none() {
        let req = req_with_schema(flat_records_schema());
        assert!(plan_format_switch(&req, None).is_none());
    }

    /// The skip matrix: each ineligibility gate yields its bounded reason.
    #[test]
    fn skip_matrix() {
        // unknown format value → fail-open forward-compat.
        let req = req_with_schema(flat_records_schema());
        assert_eq!(
            skip_reason(plan_format_switch(&req, Some("yaml"))),
            "unknown_format"
        );

        // streaming
        let mut r = req_with_schema(flat_records_schema());
        r.stream = true;
        assert_eq!(
            skip_reason(plan_format_switch(&r, Some("csv"))),
            "streaming"
        );

        // tools
        let mut r = req_with_schema(flat_records_schema());
        r.tool_choice = Some(tt_shared::messages::ToolChoice::auto());
        assert_eq!(skip_reason(plan_format_switch(&r, Some("csv"))), "tools");

        // n > 1
        let mut r = req_with_schema(flat_records_schema());
        r.n = Some(2);
        assert_eq!(skip_reason(plan_format_switch(&r, Some("csv"))), "n");

        // no response_format at all → prose protection.
        let mut r = req_with_schema(flat_records_schema());
        r.response_format = None;
        assert_eq!(
            skip_reason(plan_format_switch(&r, Some("csv"))),
            "no_schema"
        );

        // json_object → shape unverifiable.
        let mut r = req_with_schema(flat_records_schema());
        r.response_format = Some(ResponseFormat {
            r#type: "json_object".into(),
            json_schema: None,
        });
        assert_eq!(
            skip_reason(plan_format_switch(&r, Some("csv"))),
            "no_schema"
        );

        // strict json_schema → grammar lock, disabled.
        let mut r = req_with_schema(flat_records_schema());
        r.response_format = Some(ResponseFormat {
            r#type: "json_schema".into(),
            json_schema: Some(serde_json::json!({
                "name": "out", "strict": true, "schema": flat_records_schema()
            })),
        });
        assert_eq!(
            skip_reason(plan_format_switch(&r, Some("csv"))),
            "strict_schema"
        );

        // nested leaf → not flat-uniform.
        let nested = serde_json::json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}}
                }
            }
        });
        let r = req_with_schema(nested);
        assert_eq!(
            skip_reason(plan_format_switch(&r, Some("csv"))),
            "nested_schema"
        );

        // bare on a multi-property object → not a single value.
        let multi = serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "string"}
            }
        });
        let r = req_with_schema(multi);
        assert_eq!(
            skip_reason(plan_format_switch(&r, Some("bare"))),
            "not_single_value"
        );
    }

    /// CSV plan from a root-array schema: columns follow `required` order.
    #[test]
    fn csv_plan_from_root_array_schema_required_order() {
        let req = req_with_schema(flat_records_schema());
        let plan = applied(plan_format_switch(&req, Some("csv")));
        assert_eq!(plan.label, "csv");
        assert_eq!(
            plan.format,
            SwitchFormat::Csv {
                columns: vec!["name".into(), "qty".into(), "price".into()]
            }
        );
    }

    /// CSV plan from the wrapped single-array-property shape; no `required`
    /// → lexicographic column order (deterministic).
    #[test]
    fn csv_plan_from_wrapped_schema_lexicographic() {
        let wrapped = serde_json::json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "zeta": {"type": "string"},
                            "alpha": {"type": "integer"}
                        }
                    }
                }
            }
        });
        let req = req_with_schema(wrapped);
        let plan = applied(plan_format_switch(&req, Some("csv")));
        assert_eq!(
            plan.format,
            SwitchFormat::Csv {
                columns: vec!["alpha".into(), "zeta".into()]
            }
        );
    }

    /// A bare schema (no OpenAI envelope) is accepted too.
    #[test]
    fn csv_plan_accepts_bare_schema_without_envelope() {
        let mut req = req_with_schema(flat_records_schema());
        req.response_format = Some(ResponseFormat {
            r#type: "json_schema".into(),
            json_schema: Some(flat_records_schema()),
        });
        let plan = applied(plan_format_switch(&req, Some("csv")));
        assert_eq!(plan.label, "csv");
    }

    /// Bare plan: single-scalar-property object extracts the key; root scalar
    /// extracts `None`.
    #[test]
    fn bare_plan_key_extraction() {
        let single = serde_json::json!({
            "type": "object",
            "properties": { "total": {"type": "number"} }
        });
        let req = req_with_schema(single);
        let plan = applied(plan_format_switch(&req, Some("bare")));
        assert_eq!(
            plan.format,
            SwitchFormat::Bare {
                key: Some("total".into())
            }
        );

        let scalar = serde_json::json!({"type": "string"});
        let req = req_with_schema(scalar);
        let plan = applied(plan_format_switch(&req, Some("bare")));
        assert_eq!(plan.format, SwitchFormat::Bare { key: None });
    }

    /// Mutation: response_format removed, the deterministic instruction is
    /// appended as a TRAILING System message byte-exactly, and every other
    /// request byte is untouched.
    #[test]
    fn apply_mutation_is_minimal_and_byte_exact() {
        let req = req_with_schema(flat_records_schema());
        let plan = applied(plan_format_switch(&req, Some("csv")));
        let mut mutated = req.clone();
        apply_format_switch_request(&mut mutated, &plan);

        assert!(mutated.response_format.is_none());
        assert_eq!(mutated.messages.len(), req.messages.len() + 1);
        match mutated.messages.last().unwrap() {
            Message::System {
                content: MessageContent::Text(t),
            } => {
                assert_eq!(t, &format!("{CSV_INSTRUCTION_PREFIX}name,qty,price"));
            }
            other => panic!("expected trailing System instruction, got {other:?}"),
        }
        // Everything else byte-identical: clear the two mutated fields and
        // compare the serialized rest.
        let mut a = mutated.clone();
        a.messages.pop();
        let mut b = req.clone();
        b.response_format = None;
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    /// validate_csv: accepts clean CSV; strips fences + prose preamble;
    /// handles RFC-4180 quoted commas; rejects wrong interior arity; rejects
    /// a missing header.
    #[test]
    fn validate_csv_matrix() {
        let cols: Vec<String> = vec!["name".into(), "qty".into(), "price".into()];

        // Clean.
        let body = "name,qty,price\nwidget,2,9.99\ngadget,1,12.50";
        assert_eq!(validate_csv(body, &cols).unwrap(), body);

        // Fences + prose preamble stripped; trailing blank tolerated.
        let messy = "Sure! Here you go:\n```csv\nname,qty,price\nwidget,2,9.99\n```\n\n";
        assert_eq!(
            validate_csv(messy, &cols).unwrap(),
            "name,qty,price\nwidget,2,9.99"
        );

        // Quoted comma inside a field still counts as 3 fields.
        let quoted = "name,qty,price\n\"widget, deluxe\",2,9.99";
        assert_eq!(validate_csv(quoted, &cols).unwrap(), quoted);

        // Wrong interior arity → body error.
        let bad = "name,qty,price\nwidget,2\ngadget,1,12.50";
        assert_eq!(validate_csv(bad, &cols), Err("body"));

        // Unterminated quote → body error.
        let unterminated = "name,qty,price\n\"widget,2,9.99";
        assert_eq!(validate_csv(unterminated, &cols), Err("body"));

        // Missing header → header error (e.g. the model emitted JSON anyway).
        let json_anyway = "[{\"name\":\"widget\",\"qty\":2,\"price\":9.99}]";
        assert_eq!(validate_csv(json_anyway, &cols), Err("header"));
    }

    /// validate_bare: accepts a single line, strips fences, rejects
    /// multiline/empty.
    #[test]
    fn validate_bare_matrix() {
        assert_eq!(validate_bare("42.7"), Ok("42.7".into()));
        assert_eq!(validate_bare("```\n42.7\n```"), Ok("42.7".into()));
        assert_eq!(validate_bare("  blue  \n"), Ok("blue".into()));
        assert_eq!(validate_bare("The answer is:\n42.7"), Err("not_bare"));
        assert_eq!(validate_bare(""), Err("not_bare"));
        assert_eq!(validate_bare("```\n```"), Err("not_bare"));
    }

    /// Tokenizer-grounded estimate: on a realistic 10-record sample the
    /// JSON-equivalent reconstruction costs strictly more tokens than the
    /// CSV body (the whole point of the switch), and the delta is what the
    /// estimator returns.
    #[test]
    fn estimate_saved_tokens_csv_realistic_sample() {
        let cols: Vec<String> = vec!["name".into(), "qty".into(), "price".into()];
        let mut body = String::from("name,qty,price");
        for i in 0..10 {
            body.push_str(&format!("\nitem-{i},{},{}.99", i + 1, 9 + i));
        }
        let format = SwitchFormat::Csv { columns: cols };
        let saved = estimate_saved_tokens("openai", "gpt-4o", &body, &format)
            .expect("clean CSV must be estimable");
        assert!(
            saved > 0,
            "JSON-equivalent must out-token the CSV body, saved={saved}"
        );
    }

    /// Bare with a known wrapping key reconstructs `{"key": value}`; a root
    /// scalar (key=None) is NOT computable → None (caller books $0 + meters).
    #[test]
    fn estimate_saved_tokens_bare() {
        let keyed = SwitchFormat::Bare {
            key: Some("total".into()),
        };
        let saved = estimate_saved_tokens("openai", "gpt-4o", "1234.5", &keyed)
            .expect("keyed bare must be estimable");
        assert!(saved > 0, "{{\"total\":1234.5}} must out-token `1234.5`");

        let unkeyed = SwitchFormat::Bare { key: None };
        assert_eq!(
            estimate_saved_tokens("openai", "gpt-4o", "1234.5", &unkeyed),
            None,
            "no defensible JSON wrapper → not computable → $0 + meter"
        );
    }

    /// A body the splitter cannot parse is not estimable (caller books $0).
    #[test]
    fn estimate_saved_tokens_unparseable_body_is_none() {
        let cols: Vec<String> = vec!["a".into(), "b".into()];
        let format = SwitchFormat::Csv { columns: cols };
        assert_eq!(
            estimate_saved_tokens("openai", "gpt-4o", "a,b\n\"oops,1", &format),
            None
        );
    }
}
