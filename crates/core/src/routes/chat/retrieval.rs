//! Retrieval's policy-aware preparation seam and pre-transform judge capture.
//! Routing selects once on caller input. Retrieval is then a guarded transform,
//! never an opportunity to re-match and lose a previously selected guardrail.
use crate::{
    middleware::retrieval::{DeferredRetrieval, RetrievalTelemetry},
    ApiResult, AppState,
};
use std::sync::Arc;
use tt_shared::{ChatCompletionRequest, Provider, RequestContext};

type JudgeInputs = (
    Option<Arc<dyn Provider>>,
    Option<RequestContext>,
    Option<ChatCompletionRequest>,
);

/// Capture the original provider/model before a route rewrite. Privacy routes
/// later clear ALL three values so neither reference nor judge can out-leak
/// the primary dispatch. Disabled judging pays no request-clone cost.
pub(super) fn capture_judge_inputs(
    state: &AppState,
    provider: &Arc<dyn Provider>,
    ctx: &RequestContext,
    req: &ChatCompletionRequest,
) -> JudgeInputs {
    if state.judge_config.enabled && state.judge_sink.is_some() {
        (Some(provider.clone()), Some(ctx.clone()), Some(req.clone()))
    } else {
        (None, None, None)
    }
}

/// Runs only after apply_routing succeeded and the selected privacy effects
/// have been captured. The query guard sanitizes every auxiliary query before
/// embedding; the normal request-redaction stage later sanitizes retrieved
/// context before primary dispatch and response-cache insertion.
pub(super) async fn prepare_retrieval(
    deferred: Option<DeferredRetrieval>,
    req: &mut ChatCompletionRequest,
    ctx: &RequestContext,
    redact: bool,
    judge_original_req: &mut Option<ChatCompletionRequest>,
) -> ApiResult<RetrievalTelemetry> {
    let Some(deferred) = deferred else {
        return Ok(RetrievalTelemetry::default());
    };
    let telemetry = deferred.apply(req, ctx.org_id, redact).await?;
    // Non-privacy judge baselines compare the same retrieved context, on the
    // original provider/model. Do not copy the route's rewritten model or any
    // later output-shaping instruction into the reference request.
    if let Some(original) = judge_original_req.as_mut() {
        original.messages = req.messages.clone();
    }
    Ok(telemetry)
}
