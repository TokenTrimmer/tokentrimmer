//! Rule: `output-no-max-tokens`
//!
//! Detects LLM calls (OpenAI, Anthropic, Gemini) that do not specify an
//! explicit output-length constraint (`max_tokens`, `max_completion_tokens`,
//! or `max_output_tokens`). Without such a limit, models may produce
//! arbitrarily long — and expensive — outputs.

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

/// Patterns that indicate an LLM create call.
const LLM_CREATE_PATTERNS: &[&str] = &[
    "chat.completions.create",
    "completions.create",
    "messages.create",
    "generate_content(",
    "generateContent(",
];

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

    fn check(&self, source: &str, _language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        let mut findings = Vec::new();

        for (line_idx, line) in source.lines().enumerate() {
            let matched_pattern = LLM_CREATE_PATTERNS
                .iter()
                .find(|p| line.contains(*p));
            let Some(_pattern) = matched_pattern else {
                continue;
            };

            // Collect the call block (up to 60 lines) to search for
            // max_tokens parameters.
            let call_block: String = source
                .lines()
                .skip(line_idx)
                .take(60)
                .collect::<Vec<_>>()
                .join("\n");

            let has_max_tokens = MAX_TOKENS_PATTERNS
                .iter()
                .any(|mp| call_block.contains(mp));
            if has_max_tokens {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
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
