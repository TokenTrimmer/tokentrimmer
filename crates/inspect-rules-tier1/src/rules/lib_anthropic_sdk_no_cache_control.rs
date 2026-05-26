//! Rule: `lib-anthropic-sdk-no-cache-control`
//!
//! File-level check: if a file imports the Anthropic SDK and calls
//! `messages.create`, but never references `cache_control` anywhere, it is
//! almost certainly missing prompt-caching annotations.

use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when an Anthropic SDK import + `messages.create` call is found but
/// `cache_control` is absent from the entire file.
pub struct LibAnthropicSdkNoCacheControlRule;

impl LibAnthropicSdkNoCacheControlRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for LibAnthropicSdkNoCacheControlRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Return `true` when the file path indicates a test fixture.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for LibAnthropicSdkNoCacheControlRule {
    fn id(&self) -> &'static str {
        "lib-anthropic-sdk-no-cache-control"
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

        // File must import the Anthropic SDK.
        let has_import = source.contains("import anthropic")
            || source.contains("from anthropic")
            || source.contains("@anthropic-ai/sdk")
            || source.contains("Anthropic(")
            || source.contains("new Anthropic");
        if !has_import {
            return vec![];
        }

        // File must have a messages.create call.
        if !source.contains("messages.create") {
            return vec![];
        }

        // If cache_control is present anywhere, the file is handling it.
        if source.contains("cache_control") {
            return vec![];
        }

        // Find the line number of the first messages.create call.
        let create_line = source
            .lines()
            .enumerate()
            .find(|(_, l)| l.contains("messages.create"))
            .map(|(i, _)| i + 1)
            .unwrap_or(1);

        vec![Finding {
            rule_id: self.id().to_string(),
            severity: self.severity(),
            file: path.to_string(),
            line: create_line as u32,
            message: "Anthropic SDK is imported and messages.create() is called, but no \
                       cache_control annotation is present anywhere in the file. Prompt caching \
                       can reduce cached input tokens by up to 90%."
                .to_string(),
            confidence: 0.75,
            fix_hint: Some(
                "Add cache_control={\"type\": \"ephemeral\"} to system blocks that are ≥1024 \
                 tokens. See https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching"
                    .to_string(),
            ),
        }]
    }
}
