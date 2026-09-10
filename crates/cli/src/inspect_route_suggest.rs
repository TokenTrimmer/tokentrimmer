//! G06: bridge from Inspect findings to actionable route proposals.
//!
//! When `tt inspect` detects a cost-waste pattern (e.g., "flagship model
//! on classification work"), this module maps that finding to a structured
//! route suggestion — the detected model, a cheaper replacement, and the
//! Inspect rule that motivated it. The suggestion feeds the existing route
//! creation workflow (CLI route add, dashboard prefill, plan simulate).

use serde::Serialize;

/// One actionable route suggestion derived from an Inspect finding.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InspectRouteSuggestion {
    /// The Inspect rule that fired (e.g., "model-flagship-for-classification").
    pub rule_id: String,
    /// The model the code currently calls (from the finding's message).
    pub source_model: String,
    /// The cheaper model the detection suggests routing to.
    pub target_model: String,
    /// Human-readable reason (from the rule's fix_hint, structured).
    pub reason: String,
    /// The route action this maps to (always a model rewrite today).
    pub action: &'static str,
}

/// Route-actionable Inspect rule IDs.
pub const RULE_FLAGSHIP_CLASSIFICATION: &str = "model-flagship-for-classification";
pub const RULE_FLAGSHIP_EXTRACTION: &str = "model-flagship-for-extraction";
pub const RULE_REASONING_EFFORT_HIGH: &str = "model-reasoning-effort-default-high";
pub const RULE_DEPRECATED_MODEL: &str = "model-deprecated";

/// Map an Inspect finding to a route suggestion when its rule identifies
/// an actionable routing opportunity. Returns `None` for rules that need
/// code changes rather than routing changes (e.g., unbounded loops).
#[must_use]
pub fn suggest_from_finding(finding: &tt_inspect_core::Finding) -> Option<InspectRouteSuggestion> {
    let (target, reason) = match finding.rule_id.as_str() {
        RULE_FLAGSHIP_CLASSIFICATION => (
            "gpt-4o-mini",
            "Flagship model detected on classification work — a cheaper classification model can carry this traffic.",
        ),
        RULE_FLAGSHIP_EXTRACTION => (
            "gpt-4o-mini",
            "Flagship model detected on extraction work — a cheaper extraction model with JSON mode can carry this traffic.",
        ),
        RULE_REASONING_EFFORT_HIGH => (
            "gpt-4o-mini",
            "Reasoning effort defaults to high — route to a model without default high reasoning.",
        ),
        _ => return None,
    };
    let source_model =
        extract_model_from_message(&finding.message).unwrap_or_else(|| "unknown".to_string());
    Some(InspectRouteSuggestion {
        rule_id: finding.rule_id.clone(),
        source_model,
        target_model: target.to_string(),
        reason: reason.to_string(),
        action: "model_rewrite",
    })
}

/// Generate route suggestions from a full Inspect scan result.
/// Deduplicates by (source_model, target_model) so one codebase with
/// 50 instances of the same waste produces ONE suggestion.
#[must_use]
pub fn route_suggestions_from_findings(
    findings: &[tt_inspect_core::Finding],
) -> Vec<InspectRouteSuggestion> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for f in findings {
        if let Some(suggestion) = suggest_from_finding(f) {
            if seen.insert(suggestion.source_model.clone()) {
                out.push(suggestion);
            }
        }
    }
    out
}

/// Extract the detected model ID from a finding message (best-effort).
/// Finding messages typically contain the model in quotes or after a colon.
fn extract_model_from_message(message: &str) -> Option<String> {
    // Model IDs contain hyphens and dots; look for the longest token that
    // looks like a model identifier (e.g., "claude-sonnet-4-5", "gpt-4o").
    for word in message.split_whitespace() {
        let cleaned = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '.');
        if cleaned.len() > 4
            && cleaned.contains('-')
            && !cleaned.starts_with('-')
            && cleaned.chars().any(|c| c.is_ascii_digit())
        {
            return Some(cleaned.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_inspect_core::{Finding, Severity};

    fn finding(rule: &str, message: &str) -> Finding {
        Finding {
            rule_id: rule.to_string(),
            severity: Severity::Medium,
            file: "src/main.py".to_string(),
            line: 10,
            message: message.to_string(),
            confidence: 0.9,
            fix_hint: None,
        }
    }

    #[test]
    fn flagship_classification_produces_suggestion() {
        let f = finding(
            RULE_FLAGSHIP_CLASSIFICATION,
            "claude-sonnet-4-5 used for classification in src/main.py",
        );
        let s = suggest_from_finding(&f).expect("should produce a suggestion");
        assert_eq!(s.rule_id, RULE_FLAGSHIP_CLASSIFICATION);
        assert_eq!(s.source_model, "claude-sonnet-4-5");
        assert_eq!(s.target_model, "gpt-4o-mini");
        assert_eq!(s.action, "model_rewrite");
    }

    #[test]
    fn non_route_rule_produces_no_suggestion() {
        let f = finding(
            "agent-runaway-loop-tripwire",
            "agent loop without bound in server.py",
        );
        assert!(suggest_from_finding(&f).is_none());
    }

    #[test]
    fn duplicate_models_are_deduplicated() {
        let findings = vec![
            finding(RULE_FLAGSHIP_CLASSIFICATION, "claude-sonnet-4-5 in a.py"),
            finding(RULE_FLAGSHIP_CLASSIFICATION, "claude-sonnet-4-5 in b.py"),
            finding(RULE_FLAGSHIP_EXTRACTION, "claude-sonnet-4-5 in c.py"),
        ];
        let suggestions = route_suggestions_from_findings(&findings);
        assert_eq!(
            suggestions.len(),
            1,
            "same source model should produce one suggestion"
        );
        assert_eq!(suggestions[0].source_model, "claude-sonnet-4-5");
    }

    #[test]
    fn different_source_models_produce_separate_suggestions() {
        let findings = vec![
            finding(RULE_FLAGSHIP_CLASSIFICATION, "claude-sonnet-4-5 in a.py"),
            finding(RULE_FLAGSHIP_CLASSIFICATION, "gpt-4o in b.py"),
        ];
        let suggestions = route_suggestions_from_findings(&findings);
        assert_eq!(suggestions.len(), 2);
        assert_ne!(suggestions[0].source_model, suggestions[1].source_model);
    }

    #[test]
    fn unknown_model_falls_back() {
        let f = finding(
            RULE_FLAGSHIP_CLASSIFICATION,
            "flagship detected with no model id",
        );
        let s = suggest_from_finding(&f).expect("should produce a suggestion");
        assert_eq!(s.source_model, "unknown");
    }
}
