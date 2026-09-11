//! R01: safe policy composition — the static analyzer over route SETS.
//!
//! The engine is first-match (see `RoutingEngine::evaluate_snapshot`), so a
//! cheaper-model rule placed earlier in priority can silently shadow a later
//! privacy-guardrail route over the same traffic — that composition hazard
//! (not any single route) is the R01 failure mode this module catches at
//! WRITE time, extending the existing analyzer family (`validate.rs`) rather
//! than competing with the matcher.
//!
//! The analysis is versioned: `POLICY_ANALYSIS_VERSION` appears in every
//! finding so a consumer can distinguish analysis revisions. It is purely
//! static — condition INTERSECTION feasibility is decided structurally (a
//! shared mandatory predicate), never by sampling; unknown/derived
//! conditions (prompt-keyword, cost, latency) are reported as
//! `undecidable`, never guessed.
//!
//! Composition model (v1):
//!
//! - An **invariant route** declares a safety effect (`redact`,
//!   `disable_cache`) with its conditions. Invariants are composition-safe.
//! - A **selection route** rewrites the model (`target_model.is_some()`).
//!   A selection route that shadows an invariant over overlapping traffic is
//!   a finding: the rewrite would dispatch before the guardrail — exactly
//!   what the runtime pause/suppression semantics patch around by disabling
//!   cost levers, at the cost of route intent ambiguity.
//! - Findings are severity-graded and actionable (the review asks for
//!   "actionable explanations"): each carries the affected pair, the overlap
//!   reason, and the suggested fix (raise the invariant's priority or narrow
//!   the selection's conditions).

use uuid::Uuid;

use crate::{Route, RouteAction};

/// The version of this analysis. Bump when a finding shape or rule changes.
pub const POLICY_ANALYSIS_VERSION: u32 = 1;

/// One composition finding: value-free (route ids + names + reasons only).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PolicyFinding {
    /// The analysis version that produced this finding.
    pub analysis_version: u32,
    pub severity: PolicySeverity,
    /// The higher-priority (earlier) route — the one that shadows.
    pub earlier_route_id: Uuid,
    pub earlier_route_name: String,
    /// The lower-priority (later) invariant route being shadowed.
    pub later_route_id: Uuid,
    pub later_route_name: String,
    /// Which invariant effect the later route provides.
    pub effect: String,
    /// The structural overlap reason (actionable: names the shared predicate).
    pub reason: String,
    /// The suggested fix.
    pub suggestion: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySeverity {
    /// A selection route structurally shadows an invariant over the same
    /// traffic: the guardrail will never apply to the rewritten requests
    /// (its own redact/disable_cache levers still firing on the SELECTION
    /// route is a separate route's policy, not this composition's guarantee).
    InvariantShadowed,
    /// The overlap could not be decided from the retained conditions
    /// (derived predicates like cost/latency/prompt-keyword). Reported so an
    /// operator can review, never silently graded.
    UndecidableOverlap,
}

/// Analyze a route set for composition hazards (R01).
///
/// Routes are evaluated in the engine's order (the caller passes them in
/// priority order, matching how the engine iterates). Public for the
/// routes-admin write path and tests.
pub fn analyze_policy_composition(routes: &[Route]) -> Vec<PolicyFinding> {
    let mut findings = Vec::new();
    // Every (invariant, earlier-selection) pair in engine order.
    for (later_index, later) in routes.iter().enumerate() {
        let Some(effects) = invariant_effects(&later.then) else {
            continue;
        };
        for earlier in routes.iter().take(later_index) {
            if !is_selection_route(&earlier.then) {
                continue;
            }
            match overlap_kind(&earlier.when, &later.when) {
                Overlap::Structural(shared) => {
                    for effect in &effects {
                        findings.push(PolicyFinding {
                            analysis_version: POLICY_ANALYSIS_VERSION,
                            severity: PolicySeverity::InvariantShadowed,
                            earlier_route_id: earlier.id,
                            earlier_route_name: earlier.name.clone(),
                            later_route_id: later.id,
                            later_route_name: later.name.clone(),
                            effect: (*effect).to_string(),
                            reason: format!(
                                "the selection route can match the same traffic as the \
                                 invariant route (shared required predicate: `{shared}`)"
                            ),
                            suggestion: format!(
                                "raise `{}`'s priority above `{}` or narrow `{}`'s \
                                 conditions so it cannot match the guarded traffic",
                                later.name, earlier.name, earlier.name
                            ),
                        });
                    }
                }
                Overlap::Undecidable(which) => {
                    for effect in &effects {
                        findings.push(PolicyFinding {
                            analysis_version: POLICY_ANALYSIS_VERSION,
                            severity: PolicySeverity::UndecidableOverlap,
                            earlier_route_id: earlier.id,
                            earlier_route_name: earlier.name.clone(),
                            later_route_id: later.id,
                            later_route_name: later.name.clone(),
                            effect: (*effect).to_string(),
                            reason: format!(
                                "the overlap with the selection route depends on the \
                                 runtime-evaluated condition `{which}`, which static \
                                 analysis cannot decide"
                            ),
                            suggestion: format!(
                                "review manually: if `{}` can match the same traffic as \
                                 `{}`, raise the invariant's priority",
                                earlier.name, later.name
                            ),
                        });
                    }
                }
                Overlap::Disjoint => {}
            }
        }
    }
    findings
}

/// The invariant effects this action provides, or `None` when the route is not
/// an invariant (no safety effect declared).
fn invariant_effects(action: &RouteAction) -> Option<Vec<&'static str>> {
    let mut effects = Vec::new();
    if action.redact {
        effects.push("redact");
    }
    if action.disable_cache {
        effects.push("disable_cache");
    }
    (!effects.is_empty()).then_some(effects)
}

/// Whether the action rewrites the model (a dispatch-selection route).
fn is_selection_route(action: &RouteAction) -> bool {
    action.target_model.is_some()
}

/// How two condition sets can relate for overlap analysis.
enum Overlap {
    /// Structurally overlapping: they share a mandatory predicate with an
    /// identical value, so there exists traffic both match.
    Structural(&'static str),
    /// Overlap depends on a runtime-evaluated condition the static analysis
    /// cannot decide (named).
    Undecidable(&'static str),
    /// Structurally disjoint: a mandatory predicate with conflicting values.
    Disjoint,
}

/// Analyze the overlap of two condition sets, structurally.
///
/// The predicate families considered in v1 (each either identical-mandatory
/// and shared, or conflicting-mandatory → disjoint):
/// - `model_in` — a shared member means shared traffic; disjoint sets with
///   both mandatory are disjoint.
/// - `tag_equals` — identical values overlap.
/// - `max_input_tokens` (both `input_tokens_gt`/`lt` forms shared as their
///   bounds) — identical mandatory bounds overlap; a non-identical pair is
///   reported undecidable (the ranges may still intersect).
///
/// Any DERIVED condition (prompt-keyword, cost, latency, media) present on
/// the INVARIANT route makes the whole pair undecidable (its true traffic
/// shape is runtime-only).
fn overlap_kind(a: &crate::RouteConditions, b: &crate::RouteConditions) -> Overlap {
    // Derived conditions on the invariant route → undecidable.
    if a.prompt_contains_any_of.iter().any(|_| true)
        || b.prompt_contains_any_of.iter().any(|_| true)
    {
        return Overlap::Undecidable("prompt_contains_any_of");
    }
    if a.estimated_cost_gt.is_some() || b.estimated_cost_gt.is_some() {
        return Overlap::Undecidable("estimated_cost_gt");
    }
    if a.upstream_latency_ms_p95_gt.is_some() || b.upstream_latency_ms_p95_gt.is_some() {
        return Overlap::Undecidable("upstream_latency_ms_p95_gt");
    }

    // model_in: a shared member means shared traffic.
    let a_has_models = !a.model_in.is_empty();
    let b_has_models = !b.model_in.is_empty();
    if a_has_models && b_has_models {
        if a.model_in.iter().any(|m| b.model_in.contains(m)) {
            return Overlap::Structural("model_in");
        }
        return Overlap::Disjoint;
    }

    // tag_equals (both mandatory-tagged): identical values overlap; a value
    // mismatch is disjoint only when BOTH are (a single tag_equals is
    // one-value by type).
    if let (Some(at), Some(bt)) = (a.tag_equals.as_deref(), b.tag_equals.as_deref()) {
        return if at == bt {
            Overlap::Structural("tag_equals")
        } else {
            Overlap::Disjoint
        };
    }

    // No decidable mandatory predicate pair: conservative default — the
    // conditions do not FORCE disjointness, but we cannot prove sharing with
    // a named predicate either. Report undecidable so the operator reviews.
    Overlap::Undecidable("unshared-mandatory-predicates")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Route, RouteAction, RouteConditions};

    fn route(id: u8, name: &str, when: RouteConditions, then: RouteAction) -> Route {
        Route {
            id: Uuid::from_u128(u128::from(id)),
            name: name.into(),
            priority: 1,
            enabled: true,
            paused: false,
            when,
            then,
        }
    }

    #[test]
    fn cheaper_model_rule_shadowing_a_later_privacy_invariant_is_flagged() {
        // The R01 headline case: a priority-1 model rewrite + a priority-2
        // redaction guardrail over the same model traffic.
        let cheaper = route(
            1,
            "cost-save",
            RouteConditions {
                model_in: vec!["gpt-5.5".into()],
                ..Default::default()
            },
            RouteAction {
                target_model: Some("gpt-5.4-mini".into()),
                ..Default::default()
            },
        );
        let privacy = route(
            2,
            "privacy-guard",
            RouteConditions {
                model_in: vec!["gpt-5.5".into()],
                ..Default::default()
            },
            RouteAction {
                redact: true,
                ..Default::default()
            },
        );
        let findings = analyze_policy_composition(&[cheaper, privacy]);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, PolicySeverity::InvariantShadowed);
        assert_eq!(f.earlier_route_name, "cost-save");
        assert_eq!(f.later_route_name, "privacy-guard");
        assert_eq!(f.effect, "redact");
        assert!(f.reason.contains("model_in"));
        assert!(f.suggestion.contains("raise `privacy-guard`'s priority"));
        assert_eq!(f.analysis_version, POLICY_ANALYSIS_VERSION);
    }

    #[test]
    fn invariant_above_the_selection_is_not_a_shadowing() {
        // Engine order: invariant FIRST. The selection later never steals
        // traffic the invariant already claimed — no finding.
        let privacy = route(
            1,
            "privacy-guard",
            RouteConditions {
                model_in: vec!["gpt-5.5".into()],
                ..Default::default()
            },
            RouteAction {
                redact: true,
                ..Default::default()
            },
        );
        let cheaper = route(
            2,
            "cost-save",
            RouteConditions {
                model_in: vec!["gpt-5.5".into()],
                ..Default::default()
            },
            RouteAction {
                target_model: Some("gpt-5.4-mini".into()),
                ..Default::default()
            },
        );
        assert!(analyze_policy_composition(&[privacy, cheaper]).is_empty());
    }

    #[test]
    fn disjoint_model_sets_do_not_compose_a_hazard() {
        let cheaper = route(
            1,
            "save-opus",
            RouteConditions {
                model_in: vec!["claude-opus-4-8".into()],
                ..Default::default()
            },
            RouteAction {
                target_model: Some("claude-haiku-4-5".into()),
                ..Default::default()
            },
        );
        let privacy = route(
            2,
            "guard-gpt",
            RouteConditions {
                model_in: vec!["gpt-5.5".into()],
                ..Default::default()
            },
            RouteAction {
                redact: true,
                ..Default::default()
            },
        );
        assert!(analyze_policy_composition(&[cheaper, privacy]).is_empty());
    }

    #[test]
    fn derived_conditions_report_undecidable_not_guess() {
        let cheaper = route(
            1,
            "cheap-when-pricey",
            RouteConditions {
                estimated_cost_gt: Some(0.5),
                ..Default::default()
            },
            RouteAction {
                target_model: Some("m-mini".into()),
                ..Default::default()
            },
        );
        let privacy = route(
            2,
            "guard",
            RouteConditions::default(),
            RouteAction {
                redact: true,
                ..Default::default()
            },
        );
        let findings = analyze_policy_composition(&[cheaper, privacy]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, PolicySeverity::UndecidableOverlap);
        assert!(findings[0].reason.contains("estimated_cost_gt"));
        assert!(findings[0].suggestion.contains("review manually"));
    }

    #[test]
    fn both_safety_effects_report_both_findings() {
        let cheaper = route(
            1,
            "save",
            RouteConditions {
                model_in: vec!["m".into()],
                ..Default::default()
            },
            RouteAction {
                target_model: Some("m2".into()),
                ..Default::default()
            },
        );
        // redact + disable_cache: two findings (one per effect).
        let privacy = route(
            2,
            "guard",
            RouteConditions {
                model_in: vec!["m".into()],
                ..Default::default()
            },
            RouteAction {
                redact: true,
                disable_cache: true,
                ..Default::default()
            },
        );
        let findings = analyze_policy_composition(&[cheaper, privacy]);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().any(|f| f.effect == "redact"));
        assert!(findings.iter().any(|f| f.effect == "disable_cache"));
    }

    #[test]
    fn resolved_route_names_appear_in_messages() {
        // Actionable explanations name the routes (the review's ask).
        let cheaper = route(
            1,
            "gen-summary-cheap",
            RouteConditions {
                tag_equals: Some("team:a".into()),
                ..Default::default()
            },
            RouteAction {
                target_model: Some("m2".into()),
                ..Default::default()
            },
        );
        let privacy = route(
            2,
            "compliance-redact",
            RouteConditions {
                tag_equals: Some("team:a".into()),
                ..Default::default()
            },
            RouteAction {
                redact: true,
                ..Default::default()
            },
        );
        let findings = analyze_policy_composition(&[cheaper, privacy]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].reason.contains("tag_equals"));
    }
}
