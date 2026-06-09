//! Rule: `output-no-max-tokens`
//!
//! Detects LLM calls (OpenAI, Anthropic, Gemini) that do not specify an
//! explicit output-length constraint (`max_tokens`, `max_completion_tokens`,
//! or `max_output_tokens`). Without such a limit, models may produce
//! arbitrarily long — and expensive — outputs.
//!
//! # Advisory classification
//!
//! This rule is intentionally **high-recall**: it flags *every* LLM call that
//! lacks an output-length cap, regardless of context. That breadth is the
//! point — even a single uncapped call in production can produce runaway token
//! spend.
//!
//! However, this same breadth means the rule has a structurally unavoidable
//! false-positive rate on real-world OSS corpora. Official SDK example files
//! (openai-python, anthropic-sdk-python, openai-cookbook, etc.) routinely omit
//! `max_tokens` for brevity in demos; so do auth examples, grader utilities,
//! and one-off scripts. The rule cannot distinguish "demo brevity" from
//! "production omission" at the AST level — the callee pattern and argument
//! list look identical.
//!
//! As a result, this rule is **excluded from the strict per-rule corpus FP
//! gate** in `scripts/measure-fp-rate.sh --corpus`, the same way `config-*`
//! rules are excluded. It still appears in the full fixture-based FP-rate
//! report (non-corpus mode) where its precision/recall on the synthetic
//! fixture suite is excellent. Callers who want to suppress it in CI should
//! use `--fail-on critical` (this rule is `Severity::High`, not `Critical`).

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
///
/// Both the `tests/` (Rust/Python convention) and `test/` (common JS/TS
/// convention, e.g. vitest/jest suites) directory names are recognized — test
/// code legitimately omits `max_tokens` (e.g. to assert an SDK injects a
/// default), and is not a production call site.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/test/")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixture_recognizes_tests_and_test_dirs() {
        assert!(is_test_fixture("sdk-python/tests/test_client.py"));
        assert!(is_test_fixture("sdk-typescript/test/client.test.ts"));
        assert!(is_test_fixture("crates/x/tests/y.rs"));
        // Production call sites are NOT fixtures.
        assert!(!is_test_fixture("examples/python/cost_attribution.py"));
        assert!(!is_test_fixture("src/index.ts"));
    }

    #[test]
    fn rule_skips_calls_in_a_test_dir() {
        // A bare `.create({...})` with no max_tokens would normally fire, but a
        // `/test/` path is treated as a fixture and skipped.
        let src = "client.chat.completions.create({ model: 'm', messages: [] });\n";
        let r = OutputNoMaxTokensRule::new();
        assert!(r
            .check(
                src,
                Language::Typescript,
                "sdk-typescript/test/client.test.ts"
            )
            .is_empty());
        // The same call in a non-test path DOES fire.
        assert_eq!(
            r.check(src, Language::Typescript, "examples/typescript/x.ts")
                .len(),
            1
        );
    }
}
