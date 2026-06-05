//! `tt advise` — scan a repo for model usage and ask a tool-grounded model for
//! cost/routing recommendations. Read-only. Reuses the V5b-2 tool-calling loop.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context as _;
use regex::Regex;

use crate::chat::{tools, Conversation, Ledger};
use crate::context::ResolvedContext;
use crate::ui;

const ADVISOR_MODEL: &str = "gpt-4o-mini";

const ADVISOR_SYSTEM: &str = "You are a TokenTrimmer cost-optimization advisor. \
Use the provided tools (preview_cost, find_route_for, inspect_diff) to ground EVERY \
recommendation in real numbers — never invent prices, call the tools. Be concrete and \
brief: list specific routing/model changes with their dollar impact, name cheaper \
equivalents, and flag risky or wasteful prompt patterns. End with the single \
highest-impact change.";

/// File extensions worth scanning for model usage.
const SCAN_EXTS: &[&str] = &[
    "py", "js", "ts", "tsx", "jsx", "rs", "go", "rb", "java", "kt", "php", "cs",
];
/// Directories never scanned.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "vendor",
    ".next",
];
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_FILES: usize = 5000;

/// One model id found in the codebase.
pub struct ModelUsage {
    pub id: String,
    pub count: usize,
    pub example_file: String,
}

fn model_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(gpt-[\w.\-]+|claude-[\w.\-]+|gemini-[\w.\-]+|o[1-9]-[\w.\-]+|mistral-[\w.\-]+|llama-?[0-9][\w.\-]*|text-embedding-[\w.\-]+)\b",
        )
        .expect("valid model regex")
    })
}

/// Extract known model-id mentions from `text` (de-duped, first-seen order).
#[must_use]
pub fn scan_text_for_models(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in model_re().find_iter(text) {
        let id = m.as_str().to_string();
        if seen.insert(id.to_ascii_lowercase()) {
            out.push(id);
        }
    }
    out
}

/// A compact brief: detected models + the optional `--describe`, asking for
/// tool-grounded recommendations.
#[must_use]
pub fn build_context_message(detected: &[ModelUsage], describe: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("Review this project's LLM usage and recommend cost/routing optimizations.\n\n");
    if detected.is_empty() {
        s.push_str("No model usage was detected by scanning the code.\n");
    } else {
        s.push_str("Models referenced in the codebase:\n");
        for m in detected {
            s.push_str(&format!(
                "- {} (in {} file(s), e.g. {})\n",
                m.id, m.count, m.example_file
            ));
        }
    }
    if let Some(d) = describe {
        s.push_str(&format!("\nWhat the app does: {d}\n"));
    }
    s.push_str(
        "\nFor each suggestion, call preview_cost / find_route_for to ground the numbers, \
         then give the single highest-impact change.",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_extracts_model_ids() {
        let t = r#"client.chat("gpt-4o-mini"); model = "claude-3-5-sonnet"; use o3-mini;
                   var llamaindex = 1; pick "mistral-large-latest"; llama-3.3-70b"#;
        let ids = scan_text_for_models(t);
        assert!(ids.contains(&"gpt-4o-mini".to_string()), "{ids:?}");
        assert!(ids.iter().any(|s| s.eq_ignore_ascii_case("claude-3-5-sonnet")));
        assert!(ids.iter().any(|s| s.eq_ignore_ascii_case("o3-mini")));
        assert!(ids
            .iter()
            .any(|s| s.to_ascii_lowercase().starts_with("mistral-large")));
        assert!(ids
            .iter()
            .any(|s| s.to_ascii_lowercase().starts_with("llama-3.3")));
        assert!(!ids.iter().any(|s| s.to_ascii_lowercase().contains("llamaindex")));
        assert!(scan_text_for_models("no models here").is_empty());
        assert_eq!(scan_text_for_models("gpt-4o gpt-4o").len(), 1); // de-duped
    }

    #[test]
    fn context_message_lists_detected_and_describe() {
        let det = vec![ModelUsage {
            id: "gpt-4o".into(),
            count: 3,
            example_file: "src/llm.py".into(),
        }];
        let msg = build_context_message(&det, Some("a support chatbot"));
        assert!(msg.contains("gpt-4o"));
        assert!(msg.contains("3 file(s)"));
        assert!(msg.contains("src/llm.py"));
        assert!(msg.contains("a support chatbot"));
        let empty = build_context_message(&[], None);
        assert!(empty.contains("No model usage was detected"));
    }
}
