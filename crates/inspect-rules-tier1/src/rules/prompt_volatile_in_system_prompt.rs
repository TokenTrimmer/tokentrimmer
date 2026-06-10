//! Rule: `prompt-volatile-in-system-prompt`
//!
//! Detects a **volatile token** — a timestamp, `now()`/`Date.now()`/
//! `datetime.now()`/`utcnow()`, a UUID, or a long random hex/base64 token —
//! embedded *anywhere* inside a system prompt or prompt template.
//!
//! Provider prompt caching (Anthropic `cache_control`, OpenAI automatic prefix
//! cache) keys on a byte-stable system/prefix block. A value that changes on
//! every request — even when it sits in the *middle* or at the *end* of the
//! system prompt — mutates the cached block and forces a full cache miss,
//! re-billing the entire prompt at full input price. This is a silent cost
//! leak: the code looks correct and the cache simply never hits.
//!
//! This rule is broader than [`prompt-dynamic-prefix-breaks-cache`], which only
//! fires when the volatile value is the *very first* thing in the string. Here
//! we flag the token wherever it appears in a prompt/system context, and we
//! also catch *literal* timestamps and UUIDs baked into the string (e.g. values
//! stamped in at build/deploy time), which the prefix-only rule cannot see.
//!
//! False positives are held down by requiring a prompt/system/instruction
//! context word on the same line and by only matching clearly-volatile tokens
//! (a semantic version, a short hex color, or a stable username do not fire).

use std::sync::OnceLock;

use regex::Regex;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when a prompt/system string embeds a volatile (per-call) token.
pub struct PromptVolatileInSystemPromptRule;

impl PromptVolatileInSystemPromptRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for PromptVolatileInSystemPromptRule {
    fn default() -> Self {
        Self::new()
    }
}

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

/// `true` if the line is in a prompt/system context, so a volatile token is
/// actually in a cached prefix rather than an arbitrary log line, cache key,
/// or request id. This context gate is the primary false-positive firewall.
fn has_prompt_context(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("system") || l.contains("prompt") || l.contains("instruction")
}

/// Interpolated dynamic call inside an f-string `{…}` or template `${…}`:
/// `now()`, `utcnow()`, `Date.now()`, `datetime.now()`, `uuid`, `random`, … —
/// the common cache-busters, matched anywhere in the string (not just prefix).
fn interpolated_volatile_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:\{|\$\{)[^{}]*?(datetime|utcnow|now\(|\.now\b|today|timestamp|uuid|uuid4|randuuid|random|date\.now|time\.time|toisostring|monotonic|perf_counter)",
        )
        .expect("interpolated-volatile regex is valid")
    })
}

/// A literal ISO-8601 *date-time* (date plus a time component). We require the
/// time part so a bare date used as prose content does not fire.
fn literal_iso_timestamp_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}").expect("iso-timestamp regex is valid")
    })
}

/// A literal UUID (8-4-4-4-12 hex), case-insensitive.
fn literal_uuid_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
            .expect("uuid regex is valid")
    })
}

/// A long random **hex** token (≥32 hex chars) — the shape of a generated
/// nonce / session id / request token. The 32-char floor keeps short hex ids
/// and colors (e.g. `#ff8800`) out.
///
/// We deliberately scope this literal detector to hex only. A long base64
/// token would require either look-around (unsupported by the RE2-style
/// `regex` crate) or a permissive `[A-Za-z0-9+/]{40,}` class that would
/// false-positive on long identifiers / concatenated prose. Dynamic base64
/// nonces interpolated into a prompt are still caught by the `random` arm of
/// [`interpolated_volatile_regex`].
fn literal_random_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b[0-9a-fA-F]{32,}\b").expect("random-token regex is valid"))
}

/// Returns a short human label for the first volatile token found on `line`, or
/// `None` if the line carries no volatile token.
fn volatile_kind(line: &str) -> Option<&'static str> {
    if interpolated_volatile_regex().is_match(line) {
        Some("a dynamic value (timestamp, now(), uuid, random, …)")
    } else if literal_uuid_regex().is_match(line) {
        Some("a UUID")
    } else if literal_iso_timestamp_regex().is_match(line) {
        Some("an ISO-8601 timestamp")
    } else if literal_random_token_regex().is_match(line) {
        Some("a long random token")
    } else {
        None
    }
}

impl Rule for PromptVolatileInSystemPromptRule {
    fn id(&self) -> &'static str {
        "prompt-volatile-in-system-prompt"
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

        let mut findings = Vec::new();
        for (line_idx, line) in source.lines().enumerate() {
            if !has_prompt_context(line) {
                continue;
            }
            let Some(kind) = volatile_kind(line) else {
                continue;
            };
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: format!(
                    "System prompt / prompt template embeds {kind}. A value that changes per \
                     request invalidates the provider prompt cache (Anthropic cache_control, \
                     OpenAI prefix cache) on every call, re-billing the whole prompt at full \
                     input price."
                ),
                confidence: 0.8,
                fix_hint: Some(
                    "Move the volatile content OUTSIDE the cached prefix: keep the system prompt \
                     byte-stable and pass any per-request timestamp / id / token as a separate \
                     user message (or append it after the cached block) so the cacheable prefix \
                     stays identical across calls."
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

    fn scan(src: &str) -> Vec<Finding> {
        PromptVolatileInSystemPromptRule::new().check(src, Language::Python, "/src/app.py")
    }

    #[test]
    fn fires_on_interpolated_now_in_system_prompt() {
        let src = r#"system_prompt = f"You are helpful. {datetime.datetime.now()} go.""#;
        assert_eq!(scan(src).len(), 1);
    }

    #[test]
    fn fires_on_literal_uuid_in_prompt() {
        let src = r#"prompt = "Instruction id 550e8400-e29b-41d4-a716-446655440000.""#;
        assert_eq!(scan(src).len(), 1);
    }

    #[test]
    fn does_not_fire_without_prompt_context() {
        let src = r#"log = f"{datetime.datetime.now()} hello""#;
        assert!(scan(src).is_empty());
    }

    #[test]
    fn does_not_fire_on_short_hex_in_prompt() {
        let src = r#"system_prompt = "color #ff8800 brand""#;
        assert!(scan(src).is_empty());
    }

    #[test]
    fn does_not_fire_on_version_in_prompt() {
        let src = r#"system_prompt = "assistant v1.2.3 be brief""#;
        assert!(scan(src).is_empty());
    }

    #[test]
    fn skips_test_fixture_paths() {
        let src = r#"system_prompt = f"{datetime.now()} hi""#;
        let findings = PromptVolatileInSystemPromptRule::new().check(
            src,
            Language::Python,
            "/repo/crates/x/tests/rules/foo/should-detect/01.py",
        );
        assert!(
            findings.is_empty(),
            "fixture paths are suppressed in production scan"
        );
    }
}
