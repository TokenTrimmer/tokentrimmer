//! Rule: `output-no-max-tokens`
//!
//! Detects LLM calls (OpenAI, Anthropic, Gemini) that do not specify an
//! explicit output-length constraint (`max_tokens`, `max_completion_tokens`,
//! or `max_output_tokens`). Without such a limit, models may produce
//! arbitrarily long — and expensive — outputs.

use tt_inspect_core::ast::{call_sites, is_llm_create_callee};
use tt_inspect_core::parse::parse_cached;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when an LLM API call is found without any `max_tokens`-family
/// parameter.
pub struct OutputNoMaxTokensRule;

impl OutputNoMaxTokensRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for OutputNoMaxTokensRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Patterns that indicate an output-length constraint is present.
const MAX_TOKENS_PATTERNS: &[&str] = &[
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
    "maxTokens",
    "maxOutputTokens",
];

/// Return `true` when the file path indicates a test fixture.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for OutputNoMaxTokensRule {
    fn id(&self) -> &'static str {
        "output-no-max-tokens"
    }

    fn severity(&self) -> Severity {
        Severity::High
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python, Language::Typescript, Language::Javascript]
    }

    fn check(&self, source: &str, language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        // AST-backed: only real call expressions are considered, so an LLM
        // create mentioned in a comment/string no longer triggers, and the
        // `max_tokens` check is scoped to the call's own argument span.
        let Ok(tree) = parse_cached(source, language) else {
            return vec![];
        };

        let mut findings = Vec::new();
        for call in call_sites(&tree, source) {
            if !is_llm_create_callee(&call.callee) {
                continue;
            }
            if MAX_TOKENS_PATTERNS.iter().any(|mp| call.arg_contains(mp)) {
                continue;
            }
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: call.line,
                message: "LLM API call is missing an output-length constraint \
                           (max_tokens / max_completion_tokens / max_output_tokens). \
                           Without this, the model may produce unbounded — and costly — output."
                    .to_string(),
                confidence: 0.85,
                fix_hint: Some(
                    "Add max_tokens=<N> (or max_completion_tokens / max_output_tokens) \
                     appropriate for your task. For classification/extraction, 64-256 tokens \
                     is typically sufficient."
                        .to_string(),
                ),
            });
        }

        findings
    }
}
