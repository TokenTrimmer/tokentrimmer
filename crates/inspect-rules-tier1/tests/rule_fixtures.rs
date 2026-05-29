//! Per-rule fixture-driven tests.
//!
//! Each rule's `tests/rules/<id>/should-detect/` and `should-not-detect/` trees
//! are run against the rule and the detection behaviour is asserted.
//!
//! Special case: `config-no-agents-md` is a whole-scan stateful rule. It fires
//! at most once per scan based on whether an agents file was seen. The per-file
//! positive fixtures are files WITH LLM imports; the rule fires on the first
//! such file when no agents-config file is present. We test it with its own
//! helper that instantiates a fresh rule per file so the AtomicBool state is
//! not shared.

use std::fs;
use std::path::{Path, PathBuf};

use tt_inspect_core::{Language, Rule};
use tt_inspect_rules_tier1::all_rules;

/// Resolve the fixture directory for a rule by ID.
fn fixture_root(rule_id: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("rules")
        .join(rule_id)
}

/// Infer [`Language`] from file extension. Returns `None` for unknown
/// extensions, which causes the file to be treated as "not supported".
fn lang_for_ext(p: &Path) -> Option<Language> {
    match p.extension()?.to_str()? {
        "py" => Some(Language::Python),
        "ts" | "tsx" => Some(Language::Typescript),
        "js" | "jsx" => Some(Language::Javascript),
        "md" => Some(Language::Markdown),
        _ => None,
    }
}

/// Run `rule` against every file in `dir` and return `(path, finding_count)`.
///
/// Files whose language is not in `rule.supported_languages()` are returned
/// with a count of 0 (not an error — the rule just doesn't apply to them).
///
/// The path passed to `rule.check()` is a synthetic, non-fixture path
/// (e.g. `/src/app.py`) so that `is_test_fixture()` guards inside rules do
/// not suppress findings during the fixture test.  The real filesystem path is
/// retained in the returned tuple for error messages.
fn run_against_dir(rule: &dyn Rule, dir: &Path) -> Vec<(PathBuf, usize)> {
    let mut out = Vec::new();
    if !dir.exists() {
        return out;
    }

    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort(); // deterministic order

    for path in entries {
        let Some(lang) = lang_for_ext(&path) else {
            continue;
        };
        if !rule.supported_languages().contains(&lang) {
            // Rule doesn't support this language; record 0 (not a false
            // positive, just out of scope).
            out.push((path, 0));
            continue;
        }
        let source = fs::read_to_string(&path).unwrap_or_default();

        // Synthesise a non-fixture path so rules' is_test_fixture() guards
        // do not suppress detection during the test.
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");

        // For Markdown files under a rule's fixture directory, use a path that
        // looks like an agent-config file so that config rules match correctly.
        let synthetic_path = if lang == Language::Markdown {
            "/repo/AGENTS.md".to_string()
        } else {
            format!("/src/{filename}")
        };

        let findings = rule.check(&source, lang, &synthetic_path);
        out.push((path, findings.len()));
    }

    out
}

/// Standard fixture check for rules where each file is independent (most rules).
fn check_standard_fixtures(rule: &dyn Rule) {
    let id = rule.id();

    // --- skip config-no-agents-md here; handled separately below ---
    if id == "config-no-agents-md" {
        return;
    }

    let root = fixture_root(id);
    assert!(
        root.exists(),
        "rule {id}: no fixture dir at {}",
        root.display()
    );

    let should_detect = run_against_dir(rule, &root.join("should-detect"));
    let should_not_detect = run_against_dir(rule, &root.join("should-not-detect"));

    assert!(
        should_detect.len() >= 5,
        "rule {id}: only {} positive fixtures in should-detect/, need ≥5",
        should_detect.len()
    );
    assert!(
        should_not_detect.len() >= 10,
        "rule {id}: only {} negative fixtures in should-not-detect/, need ≥10",
        should_not_detect.len()
    );

    for (path, count) in &should_detect {
        assert!(
            *count > 0,
            "rule {id}: should-detect/{} produced 0 findings (expected ≥1)",
            path.display()
        );
    }

    for (path, count) in &should_not_detect {
        assert_eq!(
            *count,
            0,
            "rule {id}: should-not-detect/{} produced {count} false positive(s)",
            path.display()
        );
    }
}

/// Special fixture check for `config-no-agents-md`.
///
/// Because the rule uses `AtomicBool` interior state that accumulates across
/// `check()` calls, we instantiate a fresh rule instance for each
/// should-detect file (to ensure each file can trigger the finding
/// independently).  For should-not-detect (files with no LLM imports), a
/// fresh rule per file also works: no LLM import → no finding → assertion
/// passes.
fn check_config_no_agents_md() {
    use tt_inspect_rules_tier1::ConfigNoAgentsMdRule;

    let id = "config-no-agents-md";
    let root = fixture_root(id);
    assert!(
        root.exists(),
        "rule {id}: no fixture dir at {}",
        root.display()
    );

    let detect_dir = root.join("should-detect");
    let no_detect_dir = root.join("should-not-detect");

    // Collect files in detect dir.
    let mut detect_files: Vec<_> = fs::read_dir(&detect_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    detect_files.sort();

    let mut no_detect_files: Vec<_> = fs::read_dir(&no_detect_dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    no_detect_files.sort();

    assert!(
        detect_files.len() >= 5,
        "rule {id}: need ≥5 should-detect fixtures, found {}",
        detect_files.len()
    );
    assert!(
        no_detect_files.len() >= 10,
        "rule {id}: need ≥10 should-not-detect fixtures, found {}",
        no_detect_files.len()
    );

    // Each should-detect file must fire when checked with a fresh rule
    // (no prior AGENTS.md seen). Use a synthetic path so is_test_fixture()
    // guards do not suppress findings.
    for path in &detect_files {
        let Some(lang) = lang_for_ext(path) else {
            continue;
        };
        let rule = ConfigNoAgentsMdRule::new();
        if !rule.supported_languages().contains(&lang) {
            continue;
        }
        let source = fs::read_to_string(path).unwrap_or_default();
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let synthetic_path = format!("/src/{filename}");
        let findings = rule.check(&source, lang, &synthetic_path);
        assert!(
            !findings.is_empty(),
            "rule {id}: should-detect/{} produced 0 findings",
            path.display()
        );
    }

    // Each should-not-detect file must produce 0 findings with a fresh rule.
    for path in &no_detect_files {
        let Some(lang) = lang_for_ext(path) else {
            continue;
        };
        let rule = ConfigNoAgentsMdRule::new();
        if !rule.supported_languages().contains(&lang) {
            continue;
        }
        let source = fs::read_to_string(path).unwrap_or_default();
        let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let synthetic_path = format!("/src/{filename}");
        let findings = rule.check(&source, lang, &synthetic_path);
        assert_eq!(
            findings.len(),
            0,
            "rule {id}: should-not-detect/{} produced {} false positive(s)",
            path.display(),
            findings.len()
        );
    }
}

/// Master test: validate every rule in `all_rules()` has ≥5 positive and
/// ≥10 negative fixtures that behave correctly.
#[test]
fn every_rule_has_min_fixtures_and_detects_correctly() {
    for rule in all_rules() {
        check_standard_fixtures(rule.as_ref());
    }

    // Special-case the whole-scan stateful rule.
    check_config_no_agents_md();
}

/// Verify `all_rules()` returns exactly 15 rules.
#[test]
fn all_rules_count_is_15() {
    let rules = all_rules();
    assert_eq!(
        rules.len(),
        15,
        "all_rules() should return 15 rules, got {}",
        rules.len()
    );
}

/// Verify every rule ID is stable and non-empty.
#[test]
fn rule_ids_are_stable() {
    let expected_ids = [
        "cache-anthropic-prompt-cache-missing",
        "cache-openai-prompt-cache-eligible",
        "lib-anthropic-sdk-no-cache-control",
        "model-flagship-for-classification",
        "model-flagship-for-extraction",
        "model-deprecated",
        "output-no-max-tokens",
        "prompt-no-output-constraint",
        "prompt-bloated-system",
        "prompt-verbose-few-shot",
        "conversation-unbounded-history",
        "agent-no-termination-condition",
        "config-no-agents-md",
        "config-agents-md-contains-secrets",
        "config-agents-md-too-long",
    ];
    let rule_ids: Vec<&str> = all_rules().iter().map(|r| r.id()).collect();
    // Check all expected IDs are present.
    for id in &expected_ids {
        assert!(
            rule_ids.contains(id),
            "Expected rule ID '{id}' not found in all_rules()"
        );
    }
}
