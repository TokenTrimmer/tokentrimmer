//! Rule: `model-deprecated`
//!
//! Detects use of deprecated or legacy model identifiers from major providers
//! (OpenAI, Anthropic, Google) that are either sunset or no longer recommended.
//! Deprecated models may be more expensive or have degraded performance.

use tt_inspect_core::ast::{call_sites, is_llm_create_callee};
use tt_inspect_core::parse::parse_cached;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when a deprecated model identifier is found in an LLM API call.
pub struct ModelDeprecatedRule;

impl ModelDeprecatedRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ModelDeprecatedRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Deprecated OpenAI model identifiers.
const DEPRECATED_OPENAI_MODELS: &[&str] = &[
    "gpt-4-0314",
    "gpt-4-0613",
    "gpt-4-32k-0314",
    "gpt-4-32k-0613",
    "gpt-3.5-turbo-0301",
    "gpt-3.5-turbo-0613",
    "text-davinci-003",
    "text-davinci-002",
    "text-curie-001",
    "text-babbage-001",
    "text-ada-001",
];

/// Deprecated Anthropic model identifiers.
const DEPRECATED_ANTHROPIC_MODELS: &[&str] = &[
    "claude-1",
    "claude-1-100k",
    "claude-2",
    "claude-2.0",
    "claude-instant-1",
    "claude-instant-1-100k",
];

/// Deprecated Google model identifiers.
const DEPRECATED_GOOGLE_MODELS: &[&str] = &["text-bison-001", "text-bison@001"];

/// Return `true` when the file path indicates a test fixture.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for ModelDeprecatedRule {
    fn id(&self) -> &'static str {
        "model-deprecated"
    }

    fn severity(&self) -> Severity {
        Severity::Medium
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python, Language::Typescript, Language::Javascript]
    }

    fn check(&self, source: &str, language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        // AST-backed: only flag a deprecated model id that appears as a quoted
        // argument inside a real LLM create call, so a deprecated id mentioned
        // in a comment or unrelated string no longer false-positives.
        let Ok(tree) = parse_cached(source, language) else {
            return vec![];
        };

        let all_deprecated = [
            DEPRECATED_OPENAI_MODELS,
            DEPRECATED_ANTHROPIC_MODELS,
            DEPRECATED_GOOGLE_MODELS,
        ]
        .concat();

        let mut findings = Vec::new();
        for call in call_sites(&tree, source) {
            if !is_llm_create_callee(&call.callee) {
                continue;
            }
            for deprecated_model in &all_deprecated {
                let quoted_double = format!("\"{deprecated_model}\"");
                let quoted_single = format!("'{deprecated_model}'");
                if !(call.arg_contains(&quoted_double) || call.arg_contains(&quoted_single)) {
                    continue;
                }
                findings.push(Finding {
                    rule_id: self.id().to_string(),
                    severity: self.severity(),
                    file: path.to_string(),
                    line: call.line,
                    message: format!(
                        "Model '{deprecated_model}' is deprecated. Older models may be sunset or have degraded performance."
                    ),
                    confidence: 0.95,
                    fix_hint: Some(format!(
                        "Migrate to a current model. For {} models, consider using a current {} version.",
                        if DEPRECATED_OPENAI_MODELS.contains(deprecated_model) {
                            "OpenAI"
                        } else if DEPRECATED_ANTHROPIC_MODELS.contains(deprecated_model) {
                            "Anthropic"
                        } else {
                            "Google"
                        },
                        if DEPRECATED_OPENAI_MODELS.contains(deprecated_model) {
                            "gpt-4"
                        } else if DEPRECATED_ANTHROPIC_MODELS.contains(deprecated_model) {
                            "claude-3"
                        } else {
                            "gemini"
                        }
                    )),
                });
                break; // Only report once per call.
            }
        }

        findings
    }
}
