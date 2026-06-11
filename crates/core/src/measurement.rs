//! Shared **measured single-shot dispatch** — the costing core extracted from
//! the #146 canary shadow path so the paired-A/B quality judge's baseline
//! reference dispatch can reuse the exact same plumbing instead of forking it.
//!
//! One helper, two callers:
//!
//! * [`crate::routes::chat`]'s `dispatch_shadow` (canary shadow mode) — keeps
//!   its #146 semantics byte-for-byte: single candidate, non-streaming, no
//!   failover, short deadline, response discarded, cost recorded separately.
//! * [`crate::quality_sample::ReferenceSource::Dispatch`] — the judge's
//!   baseline-model reference dispatch inside the detached judge task, which
//!   additionally keeps the response text (the answer the judge compares
//!   against) and meters the dispatch cost as measurement tax.
//!
//! The helper deliberately does NOT resolve credentials — the caller prepares
//! the [`RequestContext`] (credentials + `ctx.deadline`) so each call site
//! keeps its own fail-closed credential policy.

use std::sync::Arc;
use std::time::Duration;

use tt_shared::{ChatCompletionRequest, ChatCompletionResponse, Provider, RequestContext};

/// A completed measured dispatch: the response plus its independently-computed
/// cost. The cost is the *measurement tax* of the dispatch — callers record it
/// in its own column/attribute and never fold it into a primary request cost.
pub(crate) struct MeasuredDispatch {
    /// The upstream response. The shadow path drops it; the judge's baseline
    /// reference path extracts the assistant text from it.
    pub(crate) response: ChatCompletionResponse,
    /// Cost (USD) of this dispatch on the served model's own pricing.
    /// `None` when the model has no catalog pricing — **unmetered, never
    /// "free"/`0`** — so callers can distinguish "genuinely ~$0" from
    /// "billed but unpriced". (The #146 shadow caller flattens `None` to its
    /// legacy `0.0` convention; the judge persists the distinction.)
    pub(crate) cost_usd: Option<f64>,
}

/// Dispatch `req` ONCE on `provider`, non-streaming, with NO failover and NO
/// retry, bounded by `deadline`, and compute the dispatch's own cost.
///
/// * `req.stream` is forced to `false` — measurement dispatches are never
///   streamed.
/// * The cost is computed exactly like the #146 shadow: the served model's own
///   pricing on both sides of [`crate::routes::chat::compute_cost`] (no flex,
///   no compression delta), times the provider's fee multiplier. `None`
///   pricing yields `cost_usd: None` (unmetered — see [`MeasuredDispatch`]).
/// * A timeout maps to `Err("deadline exceeded")`; an upstream error maps to
///   its display string. The caller decides what an error means (the shadow
///   records `succeeded=false`; the judge records nothing).
pub(crate) async fn measured_single_dispatch(
    provider: &Arc<dyn Provider>,
    mut req: ChatCompletionRequest,
    ctx: &RequestContext,
    deadline: Duration,
) -> Result<MeasuredDispatch, String> {
    req.stream = false;
    let response = match tokio::time::timeout(deadline, provider.chat_completion(req, ctx)).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("deadline exceeded".to_string()),
    };
    let cost_usd = provider.pricing(&response.model).map(|pricing| {
        crate::routes::chat::compute_cost(
            &response.usage,
            Some(&pricing),
            Some(&pricing),
            provider.fee_multiplier(),
        )
        .cost_usd
    });
    Ok(MeasuredDispatch { response, cost_usd })
}
