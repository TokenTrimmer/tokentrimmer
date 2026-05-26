//! Rule: `cache-anthropic-prompt-cache-missing`
//!
//! Detects Anthropic SDK `messages.create(...)` calls that have a long system
//! prompt (≥ 1 024 estimated tokens, proxied as ≥ 4 096 chars) but no
//! `cache_control` annotation anywhere in the same call site.

use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when an Anthropic `messages.create` call carries a long system prompt
/// but lacks a `cache_control` block, leaving Anthropic prompt-caching savings
/// on the table.
pub struct CacheAnthropicPromptCacheMissingRule;

impl CacheAnthropicPromptCacheMissingRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for CacheAnthropicPromptCacheMissingRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Return `true` when the file path indicates a test fixture that should be
/// excluded from production rule checks.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for CacheAnthropicPromptCacheMissingRule {
    fn id(&self) -> &'static str {
        "cache-anthropic-prompt-cache-missing"
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

        // Quick reject: must import anthropic and call messages.create.
        if !source.contains("messages.create") {
            return vec![];
        }
        let is_anthropic = source.contains("import anthropic")
            || source.contains("from anthropic")
            || source.contains("@anthropic-ai/sdk")
            || source.contains("Anthropic(")
            || source.contains("new Anthropic");
        if !is_anthropic {
            return vec![];
        }

        let mut findings = Vec::new();

        // Scan for `messages.create` call lines.
        for (line_idx, line) in source.lines().enumerate() {
            if !line.contains("messages.create") {
                continue;
            }

            // Collect the call block: from this line forward until we see a
            // matching close paren (rough heuristic: grab up to 200 lines).
            let call_block: String = source
                .lines()
                .skip(line_idx)
                .take(200)
                .collect::<Vec<_>>()
                .join("\n");

            // Check for cache_control anywhere in the block.
            if call_block.contains("cache_control") {
                continue;
            }

            // Check for a long system string. We look for system= or "system":
            // followed eventually by a long string literal.
            let has_long_system = has_long_system_literal(&call_block);
            if !has_long_system {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: "Anthropic messages.create() has a long system prompt (≥1024 estimated \
                           tokens) but no cache_control annotation. Add \
                           cache_control: {{\"type\": \"ephemeral\"}} to reduce input cost by \
                           up to 90%."
                    .to_string(),
                confidence: 0.85,
                fix_hint: Some(
                    "Add cache_control={\"type\": \"ephemeral\"} to the system block. \
                     See https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching"
                        .to_string(),
                ),
            });
        }

        findings
    }
}

/// Return `true` if the text contains a system literal (Python `system=`,
/// JSON `"system":`, TS/JS `system:`) whose quoted value is ≥ 4 096 chars
/// (≈ 1 024 tokens).
fn has_long_system_literal(block: &str) -> bool {
    // Find "system" markers, then scan forward for a string literal and
    // measure its length. We handle Python `system=`, JSON `"system":`,
    // and TypeScript/JavaScript unquoted `system:`.
    for marker in &["system=", "\"system\":", "'system':", "system:"] {
        let Some(pos) = block.find(marker) else {
            continue;
        };
        let rest = &block[pos + marker.len()..];
        let rest = rest.trim_start();
        let literal_len = estimate_string_literal_length(rest);
        if literal_len >= 4096 {
            return true;
        }
    }
    false
}

/// Estimate the length of the first string literal at the start of `s`.
///
/// Returns 0 when no string literal is found. This is intentionally
/// conservative — it only measures single-line or reasonably-short multi-line
/// literals that appear in the same call block.
fn estimate_string_literal_length(s: &str) -> usize {
    // Skip leading whitespace and `f` prefix for f-strings.
    let s = s.trim_start_matches(|c: char| c.is_whitespace());
    let s = s.strip_prefix('f').unwrap_or(s);
    let s = s.strip_prefix('r').unwrap_or(s);

    if s.starts_with("\"\"\"") || s.starts_with("'''") {
        // Triple-quoted: measure until closing triple quote.
        let (q, rest) = if let Some(stripped) = s.strip_prefix("\"\"\"") {
            ("\"\"\"", stripped)
        } else {
            ("'''", &s[3..])
        };
        if let Some(end) = rest.find(q) {
            return rest[..end].len();
        }
        // Unclosed: take as much as we have.
        return rest.len();
    }

    if s.starts_with('"') || s.starts_with('\'') {
        let q = if s.starts_with('"') { '"' } else { '\'' };
        let rest = &s[1..];
        // Find the closing quote, skipping escaped quotes.
        let mut len = 0usize;
        let mut chars = rest.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                chars.next(); // skip escaped char
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
