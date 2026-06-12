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
//! The estimate is sized by the mutation's [`MutationDeterminism`]: an
//! ingress-deterministic transform (redaction) dispatches byte-identical
//! prefixes every turn — the provider cache keeps hitting, so its estimate is
//! zero by construction; only a non-deterministic mutator books the prefix.
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
//! runtime: for each pass it snapshots the volatile tail, counts it with the
//! route's served provider + model tokenizer
//! ([`tt_tokenize::estimate_input_tokens_for_model`] — the billing-correct
//! encoding where one exists, a real-BPE proxy elsewhere, never characters),
//! applies the pass, and recounts. A pass is DISCARDED (tail restored
//! byte-identical — fail-open to the original request), metered
//! (`request_pass_rejected_total{pass}`), and books exactly zero savings when
//! ANY of these fired:
//!
//! - the text-projection token count increased;
//! - the token count of the tail's canonical JSON serialization increased —
//!   this covers every field the text projection cannot see (tool-call
//!   names/ids, message `name` fields, image/audio parts, per-message
//!   framing), so a pass cannot inflate the request through a non-text field
//!   invisibly;
//! - the tail message COUNT increased (splitting one message into several
//!   adds billed per-message framing the text projection cannot price);
//! - the number of NON-TEXT content parts changed in either direction (a pass
//!   has no business adding OR removing image/audio parts — their token cost
//!   cannot be measured here, so any change is unverifiable).
//!
//! Savings attribution uses the pipeline-measured TEXT-projection delta of
//! committed passes — never a pass's self-reported figure (logged at debug for
//! drift detection only). Known conservative gap: deleting a whole message
//! (dedup) books only its text tokens, not the freed per-message framing
//! overhead — savings are understated, never overstated. When the tokenizer
//! degrades to the `chars / 4` heuristic (tiktoken failed to load —
//! [`tt_tokenize::Confidence::Low`]) the gate still rejects inflation but
//! books **zero** savings: a character-derived delta is not a reconcilable
//! token saving.
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
//!   `RouteAction::compress`. On a cache-qualified multi-turn request — or a
//!   cache-qualified single-shot on a positional-auto-cache provider — it
//!   structurally no-ops (the whole prompt is stable) — deliberate: the
//!   cache-read rate dominates any whitespace trim of re-sent history.
//! - [`cache_classifier::CacheClassifierPass`] — an ACTIVE, lossless,
//!   diagnostics-only classifier (promoted from the Tier-1 inspect lints)
//!   that flags volatile markers inside a would-be-stable prefix. Driven
//!   directly by the chat handler on every request; never mutates.
//! - [`redaction::RedactionPass`] — a SAFETY guardrail (`RouteAction::redact`)
//!   that strips PII/secrets ANYWHERE in the request, stable prefix included.
//!   It is exactly the escape-hatch user: safety beats cost, so the handler
//!   drives it through [`SplitRequest::mutate_whole_request`]. Because
//!   redaction is DETERMINISTIC on the ingress bytes, its bust estimate is
//!   zero by construction ([`MutationDeterminism::DeterministicOnIngress`] —
//!   the dispatched prefix is byte-identical every turn, so the provider
//!   cache keeps hitting); only a future NON-deterministic mutator books a
//!   real negative entry. It is NOT a pipeline pass and NOT a savings feature.
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
    CacheBustEstimate, MutationDeterminism, PassContext, SplitRequest, StablePrefix, StableReason,
    VolatileTail,
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
    /// Per pass: snapshot the volatile tail → count it (text projection +
    /// canonical serialization + structure) with the served provider+model
    /// tokenizer → apply the pass → recount. Any inflation or unverifiable
    /// structural change discards the transform (tail restored byte-identical
    /// — fail-open), meters the rejection, and books zero — see the module
    /// docs for the full rejection list. Otherwise the transform commits and
    /// `before − after` on the TEXT projection (the measured delta, NOT the
    /// pass's self-report) accrues to [`PipelineOutcome::tokens_removed`] —
    /// unless the count came from the chars/4 heuristic (Low confidence),
    /// which books zero. The stable prefix is immutable by type, so gating
    /// the tail alone covers everything a pass can touch.
    ///
    /// This is also where a judge gate would attach for a future non-lossless
    /// pass: wrap the apply/recount in a verify step so the rewrite is only
    /// committed when the Wave-B2 judge confirms semantic equivalence.
    pub fn run(&self, split: &mut SplitRequest<'_>, cx: &PassContext<'_>) -> PipelineOutcome {
        let count = |text: &str| {
            tt_tokenize::estimate_input_tokens_for_model(cx.provider_id, cx.model, text)
        };
        let mut out = PipelineOutcome::default();
        for pass in &self.passes {
            let snapshot = split.tail_snapshot();
            let (before_msgs, before_nontext) = tail_structure(&snapshot);
            let before_serialized = count(&serialized_messages_text(&snapshot)).tokens;
            let before = count(&split.tail_text());

            let outcome = split.run_pass(|stable, tail| pass.apply(stable, tail, cx));

            let after = count(&split.tail_text());
            let after_tail = split.tail_snapshot();
            let (after_msgs, after_nontext) = tail_structure(&after_tail);
            let after_serialized = count(&serialized_messages_text(&after_tail)).tokens;

            let reject_reason = if after.tokens > before.tokens {
                Some("transform added text tokens")
            } else if after_serialized > before_serialized {
                Some("transform inflated the serialized request (non-text field or framing)")
            } else if after_msgs > before_msgs {
                Some("transform increased the message count (adds billed framing)")
            } else if after_nontext != before_nontext {
                Some("transform changed non-text content parts (unverifiable token effect)")
            } else {
                None
            };
            if let Some(reason) = reject_reason {
                // Discard the transform, fail open to the original bytes,
                // meter the rejection, book zero.
                split.restore_tail(snapshot);
                crate::metrics::record_request_pass_rejected(pass.name());
                tracing::warn!(
                    pass = pass.name(),
                    tokens_before = before.tokens,
                    tokens_after = after.tokens,
                    reason,
                    "token-true gate rejected request pass; failing open to the original request"
                );
                out.rejected.push(pass.name());
                continue;
            }

            let mut measured = before.tokens.saturating_sub(after.tokens);
            if measured > 0 && before.confidence == tt_tokenize::Confidence::Low {
                // chars/4 fallback (tiktoken failed to load): a character
                // delta is not a reconcilable token saving — keep the (still
                // lossless, still dispatched) transform but book $0.
                tracing::warn!(
                    pass = pass.name(),
                    char_delta = measured,
                    "tokenizer degraded to chars/4 — committing transform but booking zero savings"
                );
                measured = 0;
            }
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

/// Concatenate the text of every STRING field a pass could affect — used by
/// the split's token estimates and the token-true gate's booked delta. Covers
/// System / User / Tool content (`Text` + `Parts::Text`), user/assistant
/// `name` fields, tool `tool_call_id`s, plus Assistant content and
/// `tool_calls` (id, function name, arguments), '\n'-joined. (Names/ids are
/// approximations of their billed framing cost, but including them means a
/// pass mutating them moves the gate's count instead of being invisible —
/// and it nudges the multi-turn split estimate toward the Anthropic adapter's
/// basis, which counts tool-use names.) Non-text parts are handled by the
/// gate's structural check, not this projection.
pub(crate) fn messages_text(msgs: &[Message]) -> String {
    let mut out = String::new();
    let push_opt = |out: &mut String, s: &Option<String>| {
        if let Some(s) = s {
            out.push_str(s);
            out.push('\n');
        }
    };
    for m in msgs {
        match m {
            Message::System { content } => push_content_text(&mut out, content),
            Message::User { content, name } => {
                push_content_text(&mut out, content);
                push_opt(&mut out, name);
            }
            Message::Tool {
                content,
                tool_call_id,
            } => {
                push_content_text(&mut out, content);
                out.push_str(tool_call_id);
                out.push('\n');
            }
            Message::Assistant {
                content,
                tool_calls,
                name,
            } => {
                if let Some(c) = content {
                    push_content_text(&mut out, c);
                }
                push_opt(&mut out, name);
                for tc in tool_calls {
                    out.push_str(&tc.id);
                    out.push('\n');
                    out.push_str(&tc.function.name);
                    out.push('\n');
                    out.push_str(&tc.function.arguments);
                    out.push('\n');
                }
            }
        }
    }
    out
}

/// Concatenated text of the SYSTEM messages only — the bytes the Anthropic
/// adapter hoists into the cached system prefix. Shared by the single-shot
/// split qualification and the cache classifier's marker scan.
pub(crate) fn system_text(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        if let Message::System { content } = m {
            push_content_text(&mut out, content);
        }
    }
    out
}

/// Canonical JSON serialization of `msgs` — the gate's catch-all inflation
/// surface: every string field, non-text part, and per-message framing shows
/// up here, so nothing a pass can reach through the tail handle is invisible.
fn serialized_messages_text(msgs: &[Message]) -> String {
    serde_json::to_string(msgs).unwrap_or_default()
}

/// Structural fingerprint of a tail: `(message_count, non_text_part_count)`.
/// The gate rejects a pass that increases the former or changes the latter.
fn tail_structure(msgs: &[Message]) -> (usize, usize) {
    let non_text = msgs
        .iter()
        .map(|m| {
            let content = match m {
                Message::System { content }
                | Message::User { content, .. }
                | Message::Tool { content, .. } => Some(content),
                Message::Assistant { content, .. } => content.as_ref(),
            };
            content.map_or(0, |c| match c {
                MessageContent::Text(_) => 0,
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter(|p| !matches!(p, tt_shared::messages::ContentPart::Text { .. }))
                    .count(),
            })
        })
        .sum();
    (msgs.len(), non_text)
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

    /// A pass that INJECTS an image part claims no text tokens — the old
    /// text-only gate was blind to it. The structural check rejects it.
    struct ImageInjectingPass;
    impl RequestPass for ImageInjectingPass {
        fn name(&self) -> &'static str {
            "image-inject"
        }
        fn apply(
            &self,
            _stable: &StablePrefix<'_>,
            tail: &mut VolatileTail<'_>,
            _cx: &PassContext<'_>,
        ) -> PassOutcome {
            if let Some(Message::User { content, .. }) = tail.messages_mut().first_mut() {
                let text = match content {
                    MessageContent::Text(s) => s.clone(),
                    MessageContent::Parts(_) => String::new(),
                };
                *content = MessageContent::Parts(vec![
                    tt_shared::messages::ContentPart::Text { text },
                    tt_shared::messages::ContentPart::ImageUrl {
                        image_url: tt_shared::messages::ImageUrl {
                            url: "https://example.com/huge.png".into(),
                            detail: None,
                        },
                    },
                ]);
            }
            PassOutcome::none()
        }
    }

    #[test]
    fn gate_rejects_non_text_part_injection() {
        let pipe = PassPipeline::new().with(ImageInjectingPass);
        let mut req = req_with(vec![Message::User {
            content: MessageContent::Text("hello".into()),
            name: None,
        }]);
        let before = serde_json::to_string(&req).unwrap();
        let out = run_pipeline(&pipe, &mut req);
        assert_eq!(out.rejected, vec!["image-inject"]);
        assert_eq!(out.tokens_removed, 0);
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "image injection must be rolled back byte-identical"
        );
    }

    /// Inflating a tool-call function NAME (a billed string the old text
    /// projection ignored) is caught: the projection now includes names/ids.
    struct NameInflatingPass;
    impl RequestPass for NameInflatingPass {
        fn name(&self) -> &'static str {
            "name-inflate"
        }
        fn apply(
            &self,
            _stable: &StablePrefix<'_>,
            tail: &mut VolatileTail<'_>,
            _cx: &PassContext<'_>,
        ) -> PassOutcome {
            if let Some(Message::Assistant { tool_calls, .. }) = tail.messages_mut().first_mut() {
                if let Some(tc) = tool_calls.first_mut() {
                    tc.function.name.push_str(&"_padding".repeat(40));
                }
            }
            PassOutcome::none()
        }
    }

    #[test]
    fn gate_rejects_tool_call_name_inflation() {
        let pipe = PassPipeline::new().with(NameInflatingPass);
        let mut req = req_with(vec![Message::Assistant {
            content: None,
            tool_calls: vec![tt_shared::messages::ToolCall {
                id: "c1".into(),
                r#type: "function".into(),
                function: tt_shared::messages::ToolCallFunction {
                    name: "f".into(),
                    arguments: "{}".into(),
                },
            }],
            name: None,
        }]);
        let before = serde_json::to_string(&req).unwrap();
        let out = run_pipeline(&pipe, &mut req);
        assert_eq!(out.rejected, vec!["name-inflate"]);
        assert_eq!(serde_json::to_string(&req).unwrap(), before);
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
