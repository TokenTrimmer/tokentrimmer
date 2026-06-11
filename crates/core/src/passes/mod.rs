//! Request-pass pipeline — composable, ordered transforms applied to a
//! [`ChatCompletionRequest`](tt_shared::ChatCompletionRequest) **before**
//! upstream dispatch.
//!
//! # The cache-span invariant (type-level, not reviewer discipline)
//!
//! A [`RequestPass`] receives mutable access ONLY to the **volatile tail**
//! computed by [`SplitRequest::compute`]; the **cache-stable prefix** — the
//! system prefix on a single-shot request, the ENTIRE message list on a
//! cache-qualified multi-turn conversation (exactly the region the Anthropic
//! adapter marks with `cache_control` per #126/#150, or the prefix OpenAI
//! auto-caches at ≥ `prompt_cache_min_tokens`) — is read-only BY TYPE. A pass
//! is handed a read-only [`StablePrefix`] and a mutable [`VolatileTail`] with
//! tail-relative operations only; there is structurally no way to reach the
//! prefix, the model, `tools`, or any non-message field from inside a pass.
//!
//! Mutating the stable prefix requires [`SplitRequest::mutate_whole_request`]
//! — a deliberate escape hatch that consumes the split and returns a
//! `#[must_use]` [`CacheBustEstimate`] which MUST be booked as a **negative
//! savings entry** (the prefix tokens repriced from the ~0.1x cache-read rate
//! back to 1.0x full input) into the same attribution channel the savings use.
//!
//! Rationale: busting a cache-warm prefix re-bills it at up to ~6.4x what
//! leaving it alone costs (cache reads are ~0.1x; rewriting the prefix also
//! re-pays the ~1.25x write premium). A pass that busts cache while reporting
//! a saving is the #1 forbidden failure mode — the ledger must survive a CFO
//! reconciling provider invoices. Uniform deterministic prefix trims are
//! deliberately NOT grandfathered: "deterministic every turn" is fragile
//! across deploy-time trim changes and provider failover, so the prefix is
//! simply immutable to passes.
//!
//! # The token-true gate
//!
//! [`PassPipeline::run`] enforces "reject anything that adds tokens" at
//! runtime: for each pass it snapshots the volatile tail, counts its tokens
//! with the route's served-provider tokenizer ([`tt_tokenize::estimate_tokens`]
//! — the same estimator routing/preview/cost use), applies the pass, and
//! recounts. A pass whose transform INCREASED the token count is discarded
//! (the tail is restored byte-identical — fail-open to the original request),
//! metered (`request_pass_rejected_total{pass}`), and books exactly zero
//! savings. Savings attribution uses the pipeline-measured tokenizer delta of
//! committed passes — never a pass's self-reported figure (logged at debug for
//! drift detection only) and never characters.
//!
//! # Design constraints (this is the seam that makes "TokenTrimmer that
//! trims" true)
//!
//! - **Off by default.** The gateway never runs a mutating pass unless a
//!   matched route opts in (`RouteAction::compress`). An empty pipeline is a
//!   no-op. (Observability-only diagnostics — [`CacheClassifierPass`] — may
//!   run default-on because they change no request/response semantics.)
//! - **Token-accurate.** All deltas are measured with the served provider's
//!   tokenizer so savings reconcile against the realized prompt-token drop.
//! - **Composable + ordered.** Adding a second pass (a future, more aggressive
//!   stage gated behind the Wave-B2 judge) is `pipeline.with(pass)`.
//!
//! # What ships today
//!
//! - [`compression::CompressionPass`] (compression pass #1) — a conservative,
//!   content-lossless trim of non-prose VOLATILE-TAIL blocks, enabled by
//!   `RouteAction::compress`. On a cache-qualified multi-turn request it
//!   structurally no-ops (the whole conversation is stable) — deliberate:
//!   the cache-read rate dominates any whitespace trim of re-sent history.
//! - [`cache_classifier::CacheClassifierPass`] — an ACTIVE, lossless,
//!   diagnostics-only classifier (promoted from the Tier-1 inspect lints)
//!   that flags volatile markers inside a would-be-stable prefix. Driven
//!   directly by the chat handler on every request; never mutates.
//! - [`redaction::RedactionPass`] — a SAFETY guardrail (`RouteAction::redact`)
//!   that strips PII/secrets ANYWHERE in the request, stable prefix included.
//!   It is exactly the escape-hatch user: safety beats cost, so the handler
//!   drives it through [`SplitRequest::mutate_whole_request`] and books the
//!   [`CacheBustEstimate`] when a redaction fired inside the stable prefix.
//!   It is NOT a pipeline pass and NOT a savings feature.
//!
//! A judge gate for a future non-lossless pass would attach inside
//! [`PassPipeline::run`], wrapping the apply/recount step so a rewrite is only
//! committed when the judge confirms semantic equivalence.

pub mod cache_classifier;
pub mod compression;
pub mod redaction;
pub mod split;

pub use cache_classifier::CacheClassifierPass;
pub use compression::CompressionPass;
pub use redaction::{RedactedField, RedactedHit, RedactionPass};
pub use split::{
    CacheBustEstimate, PassContext, SplitRequest, StablePrefix, StableReason, VolatileTail,
};

use tt_shared::messages::{Message, MessageContent};

/// What a single [`RequestPass`] reports back: its self-estimated token delta
/// (for drift logging only — attribution uses the pipeline-measured delta) and
/// any diagnostic warning tokens to surface via `x-tokentrimmer-warnings`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassOutcome {
    /// Estimated input tokens the pass believes it removed. Informational:
    /// the token-true gate measures the real whole-tail delta itself and uses
    /// THAT for savings attribution; a mismatch is logged at debug.
    pub tokens_removed: u32,
    /// Diagnostic warning tokens (e.g. `cache_dynamic_prefix:uuid`) to append
    /// to the response's `x-tokentrimmer-warnings` header. Only surfaced when
    /// the pass is committed.
    pub warnings: Vec<String>,
}

impl PassOutcome {
    /// An outcome that removed nothing and reports nothing.
    #[must_use]
    pub fn none() -> PassOutcome {
        PassOutcome::default()
    }

    /// True when the pass changed nothing and reports nothing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.tokens_removed == 0 && self.warnings.is_empty()
    }
}

/// A single request transform applied before upstream dispatch.
///
/// Passes receive the cache-stable prefix READ-ONLY and the volatile tail
/// mutably — there is structurally no way to mutate the prefix, the model,
/// tools, or any non-message field from inside a pass (see the module docs).
/// `cx` carries the FINAL served provider id (the tokenizer key, so token
/// counts match what the upstream will bill), model, and pricing.
///
/// Implementations must be **conservative**: only remove content that is
/// provably redundant for the request's meaning. When unsure, do nothing.
pub trait RequestPass: Send + Sync {
    /// Stable identifier for the pass — used in logs / telemetry attribution
    /// and the `pass_rejected:<name>` warning token.
    fn name(&self) -> &'static str;

    /// Apply the transform to the volatile tail, returning what was removed.
    fn apply(
        &self,
        stable: &StablePrefix<'_>,
        tail: &mut VolatileTail<'_>,
        cx: &PassContext<'_>,
    ) -> PassOutcome;
}

/// What [`PassPipeline::run`] produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineOutcome {
    /// Whole-tail tokenizer delta summed over COMMITTED passes — this (not
    /// the passes' self-reports) is what savings attribution uses.
    pub tokens_removed: u32,
    /// Names of passes discarded by the token-true gate (their transforms
    /// were rolled back byte-identical and book zero savings).
    pub rejected: Vec<&'static str>,
    /// Diagnostic warning tokens from committed passes.
    pub warnings: Vec<String>,
}

/// Aggregated request-pass effects threaded into the cost path (both the
/// non-streaming handler and the SSE stream context).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PassEffects {
    /// Input tokens the compression pipeline removed (pipeline-measured,
    /// token-true-gated). Drives `compression_saved_usd`.
    pub compression_tokens_removed: u32,
    /// Estimated USD penalty of a deliberate stable-prefix mutation (a booked
    /// [`CacheBustEstimate`]), pre-fee. Drives
    /// `CostBreakdown::cache_bust_penalty_usd` — the negative savings entry.
    pub cache_bust_penalty_usd: f64,
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

    /// Run every pass in order under the **token-true gate**, returning the
    /// pipeline-measured outcome.
    ///
    /// Per pass: snapshot the volatile tail → count its tokens with the served
    /// provider's tokenizer → apply the pass → recount. If the count INCREASED
    /// the transform is discarded (tail restored byte-identical — fail-open),
    /// the rejection is metered + warned, and zero savings book for that pass.
    /// Otherwise the transform commits and `before − after` (the measured
    /// delta, NOT the pass's self-report) accrues to
    /// [`PipelineOutcome::tokens_removed`]. The stable prefix is immutable by
    /// type, so counting the tail alone covers everything a pass can touch.
    ///
    /// This is also where a judge gate would attach for a future non-lossless
    /// pass: wrap the apply/recount in a verify step so the rewrite is only
    /// committed when the Wave-B2 judge confirms semantic equivalence.
    pub fn run(&self, split: &mut SplitRequest<'_>, cx: &PassContext<'_>) -> PipelineOutcome {
        let mut out = PipelineOutcome::default();
        for pass in &self.passes {
            let snapshot = split.tail_snapshot();
            let before = tt_tokenize::estimate_tokens(cx.provider_id, &split.tail_text());
            let outcome = split.run_pass(|stable, tail| pass.apply(stable, tail, cx));
            let after = tt_tokenize::estimate_tokens(cx.provider_id, &split.tail_text());

            if after > before {
                // The transform ADDED tokens: discard it, fail open to the
                // original bytes, meter the rejection, book zero.
                split.restore_tail(snapshot);
                crate::metrics::record_request_pass_rejected(pass.name());
                tracing::warn!(
                    pass = pass.name(),
                    tokens_before = before,
                    tokens_after = after,
                    "token-true gate rejected request pass — transform added tokens; \
                     failing open to the original request"
                );
                out.rejected.push(pass.name());
                continue;
            }

            let measured = before.saturating_sub(after);
            if measured != outcome.tokens_removed {
                // Self-report drift is informational only — attribution always
                // uses the measured delta.
                tracing::debug!(
                    pass = pass.name(),
                    self_reported = outcome.tokens_removed,
                    measured,
                    "request pass self-report differs from measured tokenizer delta"
                );
            }
            if measured > 0 {
                tracing::debug!(
                    pass = pass.name(),
                    tokens_removed = measured,
                    "request pass removed tokens"
                );
            }
            out.tokens_removed = out.tokens_removed.saturating_add(measured);
            out.warnings.extend(outcome.warnings);
        }
        out
    }
}

/// Concatenate the text of every message field a pass could affect — used by
/// the split's token estimates and the token-true gate. Covers System / User /
/// Tool content (`Text` + `Parts::Text`) plus Assistant content and
/// `tool_calls` arguments, '\n'-joined.
pub(crate) fn messages_text(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        match m {
            Message::System { content }
            | Message::User { content, .. }
            | Message::Tool { content, .. } => push_content_text(&mut out, content),
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if let Some(c) = content {
                    push_content_text(&mut out, c);
                }
                for tc in tool_calls {
                    out.push_str(&tc.function.arguments);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Append a [`MessageContent`]'s text parts to `out`, '\n'-terminated.
pub(crate) fn push_content_text(out: &mut String, content: &MessageContent) {
    match content {
        MessageContent::Text(s) => {
            out.push_str(s);
            out.push('\n');
        }
        MessageContent::Parts(parts) => {
            for p in parts {
                if let tt_shared::messages::ContentPart::Text { text } = p {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::{Message, MessageContent};
    use tt_shared::ChatCompletionRequest;

    /// A trivial pass that does nothing, to exercise pipeline composition.
    struct NoopPass;
    impl RequestPass for NoopPass {
        fn name(&self) -> &'static str {
            "noop"
        }
        fn apply(
            &self,
            _stable: &StablePrefix<'_>,
            _tail: &mut VolatileTail<'_>,
            _cx: &PassContext<'_>,
        ) -> PassOutcome {
            PassOutcome::none()
        }
    }

    /// A misbehaving pass that APPENDS text to a tail message while claiming a
    /// saving — exactly what the token-true gate must discard.
    struct InflatingPass;
    impl RequestPass for InflatingPass {
        fn name(&self) -> &'static str {
            "inflate"
        }
        fn apply(
            &self,
            _stable: &StablePrefix<'_>,
            tail: &mut VolatileTail<'_>,
            _cx: &PassContext<'_>,
        ) -> PassOutcome {
            if let Some(Message::Tool { content, .. } | Message::System { content }) =
                tail.messages_mut().first_mut()
            {
                if let MessageContent::Text(s) = content {
                    s.push_str(&" padding".repeat(50));
                }
            }
            // It lies about its effect, too.
            PassOutcome {
                tokens_removed: 5,
                warnings: vec![],
            }
        }
    }

    /// A pass that mutates nothing but reports an enormous saving.
    struct LyingPass;
    impl RequestPass for LyingPass {
        fn name(&self) -> &'static str {
            "liar"
        }
        fn apply(
            &self,
            _stable: &StablePrefix<'_>,
            _tail: &mut VolatileTail<'_>,
            _cx: &PassContext<'_>,
        ) -> PassOutcome {
            PassOutcome {
                tokens_removed: 10_000,
                warnings: vec![],
            }
        }
    }

    fn req_with(messages: Vec<Message>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages,
            ..Default::default()
        }
    }

    /// All-volatile context (no pricing → no cache minimum → empty stable
    /// prefix) — the exact pre-split behavior for every test provider.
    fn cx() -> PassContext<'static> {
        PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        }
    }

    fn run_pipeline(pipe: &PassPipeline, req: &mut ChatCompletionRequest) -> PipelineOutcome {
        let cx = cx();
        let mut split = SplitRequest::compute(req, &cx);
        pipe.run(&mut split, &cx)
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
        let out = run_pipeline(&pipe, &mut req);
        assert_eq!(out.tokens_removed, 0);
        assert!(out.rejected.is_empty());
        assert_eq!(serde_json::to_string(&req).unwrap(), before);
    }

    #[test]
    fn noop_pass_removes_nothing_but_composes() {
        let pipe = PassPipeline::new().with(NoopPass).with(NoopPass);
        assert!(!pipe.is_empty());
        let mut req = req_with(vec![Message::System {
            content: MessageContent::Text("system".into()),
        }]);
        let out = run_pipeline(&pipe, &mut req);
        assert_eq!(out.tokens_removed, 0);
        assert!(out.rejected.is_empty());
    }

    #[test]
    fn pipeline_sums_token_deltas_in_order() {
        // The conservative compression stage on a tool block with redundant
        // trailing whitespace reports a positive, measured token delta.
        let pipe = PassPipeline::conservative_compression();
        let mut req = req_with(vec![Message::Tool {
            content: MessageContent::Text("aaaa bbbb cccc   \n\n\n\n\ndddd eeee".into()),
            tool_call_id: "call_1".into(),
        }]);
        let out = run_pipeline(&pipe, &mut req);
        assert!(
            out.tokens_removed > 0,
            "expected some tokens removed, got {}",
            out.tokens_removed
        );
        assert!(out.rejected.is_empty());
    }

    /// (a) THE token-true gate: a pass whose transform adds tokens is
    /// discarded — the tail is restored byte-identical, zero savings book, and
    /// the rejection is attributed by name.
    #[test]
    fn gate_discards_pass_that_adds_tokens() {
        let pipe = PassPipeline::new().with(InflatingPass);
        let mut req = req_with(vec![Message::Tool {
            content: MessageContent::Text("tool result".into()),
            tool_call_id: "c1".into(),
        }]);
        let before = serde_json::to_string(&req).unwrap();
        let out = run_pipeline(&pipe, &mut req);
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "inflating transform must be rolled back byte-identical"
        );
        assert_eq!(out.tokens_removed, 0, "an inflating pass books zero");
        assert_eq!(out.rejected, vec!["inflate"]);
    }

    /// Savings attribution uses the measured tokenizer delta, never the pass's
    /// self-report: a pass that changes nothing but claims 10k tokens books 0.
    #[test]
    fn gate_attribution_uses_tokenizer_delta_not_self_report() {
        let pipe = PassPipeline::new().with(LyingPass);
        let mut req = req_with(vec![Message::Tool {
            content: MessageContent::Text("unchanged".into()),
            tool_call_id: "c1".into(),
        }]);
        let out = run_pipeline(&pipe, &mut req);
        assert_eq!(
            out.tokens_removed, 0,
            "self-reported 10k must not reach the ledger"
        );
        // No inflation either → not rejected, just zero-booked.
        assert!(out.rejected.is_empty());
    }

    /// Regression guard: the gate does not reject an honest, genuinely
    /// shrinking pass — the trimmed bytes are kept and the delta books.
    #[test]
    fn gate_commits_genuinely_shrinking_pass() {
        let pipe = PassPipeline::conservative_compression();
        let mut req = req_with(vec![Message::Tool {
            content: MessageContent::Text("aaaa bbbb cccc   \n\n\n\n\ndddd eeee".into()),
            tool_call_id: "c1".into(),
        }]);
        let before = serde_json::to_string(&req).unwrap();
        let out = run_pipeline(&pipe, &mut req);
        assert!(out.tokens_removed > 0);
        assert!(out.rejected.is_empty());
        assert_ne!(
            serde_json::to_string(&req).unwrap(),
            before,
            "the shrinking transform must be committed"
        );
    }

    /// A pass after a rejected pass still runs against the restored bytes —
    /// one bad pass doesn't poison the pipeline.
    #[test]
    fn gate_isolates_rejection_per_pass() {
        let pipe = PassPipeline::new()
            .with(InflatingPass)
            .with(CompressionPass::new());
        let mut req = req_with(vec![Message::Tool {
            content: MessageContent::Text("aaaa bbbb cccc   \n\n\n\n\ndddd eeee".into()),
            tool_call_id: "c1".into(),
        }]);
        let out = run_pipeline(&pipe, &mut req);
        assert_eq!(out.rejected, vec!["inflate"]);
        assert!(
            out.tokens_removed > 0,
            "the honest pass after the rejected one still books its delta"
        );
        // The committed result reflects compression of the ORIGINAL bytes,
        // not the inflated ones.
        let Message::Tool { content, .. } = &req.messages[0] else {
            panic!("expected tool message");
        };
        let MessageContent::Text(s) = content else {
            panic!("expected text");
        };
        assert_eq!(s, "aaaa bbbb cccc\n\ndddd eeee");
    }
}
