//! Typed route validation shared by the gateway routes API (and, later, the
//! cloud admin endpoint). Same-provider mirrors ADR-018; the capability check
//! mirrors the runtime guard (`tt_shared::capability_check`).

use tt_shared::pricing::{Capability, ModelInfo};
use tt_shared::providers::known_to_differ;

use crate::{RouteAction, RouteConditions};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("cross-provider rewrite rejected: `{src}` and `target_model={target}` are on different providers; v1 routing is same-provider only (ADR-018)")]
    CrossProvider { src: String, target: String },
    #[error("target_model `{target}` is missing the `{capability}` capability required by this route's content-type condition")]
    MissingCapability {
        target: String,
        capability: &'static str,
    },
}

/// Reject only when both the source model and the target are known-but-different
/// providers. Unknown / aggregator-routed names pass (no false positive).
pub fn validate_same_provider(
    when: &RouteConditions,
    then: &RouteAction,
) -> Result<(), ValidationError> {
    // Routing to a local model is a deliberate cross-provider exception for
    // privacy (V3b) — never blocked by the same-provider rule (ADR-018).
    if tt_shared::providers::local_backend(&then.target_model).is_some() {
        return Ok(());
    }
    for src in &when.model_in {
        if known_to_differ(src, &then.target_model) {
            return Err(ValidationError::CrossProvider {
                src: src.clone(),
                target: then.target_model.clone(),
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
            force_cache_layer: None,
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
    fn same_provider_ok_and_cross_provider_rejected() {
        let when = RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        };
        assert!(validate_same_provider(&when, &action("gpt-4o-mini")).is_ok());
        assert!(validate_same_provider(&when, &action("claude-haiku-4-5")).is_err());
    }

    #[test]
    fn unknown_models_pass_same_provider() {
        let when = RouteConditions {
            model_in: vec!["llama-3.3-70b".into()],
            ..Default::default()
        };
        assert!(validate_same_provider(&when, &action("qwen-2.5-72b")).is_ok());
    }

    #[test]
    fn local_target_is_exempt_from_same_provider() {
        let when = RouteConditions {
            model_in: vec!["gpt-4o".into()],
            ..Default::default()
        };
        // Routing an OpenAI model to a local model is allowed (privacy exception).
        assert!(validate_same_provider(&when, &action("ollama/llama3.1:8b")).is_ok());
        // A genuine cross-provider (non-local) rewrite is still rejected.
        assert!(validate_same_provider(&when, &action("claude-haiku-4-5")).is_err());
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
