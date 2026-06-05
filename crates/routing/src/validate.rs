//! Typed route validation shared by the gateway routes API. The capability
//! check mirrors the runtime guard (`tt_shared::capability_check`). Cross-
//! provider rewrites are allowed (V3d-1) — see
//! docs/superpowers/specs/2026-06-04-v3d-1-cross-provider-routing-design.md.

use tt_shared::pricing::{Capability, ModelInfo};

use crate::{RouteAction, RouteConditions};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("target_model `{target}` is missing the `{capability}` capability required by this route's content-type condition")]
    MissingCapability {
        target: String,
        capability: &'static str,
    },
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
            force_cache_layer: None,
            disable_cache: false,
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
}
