//! Typed route validation shared by the gateway routes API. The capability
//! check mirrors the runtime guard (`tt_shared::capability_check`). Cross-
//! provider rewrites are allowed (V3d-1) — see
//! docs/superpowers/specs/2026-06-04-v3d-1-cross-provider-routing-design.md.

use tt_shared::pricing::{Capability, ModelInfo};

use crate::{RouteAction, RouteConditions};

// `Eq` dropped (not just `PartialEq`) because `InvalidPauseFloor` carries the
// rejected f64; no caller relied on `Eq`.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ValidationError {
    #[error("target_model `{target}` is missing the `{capability}` capability required by this route's content-type condition")]
    MissingCapability {
        target: String,
        capability: &'static str,
    },
    #[error("shadow_model `{shadow}` does not resolve to any registered provider")]
    UnresolvableShadowModel { shadow: String },
    #[error("pause_floor_pass_rate must be a fraction in (0, 1], got {got}")]
    InvalidPauseFloor { got: f64 },
    #[error("pause_min_verdicts must be >= 1")]
    InvalidPauseMinVerdicts,
    #[error("reasoning_max_effort must be \"low\" or \"medium\", got {got:?}")]
    InvalidReasoningEffortCap { got: String },
    #[error("reasoning_budget_tokens must be >= 1024 (Anthropic's documented minimum), got {got}")]
    InvalidThinkingBudgetCap { got: u32 },
    #[error("format_switch must be \"csv\" or \"bare\", got `{got}`")]
    InvalidFormatSwitch { got: String },
    #[error("format_switch and diff are mutually exclusive on one route")]
    OutputShapingConflict,
    #[error(
        "agentic_budget.route_mechanical_to `{target}` does not resolve to any registered provider"
    )]
    UnresolvableMechanicalRoute { target: String },
    #[error(
        "agentic_budget.keep_recent_pairs must be >= 1 (keeping zero verbatim defeats the C1 blast-radius bound)"
    )]
    InvalidKeepRecentPairs,
    #[error("route has neither a target_model nor any then-effect (no-op route)")]
    EmptyRoute,
    #[error("panel.strategy `{0}` is not a recognized strategy (expected one of: synthesize, best-of-n, best_of_n, majority)")]
    InvalidPanelStrategy(String),
}

/// Reject malformed auto-pause config at route-creation time: a
/// `pause_floor_pass_rate` outside `(0, 1]` (or NaN) and a
/// `pause_min_verdicts` of 0 are pure mistakes. Validated even when
/// `auto_pause` is false — bad config is bad config, and silently accepting
/// it would bite the moment the flag flips on.
pub fn validate_auto_pause(then: &RouteAction) -> Result<(), ValidationError> {
    if let Some(floor) = then.pause_floor_pass_rate {
        if floor.is_nan() || floor <= 0.0 || floor > 1.0 {
            return Err(ValidationError::InvalidPauseFloor { got: floor });
        }
    }
    if then.pause_min_verdicts == Some(0) {
        return Err(ValidationError::InvalidPauseMinVerdicts);
    }
    Ok(())
}

/// Validate the output-shaping cap fields at route-create time (bad config is
/// bad config — validated even on disabled routes, the auto-pause precedent).
/// `reasoning_max_effort` must be exactly `"low"` or `"medium"`: a `"high"`
/// cap is a no-op lie (nothing ranks above high, so it could never lower
/// anything), and an unknown/misspelled string would silently never act —
/// reject both at config time instead. `reasoning_budget_tokens` must be at
/// least 1024 (Anthropic's documented extended-thinking minimum; a smaller
/// budget would be rejected upstream, so capping TO it could only break
/// requests). `minify_json` is a plain flag with no bounds.
pub fn validate_output_shaping(then: &RouteAction) -> Result<(), ValidationError> {
    if let Some(cap) = then.reasoning_max_effort.as_deref() {
        if cap != "low" && cap != "medium" {
            return Err(ValidationError::InvalidReasoningEffortCap {
                got: cap.to_string(),
            });
        }
    }
    if let Some(budget) = then.reasoning_budget_tokens {
        if budget < 1024 {
            return Err(ValidationError::InvalidThinkingBudgetCap { got: budget });
        }
    }
    // Phase 3.3/3.4 (contract-changing levers): `format_switch` must be
    // `"csv"` or `"bare"` when set (an unknown value persisted anyway would
    // silently no-op at dispatch), and a route may not declare BOTH
    // `format_switch` and `diff` — the two levers issue conflicting emission
    // instructions; the gateway would have to pick one arbitrarily.
    if let Some(fmt) = then.format_switch.as_deref() {
        if fmt != "csv" && fmt != "bare" {
            return Err(ValidationError::InvalidFormatSwitch {
                got: fmt.to_string(),
            });
        }
        if then.diff {
            return Err(ValidationError::OutputShapingConflict);
        }
    }
    Ok(())
}

/// Reject a route whose `shadow_model` cannot resolve to a registered provider —
/// fail at CONFIG time (route creation) rather than silently no-op'ing the
/// shadow dispatch at request time. `resolves(model) -> bool` is the gateway's
/// dispatch-resolution check (`ProviderRegistry::resolve(model).is_some()`);
/// when the route declares no `shadow_model`, validation is a no-op.
///
/// Note: unlike `validate_capability` (which is permissive for unknown target
/// models, mirroring the runtime guard), an unresolvable shadow is a HARD error
/// — a shadow that can't dispatch is pure mistake, never a passthrough, and the
/// gateway should not accept a route it cannot honor.
pub fn validate_shadow_model(
    then: &RouteAction,
    resolves: impl Fn(&str) -> bool,
) -> Result<(), ValidationError> {
    if let Some(shadow) = then.shadow_model.as_deref() {
        if !resolves(shadow) {
            return Err(ValidationError::UnresolvableShadowModel {
                shadow: shadow.to_string(),
            });
        }
    }
    Ok(())
}

/// Validate a route's [`AgenticBudget`] at CONFIG time (route creation) so a
/// misconfigured opt-in fails the moment it's written, never silently at
/// dispatch — the same fail-at-config discipline as `validate_shadow_model` /
/// `validate_auto_pause`. When the route declares no `agentic_budget`,
/// validation is a no-op (the off-by-default path stays untouched).
///
/// Two checks:
/// - `route_mechanical_to` (Sub-lever 3 down-route target) must resolve to a
///   registered provider — an unresolvable down-route is a pure mistake, never
///   a passthrough, exactly like `shadow_model` (`resolves(model) -> bool` is
///   the gateway's dispatch-resolution check). When unset, no resolution is
///   required.
/// - `keep_recent_pairs` must be >= 1: keeping ZERO recent tool-result pairs
///   verbatim defeats caveat C1's blast-radius bound (it would expose the
///   load-bearing recent context to the lossy summarizer, which the blind
///   paired judge only samples at ~2%). The struct's default is 3.
pub fn validate_agentic_budget(
    then: &RouteAction,
    resolves: impl Fn(&str) -> bool,
) -> Result<(), ValidationError> {
    let Some(ab) = then.agentic_budget.as_ref() else {
        return Ok(());
    };
    if let Some(target) = ab.route_mechanical_to.as_deref() {
        if !resolves(target) {
            return Err(ValidationError::UnresolvableMechanicalRoute {
                target: target.to_string(),
            });
        }
    }
    if ab.keep_recent_pairs < 1 {
        return Err(ValidationError::InvalidKeepRecentPairs);
    }
    Ok(())
}

/// Accepted RouteAction.panel strategy strings. MUST stay a subset of what
/// tt_core's ArbiterStrategyKind::parse accepts (drift-guarded by a tt-core
/// test). tt_routing cannot depend on tt_core, so this is the routing-local
/// source of truth for route-creation validation.
pub const PANEL_STRATEGY_VALUES: [&str; 4] = ["synthesize", "best-of-n", "best_of_n", "majority"];

/// Reject a route whose panel.strategy is not a recognized strategy.
pub fn validate_panel(then: &RouteAction) -> Result<(), ValidationError> {
    if let Some(p) = &then.panel {
        if !PANEL_STRATEGY_VALUES.contains(&p.strategy.as_str()) {
            return Err(ValidationError::InvalidPanelStrategy(p.strategy.clone()));
        }
    }
    Ok(())
}

/// When the route requires image or audio input, the target must be
/// `Vision`-capable (the runtime guard sets `vision=true` for both). An unknown
/// target (`lookup` returns `None`) is permissive, matching the runtime guard.
///
/// `has_documents` is DELIBERATELY excluded from `needs_vision`: a document
/// route's whole purpose is to target a TEXT model (the Document Lane distills
/// the document to text pre-routing), so it must validate against a non-Vision
/// target rather than being rejected for lacking Vision. See the field doc on
/// [`RouteConditions::has_documents`].
pub fn validate_capability(
    when: &RouteConditions,
    then: &RouteAction,
    lookup: impl Fn(&str) -> Option<ModelInfo>,
) -> Result<(), ValidationError> {
    let needs_vision = when.has_images == Some(true) || when.has_audio == Some(true);
    if !needs_vision {
        return Ok(());
    }
    // A modifier-only route (`target_model == None`) has no rewrite target to
    // capability-check — skip, mirroring the "unknown target is permissive"
    // stance (the caller's own model is what gets dispatched and it already
    // satisfied the request, since it's the model the caller chose).
    let Some(tm) = then.target_model.as_deref() else {
        return Ok(());
    };
    if let Some(info) = lookup(tm) {
        if !info.capabilities.contains(&Capability::Vision) {
            return Err(ValidationError::MissingCapability {
                target: tm.to_string(),
                capability: "vision",
            });
        }
    }
    Ok(())
}

/// Reject a no-op route at CONFIG time: a **modifier-only** route
/// (`target_model == None`) MUST carry at least one then-effect — otherwise it
/// matches traffic, rewrites nothing, and applies nothing, which is always a
/// configuration mistake. When `target_model` is `Some`, the rewrite itself is
/// the effect, so this check always passes (the existing behavior of every
/// route that pins a model is unchanged).
///
/// "Has an effect" = ANY cost/shaping/safety lever, a non-empty `fallbacks`
/// list, or an `agentic_budget`/`shadow_model`/cap being set.
pub fn validate_route_has_effect(then: &RouteAction) -> Result<(), ValidationError> {
    if then.target_model.is_some() {
        return Ok(());
    }
    let has_effect = then.agentic_budget.is_some()
        || then.panel.is_some()
        || then.disable_cache
        || then.flex
        || then.batch
        || then.compress
        || then.doc_compaction
        || then.document_lane
        || then.content_compress
        || then.redact
        || then.format_switch.is_some()
        || then.diff
        || then.minify_json
        || then.max_cost_usd.is_some()
        || then.shadow_model.is_some()
        || then.reasoning_max_effort.is_some()
        || then.reasoning_budget_tokens.is_some()
        || !then.fallbacks.is_empty();
    if has_effect {
        Ok(())
    } else {
        Err(ValidationError::EmptyRoute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgenticBudget, RouteAction, RouteConditions, RoutePanel};
    use tt_shared::pricing::{Capability, ModelInfo};

    fn action(target: &str) -> RouteAction {
        RouteAction {
            target_model: Some(target.into()),
            fallbacks: vec![],
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: false,
            doc_compaction: false,
            document_lane: false,
            content_compress: false,
            redact: false,
            format_switch: None,
            diff: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
            minify_json: false,
            reasoning_max_effort: None,
            reasoning_budget_tokens: None,
            agentic_budget: None,
            panel: None,
        }
    }
    fn vision_model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            provider: "p".into(),
            capabilities: vec![Capability::Text, Capability::Vision],
            max_input_tokens: 1000,
            max_output_tokens: 1000,
        }
    }
    fn text_model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            provider: "p".into(),
            capabilities: vec![Capability::Text],
            max_input_tokens: 1000,
            max_output_tokens: 1000,
        }
    }

    #[test]
    fn has_images_requires_vision_target() {
        let when = RouteConditions {
            has_images: Some(true),
            ..Default::default()
        };
        let lookup = |m: &str| -> Option<ModelInfo> {
            match m {
                "vis" => Some(vision_model("vis")),
                "txt" => Some(text_model("txt")),
                _ => None,
            }
        };
        assert!(validate_capability(&when, &action("vis"), lookup).is_ok());
        assert!(validate_capability(&when, &action("txt"), lookup).is_err());
        // Unknown target is permissive (mirrors runtime guard).
        assert!(validate_capability(&when, &action("unknown"), lookup).is_ok());
    }

    #[test]
    fn no_modality_condition_skips_capability_check() {
        let when = RouteConditions::default();
        let lookup = |_: &str| -> Option<ModelInfo> { None };
        assert!(validate_capability(&when, &action("anything"), lookup).is_ok());
    }

    #[test]
    fn has_documents_validates_against_text_target() {
        // The key difference from `has_images`: a document route downgrades to a
        // TEXT model, so `has_documents:true` must NOT require a Vision target —
        // a text (non-Vision) target validates cleanly.
        let when = RouteConditions {
            has_documents: Some(true),
            ..Default::default()
        };
        let lookup = |m: &str| -> Option<ModelInfo> {
            match m {
                "txt" => Some(text_model("txt")),
                "vis" => Some(vision_model("vis")),
                _ => None,
            }
        };
        assert!(
            validate_capability(&when, &action("txt"), lookup).is_ok(),
            "a document route must validate against a text (non-Vision) target"
        );
        // A Vision target is also fine (documents impose no capability gate).
        assert!(validate_capability(&when, &action("vis"), lookup).is_ok());
    }

    #[test]
    fn shadow_model_must_resolve_to_a_provider() {
        // A resolver that only knows `gpt-4o-mini`.
        let resolves = |m: &str| m == "gpt-4o-mini";
        // No shadow_model → no-op OK.
        assert!(validate_shadow_model(&action("gpt-4o"), resolves).is_ok());
        // Resolvable shadow → OK.
        let mut ok = action("gpt-4o");
        ok.shadow_model = Some("gpt-4o-mini".into());
        assert!(validate_shadow_model(&ok, resolves).is_ok());
        // Unresolvable shadow → hard error at config time.
        let mut bad = action("gpt-4o");
        bad.shadow_model = Some("does-not-exist".into());
        assert_eq!(
            validate_shadow_model(&bad, resolves),
            Err(ValidationError::UnresolvableShadowModel {
                shadow: "does-not-exist".into()
            })
        );
    }

    /// Auto-pause config bounds: the floor must be a fraction in (0, 1] (NaN
    /// rejected), `pause_min_verdicts` must be >= 1 — validated even when
    /// `auto_pause` is false (bad config is bad config).
    #[test]
    fn validate_auto_pause_bounds() {
        // No auto-pause config at all → OK.
        assert!(validate_auto_pause(&action("m")).is_ok());

        let mut a = action("m");
        a.pause_floor_pass_rate = Some(0.9);
        assert!(validate_auto_pause(&a).is_ok());
        a.pause_floor_pass_rate = Some(1.0);
        assert!(validate_auto_pause(&a).is_ok(), "1.0 is an allowed floor");
        for bad in [0.0, -0.1, 1.5, f64::NAN] {
            a.pause_floor_pass_rate = Some(bad);
            assert!(
                matches!(
                    validate_auto_pause(&a),
                    Err(ValidationError::InvalidPauseFloor { .. })
                ),
                "floor {bad} must be rejected"
            );
        }

        let mut b = action("m");
        b.pause_min_verdicts = Some(0);
        assert!(
            matches!(
                validate_auto_pause(&b),
                Err(ValidationError::InvalidPauseMinVerdicts)
            ),
            "min == 0 must be rejected"
        );
        b.pause_min_verdicts = Some(1);
        assert!(validate_auto_pause(&b).is_ok());
        b.pause_min_verdicts = None;
        assert!(validate_auto_pause(&b).is_ok());
        // Validated even when auto_pause itself is off.
        b.auto_pause = false;
        b.pause_floor_pass_rate = Some(2.0);
        assert!(validate_auto_pause(&b).is_err());
    }

    /// Output-shaping cap bounds: `reasoning_max_effort` accepts only
    /// `"low"`/`"medium"` (a `"high"` cap is a no-op lie; unknown strings would
    /// silently never act); `reasoning_budget_tokens` must be >= 1024
    /// (Anthropic's documented floor). Validated even on disabled routes —
    /// bad config is bad config (the auto-pause precedent).
    #[test]
    fn validate_output_shaping_bounds() {
        // No output-shaping config at all → OK.
        assert!(validate_output_shaping(&action("m")).is_ok());

        let mut a = action("m");
        for ok in ["low", "medium"] {
            a.reasoning_max_effort = Some(ok.into());
            assert!(
                validate_output_shaping(&a).is_ok(),
                "cap {ok:?} must be accepted"
            );
        }
        for bad in ["high", "", "bogus", "LOW "] {
            a.reasoning_max_effort = Some(bad.into());
            assert!(
                matches!(
                    validate_output_shaping(&a),
                    Err(ValidationError::InvalidReasoningEffortCap { ref got }) if got == bad
                ),
                "cap {bad:?} must be rejected with InvalidReasoningEffortCap"
            );
        }
        a.reasoning_max_effort = None;
        assert!(validate_output_shaping(&a).is_ok());

        let mut b = action("m");
        b.reasoning_budget_tokens = Some(1024);
        assert!(validate_output_shaping(&b).is_ok(), "1024 is the floor");
        b.reasoning_budget_tokens = Some(8192);
        assert!(validate_output_shaping(&b).is_ok());
        for bad in [0u32, 1, 1023] {
            b.reasoning_budget_tokens = Some(bad);
            assert!(
                matches!(
                    validate_output_shaping(&b),
                    Err(ValidationError::InvalidThinkingBudgetCap { got }) if got == bad
                ),
                "budget {bad} must be rejected with InvalidThinkingBudgetCap"
            );
        }
        b.reasoning_budget_tokens = None;
        assert!(validate_output_shaping(&b).is_ok());

        // minify_json carries no bounds — always valid.
        let mut c = action("m");
        c.minify_json = true;
        assert!(validate_output_shaping(&c).is_ok());
    }

    /// Output-shaping config: `format_switch` accepts only `"csv"` / `"bare"`
    /// (or `None`), an unknown value is rejected at creation time, and
    /// `format_switch` + `diff` together are a conflict (mutually exclusive
    /// emission instructions). `diff` alone and `None` alone are fine.
    #[test]
    fn validate_output_shaping_rules() {
        // No shaping config at all → OK.
        assert!(validate_output_shaping(&action("m")).is_ok());

        // csv / bare alone → OK.
        let mut a = action("m");
        a.format_switch = Some("csv".into());
        assert!(validate_output_shaping(&a).is_ok());
        a.format_switch = Some("bare".into());
        assert!(validate_output_shaping(&a).is_ok());

        // diff alone → OK.
        let mut d = action("m");
        d.diff = true;
        assert!(validate_output_shaping(&d).is_ok());

        // Unknown wire value → rejected with the offending value echoed.
        let mut bad = action("m");
        bad.format_switch = Some("yaml".into());
        assert_eq!(
            validate_output_shaping(&bad),
            Err(ValidationError::InvalidFormatSwitch { got: "yaml".into() })
        );

        // Both levers on one route → conflict.
        let mut both = action("m");
        both.format_switch = Some("csv".into());
        both.diff = true;
        assert_eq!(
            validate_output_shaping(&both),
            Err(ValidationError::OutputShapingConflict)
        );
    }

    /// A `shadow_model` with no `traffic_pct` (100% shadow, primary still serves)
    /// is allowed by validation — only resolvability is required.
    #[test]
    fn shadow_without_traffic_pct_is_allowed() {
        let resolves = |m: &str| m == "claude-haiku-4-5";
        let mut a = action("gpt-4o");
        a.shadow_model = Some("claude-haiku-4-5".into());
        a.traffic_pct = None;
        assert!(validate_shadow_model(&a, resolves).is_ok());
    }

    /// Sub-lever 3 (`route_mechanical_to`) must resolve to a registered provider
    /// at CONFIG time — an unresolvable down-route target is a pure mistake that
    /// would silently no-op at dispatch (mirrors `validate_shadow_model`'s
    /// contract). When unset, validation is a no-op.
    #[test]
    fn agentic_budget_route_mechanical_must_resolve() {
        let resolves = |m: &str| m == "claude-haiku-4-5";

        // route_mechanical_to resolves → OK.
        let mut ok = action("gpt-4o");
        ok.agentic_budget = Some(AgenticBudget {
            route_mechanical_to: Some("claude-haiku-4-5".into()),
            ..Default::default()
        });
        assert!(validate_agentic_budget(&ok, resolves).is_ok());

        // Unresolvable down-route target → hard error at config time.
        let mut bad = action("gpt-4o");
        bad.agentic_budget = Some(AgenticBudget {
            route_mechanical_to: Some("nonexistent-model".into()),
            ..Default::default()
        });
        assert_eq!(
            validate_agentic_budget(&bad, resolves),
            Err(ValidationError::UnresolvableMechanicalRoute {
                target: "nonexistent-model".into()
            })
        );

        // No route_mechanical_to → no resolution required.
        let mut none = action("gpt-4o");
        none.agentic_budget = Some(AgenticBudget {
            route_mechanical_to: None,
            ..Default::default()
        });
        assert!(validate_agentic_budget(&none, resolves).is_ok());
    }

    /// `keep_recent_pairs` must be at least 1: keeping zero recent tool-result
    /// pairs verbatim defeats caveat C1's blast-radius bound (it would let the
    /// lossy summarizer touch the load-bearing recent context, which the judge
    /// only samples at ~2%). Rejected even though it carries no resolution.
    #[test]
    fn agentic_budget_keep_recent_pairs_bounded() {
        let resolves = |_: &str| true;

        // keep_recent_pairs == 0 → rejected.
        let mut zero = action("gpt-4o");
        zero.agentic_budget = Some(AgenticBudget {
            keep_recent_pairs: 0,
            ..Default::default()
        });
        assert_eq!(
            validate_agentic_budget(&zero, resolves),
            Err(ValidationError::InvalidKeepRecentPairs)
        );

        // keep_recent_pairs == 1 (the minimum) → OK.
        let mut one = action("gpt-4o");
        one.agentic_budget = Some(AgenticBudget {
            keep_recent_pairs: 1,
            ..Default::default()
        });
        assert!(validate_agentic_budget(&one, resolves).is_ok());

        // The default (3) → OK.
        let mut def = action("gpt-4o");
        def.agentic_budget = Some(AgenticBudget::default());
        assert!(validate_agentic_budget(&def, resolves).is_ok());
    }

    /// No `agentic_budget` on the route → validation is a trivial no-op (the
    /// off-by-default path stays byte-identical, never gated).
    #[test]
    fn agentic_budget_none_validates_trivially() {
        let resolves = |_: &str| false;
        assert!(validate_agentic_budget(&action("gpt-4o"), resolves).is_ok());
    }

    /// A panel-only `RouteAction` (no target_model, panel Some) passes
    /// `validate_route_has_effect` — the panel IS the effect.
    #[test]
    fn route_has_effect_panel_only_passes() {
        let mut a = action("ignored");
        a.target_model = None;
        a.panel = Some(RoutePanel {
            strategy: "synthesize".into(),
            ..Default::default()
        });
        assert!(
            validate_route_has_effect(&a).is_ok(),
            "panel-only route must pass validate_route_has_effect"
        );
    }

    /// `validate_panel` accepts each of the 4 valid strategy strings and rejects
    /// an unrecognized string with `InvalidPanelStrategy`.
    #[test]
    fn validate_panel_accepts_valid_strategies_and_rejects_bogus() {
        for strategy in ["synthesize", "best-of-n", "best_of_n", "majority"] {
            let mut a = action("m");
            a.panel = Some(RoutePanel {
                strategy: strategy.into(),
                ..Default::default()
            });
            assert!(
                validate_panel(&a).is_ok(),
                "strategy {strategy:?} must be accepted"
            );
        }
        let mut bad = action("m");
        bad.panel = Some(RoutePanel {
            strategy: "bogus".into(),
            ..Default::default()
        });
        assert!(
            matches!(
                validate_panel(&bad),
                Err(ValidationError::InvalidPanelStrategy(ref s)) if s == "bogus"
            ),
            "strategy 'bogus' must be rejected with InvalidPanelStrategy"
        );
    }

    /// No panel on the route → `validate_panel` is a trivial no-op.
    #[test]
    fn validate_panel_none_is_noop() {
        assert!(validate_panel(&action("m")).is_ok());
    }

    /// A modifier-only route (`target_model: None`) has no rewrite target to
    /// capability-check — the vision check is SKIPPED (Ok), mirroring the
    /// "unknown target is permissive" stance.
    #[test]
    fn capability_check_skipped_when_target_model_none() {
        let when = RouteConditions {
            has_images: Some(true),
            ..Default::default()
        };
        let lookup = |_: &str| -> Option<ModelInfo> { None };
        let mut a = action("ignored");
        a.target_model = None;
        a.agentic_budget = Some(AgenticBudget::default());
        assert!(validate_capability(&when, &a, lookup).is_ok());
    }

    /// `validate_route_has_effect`: a `None`-target route with no effect is an
    /// `EmptyRoute` no-op; with an `agentic_budget` it's Ok; a `Some`-target
    /// route always passes (the rewrite IS the effect).
    #[test]
    fn route_has_effect_rules() {
        // None target + no effect → EmptyRoute.
        let mut empty = action("ignored");
        empty.target_model = None;
        assert_eq!(
            validate_route_has_effect(&empty),
            Err(ValidationError::EmptyRoute)
        );

        // None target + agentic_budget → Ok.
        let mut mod_only = action("ignored");
        mod_only.target_model = None;
        mod_only.agentic_budget = Some(AgenticBudget::default());
        assert!(validate_route_has_effect(&mod_only).is_ok());

        // None target + a non-agentic effect (compress) → Ok.
        let mut compress = action("ignored");
        compress.target_model = None;
        compress.compress = true;
        assert!(validate_route_has_effect(&compress).is_ok());

        // None target + a non-agentic effect (doc_compaction) → Ok.
        let mut doc = action("ignored");
        doc.target_model = None;
        doc.doc_compaction = true;
        assert!(validate_route_has_effect(&doc).is_ok());

        // Some target + nothing else → Ok (the rewrite is the effect).
        assert!(validate_route_has_effect(&action("gpt-4o")).is_ok());
    }
}
