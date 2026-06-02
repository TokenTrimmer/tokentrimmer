//! Rule: `cache-anthropic-tools-not-cached`
//!
//! Detects Anthropic `messages.create(...)` calls that pass a `tools` array
//! but no `cache_control` anywhere in the call site. Tool definitions are
//! large and static; without a `cache_control` breakpoint they are re-sent and
//! re-billed at full input price on every call. Marking the last tool (or the
//! system block) cacheable cuts that repeated cost by up to ~90%.

use tt_inspect_core::ast::call_sites;
use tt_inspect_core::parse::parse_cached;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires on an Anthropic `messages.create` call that declares tools but never
/// annotates anything with `cache_control`.
pub struct CacheAnthropicToolsNotCachedRule;

impl CacheAnthropicToolsNotCachedRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for CacheAnthropicToolsNotCachedRule {
    fn default() -> Self {
        Self::new()
    }
}

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

/// `true` if the block declares a `tools` parameter (Python `tools=`,
/// JSON `"tools":`, or TS/JS `tools:`), as opposed to only `tool_choice`.
fn declares_tools(block: &str) -> bool {
    block.contains("tools=") || block.contains("\"tools\":") || block.contains("tools:")
}

impl Rule for CacheAnthropicToolsNotCachedRule {
    fn id(&self) -> &'static str {
        "cache-anthropic-tools-not-cached"
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

        // Quick reject: must be an Anthropic SDK module.
        let is_anthropic = source.contains("import anthropic")
            || source.contains("from anthropic")
            || source.contains("@anthropic-ai/sdk")
            || source.contains("Anthropic(")
            || source.contains("new Anthropic");
        if !is_anthropic {
            return vec![];
        }

        // AST-backed: examine real `…messages.create(...)` call nodes and scope
        // the tools / cache_control checks to each call's own argument span.
        let Ok(tree) = parse_cached(source, language) else {
            return vec![];
        };

        let mut findings = Vec::new();
        for call in call_sites(&tree, source) {
            if !call.callee.ends_with("messages.create") {
                continue;
            }
            if !declares_tools(&call.text) {
                continue;
            }
            if call.arg_contains("cache_control") {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: call.line,
                message: "Anthropic messages.create() passes a `tools` array but no \
                          cache_control. Tool definitions are static and large; without a \
                          cache breakpoint they are re-billed at full input price every call."
                    .to_string(),
                confidence: 0.8,
                fix_hint: Some(
                    "Add cache_control={\"type\": \"ephemeral\"} to the last tool definition \
                     (or the system block) so the tool schema is cached across calls."
                        .to_string(),
                ),
            });
        }

        findings
    }
}
