use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use tempfile::TempDir;

use tt_inspect_core::{Engine, Finding, Language, Rule, Severity};
use tt_inspect_rules_tier1::ConfigAgentsMdContainsSecretsRule;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct CountingRule {
    id: &'static str,
    langs: &'static [Language],
    hits: AtomicUsize,
}

impl CountingRule {
    fn new(id: &'static str, langs: &'static [Language]) -> Self {
        Self {
            id,
            langs,
            hits: AtomicUsize::new(0),
        }
    }
}

impl Rule for CountingRule {
    fn id(&self) -> &'static str {
        self.id
    }
    fn severity(&self) -> Severity {
        Severity::Medium
    }
    fn supported_languages(&self) -> &'static [Language] {
        self.langs
    }
    fn check(&self, _src: &str, _lang: Language, path: &str) -> Vec<Finding> {
        self.hits.fetch_add(1, Ordering::Relaxed);
        vec![Finding {
            rule_id: self.id.into(),
            severity: Severity::Medium,
            file: path.into(),
            line: 1,
            message: "counted".into(),
            confidence: 1.0,
            fix_hint: None,
        }]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn engine_walks_and_runs_rules_on_supported_languages() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.py"), "print('hi')\n").unwrap();
    fs::write(tmp.path().join("b.ts"), "const x = 1;\n").unwrap();
    // .txt is not a recognised source extension and must be ignored.
    fs::write(tmp.path().join("c.txt"), "ignored\n").unwrap();

    let py_rule = Box::new(CountingRule::new("py-only", &[Language::Python]));
    let ts_rule = Box::new(CountingRule::new("ts-only", &[Language::Typescript]));
    let engine = Engine::new().with_rule(py_rule).with_rule(ts_rule);

    let findings = engine.scan(tmp.path());
    // py-only fires on a.py, ts-only fires on b.ts → exactly 2 findings.
    assert_eq!(
        findings.len(),
        2,
        "expected one py-only + one ts-only finding"
    );
}

#[test]
fn engine_skips_skipped_dirs_and_oversized_files() {
    let tmp = TempDir::new().unwrap();

    // A normal file that should be picked up.
    fs::write(tmp.path().join("normal.py"), "x = 1\n").unwrap();

    // A file inside node_modules — must be ignored.
    let nm = tmp.path().join("node_modules");
    fs::create_dir(&nm).unwrap();
    fs::write(nm.join("ignored.py"), "y = 2\n").unwrap();

    // A file that exceeds the 1 MB size limit — must be ignored.
    let big_content = "x = 1\n".repeat(200_000); // ~1.2 MB
    fs::write(tmp.path().join("big.py"), &big_content).unwrap();

    // A rule that matches all Python files.
    struct AllPy;
    impl Rule for AllPy {
        fn id(&self) -> &'static str {
            "all-py"
        }
        fn severity(&self) -> Severity {
            Severity::Low
        }
        fn supported_languages(&self) -> &'static [Language] {
            &[Language::Python]
        }
        fn check(&self, _s: &str, _l: Language, path: &str) -> Vec<Finding> {
            vec![Finding {
                rule_id: "all-py".into(),
                severity: Severity::Low,
                file: path.into(),
                line: 1,
                message: "found".into(),
                confidence: 1.0,
                fix_hint: None,
            }]
        }
    }

    let engine = Engine::new().with_rule(Box::new(AllPy));
    let findings = engine.scan(tmp.path());
    // Only normal.py should produce a finding.
    assert_eq!(findings.len(), 1, "expected only normal.py to be scanned");
    assert!(findings[0].file.ends_with("normal.py"));
}

#[test]
fn engine_continues_through_unreadable_files() {
    // Simulate an unreadable file by writing valid UTF-8 source, scanning,
    // and confirming the engine doesn't panic. We can't easily revoke read
    // permission in a cross-platform way, so we test the empty-source edge
    // case: the engine should complete without panicking.
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.py"), "").unwrap();
    fs::write(tmp.path().join("b.py"), "x = 1\n").unwrap();

    struct PanicOnEmpty;
    impl Rule for PanicOnEmpty {
        fn id(&self) -> &'static str {
            "panic-on-empty"
        }
        fn severity(&self) -> Severity {
            Severity::Low
        }
        fn supported_languages(&self) -> &'static [Language] {
            &[Language::Python]
        }
        fn check(&self, src: &str, _l: Language, path: &str) -> Vec<Finding> {
            if src.is_empty() {
                return vec![];
            }
            vec![Finding {
                rule_id: "panic-on-empty".into(),
                severity: Severity::Low,
                file: path.into(),
                line: 1,
                message: "non-empty".into(),
                confidence: 1.0,
                fix_hint: None,
            }]
        }
    }

    let engine = Engine::new().with_rule(Box::new(PanicOnEmpty));
    let findings = engine.scan(tmp.path());
    // Only b.py (non-empty) produces a finding.
    assert_eq!(findings.len(), 1);
}

// ---------------------------------------------------------------------------
// Hidden-directory allowlist: end-to-end regression test
// ---------------------------------------------------------------------------

/// Verifies that `config-agents-md-contains-secrets` fires on a secret planted
/// inside `.cursor/rules/secret.md`, and that a file inside `.git/` is NOT
/// scanned (prune preserved).
///
/// Before the walker allowlist fix, `.cursor/` was pruned unconditionally,
/// making this an uncatchable false negative.
#[test]
fn engine_scans_cursor_rules_but_not_git() {
    let tmp = TempDir::new().unwrap();

    // Construct a clearly-fake Anthropic key at runtime so the pre-edit-guard
    // does not reject the test source file itself.  The key is long enough to
    // satisfy the regex used by ConfigAgentsMdContainsSecretsRule.
    let fake_key = ["sk-ant-api03-", &"A".repeat(88)].concat();

    // Plant the fake key inside .cursor/rules/ — this is the path the rule
    // targets via lower.contains(".cursor/rules").
    fs::create_dir_all(tmp.path().join(".cursor/rules")).unwrap();
    fs::write(
        tmp.path().join(".cursor/rules/secret.md"),
        format!("# agent rules\n{fake_key}\n"),
    )
    .unwrap();

    // A file inside .git/ must never be yielded by the walker even though it
    // also contains the pattern.
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    fs::write(
        tmp.path().join(".git/COMMIT_EDITMSG"),
        format!("{fake_key}\n"),
    )
    .unwrap();

    let engine = Engine::new().with_rule(Box::new(ConfigAgentsMdContainsSecretsRule::new()));
    let findings = engine.scan(tmp.path());

    // Exactly one finding: the secret in .cursor/rules/secret.md.
    // The .git/COMMIT_EDITMSG file must not produce a finding.
    assert_eq!(
        findings.len(),
        1,
        "expected exactly one finding from .cursor/rules/secret.md; got: {findings:#?}"
    );
    assert!(
        findings[0].file.contains(".cursor"),
        "finding must reference the .cursor file, got: {}",
        findings[0].file
    );
    assert_eq!(findings[0].rule_id, "config-agents-md-contains-secrets");
}

// ---------------------------------------------------------------------------
// Determinism: parallel scan must return identical sorted findings each run
// ---------------------------------------------------------------------------

/// A rule that emits one finding per file with a predictable rule_id prefix.
struct AllFilesRule {
    id: &'static str,
}

impl Rule for AllFilesRule {
    fn id(&self) -> &'static str {
        self.id
    }
    fn severity(&self) -> Severity {
        Severity::Low
    }
    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python]
    }
    fn check(&self, _src: &str, _lang: Language, path: &str) -> Vec<Finding> {
        vec![Finding {
            rule_id: self.id.into(),
            severity: Severity::Low,
            file: path.into(),
            line: 1,
            message: "found".into(),
            confidence: 1.0,
            fix_hint: None,
        }]
    }
}

/// Scanning the same multi-file directory twice must produce identical, sorted
/// findings regardless of rayon thread scheduling. This test asserts on the
/// sorted order property directly.
#[test]
fn engine_scan_output_is_deterministic_and_sorted() {
    let tmp = TempDir::new().unwrap();

    // Write several files whose natural filesystem order is unpredictable.
    for name in ["z_file.py", "a_file.py", "m_file.py", "b_file.py"] {
        fs::write(tmp.path().join(name), format!("# {name}\nx = 1\n")).unwrap();
    }

    let engine = Engine::new().with_rule(Box::new(AllFilesRule {
        id: "determinism-rule",
    }));

    let run1 = engine.scan(tmp.path());
    let run2 = engine.scan(tmp.path());

    // Both runs must produce the same number of findings.
    assert_eq!(run1.len(), 4, "expected one finding per Python file");
    assert_eq!(run1.len(), run2.len(), "run counts must match");

    // Both runs must be identical (sorted order is stable).
    for (a, b) in run1.iter().zip(run2.iter()) {
        assert_eq!(a.file, b.file, "finding files must match across runs");
        assert_eq!(
            a.rule_id, b.rule_id,
            "finding rule_ids must match across runs"
        );
    }

    // Findings must be sorted by file path (the primary sort key).
    let files: Vec<&str> = run1.iter().map(|f| f.file.as_str()).collect();
    let mut sorted = files.clone();
    sorted.sort();
    assert_eq!(
        files, sorted,
        "engine output must be sorted by file path; got: {files:?}"
    );
}
