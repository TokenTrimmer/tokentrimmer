//! Rule: `config-agents-md-contains-secrets`
//!
//! Scans `AGENTS.md`, `CLAUDE.md`, `.cursorrules`, and `.cursor/rules/*.md`
//! files for common secret patterns. These files are committed to version
//! control and loaded into AI agent contexts — either leakage vector is
//! critical.

use regex::Regex;
use std::sync::OnceLock;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when a secret pattern is found inside an agent configuration file.
pub struct ConfigAgentsMdContainsSecretsRule;

impl ConfigAgentsMdContainsSecretsRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ConfigAgentsMdContainsSecretsRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Return `true` when `path` is an agent configuration file we should scan.
fn is_agents_config_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with("agents.md")
        || lower.ends_with("claude.md")
        || lower.ends_with(".cursorrules")
        || lower.contains(".cursor/rules")
}

/// Secret pattern descriptors: (human label, regex pattern).
static SECRET_PATTERNS: &[(&str, &str)] = &[
    ("Anthropic API key", r"sk-ant-[A-Za-z0-9_-]{32,}"),
    // OpenAI scoped keys (modern): sk-proj-…, sk-svcacct-…, sk-admin-…
    (
        "OpenAI API key (scoped)",
        r"sk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{20,}",
    ),
    // Legacy bare OpenAI key: sk- + 32+ base62 chars. Raising the floor from
    // the old {20,} cuts the broad false-positive class (random sk-+20 strings)
    // while still catching real legacy keys (sk- + 48 chars).
    ("OpenAI API key (legacy)", r"sk-[A-Za-z0-9]{32,}"),
    ("Stripe live secret key", r"sk_live_[A-Za-z0-9]{20,}"),
    ("AWS Access Key ID", r"AKIA[0-9A-Z]{16}"),
    ("Google API key", r"AIza[0-9A-Za-z_-]{35}"),
    ("GitHub personal access token", r"ghp_[A-Za-z0-9]{36,}"),
    (
        "Generic API key / secret / token",
        r#"(?i)(api[_\-]?key|secret|token)["'\s]*[:=]\s*["'][A-Za-z0-9!@#$%^&*_\-]{20,}["']"#,
    ),
];

/// Compiled regexes, initialised once.
static COMPILED: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

fn compiled_patterns() -> &'static Vec<(&'static str, Regex)> {
    COMPILED.get_or_init(|| {
        SECRET_PATTERNS
            .iter()
            .map(|(label, pattern)| (*label, Regex::new(pattern).expect("invalid secret regex")))
            .collect()
    })
}

impl Rule for ConfigAgentsMdContainsSecretsRule {
    fn id(&self) -> &'static str {
        "config-agents-md-contains-secrets"
    }

    fn severity(&self) -> Severity {
        Severity::Critical
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Markdown]
    }

    fn check(&self, source: &str, _language: Language, path: &str) -> Vec<Finding> {
        // Only check agent configuration files.
        if !is_agents_config_file(path) {
            return vec![];
        }

        let patterns = compiled_patterns();
        let mut findings = Vec::new();

        for (line_idx, line) in source.lines().enumerate() {
            for (label, re) in patterns {
                if re.is_match(line) {
                    findings.push(Finding {
                        rule_id: self.id().to_string(),
                        severity: self.severity(),
                        file: path.to_string(),
                        line: (line_idx + 1) as u32,
                        message: format!(
                            "Possible {label} detected in agent config file. This file is \
                             committed to version control and read by AI tools — secret \
                             exposure is critical."
                        ),
                        confidence: 0.95,
                        fix_hint: Some(
                            "Remove the secret immediately. Rotate the compromised key. \
                             Use environment variables or a secrets manager instead."
                                .to_string(),
                        ),
                    });
                    // One finding per line is sufficient.
                    break;
                }
            }
        }

        findings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(line: &str) -> Vec<Finding> {
        ConfigAgentsMdContainsSecretsRule::new().check(line, Language::Markdown, "CLAUDE.md")
    }

    #[test]
    fn detects_modern_scoped_openai_keys() {
        assert!(
            !scan("OPENAI_API_KEY=sk-proj-abcdefghijklmno0123456789_-XY").is_empty(),
            "sk-proj- scoped key should fire"
        );
        assert!(
            !scan("key: sk-svcacct-abcdefghijklmnopqrst0123").is_empty(),
            "sk-svcacct- scoped key should fire"
        );
    }

    #[test]
    fn detects_legacy_48char_openai_key() {
        // Legacy bare key: sk- + exactly 48 base62 chars.
        let key = format!("token = sk-{}", "a".repeat(48));
        assert!(!scan(&key).is_empty(), "legacy 48-char key should fire");
    }

    #[test]
    fn does_not_fire_on_short_sk_dash_junk() {
        // The old `sk-`+20-alnum pattern over-matched; a bare sk- + 20 chars
        // that is neither a scoped key nor a 48-char legacy key must not fire
        // as an OpenAI key.
        let findings = scan("ref = sk-abcdefghij1234567890");
        assert!(
            findings.is_empty(),
            "sk-+20-alnum junk should no longer match as an OpenAI key, got {findings:?}"
        );
    }

    #[test]
    fn still_detects_anthropic_keys() {
        let key = format!("ANTHROPIC_API_KEY=sk-ant-{}", "a".repeat(40));
        let findings = scan(&key);
        assert!(!findings.is_empty(), "anthropic key should still fire");
    }

    #[test]
    fn ignores_non_config_files() {
        let key = format!("sk-proj-{}", "a".repeat(30));
        let findings =
            ConfigAgentsMdContainsSecretsRule::new().check(&key, Language::Markdown, "README.md");
        assert!(findings.is_empty(), "non-config file must not be scanned");
    }
}
