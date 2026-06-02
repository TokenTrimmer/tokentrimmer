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
    let est = token_estimator::estimate(hit.provider, &req.messages, req.max_tokens);
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
