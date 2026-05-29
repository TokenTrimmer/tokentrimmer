//! Rule: `model-flagship-for-extraction`
//!
//! Detects LLM calls that use a flagship (expensive) model for a structured
//! data extraction task — identifiable by extraction-intent keywords. A
//! smaller model would suffice at a fraction of the cost.

use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when a flagship model is used with an extraction-style prompt.
pub struct ModelFlagshipForExtractionRule;

impl ModelFlagshipForExtractionRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ModelFlagshipForExtractionRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Flagship model names.
const FLAGSHIP_MODELS: &[&str] = &[
    "gpt-4o",
    "gpt-4-turbo",
    "claude-opus",
    "claude-3-opus",
    "claude-sonnet",
    "claude-3-sonnet",
    "claude-3-5-sonnet",
    "claude-3-7-sonnet",
    "gemini-1.5-pro",
    "gemini-2.0-pro",
    "gemini-2.5-pro",
    "gemini-pro",
];

/// Small-model identifiers — skip flagging these.
const SMALL_MODEL_SIGNALS: &[&str] = &["mini", "haiku", "flash", "nano"];

/// Keywords indicating an extraction / parsing task.
const EXTRACTION_KEYWORDS: &[&str] = &[
    "extract",
    "parse",
    "structured output",
    "json schema",
    "key-value",
    "key_value",
    "list of",
    "pull out",
    "identify fields",
    "entity extraction",
    "named entity",
];

/// Return `true` when the file path indicates a test fixture.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

/// Return `true` if the model string looks like a flagship model.
fn is_flagship(model_str: &str) -> bool {
    let lower = model_str.to_lowercase();
    if SMALL_MODEL_SIGNALS.iter().any(|s| lower.contains(s)) {
        return false;
    }
    FLAGSHIP_MODELS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

impl Rule for ModelFlagshipForExtractionRule {
    fn id(&self) -> &'static str {
        "model-flagship-for-extraction"
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
            if !line.contains("model=")
                && !line.contains("\"model\"")
                && !line.contains("'model'")
                && !line.contains("model:")
            {
                continue;
            }

            let model_value = extract_model_value(line);
            if model_value.is_empty() {
                continue;
            }

            if !is_flagship(&model_value) {
                continue;
            }

            // Collect surrounding context.
            let context: String = source
                .lines()
                .skip(line_idx.saturating_sub(10))
                .take(80)
                .collect::<Vec<_>>()
                .join("\n");

            let lower_ctx = context.to_lowercase();

            let has_extraction = EXTRACTION_KEYWORDS.iter().any(|kw| lower_ctx.contains(kw));
            if !has_extraction {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: format!(
                    "Flagship model '{model_value}' used for what looks like a structured \
                     extraction task. Consider a smaller model (Haiku, GPT-4o-mini, Gemini \
                     Flash) with response_format=json_object for equivalent quality at \
                     15-50× less cost."
                ),
                confidence: 0.6,
                fix_hint: Some(
                    "Use claude-haiku-3-5, gpt-4o-mini, or gemini-2.0-flash with JSON mode \
                     for extraction tasks."
                        .to_string(),
                ),
            });
        }

        findings
    }
}

/// Extract the model string value from a line.
fn extract_model_value(line: &str) -> String {
    let after_model = if let Some(pos) = line.find("model") {
        &line[pos + 5..]
    } else {
        return String::new();
    };

    let rest = after_model.trim_start_matches(|c: char| {
        c == '=' || c == ':' || c.is_whitespace() || c == '"' || c == '\''
    });

    let value: String = rest
        .chars()
        .take_while(|c| *c != '"' && *c != '\'' && *c != ',' && *c != ')' && *c != '}')
        .collect();

    value.trim().to_string()
}
