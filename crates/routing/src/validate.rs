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

/// When the route requires image or audio input, the target must be
/// `Vision`-capable (the runtime guard sets `vision=true` for both). An unknown
/// target (`lookup` returns `None`) is permissive, matching the runtime guard.
pub fn validate_capability(
    when: &RouteConditions,
    then: &RouteAction,
    lookup: impl Fn(&str) -> Option<ModelInfo>,
) -> Result<(), ValidationError> {
    let needs_vision = when.has_images == Some(true) || when.has_audio == Some(true);
    if !needs_vision {
        return Ok(());
    }
    if let Some(info) = lookup(&then.target_model) {
        if !info.capabilities.contains(&Capability::Vision) {
            return Err(ValidationError::MissingCapability {
                target: then.target_model.clone(),
                capability: "vision",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RouteAction, RouteConditions};
    use tt_shared::pricing::{Capability, ModelInfo};

    fn action(target: &str) -> RouteAction {
        RouteAction {
            target_model: target.into(),
            fallbacks: vec![],
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            batch: false,
            compress: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
            auto_pause: false,
            pause_floor_pass_rate: None,
            pause_min_verdicts: None,
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
}
