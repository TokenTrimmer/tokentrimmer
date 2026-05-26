use std::fs;

use tempfile::TempDir;
use tt_inspect_core::Engine;
use tt_inspect_rules_tier1::TestRule;

#[test]
fn test_rule_fires_on_marker() {
    let tmp = TempDir::new().unwrap();
    // a.py contains the marker — one finding expected.
    fs::write(
        tmp.path().join("a.py"),
        "x = 1  # some code\n// TT_TEST_FIRE\n",
    )
    .unwrap();
    // b.ts is clean — no findings expected.
    fs::write(tmp.path().join("b.ts"), "// clean file\nconst x = 1;\n").unwrap();

    let engine = Engine::new().with_rule(Box::new(TestRule));
    let findings = engine.scan(tmp.path());

    assert_eq!(findings.len(), 1, "expected exactly one finding from a.py");
    assert_eq!(findings[0].rule_id, "test-fire");
    assert!(
        findings[0].file.ends_with("a.py"),
        "finding should be in a.py, got {}",
        findings[0].file
    );
    assert_eq!(findings[0].line, 2, "marker is on line 2");
}
