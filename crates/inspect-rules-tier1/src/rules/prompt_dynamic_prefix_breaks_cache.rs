//! Rule: `prompt-dynamic-prefix-breaks-cache`
//!
//! Detects a system/prompt string whose **very first** characters are a
//! dynamic interpolation (timestamp, `uuid`, `datetime.now()`, `Date.now()`,
//! random value, …). Prompt caching (OpenAI automatic prefix cache, Anthropic
//! `cache_control`) keys on a stable prefix; a value that changes every call
//! at the *start* of the prompt invalidates the cache on every request, so the
//! whole prompt is re-billed at full price. Move the dynamic value to the end.

use std::sync::OnceLock;

use regex::Regex;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when a prompt/system string begins with a dynamic interpolation.
pub struct PromptDynamicPrefixBreaksCacheRule;

impl PromptDynamicPrefixBreaksCacheRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for PromptDynamicPrefixBreaksCacheRule {
    fn default() -> Self {
        Self::new()
    }
}

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

/// Matches a string literal that STARTS with a dynamic interpolation:
/// a Python f-string `f"{…dynamic…}` / `f'''{…}` or a JS template
/// `` `${…dynamic…} `` where the interpolation is the first thing in the
/// string. The dynamic token alternation covers the common cache-busters.
fn dynamic_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:f(?:"""|'''|"|')\s*\{|`\s*\$\{)[^}]*?(datetime|utcnow|now\b|today|timestamp|uuid|random|date\.|time\.|date\.now|toisostring)"#,
        )
        .expect("dynamic-prefix regex is valid")
    })
}

/// `true` if the line is in a prompt/system context (so the dynamic prefix is
/// actually a prompt, not an arbitrary log line).
fn has_prompt_context(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("system") || l.contains("prompt") || l.contains("instruction")
}

impl Rule for PromptDynamicPrefixBreaksCacheRule {
    fn id(&self) -> &'static str {
        "prompt-dynamic-prefix-breaks-cache"
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

        let re = dynamic_prefix_regex();
        let mut findings = Vec::new();
        for (line_idx, line) in source.lines().enumerate() {
            if !has_prompt_context(line) {
                continue;
            }
            if !re.is_match(line) {
                continue;
            }
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: "Prompt/system string begins with a dynamic value (timestamp, uuid, \
                          now(), …). A changing prefix invalidates prompt caching every call, \
                          re-billing the entire prompt at full input price."
                    .to_string(),
                confidence: 0.8,
                fix_hint: Some(
                    "Keep the prompt prefix static and append any per-request dynamic value at \
                     the END (or pass it as a separate user message) so the cacheable prefix is \
                     stable across calls."
                        .to_string(),
                ),
            });
        }

        findings
    }
}
