//! Cheaper-equivalent route suggestions.
//!
//! For each candidate cheaper model, compute the cost. Per-task-class
//! whitelists determine acceptability. Quality risk band is `UNKNOWN`
//! by default; cloud-side enrichment may upgrade to LOW/MEDIUM/HIGH.

use crate::classifier::TaskClass;
use crate::pricing::cost_usd;
use crate::types::{QualityRiskBand, RouteSuggestion};

/// Candidate cheaper models per task class, each paired with its provider so
/// pricing is attributed from the RIGHT catalog (avoids the probe-order
/// mis-attribution for any cross-listed model). Ordered by preference.
fn candidates_for(class: TaskClass) -> &'static [(&'static str, &'static str)] {
    match class {
        TaskClass::Classification => &[
            ("claude-haiku-4-5", "anthropic"),
            ("gpt-4o-mini", "openai"),
            ("gemini-3.1-flash-lite", "gemini"),
        ],
        TaskClass::Extraction => &[
            ("claude-haiku-4-5", "anthropic"),
            ("gpt-4o-mini", "openai"),
            ("gemini-3.5-flash", "gemini"),
        ],
        TaskClass::Chat => &[("claude-haiku-4-5", "anthropic"), ("gpt-4o-mini", "openai")],
        TaskClass::Code => &[("claude-haiku-4-5", "anthropic"), ("gpt-4o-mini", "openai")],
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
    for &(candidate, provider) in candidates_for(task_class) {
        if candidate == current_model {
            continue;
        }
        let Ok(hit) = crate::pricing::lookup_with_provider(candidate, provider) else {
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
                "{candidate} is a lower-cost model for {task_class:?}-style tasks by a static \
                 pricing + capability heuristic — not based on your telemetry. Quality \
                 band not yet computed for your org (UNKNOWN); enable Plan engine \
                 quality scoring to upgrade to LOW/MEDIUM/HIGH.",
            ),
            applicable: true,
        });
        if out.len() >= 3 {
            break;
        }
    }
    out
}

/// Populate the reserved [`QualityRiskBand`] hook on a suggestion from a judged
/// band, where one is available.
///
/// The live gateway runs a sampled quality judge on rerouted-DOWN traffic
/// (`tt_core::quality_sample`) and records a per-`(requested_model → served_model)`
/// [`QualityRiskBand`]. This is the enrichment seam that lifts a suggestion from
/// the default `Unknown` to that judged band: `judged_band_for(&suggestion)`
/// returns `Some(band)` when the judge has scored that swap, `None` to leave the
/// suggestion at `Unknown`. Only `LOW`/`MEDIUM`/`HIGH` overwrite; a `None` or
/// `Unknown` lookup is a no-op so an unjudged swap stays honestly `Unknown`.
///
/// Record-only: enrichment never changes `applicable` or pauses a route — the
/// band is an advisory signal surfaced to the user.
pub fn enrich_with_judged_bands<F>(suggestions: &mut [RouteSuggestion], judged_band_for: F)
where
    F: Fn(&RouteSuggestion) -> Option<QualityRiskBand>,
{
    for s in suggestions.iter_mut() {
        if let Some(band) = judged_band_for(s) {
            if band != QualityRiskBand::Unknown {
                s.quality_risk_band = band;
            }
        }
    }
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

    /// Rationale must not claim telemetry-backed history it doesn't have — the
    /// suggestion is a static pricing/capability heuristic (mirrors the honesty
    /// guard on `find_route_for`).
    #[test]
    fn rationale_makes_no_unsubstantiated_history_claim() {
        let v = suggest("claude-opus-4-7", 1.0, 1000, 1000, TaskClass::Extraction);
        assert!(!v.is_empty(), "expected at least one cheaper suggestion");
        for s in &v {
            let lower = s.rationale.to_lowercase();
            assert!(
                !lower.contains("historically"),
                "rationale must not claim historical/telemetry data: {}",
                s.rationale
            );
            assert!(
                lower.contains("not based on your telemetry"),
                "rationale must disclose it is a heuristic, not telemetry: {}",
                s.rationale
            );
        }
    }

    fn suggestion(model: &str) -> RouteSuggestion {
        RouteSuggestion {
            route: format!("swap-to-{model}"),
            model: model.into(),
            cost_usd: 0.001,
            savings_usd: 0.01,
            quality_risk_band: QualityRiskBand::Unknown,
            rationale: "x".into(),
            applicable: true,
        }
    }

    #[test]
    fn enrichment_upgrades_unknown_to_judged_band() {
        let mut v = vec![suggestion("gpt-4o-mini"), suggestion("claude-haiku-4-5")];
        enrich_with_judged_bands(&mut v, |s| {
            (s.model == "gpt-4o-mini").then_some(QualityRiskBand::High)
        });
        assert_eq!(v[0].quality_risk_band, QualityRiskBand::High);
        // No judged band for the second swap → stays Unknown.
        assert_eq!(v[1].quality_risk_band, QualityRiskBand::Unknown);
    }

    #[test]
    fn enrichment_none_or_unknown_is_a_noop() {
        let mut v = vec![suggestion("gpt-4o-mini")];
        enrich_with_judged_bands(&mut v, |_| None);
        assert_eq!(v[0].quality_risk_band, QualityRiskBand::Unknown);
        enrich_with_judged_bands(&mut v, |_| Some(QualityRiskBand::Unknown));
        assert_eq!(v[0].quality_risk_band, QualityRiskBand::Unknown);
    }
}
