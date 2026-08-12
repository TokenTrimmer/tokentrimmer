//! Pure effective-action re-execution + counterfactual net-savings projection
//! for historical route previews (P1-ROUTES-03).
//!
//! The historical route-preview surface consumes the pinned
//! `tokentrimmer.route.v1` contract definitions owned by the public trees: the
//! manifest-hashed route-preview v2 coverage corpus classifies which
//! [`tt_routing::RouteConditions`] fields a consumer can safely apply to a
//! retained request-log row, and the canonical matcher
//! ([`tt_routing::RouteFeatureSnapshot`] + [`tt_routing::RoutingEngine`])
//! selects the winning route. This module re-derives, purely, what that winning
//! route's configured action chain *would do* for a matched request and
//! projects the request's counterfactual net savings — without consulting a
//! registry, a pricing catalog at runtime, live latency, dispatch state, a
//! clock, or any I/O. Re-execution is deterministic: identical pinned action ⇒
//! identical decisions, so every historical preview embeds the same
//! rewrite/fusion/detour decisions the live matcher records.
//!
//! # Honesty contract (do not weaken)
//!
//! * The net-savings projection applies the shared
//!   [`tt_shared::estimate_request_delta_v1`] formula
//!   (`baseline − cost − provider-cache − cache-bust − summarizer-tax`) and is
//!   labelled a **catalog-priced estimate** (approximate, **not measured**).
//!   It is never invoice reconciliation and never provider-authoritative
//!   pricing — those boundaries are machine-readable on every projection.
//! * Zero and **negative** nets are preserved exactly: a regression is never
//!   zero-floored, and a measured zero is never mislabelled `Unmeasured`.
//! * Missing or unpriceable evidence yields [`ProjectionState::Unmeasured`]
//!   with **no** invented value. The shared formula returns `None` for any
//!   absent, non-finite, or negative component; this module propagates that as
//!   `Unmeasured` instead of substituting a value.

use serde::Serialize;
use tt_routing::{Route, RouteAction};
use tt_shared::{estimate_request_delta_v1, RequestDeltaInput, REQUEST_DELTA_ESTIMATE_V1};

/// Canonical dispatch path a route's effective action chain selects for a
/// matched historical request, pinned by `tokentrimmer.route.v1` precedence:
/// a workflow detour is mutually exclusive with a panel and a `target_model`
/// (validated at route creation), and a fusion/panel governs dispatch when both
/// a panel and a `target_model` are configured (the rewrite is then inert).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchPath {
    /// Direct upstream dispatch, optionally re-written to `target_model`.
    Direct,
    /// Fusion/panel fan-out governs dispatch (no simple rewrite).
    Fusion,
    /// Workflow detour/shadow governs dispatch.
    Workflow,
}

/// Deterministic re-execution of a winning route's effective action chain,
/// derived only from the pinned `tokentrimmer.route.v1` `then` fields. Pure;
/// value-free (no customer text); `Serialize` so a preview can retain the
/// bounded decision next to the matched row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct EffectiveActionExecution {
    /// The dispatch path the route's pinned action selects.
    pub dispatch_path: DispatchPath,
    /// Model the request is re-written to before a **direct** dispatch.
    ///
    /// `None` on the direct path = **modifier-only** route (keep the caller's
    /// requested model; only the action's other effects apply). `None` on any
    /// non-direct path because a panel/workflow governs dispatch instead.
    pub rewrite_target: Option<String>,
    /// `true` when a `target_model` is configured but cannot govern dispatch
    /// because a fusion panel (or, defensively, a workflow) wins by canonical
    /// precedence — the configured rewrite is inert, never a savings claim.
    pub rewrite_inert: bool,
    /// `true` for a modifier-only **direct** route: no rewrite, the caller's
    /// model is kept, and only the action's effects apply.
    pub modifier_only: bool,
    /// Fusion/panel strategy (`"synthesize" | "best_of_n" | "majority"`) when
    /// [`DispatchPath::Fusion`].
    pub fusion_strategy: Option<String>,
    /// Workflow mode (`"detour" | "shadow"`) when [`DispatchPath::Workflow`].
    /// `"detour"` is the route.v1 default when the field is omitted.
    pub workflow_mode: Option<String>,
    /// Detour decision: `true` when the route marks matched traffic
    /// **batch-eligible**. Advisory only — the gateway dispatches
    /// synchronously today, so this does NOT detour the bill and contributes
    /// NO counterfactual savings.
    pub batch_detour: bool,
    /// Deterministic canary split (percent, 0–100) when configured. The
    /// per-request arm is decided by the pinned
    /// [`tt_routing::sticky_traffic_split`] hash elsewhere; this only records
    /// that the route's effective rewrite is split, not unconditional.
    pub traffic_split_pct: Option<u32>,
}

impl EffectiveActionExecution {
    /// Re-execute an action chain from its pinned route.v1 fields.
    ///
    /// Deterministic and total: every field of [`tt_routing::RouteAction`] maps
    /// to exactly one decision, with no external input. Identical action ⇒
    /// identical execution.
    #[must_use]
    pub fn reexecute(then: &RouteAction) -> Self {
        // Canonical dispatch-path precedence (route.v1 validation): workflow
        // detour wins when configured; otherwise a fusion panel governs;
        // otherwise a direct dispatch (optionally re-written).
        let (dispatch_path, fusion_strategy, workflow_mode) =
            if let Some(workflow) = then.workflow.as_ref() {
                (
                    DispatchPath::Workflow,
                    None,
                    Some(workflow.mode.clone().unwrap_or_else(|| "detour".to_owned())),
                )
            } else if let Some(panel) = then.panel.as_ref() {
                (DispatchPath::Fusion, Some(panel.strategy.clone()), None)
            } else {
                (DispatchPath::Direct, None, None)
            };

        let configured_target = then.target_model.clone();
        let is_direct = matches!(dispatch_path, DispatchPath::Direct);
        // A configured target can only govern dispatch on the direct path; on a
        // fusion/workflow path the rewrite is inert (it is not a savings claim).
        let rewrite_target = if is_direct {
            configured_target.clone()
        } else {
            None
        };

        Self {
            dispatch_path,
            rewrite_target,
            rewrite_inert: configured_target.is_some() && !is_direct,
            modifier_only: is_direct && configured_target.is_none(),
            fusion_strategy,
            workflow_mode,
            batch_detour: then.batch,
            traffic_split_pct: then.traffic_pct,
        }
    }
}

/// Outcome state of a counterfactual net-savings projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionState {
    /// Every formula component was present, finite, and non-negative — the
    /// value is a catalog-priced estimate.
    Projected,
    /// Served/cost evidence is missing or unpriceable — **no** value is
    /// invented and the projection carries `None` amounts.
    Unmeasured,
}

/// Machine-readable honesty labels every projection carries (P1-ROUTES-03).
///
/// A projection is a *catalog-priced estimate* built from the request's
/// recorded component evidence: it is approximate, **not measured**, never
/// invoice reconciliation, and never provider-authoritative pricing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ProjectionLabels {
    /// Priced from the shared catalog (or recorded components), not a bill.
    pub catalog_priced: bool,
    /// Approximate — a projection, not a measured figure.
    pub approximate: bool,
    /// Not measured — never treated as a measured/reconciled value.
    pub not_measured: bool,
    /// Never invoice reconciliation (always `false`).
    pub invoice_reconciled: bool,
    /// Never provider-authoritative pricing (always `false`).
    pub provider_authoritative: bool,
}

impl ProjectionLabels {
    /// Labels pinned for every counterfactual projection this module emits.
    pub const HONEST: Self = Self {
        catalog_priced: true,
        approximate: true,
        not_measured: true,
        invoice_reconciled: false,
        provider_authoritative: false,
    };

    /// Bounded, retainable boundary note for the projection.
    pub const BOUNDARY: &'static str = "catalog-priced counterfactual estimate (approximate, not measured); not invoice reconciliation and not provider-authoritative pricing.";
}

/// Counterfactual net savings for one historical preview match, computed from
/// the request's recorded served/cost component evidence via the shared
/// `tt.request-delta-estimate.v1` formula.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CounterfactualProjection {
    /// Formula identity (`tt.request-delta-estimate.v1`).
    pub formula_version: &'static str,
    /// `Projected` when every component was present/finite; `Unmeasured`
    /// otherwise (no invented value).
    pub state: ProjectionState,
    /// Signed net = `baseline − cost − provider-cache − cache-bust −
    /// summarizer-tax`. **May be negative** — never zero-floored.
    pub signed_net_usd: Option<f64>,
    /// `max(signed_net_usd, 0)` when projected.
    pub positive_usd: Option<f64>,
    /// `max(−signed_net_usd, 0)` when projected — a regression stays visible.
    pub regression_usd: Option<f64>,
    /// Honesty labels (approximate, not measured, catalog-priced).
    pub labels: ProjectionLabels,
    /// Bounded, retainable boundary note for the value.
    pub boundary: &'static str,
}

impl CounterfactualProjection {
    /// Project the counterfactual net savings from complete component evidence.
    ///
    /// Propagates the shared formula's honesty: any absent, non-finite, or
    /// negative component yields [`ProjectionState::Unmeasured`] with `None`
    /// amounts rather than a fabricated number, while a complete (even
    /// zero-valued or negative-signed) input is `Projected` and preserved.
    #[must_use]
    pub fn project(input: RequestDeltaInput) -> Self {
        match estimate_request_delta_v1(input) {
            Some(estimate) => Self {
                formula_version: REQUEST_DELTA_ESTIMATE_V1,
                state: ProjectionState::Projected,
                signed_net_usd: Some(estimate.signed_request_delta_usd),
                positive_usd: Some(estimate.positive_request_delta_usd),
                regression_usd: Some(estimate.regression_request_delta_usd),
                labels: ProjectionLabels::HONEST,
                boundary: ProjectionLabels::BOUNDARY,
            },
            None => Self {
                formula_version: REQUEST_DELTA_ESTIMATE_V1,
                state: ProjectionState::Unmeasured,
                signed_net_usd: None,
                positive_usd: None,
                regression_usd: None,
                labels: ProjectionLabels::HONEST,
                boundary: ProjectionLabels::BOUNDARY,
            },
        }
    }
}

/// One historical preview match projected: the winning route's effective
/// action re-execution plus the request's counterfactual net savings. Pure and
/// value-free — safe to retain alongside the bounded coverage caveats.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreviewActionProjection {
    /// Deterministic re-execution of the winning route's effective actions.
    pub execution: EffectiveActionExecution,
    /// Counterfactual net savings from the request's served/cost evidence.
    pub net_savings: CounterfactualProjection,
}

/// Project a historical preview match (winning route + served/cost evidence).
///
/// Deterministic in `route` and `input`: identical inputs ⇒ identical output,
/// with no registry, pricing catalog, latency, dispatch, clock, or I/O.
#[must_use]
pub fn project_preview_match(route: &Route, input: RequestDeltaInput) -> PreviewActionProjection {
    PreviewActionProjection {
        execution: EffectiveActionExecution::reexecute(&route.then),
        net_savings: CounterfactualProjection::project(input),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;
    use tt_routing::{Route, RouteAction, RouteConditions, RouteFeatureSnapshot, RoutingEngine};
    use tt_shared::RequestDeltaInput;
    use uuid::Uuid;

    use super::{
        CounterfactualProjection, DispatchPath, EffectiveActionExecution, ProjectionLabels,
        ProjectionState,
    };

    // -----------------------------------------------------------------------
    // Corpus vectors: pinned route actions → the matcher's recorded decision.
    //
    // Each vector is a route built from route.v1 fields exactly as the store
    // would persist it. The test first runs the canonical historical matcher
    // over a retained-feature snapshot (the same `from_retained_features` path
    // a preview consumer uses) and asserts the route is recorded as the
    // winner; it then re-executes that winning route's action chain and asserts
    // the decision equals the vector's expected effective action. Re-execution
    // therefore matches the matcher's recorded decision for every vector.
    // -----------------------------------------------------------------------

    /// Stable route id generator so vectors never collide across runs/tests.
    static NEXT_ID: AtomicU64 = AtomicU64::new(0x5000);

    fn route_vector(id: &'static str, then: RouteAction) -> (String, Route) {
        let route_id = Uuid::from_u128(NEXT_ID.fetch_add(1, Ordering::Relaxed) as u128 + 1);
        (
            id.to_owned(),
            Route {
                id: route_id,
                name: id.to_owned(),
                priority: 10,
                enabled: true,
                when: RouteConditions::default(),
                then,
                paused: false,
            },
        )
    }

    /// Every route vector carries its own expected effective action, so a
    /// change to a pinned decision fails the exact named vector.
    struct Vector {
        id: String,
        route: Route,
        expected: ExpectedAction,
    }

    struct ExpectedAction {
        dispatch_path: DispatchPath,
        rewrite_target: Option<&'static str>,
        rewrite_inert: bool,
        modifier_only: bool,
        fusion_strategy: Option<&'static str>,
        workflow_mode: Option<&'static str>,
        batch_detour: bool,
        traffic_split_pct: Option<u32>,
    }

    fn expected(
        dispatch_path: DispatchPath,
        rewrite_target: Option<&'static str>,
        rewrite_inert: bool,
        modifier_only: bool,
    ) -> ExpectedAction {
        ExpectedAction {
            dispatch_path,
            rewrite_target,
            rewrite_inert,
            modifier_only,
            fusion_strategy: None,
            workflow_mode: None,
            batch_detour: false,
            traffic_split_pct: None,
        }
    }

    fn vectors() -> Vec<Vector> {
        let direct_rewrite = route_vector(
            "direct_rewrite",
            RouteAction {
                target_model: Some("gpt-4o-mini".to_owned()),
                ..Default::default()
            },
        );
        let modifier_only = route_vector(
            "modifier_only",
            RouteAction {
                // Modifier-only: no rewrite, keep the caller's model, apply effects.
                target_model: None,
                compress: true,
                ..Default::default()
            },
        );
        let fusion = route_vector(
            "fusion_panel",
            RouteAction {
                target_model: None,
                panel: Some(tt_routing::RoutePanel {
                    strategy: "best_of_n".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let fusion_inert_rewrite = route_vector(
            "fusion_inert_rewrite",
            RouteAction {
                // Route.v1: when a panel is present the panel governs dispatch and
                // the configured rewrite is inert.
                target_model: Some("gpt-4o-mini".to_owned()),
                panel: Some(tt_routing::RoutePanel {
                    strategy: "synthesize".to_owned(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let workflow_detour = route_vector(
            "workflow_detour",
            RouteAction {
                workflow: Some(tt_routing::RouteWorkflow {
                    workflow_id: "00000000-0000-0000-0000-000000000001".to_owned(),
                    mode: None, // route.v1 default "detour"
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let workflow_shadow = route_vector(
            "workflow_shadow",
            RouteAction {
                workflow: Some(tt_routing::RouteWorkflow {
                    workflow_id: "00000000-0000-0000-0000-000000000002".to_owned(),
                    mode: Some("shadow".to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let batch_detour = route_vector(
            "batch_detour",
            RouteAction {
                batch: true,
                target_model: Some("gpt-4o-mini".to_owned()),
                ..Default::default()
            },
        );
        let split_rewrite = route_vector(
            "split_rewrite",
            RouteAction {
                target_model: Some("gpt-4o-mini".to_owned()),
                traffic_pct: Some(25),
                ..Default::default()
            },
        );

        let mut v = vec![
            Vector {
                id: direct_rewrite.0,
                route: direct_rewrite.1,
                expected: expected(DispatchPath::Direct, Some("gpt-4o-mini"), false, false),
            },
            Vector {
                id: modifier_only.0,
                route: modifier_only.1,
                expected: expected(DispatchPath::Direct, None, false, true),
            },
            Vector {
                id: fusion.0,
                route: fusion.1,
                expected: expected(DispatchPath::Fusion, None, false, false),
            },
            Vector {
                id: fusion_inert_rewrite.0,
                route: fusion_inert_rewrite.1,
                expected: expected(DispatchPath::Fusion, None, true, false),
            },
            Vector {
                id: workflow_detour.0,
                route: workflow_detour.1,
                expected: expected(DispatchPath::Workflow, None, false, false),
            },
            Vector {
                id: workflow_shadow.0,
                route: workflow_shadow.1,
                expected: expected(DispatchPath::Workflow, None, false, false),
            },
            Vector {
                id: batch_detour.0,
                route: batch_detour.1,
                expected: expected(DispatchPath::Direct, Some("gpt-4o-mini"), false, false),
            },
            Vector {
                id: split_rewrite.0,
                route: split_rewrite.1,
                expected: expected(DispatchPath::Direct, Some("gpt-4o-mini"), false, false),
            },
        ];

        // Strategy / mode / detour / split specifics (kept out of `expected`
        // for readability of the shared `DispatchPath` rows above).
        v.iter_mut()
            .find(|v| v.id == "fusion_panel")
            .unwrap()
            .expected
            .fusion_strategy = Some("best_of_n");
        v.iter_mut()
            .find(|v| v.id == "fusion_inert_rewrite")
            .unwrap()
            .expected
            .fusion_strategy = Some("synthesize");
        v.iter_mut()
            .find(|v| v.id == "workflow_detour")
            .unwrap()
            .expected
            .workflow_mode = Some("detour");
        v.iter_mut()
            .find(|v| v.id == "workflow_shadow")
            .unwrap()
            .expected
            .workflow_mode = Some("shadow");
        v.iter_mut()
            .find(|v| v.id == "batch_detour")
            .unwrap()
            .expected
            .batch_detour = true;
        v.iter_mut()
            .find(|v| v.id == "split_rewrite")
            .unwrap()
            .expected
            .traffic_split_pct = Some(25);
        v
    }

    #[test]
    fn corpus_effective_action_reexecution_matches_matcher_recorded_decision() {
        let vectors = vectors();
        for v in &vectors {
            // The canonical historical matcher over a retained-feature snapshot
            // must record this route as the winner (empty conditions match any
            // retained request).
            let engine = RoutingEngine::with_routes(vec![v.route.clone()]);
            let snapshot = RouteFeatureSnapshot::from_retained_features(
                "gpt-4o".to_owned(),
                120,
                Some("preview".to_owned()),
            );
            let evaluation = engine.evaluate_snapshot_with_trace(&snapshot);
            assert_eq!(
                evaluation.trace.selected_route_id,
                Some(v.route.id),
                "{}: canonical matcher must record this route as the winner",
                v.id
            );
            let winning = evaluation
                .matched_route
                .expect("winning route must be re-executable");

            let execution = EffectiveActionExecution::reexecute(&winning.then);
            assert_eq!(
                execution.dispatch_path, v.expected.dispatch_path,
                "{}: dispatch path",
                v.id
            );
            assert_eq!(
                execution.rewrite_target.as_deref(),
                v.expected.rewrite_target,
                "{}: rewrite target",
                v.id
            );
            assert_eq!(
                execution.rewrite_inert, v.expected.rewrite_inert,
                "{}: rewrite inert",
                v.id
            );
            assert_eq!(
                execution.modifier_only, v.expected.modifier_only,
                "{}: modifier-only",
                v.id
            );
            assert_eq!(
                execution.fusion_strategy.as_deref(),
                v.expected.fusion_strategy,
                "{}: fusion strategy",
                v.id
            );
            assert_eq!(
                execution.workflow_mode.as_deref(),
                v.expected.workflow_mode,
                "{}: workflow mode",
                v.id
            );
            assert_eq!(
                execution.batch_detour, v.expected.batch_detour,
                "{}: batch detour",
                v.id
            );
            assert_eq!(
                execution.traffic_split_pct, v.expected.traffic_split_pct,
                "{}: traffic split",
                v.id
            );
        }
    }

    #[test]
    fn composition_project_preview_match_reexecutes_winner_and_projects_net() {
        let (id, route) = route_vector(
            "composition_direct_rewrite",
            RouteAction {
                target_model: Some("gpt-4o-mini".to_owned()),
                ..Default::default()
            },
        );
        let engine = RoutingEngine::with_routes(vec![route.clone()]);
        let snapshot = RouteFeatureSnapshot::from_retained_features("gpt-4o".to_owned(), 90, None);
        let evaluation = engine.evaluate_snapshot_with_trace(&snapshot);
        assert_eq!(evaluation.trace.selected_route_id, Some(route.id));
        let winner = evaluation.matched_route.unwrap();

        let result = super::project_preview_match(winner, POSITIVE);
        assert_eq!(result.execution.dispatch_path, DispatchPath::Direct);
        assert_eq!(
            result.execution.rewrite_target.as_deref(),
            Some("gpt-4o-mini")
        );
        assert_eq!(result.net_savings.state, ProjectionState::Projected);
        assert!(
            (result.net_savings.signed_net_usd.unwrap() - 0.40).abs() < 1e-12,
            "{id}: composition must project the +0.40 net"
        );
    }

    #[test]
    fn reexecution_is_value_free_and_retainable() {
        // Every execution serializes without customer values or provenance, so
        // a preview can retain the bounded decision next to the matched row.
        for v in vectors() {
            let execution = EffectiveActionExecution::reexecute(&v.route.then);
            let value = serde_json::to_value(&execution)
                .unwrap_or_else(|e| panic!("{}: execution must serialize: {e}", v.id));
            assert!(
                value.is_object(),
                "{}: execution must serialize to an object",
                v.id
            );
        }
    }

    // -----------------------------------------------------------------------
    // Counterfactual net-savings projection honesty.
    // -----------------------------------------------------------------------

    /// `baseline 1.0 − cost 0.4 − cache 0.1 − bust 0.05 − tax 0.05 = +0.40`.
    const POSITIVE: RequestDeltaInput = RequestDeltaInput {
        baseline_cost_usd: Some(1.0),
        cost_usd: Some(0.4),
        provider_cache_saved_usd: Some(0.1),
        cache_bust_penalty_usd: Some(0.05),
        summarizer_tax_usd: Some(0.05),
    };

    /// `baseline 0.8 − cost 0.7 − cache 0.05 − bust 0.1 − tax 0.2 = −0.25`.
    const REGRESSION: RequestDeltaInput = RequestDeltaInput {
        baseline_cost_usd: Some(0.8),
        cost_usd: Some(0.7),
        provider_cache_saved_usd: Some(0.05),
        cache_bust_penalty_usd: Some(0.1),
        summarizer_tax_usd: Some(0.2),
    };

    /// Balanced-to-zero in f64 (all powers of two): `1.0 − 0.5 − 0.25 −
    /// 0.125 − 0.125 = 0.0` exactly.
    const ZERO: RequestDeltaInput = RequestDeltaInput {
        baseline_cost_usd: Some(1.0),
        cost_usd: Some(0.5),
        provider_cache_saved_usd: Some(0.25),
        cache_bust_penalty_usd: Some(0.125),
        summarizer_tax_usd: Some(0.125),
    };

    #[test]
    fn negative_net_projection_is_preserved_never_zero_floored() {
        let projection = CounterfactualProjection::project(REGRESSION);
        assert_eq!(projection.state, ProjectionState::Projected);
        let signed = projection
            .signed_net_usd
            .expect("regression stays projected");
        assert!(
            (signed + 0.25).abs() < 1e-12,
            "negative net must be preserved, got {signed}"
        );
        let positive = projection.positive_usd.expect("positive half present");
        let regression = projection.regression_usd.expect("regression half present");
        assert!(
            positive.abs() < 1e-12,
            "positive must be zero, got {positive}"
        );
        assert!(
            (regression - 0.25).abs() < 1e-12,
            "regression magnitude must be 0.25, got {regression}"
        );
    }

    #[test]
    fn zero_net_is_measured_not_mislabelled_unmeasured() {
        let projection = CounterfactualProjection::project(ZERO);
        assert_eq!(
            projection.state,
            ProjectionState::Projected,
            "a complete, balanced-to-zero input is measured zero — never unmeasured"
        );
        assert_eq!(projection.signed_net_usd, Some(0.0));
        assert_eq!(projection.positive_usd, Some(0.0));
        assert_eq!(projection.regression_usd, Some(0.0));
    }

    #[test]
    fn positive_net_projection_surfaces_signed_and_positive_usd() {
        let projection = CounterfactualProjection::project(POSITIVE);
        assert_eq!(projection.state, ProjectionState::Projected);
        assert!(
            (projection.signed_net_usd.unwrap() - 0.40).abs() < 1e-12,
            "signed net must be +0.40"
        );
        assert!((projection.positive_usd.unwrap() - 0.40).abs() < 1e-12);
        assert!(projection.regression_usd.unwrap().abs() < 1e-12);
    }

    #[test]
    fn missing_or_unpriceable_evidence_marks_unmeasured_not_invented() {
        let missing_component = RequestDeltaInput {
            cost_usd: None, // absent served-cost evidence
            ..POSITIVE
        };
        let projection = CounterfactualProjection::project(missing_component);
        assert_eq!(projection.state, ProjectionState::Unmeasured);
        assert_eq!(projection.signed_net_usd, None, "no invented net");
        assert_eq!(projection.positive_usd, None, "no invented positive");
        assert_eq!(projection.regression_usd, None, "no invented regression");

        let non_finite = RequestDeltaInput {
            cost_usd: Some(f64::NAN),
            ..POSITIVE
        };
        assert_eq!(
            CounterfactualProjection::project(non_finite).state,
            ProjectionState::Unmeasured
        );

        let negative_component = RequestDeltaInput {
            cost_usd: Some(-0.1),
            ..POSITIVE
        };
        assert_eq!(
            CounterfactualProjection::project(negative_component).state,
            ProjectionState::Unmeasured
        );
    }

    #[test]
    fn every_projection_carries_catalog_priced_honesty_labels() {
        for input in [POSITIVE, REGRESSION, ZERO] {
            let projection = CounterfactualProjection::project(input);
            assert_eq!(
                projection.labels,
                ProjectionLabels::HONEST,
                "projected values must carry the pinned honesty labels"
            );
            assert!(!projection.labels.invoice_reconciled);
            assert!(!projection.labels.provider_authoritative);
            assert!(projection.labels.approximate);
            assert!(projection.labels.not_measured);
            assert!(projection.labels.catalog_priced);
        }

        // Unmeasured carries the same bounded caveats — never a stronger claim.
        let unmeasured = CounterfactualProjection::project(RequestDeltaInput {
            cost_usd: None,
            ..POSITIVE
        });
        assert_eq!(unmeasured.labels, ProjectionLabels::HONEST);
    }

    #[test]
    fn projection_serializes_with_formula_identity_and_labels() {
        let projection = CounterfactualProjection::project(POSITIVE);
        let value = serde_json::to_value(projection).expect("projection must serialize");
        assert_eq!(
            value["formula_version"],
            json!("tt.request-delta-estimate.v1")
        );
        assert_eq!(value["state"], json!("projected"));
        assert_eq!(value["labels"]["not_measured"], json!(true));
        assert_eq!(value["labels"]["invoice_reconciled"], json!(false));
    }
}
