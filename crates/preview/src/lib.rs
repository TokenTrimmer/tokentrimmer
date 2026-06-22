//! `tt-preview` — pure cost preview engine.
//!
//! See `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md`.

pub mod cache_projection;
pub mod classifier;
pub mod error;
pub mod pricing;
pub mod route_suggestions;
pub mod token_estimator;
pub mod types;

pub use error::PreviewError;
pub use types::{
    CacheProjections, CurrentEstimate, EstimationConfidence, PreviewRequest, PreviewResponse,
    QualityRiskBand, RouteSuggestion, Suggestion,
};

use uuid::Uuid;

/// Top-level entry point. Returns a complete `PreviewResponse`. The only
/// way this returns `Err` is if the model is unknown AND the optional
/// fallback heuristic also fails — in practice the handler converts that
/// into a 400 with a clear message.
pub fn preview(req: &PreviewRequest) -> Result<PreviewResponse, PreviewError> {
    let mut warnings = Vec::new();

    let hit = pricing::lookup(&req.model)?;
    // Clamp the projected output to the model's real catalog max-output when the
    // model is catalogued (keeps an over-large or absent `max_tokens` honest);
    // unknown models pass through uncapped.
    let model_max_output = tt_shared::model_catalog()
        .model_info(hit.provider, &req.model)
        .map(|mi| u32::try_from(mi.max_output_tokens).unwrap_or(u32::MAX));
    let est = token_estimator::estimate(
        hit.provider,
        &req.messages,
        req.max_tokens,
        model_max_output,
    );
    let cost = pricing::cost_usd(est.input_tokens, est.output_tokens, &hit);

    let task_class = classifier::classify(&req.messages);

    let cache = cache_projection::project(
        cost,
        cache_projection::DEFAULT_L1_HIT_PROBABILITY,
        cache_projection::DEFAULT_L2_HIT_PROBABILITY,
    );

    let suggestions = route_suggestions::suggest(
        &req.model,
        cost,
        est.input_tokens,
        est.output_tokens,
        task_class,
    );
    if suggestions.is_empty() && !matches!(task_class, classifier::TaskClass::Agent) {
        warnings.push(format!(
            "no cheaper-equivalent candidates for {} on this task class — \
             current model may already be the cheapest in family",
            req.model,
        ));
    }

    Ok(PreviewResponse {
        current: CurrentEstimate {
            model: req.model.clone(),
            provider: hit.provider.to_string(),
            input_tokens_estimated: est.input_tokens,
            output_tokens_estimated: est.output_tokens,
            cost_usd: cost,
            estimation_confidence: est.confidence,
        },
        cache_projections: cache,
        route_suggestions: suggestions,
        warnings,
        trace_id: Uuid::new_v4().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use serde_json::json;

    fn req_with_max(model: &str, max_tokens: Option<u32>) -> PreviewRequest {
        PreviewRequest {
            model: model.to_string(),
            messages: vec![Message {
                role: "user".into(),
                content: json!("hello"),
            }],
            max_tokens,
            tools: None,
            stream: None,
            tt_extras: Default::default(),
        }
    }

    /// End-to-end: a catalogued model with an over-large `max_tokens` has its
    /// projected output clamped to the model's real catalog max-output, rather
    /// than the caller's inflated ceiling or the old hardcoded 4096 cap.
    /// `gpt-4o` is catalogued (openai, max_output_tokens = 16000).
    #[test]
    fn over_large_max_tokens_clamped_to_catalog_max_output() {
        let model = "gpt-4o";
        let catalog_max = tt_shared::model_catalog()
            .model_info("openai", model)
            .expect("gpt-4o must be catalogued")
            .max_output_tokens;
        let resp = preview(&req_with_max(model, Some(catalog_max as u32 + 50_000))).unwrap();
        assert_eq!(resp.current.output_tokens_estimated as u64, catalog_max);
    }

    /// A modest explicit `max_tokens` below the model max is honored as-is.
    #[test]
    fn modest_max_tokens_is_honored() {
        let resp = preview(&req_with_max("gpt-4o", Some(1000))).unwrap();
        assert_eq!(resp.current.output_tokens_estimated, 1000);
    }
}
