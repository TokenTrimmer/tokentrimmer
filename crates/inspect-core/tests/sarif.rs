//! End-to-end SARIF emission: run the real engine on a fixture, emit SARIF,
//! parse it back as JSON, and assert the SARIF 2.1.0 structural invariants.

use std::fs;

use serde_json::Value;
use tempfile::TempDir;

use tt_inspect_core::{output::format_sarif, Engine};
use tt_inspect_rules_tier1::TestRule;

/// Run the engine + a deterministic rule over a fixture, emit SARIF, parse it
/// back, and assert the required SARIF 2.1.0 structure plus a result whose
/// location matches the finding.
#[test]
fn engine_emits_valid_sarif_with_a_matching_result() {
    let tmp = TempDir::new().unwrap();
    // `TestRule` fires on the literal `// TT_TEST_FIRE`. Put it on line 2 so we
    // can assert startLine maps through faithfully.
    fs::write(tmp.path().join("app.py"), "x = 1\ny = 2  // TT_TEST_FIRE\n").unwrap();

    let engine = Engine::new().with_rule(Box::new(TestRule));
    let findings = engine.scan(tmp.path());
    assert_eq!(
        findings.len(),
        1,
        "fixture should yield exactly one finding"
    );

    let sarif = format_sarif(&findings);
    let v: Value = serde_json::from_str(&sarif).expect("emitted SARIF must be valid JSON");

    // Required top-level SARIF 2.1.0 fields.
    assert_eq!(v["version"], "2.1.0");
    let schema = v["$schema"].as_str().unwrap();
    assert!(
        schema.contains("sarif") && schema.contains("2.1.0"),
        "missing/incorrect $schema: {schema}"
    );

    // tool.driver identifies TokenTrimmer Inspect and lists >= 1 rule.
    let driver = &v["runs"][0]["tool"]["driver"];
    assert_eq!(driver["name"], "TokenTrimmer Inspect");
    let rules = driver["rules"].as_array().unwrap();
    assert!(!rules.is_empty(), "expected >= 1 rule in tool.driver.rules");
    assert_eq!(rules[0]["id"], "test-fire");

    // results[] maps the finding: ruleId + level + physicalLocation(uri, startLine).
    let results = v["runs"][0]["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert_eq!(r["ruleId"], "test-fire");
    assert_eq!(r["level"], "note"); // TestRule severity is Low → note.
    let loc = &r["locations"][0]["physicalLocation"];
    assert!(
        loc["artifactLocation"]["uri"]
            .as_str()
            .unwrap()
            .ends_with("app.py"),
        "uri should point at the scanned file"
    );
    assert_eq!(
        loc["region"]["startLine"], findings[0].line,
        "startLine must match the finding's line"
    );
}

/// A clean scan (no findings) must still emit a valid, well-formed SARIF log
/// with an empty results[] array — GitHub Code Scanning treats this as a pass.
#[test]
fn clean_scan_emits_valid_empty_results_sarif() {
    let tmp = TempDir::new().unwrap();
    // No `// TT_TEST_FIRE` marker → TestRule fires nothing.
    fs::write(tmp.path().join("clean.py"), "x = 1\n").unwrap();

    let engine = Engine::new().with_rule(Box::new(TestRule));
    let findings = engine.scan(tmp.path());
    assert!(findings.is_empty(), "fixture must be clean");

    let v: Value =
        serde_json::from_str(&format_sarif(&findings)).expect("empty SARIF must be valid JSON");
    assert_eq!(v["version"], "2.1.0");
    assert_eq!(
        v["runs"][0]["tool"]["driver"]["name"],
        "TokenTrimmer Inspect"
    );
    assert_eq!(v["runs"][0]["results"].as_array().unwrap().len(), 0);
    assert_eq!(
        v["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}
