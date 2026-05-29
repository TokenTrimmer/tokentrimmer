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
    ("OpenAI API key", r"sk-[A-Za-z0-9]{20,}"),
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
