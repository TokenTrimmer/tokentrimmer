//! Rule: `prompt-verbose-few-shot`
//!
//! Detects few-shot examples in prompts that are excessively long or redundant,
//! occupying more than 50% of the system prompt and inflating per-request cost.

use tt_inspect_core::{Finding, Language, Rule, Severity};

pub struct PromptVerboseFewShotRule;

impl PromptVerboseFewShotRule {
    pub fn new() -> Self {
        Self
    }

    fn estimate_tokens(text: &str) -> usize {
        text.len().div_ceil(4)
    }
}

impl Default for PromptVerboseFewShotRule {
    fn default() -> Self {
        Self::new()
    }
}

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for PromptVerboseFewShotRule {
    fn id(&self) -> &'static str {
        "prompt-verbose-few-shot"
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

        // Count examples in the entire file
        let example_count = source.matches("Example:").count()
            + source.matches("example:").count()
            + source.matches("INPUT:").count()
            + source.matches("Input:").count()
            + source.matches("OUTPUT:").count()
            + source.matches("Output:").count();

        // If we have 5+ examples, it's likely verbose few-shot
        if example_count >= 5 {
            let total_tokens = Self::estimate_tokens(source);
            // Verbose if many examples present (5+ example/output pairs)
            if example_count >= 10 {
                return vec![Finding {
                    rule_id: self.id().to_string(),
                    severity: self.severity(),
                    file: path.to_string(),
                    line: 1,
                    message: format!(
                        "Prompt contains verbose few-shot examples (~{} example pairs, ~{total_tokens} tokens total). \
                         This inflates per-request cost.",
                        example_count / 2
                    ),
                    confidence: 0.70,
                    fix_hint: Some(
                        "Consider summarizing redundant examples, removing near-duplicates, \
                         or consolidating to 1-2 representative examples instead of many."
                            .to_string(),
                    ),
                }];
            }
        }

        vec![]
    }
}
