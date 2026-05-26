//! Rule: `conversation-unbounded-history`
//!
//! Detects conversation-history lists that are appended to without any
//! pruning or sliding-window mechanism. Unbounded history causes cost to
//! grow linearly (or worse) per conversation turn.

use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when a file imports an LLM SDK, appends to a messages list, and
/// shows no evidence of history-pruning.
pub struct ConversationUnboundedHistoryRule;

impl ConversationUnboundedHistoryRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ConversationUnboundedHistoryRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Patterns indicating a message-list append.
const APPEND_PATTERNS: &[&str] = &[
    "messages.append(",
    "conversation.append(",
    "history.append(",
    "chat_history.append(",
    "messages.push(",
    "conversation.push(",
    "history.push(",
    "chat_history.push(",
    "msgs.append(",
    "msgs.push(",
];

/// Patterns indicating history pruning or slicing.
const PRUNE_PATTERNS: &[&str] = &[
    "messages[-",
    "messages[:",
    "conversation[-",
    "conversation[:",
    "history[-",
    "history[:",
    "len(messages)",
    "len(conversation)",
    "len(history)",
    ".slice(-",
    ".slice(0,",
    "splice(",
    "max_history",
    "max_messages",
    "WINDOW",
    "window_size",
    "trim_messages",
    "prune",
];

/// LLM SDK import indicators.
const LLM_IMPORT_PATTERNS: &[&str] = &[
    "import openai",
    "from openai",
    "from \"openai\"",
    "from 'openai'",
    "import anthropic",
    "from anthropic",
    "from \"anthropic\"",
    "from 'anthropic'",
    "@anthropic-ai/sdk",
    "import langchain",
    "from langchain",
    "openai/",
    "anthropic/",
];

/// Return `true` when the file path indicates a test fixture.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for ConversationUnboundedHistoryRule {
    fn id(&self) -> &'static str {
        "conversation-unbounded-history"
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

        // File must import an LLM SDK.
        let has_llm_import = LLM_IMPORT_PATTERNS.iter().any(|p| source.contains(p));
        if !has_llm_import {
            return vec![];
        }

        // File must append to a messages list.
        let append_line = APPEND_PATTERNS.iter().find_map(|p| {
            source
                .lines()
                .enumerate()
                .find(|(_, l)| l.contains(p))
                .map(|(i, _)| (i, *p))
        });
        let Some((append_line_idx, _)) = append_line else {
            return vec![];
        };

        // If ANY pruning pattern exists in the entire file, skip.
        let has_prune = PRUNE_PATTERNS.iter().any(|p| source.contains(p));
        if has_prune {
            return vec![];
        }

        vec![Finding {
            rule_id: self.id().to_string(),
            severity: self.severity(),
            file: path.to_string(),
            line: (append_line_idx + 1) as u32,
            message: "Conversation history is appended to without any pruning, slicing, or \
                       sliding-window mechanism. Unbounded history causes per-turn cost to \
                       grow linearly, and may hit context-window limits."
                .to_string(),
            confidence: 0.6,
            fix_hint: Some(
                "Add a sliding window: messages = messages[-N:] or summarise older turns. \
                 For long conversations, consider a hierarchical context strategy."
                    .to_string(),
            ),
        }]
    }
}
