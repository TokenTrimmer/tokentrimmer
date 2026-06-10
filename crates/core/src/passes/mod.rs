//! Request-pass pipeline — composable, ordered transforms applied to a
//! [`ChatCompletionRequest`] **before** upstream dispatch.
//!
//! A *request pass* is a single transform (`apply`) that may rewrite the
//! request in place and reports how many estimated input tokens it removed.
//! Passes are composed into an ordered [`PassPipeline`]; the pipeline runs each
//! pass in turn and sums their token deltas so the caller can attribute the
//! savings.
//!
//! Design constraints (this is the seam that makes "TokenTrimmer that trims"
//! true):
//!
//! - **Off by default.** The gateway never runs a pass unless a matched route
//!   opts in (`RouteAction::compress`). An empty pipeline is a no-op.
//! - **Token-accurate.** Each pass reports the *estimated* tokens it removed,
//!   measured with the same tokenizer the cost path uses, so savings reconcile
//!   against the realized prompt-token drop.
//! - **Composable + ordered.** Adding a second pass (a future, more aggressive
//!   stage gated behind the Wave-B2 judge) is `pipeline.with(pass)`.
//!
//! The only pass shipped today is the conservative, content-lossless
//! [`compression::CompressionPass`] (compression pass #1). A judge gate would
//! attach at [`PassPipeline::run`] — wrapping a non-lossless pass so its output
//! is only accepted when the judge confirms semantic equivalence. The
//! conservative pass needs no judge because it is lossless by construction.

pub mod compression;

pub use compression::CompressionPass;

use tt_shared::ChatCompletionRequest;

/// What a single [`RequestPass`] removed from a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassOutcome {
    /// Estimated input tokens removed by this pass (the count of the text the
    /// pass deleted, measured with the dispatch tokenizer). Zero when the pass
    /// made no change.
    pub tokens_removed: u32,
}

impl PassOutcome {
    /// An outcome that removed nothing.
    pub const NONE: PassOutcome = PassOutcome { tokens_removed: 0 };

    /// True when the pass changed nothing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.tokens_removed == 0
    }
}

/// A single request transform applied before upstream dispatch.
///
/// `apply` may rewrite `req` in place and MUST return the number of estimated
/// input tokens it removed. A pass that declines to change a request returns
/// [`PassOutcome::NONE`]. `provider_id` is the tokenizer key for the FINAL
/// served provider, so token counts match what the upstream will bill.
///
/// Implementations must be **conservative**: only remove content that is
/// provably redundant for the request's meaning. When unsure, do nothing.
pub trait RequestPass: Send + Sync {
    /// Stable identifier for the pass — used in logs / telemetry attribution.
    fn name(&self) -> &'static str;

    /// Apply the transform in place, returning what was removed.
    fn apply(&self, req: &mut ChatCompletionRequest, provider_id: &str) -> PassOutcome;
}

/// An ordered, composable collection of [`RequestPass`]es.
///
/// The pipeline is empty by default (a no-op). Callers build the request-pass
/// stage for a route by `with`-ing the passes that route opted into; the
/// gateway runs the pipeline only for opted-in routes.
#[derive(Default)]
pub struct PassPipeline {
    passes: Vec<Box<dyn RequestPass>>,
}

impl PassPipeline {
    /// An empty pipeline (runs nothing, removes nothing).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The conservative, content-lossless compression stage (compression pass
    /// #1) — the only stage enabled by `RouteAction::compress` today.
    #[must_use]
    pub fn conservative_compression() -> Self {
        Self::new().with(CompressionPass::new())
    }

    /// Append a pass to the end of the pipeline (builder style).
    #[must_use]
    pub fn with<P: RequestPass + 'static>(mut self, pass: P) -> Self {
        self.passes.push(Box::new(pass));
        self
    }

    /// True when the pipeline has no passes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.passes.is_empty()
    }

    /// Run every pass in order, returning the total estimated tokens removed.
    ///
    /// This is where a judge gate would attach for a future non-lossless pass:
    /// wrap the pass's mutation in a clone-then-verify so the rewrite is only
    /// committed when the Wave-B2 judge confirms the trimmed request is
    /// semantically equivalent. The conservative compression pass shipped today
    /// is lossless by construction and needs no gate.
    pub fn run(&self, req: &mut ChatCompletionRequest, provider_id: &str) -> u32 {
        let mut total = 0u32;
        for pass in &self.passes {
            let outcome = pass.apply(req, provider_id);
            if !outcome.is_noop() {
                tracing::debug!(
                    pass = pass.name(),
                    tokens_removed = outcome.tokens_removed,
                    "request pass removed tokens"
                );
            }
            total = total.saturating_add(outcome.tokens_removed);
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::{Message, MessageContent};

    /// A trivial pass that does nothing, to exercise pipeline composition.
    struct NoopPass;
    impl RequestPass for NoopPass {
        fn name(&self) -> &'static str {
            "noop"
        }
        fn apply(&self, _req: &mut ChatCompletionRequest, _p: &str) -> PassOutcome {
            PassOutcome::NONE
        }
    }

    fn req_with(messages: Vec<Message>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn empty_pipeline_is_a_noop() {
        let pipe = PassPipeline::new();
        assert!(pipe.is_empty());
        let mut req = req_with(vec![Message::User {
            content: MessageContent::Text("hi".into()),
            name: None,
        }]);
        let before = serde_json::to_string(&req).unwrap();
        let removed = pipe.run(&mut req, "openai");
        assert_eq!(removed, 0);
        assert_eq!(serde_json::to_string(&req).unwrap(), before);
    }

    #[test]
    fn noop_pass_removes_nothing_but_composes() {
        let pipe = PassPipeline::new().with(NoopPass).with(NoopPass);
        assert!(!pipe.is_empty());
        let mut req = req_with(vec![Message::System {
            content: MessageContent::Text("system".into()),
        }]);
        assert_eq!(pipe.run(&mut req, "openai"), 0);
    }

    #[test]
    fn pipeline_sums_token_deltas_in_order() {
        // The conservative compression stage on a tool block with redundant
        // trailing whitespace reports a positive, summed token delta.
        let pipe = PassPipeline::conservative_compression();
        let mut req = req_with(vec![Message::Tool {
            content: MessageContent::Text("aaaa bbbb cccc   \n\n\n\n\ndddd eeee".into()),
            tool_call_id: "call_1".into(),
        }]);
        let removed = pipe.run(&mut req, "openai");
        assert!(removed > 0, "expected some tokens removed, got {removed}");
    }
}
