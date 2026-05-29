//! Rule: `model-reasoning-effort-default-high`
//!
//! Detects reasoning-model calls (`o1`, `o3`, `o4-mini`, …) that either set
//! `reasoning_effort="high"` explicitly or omit `reasoning_effort` entirely —
//! in which case the API defaults to a high effort level. High reasoning
//! effort spends many (billed) reasoning tokens; most tasks are well served by
//! `"low"` or `"medium"`, often at a fraction of the cost.

use std::sync::OnceLock;

use regex::Regex;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires on a reasoning-model call with high (or defaulted-high) reasoning effort.
pub struct ModelReasoningEffortDefaultHighRule;

impl ModelReasoningEffortDefaultHighRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for ModelReasoningEffortDefaultHighRule {
    fn default() -> Self {
        Self::new()
    }
}

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

const LLM_CREATE_PATTERNS: &[&str] = &[
    "chat.completions.create",
    "completions.create",
    "responses.create",
];

/// Reasoning-model id fragments, matched as quoted model strings to avoid
/// matching unrelated identifiers.
const REASONING_MODEL_TOKENS: &[&str] = &[
    "\"o1\"",
    "\"o1-",
    "'o1'",
    "'o1-",
    "\"o3\"",
    "\"o3-",
    "'o3'",
    "'o3-",
    "\"o4-mini\"",
    "'o4-mini'",
    "\"o4-mini-",
    "'o4-mini-",
];

fn references_reasoning_model(block: &str) -> bool {
    REASONING_MODEL_TOKENS.iter().any(|t| block.contains(t))
}

/// `true` if `reasoning_effort` is explicitly set to "high". Tolerates
/// `reasoning_effort="high"`, `"reasoning_effort": "high"`, and
/// `reasoning_effort: 'high'` (the optional `["']?` absorbs a JSON key's
/// closing quote).
fn effort_is_high(block: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"(?i)reasoning_effort["']?\s*[:=]\s*["']high["']"#)
            .expect("reasoning_effort regex is valid")
    });
    re.is_match(block)
}

impl Rule for ModelReasoningEffortDefaultHighRule {
    fn id(&self) -> &'static str {
        "model-reasoning-effort-default-high"
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
            if !LLM_CREATE_PATTERNS.iter().any(|p| line.contains(*p)) {
                continue;
            }
            let call_block: String = source
                .lines()
                .skip(line_idx)
                .take(60)
                .collect::<Vec<_>>()
                .join("\n");

            if !references_reasoning_model(&call_block) {
                continue;
            }

            let has_effort = call_block.contains("reasoning_effort");
            let high = effort_is_high(&call_block);
            // Fire when effort is omitted (defaults high) or explicitly high.
            if has_effort && !high {
                continue; // an explicit low/medium effort — fine.
            }

            let reason = if high {
                "sets reasoning_effort=\"high\""
            } else {
                "omits reasoning_effort, which defaults to a high effort level"
            };

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: (line_idx + 1) as u32,
                message: format!(
                    "Reasoning-model call {reason}. High reasoning effort spends many billed \
                     reasoning tokens; most tasks do well at \"low\" or \"medium\"."
                ),
                confidence: 0.75,
                fix_hint: Some(
                    "Set reasoning_effort=\"low\" (or \"medium\") explicitly and raise it only \
                     for tasks that measurably need deeper reasoning."
                        .to_string(),
                ),
            });
        }

        findings
    }
}
