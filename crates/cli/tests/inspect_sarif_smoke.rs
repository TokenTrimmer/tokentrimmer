//! End-to-end: run the built `tt` binary with `inspect --format sarif` against
//! a fixture, capture stdout, and assert it parses as a valid SARIF 2.1.0 log.
//!
//! Mirrors the other CLI smoke tests: spawn the real binary so the `--format`
//! wiring in `main()` is exercised, not just the in-process emitter (which is
//! covered by `tt-inspect-core`'s own tests).

use std::process::{Command, Stdio};

use serde_json::Value;

/// Run `tt <args>` with an isolated `$HOME`, capture stdout/stderr, and return
/// (success, stdout, stderr).
fn run_tt(home: &std::path::Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tt"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TT_API_KEY")
        .env_remove("TT_API_BASE")
        .stdin(Stdio::null())
        .output()
        .expect("spawn tt binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A clean directory still emits a valid, parseable SARIF log to stdout with an
/// empty results[] — and the process exits 0 (no findings to gate on).
#[test]
fn inspect_format_sarif_clean_dir_emits_parseable_empty_sarif() {
    let home = tempfile::tempdir().unwrap();
    let scan = tempfile::tempdir().unwrap();
    std::fs::write(scan.path().join("clean.py"), "x = 1\n").unwrap();

    let (ok, stdout, stderr) = run_tt(
        home.path(),
        &[
            "inspect",
            scan.path().to_str().unwrap(),
            "--format",
            "sarif",
        ],
    );
    assert!(ok, "clean scan should exit 0; stderr:\n{stderr}");

    let v: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout not SARIF JSON: {e}\n{stdout}"));
    assert_eq!(v["version"], "2.1.0");
    assert_eq!(
        v["runs"][0]["tool"]["driver"]["name"],
        "TokenTrimmer Inspect"
    );
    assert_eq!(v["runs"][0]["results"].as_array().unwrap().len(), 0);
}

/// A fixture that trips a real rule emits SARIF whose results[] carries the
/// finding with a ruleId, a level, and a physicalLocation. The process exits
/// non-zero because the finding gates at the default `--fail-on high`, so we
/// don't assert on exit status here — only on the SARIF emitted to stdout.
#[test]
fn inspect_format_sarif_with_finding_maps_to_a_result() {
    let home = tempfile::tempdir().unwrap();
    let scan = tempfile::tempdir().unwrap();

    // `config-agents-md-contains-secrets` fires on a key under `.cursor/rules/`.
    // Build a clearly-fake Anthropic key at runtime so the pre-edit guard does
    // not reject this test source.
    let fake_key = ["sk-ant-api03-", &"A".repeat(88)].concat();
    std::fs::create_dir_all(scan.path().join(".cursor/rules")).unwrap();
    std::fs::write(
        scan.path().join(".cursor/rules/secret.md"),
        format!("# agent rules\n{fake_key}\n"),
    )
    .unwrap();

    // `--fail-on critical` keeps the process exit 0 so we don't conflate the
    // gate's non-zero exit with a real failure; SARIF still lands on stdout.
    let (_ok, stdout, stderr) = run_tt(
        home.path(),
        &[
            "inspect",
            scan.path().to_str().unwrap(),
            "--format",
            "sarif",
            "--fail-on",
            "critical",
        ],
    );

    let v: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("stdout not SARIF JSON: {e}\nstdout:\n{stdout}\nstderr:\n{stderr}")
    });
    assert_eq!(v["version"], "2.1.0");

    let results = v["runs"][0]["results"].as_array().unwrap();
    assert!(
        !results.is_empty(),
        "expected >= 1 result for the planted secret; stdout:\n{stdout}"
    );
    let r = &results[0];
    assert!(r["ruleId"].is_string(), "result needs a ruleId");
    assert!(
        ["error", "warning", "note"].contains(&r["level"].as_str().unwrap_or("")),
        "result level must be a SARIF level, got {:?}",
        r["level"]
    );
    let loc = &r["locations"][0]["physicalLocation"];
    assert!(
        loc["artifactLocation"]["uri"].is_string(),
        "result location needs a uri"
    );
    assert!(
        loc["region"]["startLine"].as_u64().unwrap_or(0) >= 1,
        "result startLine must be >= 1"
    );
}
