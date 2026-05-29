//! Cheaper-equivalent route suggestions.
//!
//! For each candidate cheaper model, compute the cost. Per-task-class
//! whitelists determine acceptability. Quality risk band is `UNKNOWN`
//! by default; cloud-side enrichment may upgrade to LOW/MEDIUM/HIGH.

use crate::classifier::TaskClass;
use crate::pricing::cost_usd;
use crate::types::{QualityRiskBand, RouteSuggestion};

/// Candidate cheaper models per task class. Ordered by preference.
fn candidates_for(class: TaskClass) -> &'static [&'static str] {
    match class {
        TaskClass::Classification => &["claude-haiku-4-5", "gpt-4o-mini", "gemini-2-5-flash-lite"],
        TaskClass::Extraction => &["claude-haiku-4-5", "gpt-4o-mini", "gemini-2-5-flash"],
        TaskClass::Chat => &["claude-haiku-4-5", "gpt-4o-mini"],
        TaskClass::Code => &["claude-haiku-4-5", "gpt-4o-mini"],
        TaskClass::Agent => &[],
    }
}

pub fn suggest(
    current_model: &str,
    current_cost_usd: f64,
    input_tokens: u32,
    output_tokens: u32,
    task_class: TaskClass,
) -> Vec<RouteSuggestion> {
    let mut out = Vec::new();
    for &candidate in candidates_for(task_class) {
        if candidate == current_model {
            continue;
        }
        let Ok(hit) = crate::pricing::lookup(candidate) else {
            continue;
        };
        let cost = cost_usd(input_tokens, output_tokens, &hit);
        if cost >= current_cost_usd {
            continue;
        }
        out.push(RouteSuggestion {
            route: format!("swap-to-{candidate}"),
            model: candidate.into(),
            cost_usd: cost,
            savings_usd: current_cost_usd - cost,
            quality_risk_band: QualityRiskBand::Unknown,
            rationale: format!(
                "{candidate} historically handles {:?} tasks at lower cost. Quality \
                 band not yet computed for your org (UNKNOWN); enable Plan engine \
                 quality scoring to upgrade to LOW/MEDIUM/HIGH.",
                task_class,
            ),
            applicable: true,
        });
        if out.len() >= 3 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_class_yields_no_suggestions() {
        let v = suggest("claude-opus", 1.0, 100, 100, TaskClass::Agent);
        assert!(v.is_empty());
    }

    #[test]
    fn excludes_current_model() {
        let v = suggest(
            "claude-haiku-4-5",
            0.001,
            100,
            100,
            TaskClass::Classification,
        );
        assert!(!v.iter().any(|s| s.model == "claude-haiku-4-5"));
    }

    #[test]
    fn caps_at_3_suggestions() {
        let v = suggest("claude-opus", 1.0, 1000, 1000, TaskClass::Extraction);
        assert!(v.len() <= 3);
    }
}
