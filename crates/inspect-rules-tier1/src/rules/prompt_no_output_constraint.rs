//! Rule: `prompt-no-output-constraint`
//!
//! Detects LLM calls where outputs are expected to be short (classification,
//! yes/no answers, extraction) but no output length constraint is specified.
//! This rule is similar to output-no-max-tokens but focuses on prompt-level
//! signals that suggest output should be bounded.

use tt_inspect_core::{Finding, Language, Rule, Severity};

pub struct PromptNoOutputConstraintRule;

impl PromptNoOutputConstraintRule {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PromptNoOutputConstraintRule {
    fn default() -> Self {
        Self::new()
    }
}

const LLM_CREATE_PATTERNS: &[&str] = &[
    "chat.completions.create",
    "completions.create",
    "messages.create",
    "generate_content(",
    "generateContent(",
];

const MAX_TOKENS_PATTERNS: &[&str] = &[
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
    "maxTokens",
    "maxOutputTokens",
];

const CONSTRAINT_KEYWORDS: &[&str] = &[
    "classify",
    "extract",
    "is this",
    "yes or no",
    "one word",
    "one sentence",
    "brief",
    "concise",
    "answer only",
];

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for PromptNoOutputConstraintRule {
    fn id(&self) -> &'static str {
        "prompt-no-output-constraint"
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
            let matched = LLM_CREATE_PATTERNS.iter().any(|p| line.contains(p));
            if !matched {
                continue;
            }

            let call_block: String = source
                .lines()
                .skip(line_idx)
                .take(40)
                .collect::<Vec<_>>()
                .join("\n");

            let has_max_tokens = MAX_TOKENS_PATTERNS.iter().any(|mp| call_block.contains(mp));
            if has_max_tokens {
                continue;
            }

            let has_constraint_signal = CONSTRAINT_KEYWORDS
                .iter()
                .any(|kw| call_block.to_lowercase().contains(kw));
            if !has_constraint_signal {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: "LLM call appears to be for a constrained task (classification, \
                           extraction, yes/no) but lacks an output-length constraint. \
                           This can lead to unexpectedly long outputs."
                    .to_string(),
                confidence: 0.75,
                fix_hint: Some(
                    "Add max_tokens parameter. For classification/extraction, 64-256 tokens \
                     is usually sufficient."
                        .to_string(),
                ),
            });
        }

        findings
    }
}
