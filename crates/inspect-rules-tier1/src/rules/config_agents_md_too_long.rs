//! Rule: `config-agents-md-too-long`
//!
//! Whole-scan check: fires once when an AGENTS.md, CLAUDE.md, or .cursorrules
//! file in the repo exceeds 4,000 tokens. Bloated project config files are
//! loaded into every agent context, multiplying the cost across all LLM calls.

use tt_inspect_core::{Finding, Language, Rule, Severity};

pub struct ConfigAgentsMdTooLongRule;

impl ConfigAgentsMdTooLongRule {
    pub fn new() -> Self {
        Self
    }

    fn estimate_tokens(text: &str) -> usize {
        text.len().div_ceil(4)
    }
}

impl Default for ConfigAgentsMdTooLongRule {
    fn default() -> Self {
        Self::new()
    }
}

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for ConfigAgentsMdTooLongRule {
    fn id(&self) -> &'static str {
        "config-agents-md-too-long"
    }

    fn severity(&self) -> Severity {
        Severity::High
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[
            Language::Python,
            Language::Typescript,
            Language::Javascript,
            Language::Markdown,
        ]
    }

    fn check(&self, source: &str, _language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        // Check if this is an agents config file by name
        let lower_path = path.to_lowercase();
        if !lower_path.ends_with("agents.md")
            && !lower_path.ends_with("claude.md")
            && !lower_path.ends_with(".cursorrules")
        {
            return vec![];
        }

        let tokens = Self::estimate_tokens(source);
        if tokens > 4000 {
            return vec![Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: 1,
                message: format!(
                    "AGENTS.md (or CLAUDE.md) file exceeds 4,000 tokens (~{tokens} tokens). \
                     This file is loaded into every AI agent context, multiplying cost across all calls."
                ),
                confidence: 0.95,
                fix_hint: Some(
                    "Review the file for redundancy, overly verbose descriptions, or low-value sections. \
                     Consider splitting into role-specific guides (e.g., AGENTS-CLI.md, AGENTS-API.md)."
                        .to_string(),
                ),
            }];
        }

        vec![]
    }
}
