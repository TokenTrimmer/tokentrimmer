//! Rule: `output-n-greater-than-one`
//!
//! Detects chat-completion calls that request more than one completion
//! (`n=2`, `"n": 3`, or Gemini's `candidate_count` / `candidateCount` > 1).
//! Each extra completion bills full output tokens, so `n=k` multiplies output
//! cost by `k` — usually unintended, and rarely the cheapest way to get
//! variety.

use std::sync::OnceLock;

use regex::Regex;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when an LLM call requests multiple completions.
pub struct OutputNGreaterThanOneRule;

impl OutputNGreaterThanOneRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for OutputNGreaterThanOneRule {
    fn default() -> Self {
        Self::new()
    }
}

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

const LLM_CREATE_PATTERNS: &[&str] = &[
    "chat.completions.create",
    "completions.create",
    "messages.create",
    "generate_content(",
    "generateContent(",
];

/// Matches an `n` / `candidate_count` / `candidateCount` parameter set to an
/// integer ≥ 2. `\bn"?` allows the JSON `"n":` key form; the value alternation
/// excludes 0 and 1.
fn multi_output_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?:\bn"?|\bcandidate_count"?|\bcandidateCount"?)\s*[:=]\s*([2-9]|[1-9][0-9]+)"#,
        )
        .expect("multi-output regex is valid")
    })
}

impl Rule for OutputNGreaterThanOneRule {
    fn id(&self) -> &'static str {
        "output-n-greater-than-one"
    }

    fn severity(&self) -> Severity {
        Severity::Medium
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python, Language::Typescript, Language::Javascript]
    }

    fn check(&self, source: &str, _language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        let mut findings = Vec::new();
        for (line_idx, line) in source.lines().enumerate() {
            if !LLM_CREATE_PATTERNS.iter().any(|p| line.contains(*p)) {
                continue;
            }
            let call_block: String = source
                .lines()
                .skip(line_idx)
                .take(60)
                .collect::<Vec<_>>()
                .join("\n");

            if !multi_output_regex().is_match(&call_block) {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: "LLM call requests multiple completions (n / candidate_count > 1). \
                          Each extra completion bills full output tokens, multiplying output \
                          cost. Request one completion unless you genuinely need several."
                    .to_string(),
                confidence: 0.85,
                fix_hint: Some(
                    "Set n=1 (the default) and, if you need variety, prefer a single call with \
                     a higher temperature or a follow-up only when required."
                        .to_string(),
                ),
            });
        }

        findings
    }
}
