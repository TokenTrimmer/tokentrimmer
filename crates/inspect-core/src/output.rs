//! Formatting helpers that turn a slice of [`Finding`]s into a human-readable
//! markdown report, a machine-readable JSON array, or a SARIF 2.1.0 log.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::{Finding, Severity};

/// SARIF schema URL that the 2.1.0 spec recommends for the `$schema` field.
const SARIF_SCHEMA: &str =
    "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/main/sarif-2.1/schema/sarif-schema-2.1.0.json";

/// Map an Inspect [`Severity`] to a SARIF result/configuration `level`.
///
/// SARIF only defines `error`, `warning`, `note`, and `none`. We collapse the
/// four Inspect severities onto the three meaningful SARIF levels so that
/// GitHub Code Scanning renders the annotations with sensible weight:
/// `Critical`/`High` → `error`, `Medium` → `warning`, `Low` → `note`.
fn sarif_level(sev: Severity) -> &'static str {
    match sev {
        Severity::Critical | Severity::High => "error",
        Severity::Medium => "warning",
        Severity::Low => "note",
    }
}

/// Render `findings` as a pretty-printed JSON array.
///
/// Returns `"[]"` on serialisation failure (which should never occur given
/// the types involved).
pub fn format_json(findings: &[Finding]) -> String {
    serde_json::to_string_pretty(findings).unwrap_or_else(|_| "[]".into())
}

/// Render `findings` as a markdown report grouped by severity (descending).
///
/// When `findings` is empty the output is a short "No findings." section so
/// that CI logs are unambiguous.
pub fn format_markdown(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "# TokenTrimmer Inspect\n\nNo findings.\n".into();
    }

    let mut out = String::new();
    out.push_str("# TokenTrimmer Inspect\n\n");
    out.push_str(&format!("Found **{}** finding(s).\n\n", findings.len()));

    // Emit groups in descending severity order.
    for sev in [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
    ] {
        let bucket: Vec<&Finding> = findings.iter().filter(|f| f.severity == sev).collect();
        if bucket.is_empty() {
            continue;
        }
        out.push_str(&format!("## {:?} ({})\n\n", sev, bucket.len()));
        for f in bucket {
            out.push_str(&format!(
                "- **{}** `{}:{}` — {} _(confidence {:.0}%)_\n",
                f.rule_id,
                f.file,
                f.line,
                f.message,
                f.confidence * 100.0
            ));
            if let Some(hint) = &f.fix_hint {
                out.push_str(&format!("    Fix: {hint}\n"));
            }
        }
        out.push('\n');
    }
    out
}

/// Render `findings` as a **SARIF 2.1.0** log (Static Analysis Results
/// Interchange Format), suitable for upload to the GitHub Code Scanning /
/// Security tab and for inline PR annotations.
///
/// The shape produced is the minimal-but-complete required set:
///
/// ```text
/// {
///   "$schema": "...sarif-schema-2.1.0.json",
///   "version": "2.1.0",
///   "runs": [{
///     "tool": { "driver": { "name": "TokenTrimmer Inspect", ..., "rules": [ ... ] } },
///     "results": [{
///       "ruleId": "<rule_id>",
///       "level": "error" | "warning" | "note",
///       "message": { "text": "<message>" },
///       "locations": [{ "physicalLocation": {
///         "artifactLocation": { "uri": "<repo-relative path>" },
///         "region": { "startLine": <line> }
///       }}],
///       "properties": { "confidence": <0..1>, "fixHint"?: "...", "estMonthlyUsd"?: <n> }
///     }]
///   }]
/// }
/// ```
///
/// The `tool.driver.rules[]` array is derived from the distinct `rule_id`s
/// present in `findings`, each carrying a `defaultConfiguration.level` mapped
/// from the finding's severity. A clean scan therefore produces a valid SARIF
/// log with an empty `rules[]` and empty `results[]`.
///
/// Dependency-light by design: this is hand-built `serde_json` — no SARIF
/// crate. Extras that don't fit the SARIF result schema (the finding's
/// `confidence`, `fix_hint`, and any future `est_monthly_usd`) are carried in
/// `result.properties` so no information is lost.
///
/// Returns `"{}"` on serialisation failure (which should never occur given the
/// types involved).
pub fn format_sarif(findings: &[Finding]) -> String {
    // Derive the rule descriptors from the findings. A BTreeMap keeps the
    // `rules[]` array deterministic (sorted by ruleId) regardless of finding
    // order, and de-duplicates rules that fire more than once. We keep the
    // *highest* severity seen for a given rule so the rule's default level
    // reflects its worst observed impact.
    let mut rule_levels: BTreeMap<&str, Severity> = BTreeMap::new();
    for f in findings {
        rule_levels
            .entry(f.rule_id.as_str())
            .and_modify(|existing| {
                if f.severity.weight() > existing.weight() {
                    *existing = f.severity;
                }
            })
            .or_insert(f.severity);
    }

    let rules: Vec<Value> = rule_levels
        .iter()
        .map(|(id, sev)| {
            json!({
                "id": id,
                "defaultConfiguration": { "level": sarif_level(*sev) }
            })
        })
        .collect();

    let results: Vec<Value> = findings
        .iter()
        .map(|f| {
            // SARIF physicalLocation.region.startLine must be >= 1.
            let start_line = f.line.max(1);

            let mut properties = serde_json::Map::new();
            properties.insert("confidence".into(), json!(f.confidence));
            if let Some(hint) = &f.fix_hint {
                properties.insert("fixHint".into(), json!(hint));
            }

            json!({
                "ruleId": f.rule_id,
                "level": sarif_level(f.severity),
                "message": { "text": f.message },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": f.file },
                        "region": { "startLine": start_line }
                    }
                }],
                "properties": Value::Object(properties)
            })
        })
        .collect();

    let sarif = json!({
        "$schema": SARIF_SCHEMA,
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "TokenTrimmer Inspect",
                    "informationUri": "https://github.com/tokentrimmer/tokentrimmer",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules
                }
            },
            "results": results
        }]
    });

    serde_json::to_string_pretty(&sarif).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(rule_id: &str, sev: Severity, file: &str, line: u32) -> Finding {
        Finding {
            rule_id: rule_id.into(),
            severity: sev,
            file: file.into(),
            line,
            message: format!("{rule_id} fired"),
            confidence: 0.9,
            fix_hint: Some("do the thing".into()),
        }
    }

    #[test]
    fn sarif_level_mapping_covers_all_severities() {
        assert_eq!(sarif_level(Severity::Critical), "error");
        assert_eq!(sarif_level(Severity::High), "error");
        assert_eq!(sarif_level(Severity::Medium), "warning");
        assert_eq!(sarif_level(Severity::Low), "note");
    }

    #[test]
    fn sarif_has_required_top_level_fields() {
        let v: Value = serde_json::from_str(&format_sarif(&[])).expect("valid JSON");
        assert_eq!(v["version"], "2.1.0");
        let schema = v["$schema"].as_str().unwrap();
        assert!(
            schema.contains("sarif") && schema.contains("2.1.0"),
            "unexpected $schema: {schema}"
        );
        let driver = &v["runs"][0]["tool"]["driver"];
        assert_eq!(driver["name"], "TokenTrimmer Inspect");
        // Clean scan: valid empty-results SARIF, empty rules[].
        assert_eq!(v["runs"][0]["results"].as_array().unwrap().len(), 0);
        assert_eq!(driver["rules"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn sarif_maps_a_finding_to_a_result_and_rule() {
        let f = finding("model-deprecated", Severity::High, "src/app.py", 42);
        let v: Value = serde_json::from_str(&format_sarif(&[f])).expect("valid JSON");

        // tool.driver.rules[] carries the rule, mapped from its severity.
        let rules = v["runs"][0]["tool"]["driver"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["id"], "model-deprecated");
        assert_eq!(rules[0]["defaultConfiguration"]["level"], "error");

        // The result mirrors ruleId + level + physicalLocation.
        let results = v["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r["ruleId"], "model-deprecated");
        assert_eq!(r["level"], "error");
        assert_eq!(r["message"]["text"], "model-deprecated fired");
        let loc = &r["locations"][0]["physicalLocation"];
        assert_eq!(loc["artifactLocation"]["uri"], "src/app.py");
        assert_eq!(loc["region"]["startLine"], 42);
        // Extras land in properties.
        assert_eq!(r["properties"]["fixHint"], "do the thing");
        assert!(r["properties"]["confidence"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn sarif_dedups_rules_and_keeps_worst_severity() {
        // Same rule fires twice at different severities; rules[] has one entry
        // at the worst (error) level, results[] has both.
        let findings = vec![
            finding("dup-rule", Severity::Low, "a.py", 1),
            finding("dup-rule", Severity::Critical, "b.py", 2),
        ];
        let v: Value = serde_json::from_str(&format_sarif(&findings)).expect("valid JSON");
        let rules = v["runs"][0]["tool"]["driver"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1, "duplicate rule_id collapses to one rule");
        assert_eq!(rules[0]["defaultConfiguration"]["level"], "error");
        assert_eq!(v["runs"][0]["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn sarif_clamps_zero_line_to_one() {
        // SARIF requires region.startLine >= 1; a 0-line finding must not emit 0.
        let f = finding("r", Severity::Medium, "x.py", 0);
        let v: Value = serde_json::from_str(&format_sarif(&[f])).expect("valid JSON");
        let line = v["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"]
            ["startLine"]
            .as_u64()
            .unwrap();
        assert_eq!(line, 1);
    }
}
