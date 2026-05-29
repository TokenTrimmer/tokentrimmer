//! Rule: `cache-openai-prompt-cache-eligible`
//!
//! Detects OpenAI SDK chat completion calls that contain a long static system
//! prefix (≥ 1 024 estimated tokens, proxied as ≥ 4 096 chars) where prompt
//! caching could reduce input token cost by ~50%.

use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when an OpenAI `chat.completions.create` call carries a long system
/// message string literal — a candidate for OpenAI automatic prompt caching.
pub struct CacheOpenaiPromptCacheEligibleRule;

impl CacheOpenaiPromptCacheEligibleRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for CacheOpenaiPromptCacheEligibleRule {
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

impl Rule for CacheOpenaiPromptCacheEligibleRule {
    fn id(&self) -> &'static str {
        "cache-openai-prompt-cache-eligible"
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

        // Quick reject: must use openai and call completions.create.
        let is_openai = source.contains("import openai")
            || source.contains("from openai")
            || source.contains("openai/")
            || source.contains("OpenAI(")
            || source.contains("new OpenAI");
        if !is_openai {
            return vec![];
        }

        if !source.contains("completions.create") && !source.contains("chat.completions") {
            return vec![];
        }

        let mut findings = Vec::new();

        for (line_idx, line) in source.lines().enumerate() {
            // Match completion call entry points.
            if !line.contains("completions.create") && !line.contains("chat.completions") {
                continue;
            }

            // Collect the call block.
            let call_block: String = source
                .lines()
                .skip(line_idx)
                .take(200)
                .collect::<Vec<_>>()
                .join("\n");

            // Look for a system-role message with long content.
            if !has_long_system_in_messages(&call_block) {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: "OpenAI chat completion has a long static system prefix (≥1024 \
                           estimated tokens). OpenAI automatic prompt caching activates at \
                           ≥1024 tokens — ensure the prefix is stable to maximise the \
                           ~50% input-cost discount."
                    .to_string(),
                confidence: 0.7,
                fix_hint: Some(
                    "Keep the system message prefix static and deterministic. Dynamic content \
                     (timestamps, user IDs) should go in later messages, not the system prefix."
                        .to_string(),
                ),
            });
        }

        findings
    }
}

/// Return `true` when the call block contains a system-role message whose
/// content literal is ≥ 4 096 chars (≈ 1 024 tokens).
fn has_long_system_in_messages(block: &str) -> bool {
    // Find "system" role markers in the messages array.
    let system_markers = [
        "\"role\": \"system\"",
        "'role': 'system'",
        "role: \"system\"",
        "role: 'system'",
    ];
    for marker in &system_markers {
        let Some(role_pos) = block.find(marker) else {
            continue;
        };
        // Look for content after the role key.
        let after_role = &block[role_pos..];
        // Find "content" key following the role.
        let content_markers = ["\"content\":", "'content':", "content:", "content ="];
        for cm in &content_markers {
            let Some(content_pos) = after_role.find(cm) else {
                continue;
            };
            let after_content = &after_role[content_pos + cm.len()..];
            let after_content = after_content.trim_start();
            let len = estimate_string_literal_length(after_content);
            if len >= 4096 {
                return true;
            }
        }
    }
    false
}

/// Estimate the length of the first string literal at the start of `s`.
fn estimate_string_literal_length(s: &str) -> usize {
    let s = s.trim_start_matches(|c: char| c.is_whitespace());
    let s = s.strip_prefix('f').unwrap_or(s);
    let s = s.strip_prefix('r').unwrap_or(s);

    if s.starts_with("\"\"\"") || s.starts_with("'''") {
        let (q, rest) = if let Some(stripped) = s.strip_prefix("\"\"\"") {
            ("\"\"\"", stripped)
        } else {
            ("'''", &s[3..])
        };
        if let Some(end) = rest.find(q) {
            return rest[..end].len();
        }
        return rest.len();
    }

    if s.starts_with('"') || s.starts_with('\'') {
        let q = if s.starts_with('"') { '"' } else { '\'' };
        let rest = &s[1..];
        let mut len = 0usize;
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                chars.next();
                len += 2;
            } else if c == q {
                break;
            } else {
                len += c.len_utf8();
            }
        }
        return len;
    }

    0
}
