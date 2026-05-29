//! Rule: `prompt-bloated-system`
//!
//! Detects system prompts that exceed a token threshold (approximately 4000 tokens),
//! which indicates excessive verbosity or redundancy that inflates per-request cost.

use tt_inspect_core::{Finding, Language, Rule, Severity};

pub struct PromptBloatedSystemRule;

impl PromptBloatedSystemRule {
    pub fn new() -> Self {
        Self
    }

    /// Estimate tokens in text. Simple heuristic: ~4 characters per token.
    fn estimate_tokens(text: &str) -> usize {
        text.len().div_ceil(4)
    }
}

impl Default for PromptBloatedSystemRule {
    fn default() -> Self {
        Self::new()
    }
}

const TOKEN_THRESHOLD: usize = 4000;

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for PromptBloatedSystemRule {
    fn id(&self) -> &'static str {
        "prompt-bloated-system"
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

        // Look for any assignment to system variables/parameters
        for (line_idx, line) in source.lines().enumerate() {
            if !line.contains("system") {
                continue;
            }

            // Extract a block starting from this line to analyze the full prompt
            let prompt_block: String = source
                .lines()
                .skip(line_idx)
                .take(300)
                .collect::<Vec<_>>()
                .join("\n");

            let token_estimate = Self::estimate_tokens(&prompt_block);
            if token_estimate >= TOKEN_THRESHOLD {
                findings.push(Finding {
                    rule_id: self.id().to_string(),
                    severity: self.severity(),
                    file: path.to_string(),
                    line: (line_idx + 1) as u32,
                    message: format!(
                        "System prompt is bloated (~{token_estimate} tokens). Excessive system prompt \
                         length increases per-request cost significantly, especially at scale."
                    ),
                    confidence: 0.80,
                    fix_hint: Some(
                        "Review the system prompt for redundancy, low-value instructions, or \
                         overly verbose explanations. Consider summarizing or removing sections \
                         that duplicate model defaults."
                            .to_string(),
                    ),
                });
                break; // Report once per file
            }
        }

        findings
    }
}
