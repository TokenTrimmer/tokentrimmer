//! Inspect core — rule trait, tree-sitter harness, finding types.
//!
//! See `docs/01-inspect-rule-catalog.md` for the rule taxonomy.

pub mod ast;
pub mod engine;
pub mod output;
pub mod parse;
pub mod symbols;
pub mod walk;

pub use engine::Engine;

/// Re-export the underlying `tree-sitter` crate so downstream crates (e.g.
/// `tt-core`'s content-aware code compressor) can name the [`tree_sitter::Tree`]
/// / [`tree_sitter::Node`] types returned by [`parse::parse_cached`] WITHOUT
/// taking their own `tree-sitter` dependency — the single shared grammar/version.
pub use tree_sitter;

use serde::{Deserialize, Serialize};

/// Severity level for a finding.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Numeric weight — used by `--fail-on` comparison and markdown grouping.
    pub fn weight(self) -> u8 {
        match self {
            Severity::Low => 1,
            Severity::Medium => 2,
            Severity::High => 3,
            Severity::Critical => 4,
        }
    }

    /// Parse from a CLI flag value. Accepts `"low"`, `"medium"`, `"high"`,
    /// `"critical"` (case-insensitive).
    pub fn from_str_ci(s: &str) -> Option<Severity> {
        match s.to_lowercase().as_str() {
            "low" => Some(Severity::Low),
            "medium" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            "critical" => Some(Severity::Critical),
            _ => None,
        }
    }
}

/// Source-language variants that the engine can parse and route rules to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Python,
    Typescript,
    Javascript,
    /// Markdown files such as `AGENTS.md`, `CLAUDE.md`, and `.cursorrules`.
    Markdown,
}

impl Language {
    /// Infer the source language from a file extension (no leading dot), or
    /// `None` for an extension the engine does not scan.
    ///
    /// This is the single source of truth for the extension→language mapping,
    /// shared by the directory [`walk`](crate::walk) and the single-file
    /// `inspect_diff` MCP tool so the two cannot silently drift.
    pub fn from_extension(ext: &str) -> Option<Language> {
        match ext {
            "py" => Some(Language::Python),
            "ts" | "tsx" => Some(Language::Typescript),
            "js" | "jsx" | "mjs" | "cjs" => Some(Language::Javascript),
            "md" => Some(Language::Markdown),
            _ => None,
        }
    }
}

/// A single diagnostic emitted by a rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Stable rule identifier, e.g. `"cache-anthropic-prompt-cache-missing"`.
    pub rule_id: String,
    /// Severity of the finding.
    pub severity: Severity,
    /// File path (relative to scan root if possible).
    pub file: String,
    /// 1-based line number of the finding.
    pub line: u32,
    /// Human-readable description of what was detected.
    pub message: String,
    /// 0.0-1.0. Threshold for --fail-on=high is 0.85.
    pub confidence: f32,
    /// Optional actionable hint for fixing the issue.
    pub fix_hint: Option<String>,
}

/// A single static-analysis rule.
///
/// Implement this trait to add a new check to the engine. Rules are registered
/// at startup; the engine calls `check` once per file whose language is listed
/// in `supported_languages`.
pub trait Rule: Send + Sync {
    /// Stable, unique identifier for this rule.
    fn id(&self) -> &'static str;
    /// Default severity when the rule fires.
    fn severity(&self) -> Severity;
    /// Languages this rule supports; the engine skips the rule for other files.
    fn supported_languages(&self) -> &'static [Language];
    /// Analyse `source` (and optionally an AST) and return any findings.
    fn check(&self, source: &str, language: Language, path: &str) -> Vec<Finding>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_extension_maps_known_and_rejects_unknown() {
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("ts"), Some(Language::Typescript));
        assert_eq!(Language::from_extension("tsx"), Some(Language::Typescript));
        assert_eq!(Language::from_extension("js"), Some(Language::Javascript));
        assert_eq!(Language::from_extension("cjs"), Some(Language::Javascript));
        assert_eq!(Language::from_extension("md"), Some(Language::Markdown));
        // Unknown / extension-less → not scanned.
        assert_eq!(Language::from_extension("txt"), None);
        assert_eq!(Language::from_extension(""), None);
        assert_eq!(Language::from_extension("rs"), None);
    }

    #[test]
    fn language_serializes_lowercase() {
        // inspect_diff echoes this back as `detected_language`.
        assert_eq!(
            serde_json::to_value(Language::Python).unwrap(),
            serde_json::json!("python")
        );
    }
}
