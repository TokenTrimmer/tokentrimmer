use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Context, Result};
use schemars::schema::RootSchema;
use serde_json::{json, Map, Value};

pub(crate) struct ContractSchema {
    pub(crate) file_name: &'static str,
    pub(crate) ts_name: &'static str,
    pub(crate) value: Value,
}

pub(crate) fn generate_schemas() -> Result<Vec<ContractSchema>> {
    let mut vcr = root_value(schemars::schema_for!(tt_telemetry::vcr::VcrReceipt))?;
    prepare_root(
        &mut vcr,
        "urn:tokentrimmer:receipt:vcr:structural-schema:v1",
        "TokenTrimmer compression receipt",
        "Structural VCR v1 contract generated from tt_telemetry::vcr::VcrReceipt.",
    )?;
    set_const(&mut vcr, "schema_version", json!(1))?;
    set_pattern(&mut vcr, "verifying_key_hex", "^[0-9A-Fa-f]{64}$")?;
    set_pattern(&mut vcr, "signature", "^[0-9A-Fa-f]{128}$")?;
    set_format(&mut vcr, "ts", "date-time")?;

    let mut l2 = root_value(schemars::schema_for!(tt_telemetry::l2_receipt::L2Receipt))?;
    prepare_root(
        &mut l2,
        "urn:tokentrimmer:receipt:l2:structural-schema:v1",
        "TokenTrimmer L2 cache-hit receipt",
        "Structural L2 v1 contract generated from tt_telemetry::l2_receipt::L2Receipt.",
    )?;
    set_const(&mut l2, "schema_version", json!(1))?;
    set_enum(
        &mut l2,
        "verdict",
        &[
            tt_telemetry::l2_receipt::VERDICT_CONFIDENT,
            tt_telemetry::l2_receipt::VERDICT_VERIFIED,
            tt_telemetry::l2_receipt::VERDICT_UNVERIFIABLE,
            tt_telemetry::l2_receipt::VERDICT_REJECTED,
        ],
    )?;
    set_pattern(&mut l2, "verifying_key_hex", "^[0-9A-Fa-f]{64}$")?;
    set_pattern(&mut l2, "signature", "^[0-9A-Fa-f]{128}$")?;
    set_format(&mut l2, "ts", "date-time")?;

    let mut wfr = root_value(schemars::schema_for!(tt_telemetry::wfr_receipt::WfrReceipt))?;
    prepare_root(
        &mut wfr,
        "urn:tokentrimmer:receipt:wfr:structural-schema:v1-v4",
        "TokenTrimmer workflow-run receipt",
        "Structural WFR v1-v4 contract generated from tt_telemetry::wfr_receipt::WfrReceipt.",
    )?;
    strengthen_wfr(&mut wfr)?;

    let mut arr = root_value(schemars::schema_for!(
        tt_telemetry::arr_receipt::AgentRunReceipt
    ))?;
    prepare_root(
        &mut arr,
        "urn:tokentrimmer:receipt:arr:structural-schema:v1-v2",
        "TokenTrimmer agent-run receipt",
        "Structural ARR v1-v2 contract generated from tt_telemetry::arr_receipt::AgentRunReceipt.",
    )?;
    strengthen_arr(&mut arr)?;

    let mut bundle = root_value(schemars::schema_for!(tt_plan_core::SavingsBundle))?;
    prepare_root(
        &mut bundle,
        "urn:tokentrimmer:savings-bundle:structural-schema:v1",
        "TokenTrimmer reproducible savings bundle",
        "Structural bundle v1 contract generated from tt_plan_core::SavingsBundle and its nested replay wire types.",
    )?;
    set_const(&mut bundle, "schema_version", json!(1))?;

    Ok(vec![
        ContractSchema {
            file_name: "vcr-receipt.schema.json",
            ts_name: "VcrReceipt",
            value: vcr,
        },
        ContractSchema {
            file_name: "l2-receipt.schema.json",
            ts_name: "L2Receipt",
            value: l2,
        },
        ContractSchema {
            file_name: "wfr-receipt.schema.json",
            ts_name: "WfrReceipt",
            value: wfr,
        },
        ContractSchema {
            file_name: "arr-receipt.schema.json",
            ts_name: "AgentRunReceipt",
            value: arr,
        },
        ContractSchema {
            file_name: "savings-bundle.schema.json",
            ts_name: "SavingsBundle",
            value: bundle,
        },
    ])
}

fn root_value(schema: RootSchema) -> Result<Value> {
    serde_json::to_value(schema).context("serialize generated RootSchema")
}

fn prepare_root(value: &mut Value, id: &str, title: &str, comment: &str) -> Result<()> {
    normalize_draft_2020_12(value);
    let root = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("generated schema root is not an object"))?;
    root.insert(
        "$schema".into(),
        Value::String("https://json-schema.org/draft/2020-12/schema".into()),
    );
    root.insert("$id".into(), Value::String(id.into()));
    root.insert("title".into(), Value::String(title.into()));
    root.insert("$comment".into(), Value::String(format!(
        "{comment} Structure alone does not prove signature integrity, issuer identity, math replay, provider usage, or invoice reconciliation."
    )));
    root.insert("additionalProperties".into(), Value::Bool(true));
    Ok(())
}

fn normalize_draft_2020_12(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(definitions) = map.remove("definitions") {
                map.insert("$defs".into(), definitions);
            }
            if let Some(Value::String(reference)) = map.get_mut("$ref") {
                if let Some(name) = reference.strip_prefix("#/definitions/") {
                    *reference = format!("#/$defs/{name}");
                }
            }
            for child in map.values_mut() {
                normalize_draft_2020_12(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                normalize_draft_2020_12(child);
            }
        }
        _ => {}
    }
}

fn property_mut<'a>(schema: &'a mut Value, name: &str) -> Result<&'a mut Map<String, Value>> {
    schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(name))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("generated schema is missing property {name}"))
}

fn set_const(schema: &mut Value, name: &str, value: Value) -> Result<()> {
    property_mut(schema, name)?.insert("const".into(), value);
    Ok(())
}

fn set_enum(schema: &mut Value, name: &str, values: &[&str]) -> Result<()> {
    property_mut(schema, name)?.insert("enum".into(), json!(values));
    Ok(())
}

fn set_pattern(schema: &mut Value, name: &str, pattern: &str) -> Result<()> {
    property_mut(schema, name)?.insert("pattern".into(), Value::String(pattern.into()));
    Ok(())
}

fn set_format(schema: &mut Value, name: &str, format: &str) -> Result<()> {
    property_mut(schema, name)?.insert("format".into(), Value::String(format.into()));
    Ok(())
}

fn set_nonempty_pipe_free(schema: &mut Value, name: &str) -> Result<()> {
    let property = property_mut(schema, name)?;
    property.insert("minLength".into(), json!(1));
    property.insert("pattern".into(), Value::String("^[^|]+$".into()));
    Ok(())
}

fn strengthen_common_run_receipt(schema: &mut Value) -> Result<()> {
    set_nonempty_pipe_free(schema, "status")?;
    set_pattern(schema, "signature_hex", "^[0-9A-Fa-f]{128}$")?;
    set_pattern(schema, "verifying_key_hex", "^[0-9A-Fa-f]{64}$")?;
    set_format(schema, "signed_at", "date-time")?;
    for name in [
        "request_delta_eligible_requests",
        "request_delta_measured_requests",
    ] {
        property_mut(schema, name)?.insert("minimum".into(), json!(0));
    }
    Ok(())
}

fn strengthen_wfr(schema: &mut Value) -> Result<()> {
    strengthen_common_run_receipt(schema)?;
    set_enum(schema, "canonical_version", &["v1", "v2", "v3", "v4"])?;
    schema.as_object_mut().context("WFR schema root")?.insert(
        "allOf".into(),
        json!([
            {
                "if": {"properties": {"canonical_version": {"enum": ["v1", "v3"]}}, "required": ["canonical_version"]},
                "then": {"properties": {"quality_verdict": {"type": "null"}}}
            },
            {
                "if": {"properties": {"canonical_version": {"enum": ["v2", "v4"]}}, "required": ["canonical_version"]},
                "then": {
                    "required": ["quality_verdict"],
                    "properties": {"quality_verdict": {"type": "string", "minLength": 1, "pattern": "^[^|]+$"}}
                }
            },
            legacy_request_delta_rule(&["v1", "v2"]),
            strict_request_delta_rule(&["v3", "v4"])
        ]),
    );
    Ok(())
}

fn strengthen_arr(schema: &mut Value) -> Result<()> {
    strengthen_common_run_receipt(schema)?;
    set_enum(schema, "canonical_version", &["v1", "v2"])?;
    schema.as_object_mut().context("ARR schema root")?.insert(
        "allOf".into(),
        json!([
            legacy_request_delta_rule(&["v1"]),
            strict_request_delta_rule(&["v2"])
        ]),
    );
    Ok(())
}

fn legacy_request_delta_rule(versions: &[&str]) -> Value {
    json!({
        "if": {"properties": {"canonical_version": {"enum": versions}}, "required": ["canonical_version"]},
        "then": {"properties": {
            "signed_request_delta_micros": {"type": "null"},
            "signed_request_delta_usd": {"type": "null"},
            "request_delta_formula_version": {"type": "null"},
            "request_delta_eligible_requests": {"type": "null"},
            "request_delta_measured_requests": {"type": "null"}
        }}
    })
}

fn strict_request_delta_rule(versions: &[&str]) -> Value {
    json!({
        "if": {"properties": {"canonical_version": {"enum": versions}}, "required": ["canonical_version"]},
        "then": {
            "required": [
                "signed_request_delta_micros",
                "request_delta_formula_version",
                "request_delta_eligible_requests",
                "request_delta_measured_requests"
            ],
            "properties": {
                "signed_request_delta_micros": {"type": "integer"},
                "request_delta_formula_version": {"const": tt_shared::REQUEST_DELTA_ESTIMATE_V1},
                "request_delta_eligible_requests": {"type": "integer", "minimum": 1},
                "request_delta_measured_requests": {"type": "integer", "minimum": 1}
            }
        }
    })
}

pub(crate) fn render_typescript(schemas: &[ContractSchema]) -> Result<String> {
    let mut definitions = BTreeMap::<String, Value>::new();
    for contract in schemas {
        if let Some(items) = contract.value.get("$defs").and_then(Value::as_object) {
            for (name, schema) in items {
                if let Some(existing) = definitions.insert(name.clone(), schema.clone()) {
                    if existing != *schema {
                        return Err(anyhow!("conflicting JSON Schema definition {name}"));
                    }
                }
            }
        }
    }

    let mut output = String::from(
        "// @generated by `cargo run -p tt-ts-types -- write`; DO NOT EDIT.\n\
         // JSON integer fields are `number` here. Runtime consumers must retain their\n\
         // existing Number.isSafeInteger checks before cryptographic verification.\n\n",
    );
    for (name, definition) in definitions {
        output.push_str(&format!(
            "export type {name} = {};\n\n",
            ts_type(&definition, 0)?
        ));
    }
    for contract in schemas {
        output.push_str(&format!(
            "export type {} = {};\n\n",
            contract.ts_name,
            ts_type(&contract.value, 0)?
        ));
    }
    output.push_str(
        "export type SignedReceipt = VcrReceipt | L2Receipt | WfrReceipt | AgentRunReceipt;\n",
    );
    Ok(output)
}

fn ts_type(schema: &Value, depth: usize) -> Result<String> {
    if schema == &Value::Bool(true) || schema == &json!({}) {
        return Ok("unknown".into());
    }
    if schema == &Value::Bool(false) {
        return Ok("never".into());
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return Ok(reference
            .rsplit('/')
            .next()
            .context("empty JSON Schema reference")?
            .replace("~1", "/")
            .replace("~0", "~"));
    }
    if let Some(constant) = schema.get("const") {
        return Ok(json_literal(constant));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return Ok(values
            .iter()
            .map(json_literal)
            .collect::<Vec<_>>()
            .join(" | "));
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(items) = schema.get(keyword).and_then(Value::as_array) {
            let rendered = items
                .iter()
                .map(|item| ts_type(item, depth))
                .collect::<Result<Vec<_>>>()?;
            return Ok(rendered.join(" | "));
        }
    }
    if schema.get("properties").is_none() {
        if let Some(items) = schema.get("allOf").and_then(Value::as_array) {
            let rendered = items
                .iter()
                .filter(|item| item.get("if").is_none())
                .map(|item| ts_type(item, depth))
                .collect::<Result<Vec<_>>>()?;
            if !rendered.is_empty() {
                return Ok(rendered.join(" & "));
            }
        }
    }

    match schema.get("type") {
        Some(Value::Array(types)) => {
            let rendered = types
                .iter()
                .map(|kind| ts_kind(schema, kind.as_str().unwrap_or_default(), depth))
                .collect::<Result<Vec<_>>>()?;
            Ok(rendered.join(" | "))
        }
        Some(Value::String(kind)) => ts_kind(schema, kind, depth),
        None if schema.get("properties").is_some() => ts_object(schema, depth),
        None => Ok("unknown".into()),
        _ => Ok("unknown".into()),
    }
}

fn ts_kind(schema: &Value, kind: &str, depth: usize) -> Result<String> {
    match kind {
        "object" => ts_object(schema, depth),
        "array" => match schema.get("items") {
            Some(Value::Array(items)) => Ok(format!(
                "[{}]",
                items
                    .iter()
                    .map(|item| ts_type(item, depth))
                    .collect::<Result<Vec<_>>>()?
                    .join(", ")
            )),
            Some(item) => Ok(format!("Array<{}>", ts_type(item, depth)?)),
            None => Ok("Array<unknown>".into()),
        },
        other => Ok(ts_primitive(other).into()),
    }
}

fn ts_object(schema: &Value, depth: usize) -> Result<String> {
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    if properties.is_empty() {
        if let Some(additional) = schema.get("additionalProperties") {
            return match additional {
                Value::Bool(false) => Ok("Record<string, never>".into()),
                Value::Bool(true) => Ok("Record<string, unknown>".into()),
                value => Ok(format!("Record<string, {}>", ts_type(value, depth)?)),
            };
        }
    }

    let indent = "  ".repeat(depth);
    let child_indent = "  ".repeat(depth + 1);
    let mut lines = Vec::new();
    for (name, property) in properties {
        let optional = if required.contains(name.as_str()) {
            ""
        } else {
            "?"
        };
        lines.push(format!(
            "{child_indent}{}{optional}: {};",
            ts_property_name(&name),
            ts_type(&property, depth + 1)?
        ));
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(true)) {
        lines.push(format!("{child_indent}[key: string]: unknown;"));
    }
    Ok(format!("{{\n{}\n{indent}}}", lines.join("\n")))
}

fn ts_primitive(kind: &str) -> &'static str {
    match kind {
        "string" => "string",
        "integer" | "number" => "number",
        "boolean" => "boolean",
        "null" => "null",
        "object" => "Record<string, unknown>",
        "array" => "Array<unknown>",
        _ => "unknown",
    }
}

fn json_literal(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "never".into())
}

fn ts_property_name(name: &str) -> String {
    let mut chars = name.chars();
    let valid_first = chars
        .next()
        .is_some_and(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphabetic());
    let valid_rest = chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric());
    if valid_first && valid_rest {
        name.into()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| "\"invalid\"".into())
    }
}
