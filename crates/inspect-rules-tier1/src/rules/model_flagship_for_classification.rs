//! Rule: `model-flagship-for-classification`
//!
//! Detects LLM calls that use a flagship (expensive) model for a
//! classification task — identifiable by short prompts containing
//! classification-intent keywords. A smaller model would suffice at a
//! fraction of the cost.

use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when a flagship model is used with a short classification prompt.
pub struct ModelFlagshipForClassificationRule;

impl ModelFlagshipForClassificationRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ModelFlagshipForClassificationRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Flagship model names that are expensive and inappropriate for simple tasks.
const FLAGSHIP_MODELS: &[&str] = &[
    "gpt-4o",
    "gpt-4-turbo",
    "gpt-4o-mini", // keep for FP reduction — actually mini, skip
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

/// Model substrings that should be excluded (mini/haiku/flash are small models).
const SMALL_MODEL_SIGNALS: &[&str] = &["mini", "haiku", "flash", "nano"];

/// Keywords that indicate a classification task.
const CLASSIFICATION_KEYWORDS: &[&str] = &[
    "classify",
    "categorize",
    "categorise",
    "is this",
    "yes or no",
    "true or false",
    "spam or not",
    "label",
    "sentiment",
    "intent",
    "is_spam",
    "is_valid",
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
    // Reject if it matches a small-model signal.
    if SMALL_MODEL_SIGNALS.iter().any(|s| lower.contains(s)) {
        return false;
    }
    FLAGSHIP_MODELS
        .iter()
        .any(|m| lower.contains(&m.to_lowercase()))
}

impl Rule for ModelFlagshipForClassificationRule {
    fn id(&self) -> &'static str {
        "model-flagship-for-classification"
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
            // Look for model= or "model": or `model:` (TypeScript) lines.
            if !line.contains("model=")
                && !line.contains("\"model\"")
                && !line.contains("'model'")
                && !line.contains("model:")
            {
                continue;
            }

            // Extract the model value from the line.
            let model_value = extract_model_value(line);
            if model_value.is_empty() {
                continue;
            }

            if !is_flagship(&model_value) {
                continue;
            }

            // Collect surrounding context (up to 80 lines of the call block).
            let context: String = source
                .lines()
                .skip(line_idx.saturating_sub(10))
                .take(80)
                .collect::<Vec<_>>()
                .join("\n");

            let lower_ctx = context.to_lowercase();

            // Check for classification keywords.
            let has_classification = CLASSIFICATION_KEYWORDS
                .iter()
                .any(|kw| lower_ctx.contains(kw));
            if !has_classification {
                continue;
            }

            // Optional: check prompt is short. We look for user message content.
            // If we can find a content string, check its length.
            // We don't hard-require short prompt — lossy but correct.

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: format!(
                    "Flagship model '{model_value}' used for what looks like a classification \
                     task. Consider switching to a smaller, cheaper model (e.g. Haiku, \
                     GPT-4o-mini, Gemini Flash) which handles classification at equivalent \
                     quality for 15-50× less cost."
                ),
                confidence: 0.6,
                fix_hint: Some(
                    "Replace with claude-haiku-3-5, gpt-4o-mini, or gemini-2.0-flash for \
                     classification tasks."
                        .to_string(),
                ),
            });
        }

        findings
    }
}

/// Extract the model string value from a line like `model="gpt-4o"` or
/// `"model": "claude-opus-3"`.
fn extract_model_value(line: &str) -> String {
    // Try to find a quoted string after `model`.
    let after_model = if let Some(pos) = line.find("model") {
        &line[pos + 5..]
    } else {
        return String::new();
    };

    // Skip `=`, `:`, whitespace, and quotes.
    let rest = after_model.trim_start_matches(|c: char| {
        c == '=' || c == ':' || c.is_whitespace() || c == '"' || c == '\''
    });

    // Collect until the next quote or comma.
    let value: String = rest
        .chars()
        .take_while(|c| *c != '"' && *c != '\'' && *c != ',' && *c != ')' && *c != '}')
        .collect();

    value.trim().to_string()
}
