//! Agentic context budget — the route-grained mode that brings the CLI's
//! loop-aware levers server-side (plan `2026-06-13-agentic-cost-context-budget`,
//! spec `2026-06-13-agentic-cost-reduction-research`).
//!
//! Loop traffic burns tokens re-sending a growing transcript every turn. This
//! module nets four sub-levers per request — they do NOT stack at face value
//! (caching and routing fight; elision busts the prefix it trims from), so the
//! planner reconciles them and books the interactions honestly:
//!
//! - **Sub-lever 1 — [`cache_prefix`]** (lossless): annotate the
//!   `cache_control` breakpoints the caller forgot so the provider's prompt
//!   cache fires on the static + history prefix. Books NO TT savings and NO
//!   bust — pure provider-cache enablement, reported on the separate
//!   `provider_cache_saved_usd` axis (spec §4.3). **Shipped here (Task 4).**
//! - **Sub-lever 2** — field-drop (lossless, token-true-gated, run in
//!   [`AgenticBudgetPlanner::plan`]) + summarize (lossy, judge-gated, invocable
//!   via [`AgenticBudgetPlanner::plan_summary`]) stale tool results.
//! - **Sub-lever 3** — down-route mechanical sub-steps in a cache-isolated
//!   lane. The planner emits the `subagent_lane:<target>` marker; the actual
//!   routed dispatch lane is the chat HANDLER's to wire *(deferred there)*.
//! - **Sub-lever 4** — semantic sub-step cache for read-only sub-steps. The
//!   planner marks the read-only-eligible sub-steps (`substep_cache:<tool>`);
//!   the async L2 sub-step lookup/serve (needs the embedder + store) is the
//!   handler's to wire, exactly like Sub-lever 3's dispatch lane.
//!
//! # Off by default (load-bearing)
//!
//! The mode is opt-in via `RouteAction::agentic_budget` (`None` by default,
//! serde-omitted). The default request path — no agentic budget configured —
//! is byte-identical: the planner is never constructed, so it adds no tokens,
//! no headers, and changes no behavior.

pub mod cache_prefix;
pub mod elide;
pub mod substep_cache;
pub mod summarize_judge;

#[cfg(test)]
mod compose_test;

pub use cache_prefix::{annotate_cache_prefix, CachePrefixPlan};
pub use elide::ElidePass;
pub use substep_cache::{
    classify_substep, SubstepCache, SubstepEntry, SubstepKind, SubstepLookup, SubstepSpace,
};
pub use summarize_judge::{
    map_summary_verdict, AdaptiveSummaryGate, AlwaysCommitGate, NeverCommitGate, SummarizeCall,
    SummarizeStep, Summarizer, SummaryEdit, SummaryGate, SummaryOutcome,
};

use tt_routing::AgenticBudget;

use crate::passes::split::{CacheBustEstimate, PassContext, SplitRequest};
use crate::passes::PassEffects;

/// Orchestrates the agentic-budget sub-levers for a request, netting their
/// interactions before dispatch.
///
/// The planner runs **before** the [`PassPipeline`](crate::passes::PassPipeline):
/// Sub-lever 1 confirms/annotates the cache-prefix breakpoint (it touches the
/// prefix's framing, not content, so it is not a tail-mutating
/// [`RequestPass`](crate::passes::RequestPass)), and the later sub-levers feed
/// the pass stage. It is constructed only for routes that opted into the mode;
/// the default (un-opted) path never builds one, keeping that path
/// byte-identical.
///
/// Task 6 nets the levers per request and books the cache-bust; subsequent
/// tasks attach the lossy summary half and Sub-lever 4.
#[derive(Debug, Clone, Copy, Default)]
pub struct AgenticBudgetPlanner;

/// What [`AgenticBudgetPlanner::plan`] produced: the aggregated honest
/// accounting buckets threaded into the cost path ([`PassEffects`]) plus any
/// diagnostic warning tokens (`cache_bust:<source>`, the cache-isolated
/// subagent-lane tag) to surface via `x-tokentrimmer-warnings`.
///
/// `PlanOutcome::default()` is the no-op outcome — what the off-by-default
/// (un-opted) path books: zero effects, no warnings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanOutcome {
    /// The three-bucket honest accounting (field-drop / summary tokens, tax,
    /// cache-bust penalty) for the cost path. `PassEffects::default()` on the
    /// no-op path.
    pub effects: PassEffects,
    /// Diagnostic warning tokens to append to `x-tokentrimmer-warnings` (only
    /// the booked cache-bust `cache_bust:<source>` and the cache-isolated
    /// subagent-lane tag — never the routine no-op path).
    pub warnings: Vec<String>,
}

impl AgenticBudgetPlanner {
    /// A planner with the mode's defaults.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Net the agentic-budget levers for ONE request, in the spec's strict
    /// priority order, booking their interactions honestly (R1).
    ///
    /// `None` → a total no-op: the request is byte-identical and the outcome is
    /// [`PlanOutcome::default`] ([`PassEffects::default`], no warnings). This is
    /// the off-by-default contract at the planner seam — the un-opted path never
    /// touches the request. Task 9 calls this from the chat handler with the
    /// route's `agentic_budget` (`None` for every unrouted request).
    pub fn plan_optional(
        ab: Option<&AgenticBudget>,
        split: &mut SplitRequest<'_>,
        cx: &PassContext<'_>,
    ) -> PlanOutcome {
        match ab {
            Some(ab) => Self::plan(ab, split, cx),
            None => PlanOutcome::default(),
        }
    }

    /// Net the levers for an opted-in `ab` (see [`Self::plan_optional`] for the
    /// `None` no-op). Runs the levers in strict priority order:
    ///
    /// 1. **Sub-lever 1** ([`annotate_cache_prefix`]): confirm/annotate the
    ///    cache-prefix breakpoint when `cache_prefix` is set. Books NO TT
    ///    savings and NO bust — pure provider-cache enablement (it never
    ///    mutates prefix CONTENT, so the escape hatch is not engaged).
    /// 2. **Sub-lever 2a** ([`ElidePass`]): field-drop / minify stale KNOWN-tool
    ///    results when `elide_stale_tools` is set, behind the pipeline's
    ///    token-true gate (operating on the volatile tail only — never the
    ///    prefix). The committed field-drop is gated by `clear_at_least_tokens`:
    ///    an elision freeing fewer tokens than that is SKIPPED, because the
    ///    re-cache a tail mutation forces costs more than a sub-threshold trim
    ///    saves (R1 cache-thrash guard). (Sub-lever 2b summary — Task 7 — and
    ///    Sub-lever 3 routing wire in later.)
    /// 3. **Sub-lever 3** (`route_mechanical_to`): mark routed work as a
    ///    CACHE-ISOLATED subagent lane (a distinct conversation tag), never
    ///    interleaved on the cached prefix — caching and routing fight (R1
    ///    cache-vs-route), so routed work runs on its own prefix lane.
    /// 4. **Sub-lever 4** (`semantic_substep_cache`): mark each READ-ONLY /
    ///    idempotent sub-step (classified fail-closed via [`classify_substep`])
    ///    as a `substep_cache:<tool>` warning. Read-only-safe and net-positive
    ///    by construction — it only INSPECTS the request and books no effect; a
    ///    realized hit on the handler's async L2 sub-step lookup books only its
    ///    measured delta (caveat C2).
    ///
    /// When a lever DOES mutate the stable prefix (only via the
    /// [`SplitRequest::mutate_whole_request`] escape hatch), the booked
    /// [`CacheBustEstimate`] is surfaced as a `cache_bust:<source>` warning and
    /// its `penalty_usd` lands in [`PassEffects::cache_bust_penalty_usd`] — never
    /// hidden.
    pub fn plan(
        ab: &AgenticBudget,
        split: &mut SplitRequest<'_>,
        cx: &PassContext<'_>,
    ) -> PlanOutcome {
        let mut out = PlanOutcome::default();

        // ── Sub-lever 1 (lossless): cache-prefix breakpoint annotation ──────
        // Confirm/annotate the boundary the provider keys its prompt cache on.
        // It NEVER mutates prefix CONTENT, so the escape hatch is not engaged
        // and the returned estimate is `CacheBustEstimate::NONE` by
        // construction — it books NO TT savings and NO bust (the provider's
        // discount lands on the separate `provider_cache_saved_usd` axis,
        // spec §4.3). We surface no warning for the routine annotation.
        if ab.cache_prefix {
            let (_plan, bust) = annotate_cache_prefix(split.request(), cx);
            // Sub-lever 1 cannot bust (no content mutation); booking is a no-op
            // and `book_bust` discards the `#[must_use]` estimate explicitly.
            Self::book_bust(&mut out, bust, cx.pricing);
        }

        // ── Sub-lever 2a (lossless, token-true-gated): field-drop ───────────
        // Run `ElidePass` on the VOLATILE TAIL only (the prefix is unreachable
        // by type), behind the pipeline's token-true gate. A committed
        // field-drop must free at least `clear_at_least_tokens` to justify the
        // re-cache a tail mutation forces; a sub-threshold trim is rolled back
        // byte-identical and books zero (R1 cache-thrash guard).
        if ab.elide_stale_tools {
            let freed = Self::run_field_drop(ab, split, cx);
            out.effects.elide_field_drop_tokens_removed = freed;
        }

        // ── Sub-lever 3 (R1 cache-vs-route isolation): mark the routed lane ──
        // Caching and routing fight: a mid-loop model switch busts the prefix
        // cache, so routed work runs as a CACHE-ISOLATED subagent lane (its own
        // conversation tag), never interleaved on the cached prefix. We surface
        // the lane marker as a warning so the routed dispatch lands on a
        // distinct prefix; the actual dispatch lane is the handler's to wire.
        if let Some(target) = &ab.route_mechanical_to {
            out.warnings.push(format!("subagent_lane:{target}"));
        }

        // ── Sub-lever 4 (read-only-safe, net-positive by construction): mark
        // read-only sub-steps eligible for the semantic sub-step cache ───────
        // Scan the transcript's tool calls and surface each READ-ONLY /
        // idempotent sub-step (classified fail-closed via `classify_substep`) as
        // a `substep_cache:<tool>` marker. Caching a read-only sub-step's result
        // is pure upside — this step never mutates the request and only ever
        // lets the handler AVOID a re-call, so it books no effect here (a
        // realized hit books only its MEASURED delta on the handler's async L2
        // lookup, caveat C2). The async lookup/serve (embedder + store) is the
        // handler's to wire, exactly like Sub-lever 3's dispatch lane.
        if ab.semantic_substep_cache {
            Self::mark_substep_cacheable(&mut out, split.request());
        }

        out
    }

    /// Surface each READ-ONLY sub-step in the transcript as a
    /// `substep_cache:<tool>` warning (deduped, first-seen order — deterministic).
    /// Mutating / unknown tools are fail-closed (never marked). Read-only by
    /// construction: it only inspects the request.
    fn mark_substep_cacheable(out: &mut PlanOutcome, req: &tt_shared::ChatCompletionRequest) {
        use tt_shared::messages::Message;
        for msg in &req.messages {
            let Message::Assistant { tool_calls, .. } = msg else {
                continue;
            };
            for tc in tool_calls {
                if classify_substep(&tc.function.name) == SubstepKind::ReadOnly {
                    let marker = format!("substep_cache:{}", tc.function.name);
                    if !out.warnings.contains(&marker) {
                        out.warnings.push(marker);
                    }
                }
            }
        }
    }

    /// Make Sub-lever 2b (lossy summarize) invocable from the planner, behind the
    /// existing blind-paired-judge gate.
    ///
    /// [`plan`](Self::plan) runs the LOSSLESS half of Sub-lever 2 (2a field-drop)
    /// on the token-true gate; the LOSSY summary half commits ONLY behind a
    /// [`SummaryGate`] (the judge's verdict projection, caveat C1). This is the
    /// planner seam that wires that step: it is a no-op unless
    /// `ab.elide_stale_tools` is set, and even then commits nothing for a class
    /// the `gate` does not trust (fail OPEN to the un-summarized bytes). It keeps
    /// the last `ab.keep_recent_pairs` tool results verbatim and operates on the
    /// volatile tail only (the prefix is unreachable by type).
    ///
    /// Returns the [`SummaryOutcome`] — committed edits, the self-reported byte
    /// delta, and the ledgered summarizer tax — so the caller feeds the judge
    /// samples and ledgers the tax. The production call site (the chat HANDLER,
    /// with the real [`AdaptiveSummaryGate`] and a metered [`Summarizer`]) is
    /// wired separately; the handler owns the judge state and the auxiliary
    /// provider (deferred there).
    pub fn plan_summary(
        ab: &AgenticBudget,
        split: &mut SplitRequest<'_>,
        gate: &dyn SummaryGate,
        summarizer: &dyn Summarizer,
    ) -> SummaryOutcome {
        if !ab.elide_stale_tools {
            return SummaryOutcome::default();
        }
        let step = SummarizeStep::new(ab.keep_recent_pairs, gate, summarizer);
        split.run_pass(|_stable, tail| step.apply(tail))
    }

    /// Run the field-drop pass over the volatile tail behind the token-true
    /// gate, enforcing `clear_at_least_tokens`. Returns the pipeline-measured
    /// tokens removed when the trim clears the threshold, otherwise rolls the
    /// tail back byte-identical and returns 0 (R1 cache-thrash guard).
    fn run_field_drop(
        ab: &AgenticBudget,
        split: &mut SplitRequest<'_>,
        cx: &PassContext<'_>,
    ) -> u32 {
        // Snapshot the tail so a sub-threshold trim can be rolled back
        // byte-identical — a forced re-cache is only worth it above the floor.
        let snapshot = split.tail_snapshot();
        let pipe = crate::passes::PassPipeline::new().with(ElidePass::new(ab.keep_recent_pairs));
        let outcome = pipe.run(split, cx);
        let freed = outcome.tokens_removed;

        if freed < ab.clear_at_least_tokens {
            // Below the floor: the re-cache this elision forces costs more than
            // it saves — discard it, fail open to the original tail bytes.
            split.restore_tail(snapshot);
            0
        } else {
            freed
        }
    }

    /// Book a (possibly-zero) [`CacheBustEstimate`] into the plan outcome: add
    /// its `penalty_usd` to [`PassEffects::cache_bust_penalty_usd`] and surface
    /// it as a `cache_bust:<source>` warning. A zero/`NONE` estimate is
    /// discarded explicitly (no penalty, no warning) so the `#[must_use]`
    /// booking contract is honored on every path — a cache bust is never
    /// hidden, and a non-bust never books a phantom penalty.
    pub fn book_bust(
        out: &mut PlanOutcome,
        bust: CacheBustEstimate,
        pricing: Option<&tt_shared::pricing::ModelPricing>,
    ) {
        let penalty = bust.penalty_usd(pricing);
        if penalty > 0.0 {
            out.effects.cache_bust_penalty_usd += penalty;
            out.warnings.push(format!("cache_bust:{}", bust.source));
        } else {
            // No prefix was mutated (or no rate delta to price it): drop the
            // `#[must_use]` estimate as a deliberate, greppable no-op.
            bust.discard_unused();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tt_routing::AgenticBudget;
    use tt_shared::messages::{Message, MessageContent, ToolCall, ToolCallFunction};
    use tt_shared::ChatCompletionRequest;

    use crate::passes::{PassContext, SplitRequest};

    fn user(text: &str) -> Message {
        Message::User {
            content: MessageContent::Text(text.into()),
            name: None,
        }
    }
    fn assistant_call(call_id: &str, tool: &str) -> Message {
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: tool.into(),
                    arguments: "{}".into(),
                },
            }],
            name: None,
        }
    }
    fn tool_result(call_id: &str, content: serde_json::Value) -> Message {
        Message::Tool {
            content: MessageContent::Text(content.to_string()),
            tool_call_id: call_id.into(),
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
    /// prefix): the whole message list is the volatile tail, so `ElidePass` can
    /// reach the tool results under test.
    fn cx() -> PassContext<'static> {
        PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        }
    }

    /// A budget with every lever off — the explicit base for per-lever tests.
    fn budget() -> AgenticBudget {
        AgenticBudget::default()
    }

    /// An `inspect_diff` result carrying a droppable tempfile `file` path: the
    /// field-drop frees a known, positive number of tokens.
    fn diff_result() -> serde_json::Value {
        json!({
            "scanned": true,
            "detected_language": "python",
            "findings": [{
                "rule_id": "cache-missing",
                "severity": "high",
                "file": "/tmp/.tt-scan-8c1f2-with-a-long-noisy-tempfile-path.py",
                "line": 3,
                "message": "m",
                "confidence": 0.9,
                "fix_hint": "h"
            }]
        })
    }

    /// `agentic_budget == None` → the planner is a TOTAL no-op: the request is
    /// byte-identical and the outcome is `PlanOutcome::default()`
    /// (`PassEffects::default()`, no warnings). The off-by-default contract at
    /// the planner seam.
    #[test]
    fn planner_off_when_none() {
        let mut req = req_with(vec![
            user("scan my diff"),
            assistant_call("c1", "inspect_diff"),
            tool_result("c1", diff_result()),
        ]);
        let before = serde_json::to_string(&req).unwrap();
        let cx = cx();
        // Scope the split so its `&mut req` borrow ends before we re-read `req`.
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan_optional(None, &mut split, &cx)
        };

        assert_eq!(
            out,
            PlanOutcome::default(),
            "None must book the no-op outcome"
        );
        assert_eq!(out.effects, PassEffects::default());
        assert!(out.warnings.is_empty());
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "the un-opted (None) path must leave the request byte-identical"
        );
    }

    /// When an elision would free FEWER than `clear_at_least_tokens`, the
    /// planner SKIPS it (R1 cache-thrash guard): a sub-threshold elision busts
    /// the prefix for less than it saves, so the field-drop is rolled back
    /// byte-identical and books zero.
    #[test]
    fn planner_elide_below_clear_at_least_is_skipped() {
        // Two tool results so that with `keep_recent_pairs=1` the OLDER one
        // (idx 2) is eligible for field-drop while the recent one stays
        // verbatim — a realistic, validation-legal config (keep >= 1).
        let mut req = req_with(vec![
            user("scan my diff"),
            assistant_call("c1", "inspect_diff"),
            tool_result("c1", diff_result()),
            assistant_call("c2", "inspect_diff"),
            tool_result("c2", diff_result()),
        ]);
        let cx = cx();
        // First measure the real field-drop delta with the threshold OFF.
        let baseline = {
            let mut r = req.clone();
            let mut split = SplitRequest::compute(&mut r, &cx);
            let ab = AgenticBudget {
                elide_stale_tools: true,
                keep_recent_pairs: 1,
                clear_at_least_tokens: 0,
                ..budget()
            };
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        let freed = baseline.effects.elide_field_drop_tokens_removed;
        assert!(
            freed > 0,
            "the field-drop must free a positive number of tokens with the threshold off"
        );

        // Now set the threshold ABOVE what the elision can free → it is skipped.
        let before = serde_json::to_string(&req).unwrap();
        let ab = AgenticBudget {
            elide_stale_tools: true,
            keep_recent_pairs: 1,
            clear_at_least_tokens: freed + 1_000,
            ..budget()
        };
        // Scope the split so its `&mut req` borrow ends before we re-read `req`.
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };

        assert_eq!(
            out.effects.elide_field_drop_tokens_removed, 0,
            "a sub-threshold elision must book zero (skipped)"
        );
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "a skipped elision must leave the request byte-identical (no cache thrash)"
        );
    }

    /// With both `cache_prefix=true` and `route_mechanical_to=Some(_)`, the
    /// planner marks routed work as a CACHE-ISOLATED lane (a distinct tag),
    /// never interleaving it on the cached prefix (R1 cache-vs-route).
    #[test]
    fn planner_cache_and_route_are_isolated() {
        // A cache-qualified multi-turn Anthropic conversation so Sub-lever 1
        // actually annotates a prefix to be isolated FROM.
        let cx = PassContext {
            provider_id: "anthropic",
            model: "claude-sonnet-4-6",
            pricing: None,
        };
        let long_history = "word ".repeat(3000);
        let mut req = req_with(vec![
            Message::System {
                content: MessageContent::Text("you are a helpful assistant".into()),
            },
            user(&long_history),
            Message::Assistant {
                content: Some(MessageContent::Text("an answer".into())),
                tool_calls: vec![],
                name: None,
            },
            user("a follow-up"),
        ]);
        let mut split = SplitRequest::compute(&mut req, &cx);
        let ab = AgenticBudget {
            cache_prefix: true,
            route_mechanical_to: Some("haiku-4-5".into()),
            ..budget()
        };
        let out = AgenticBudgetPlanner::plan(&ab, &mut split, &cx);

        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("subagent_lane") && w.contains("haiku-4-5")),
            "routed work must be marked a cache-isolated subagent lane: {:?}",
            out.warnings
        );
    }

    /// When a lever DOES mutate the stable prefix (only via the
    /// `mutate_whole_request` escape hatch), the booked `CacheBustEstimate` is
    /// surfaced as `cache_bust:<source>` and its penalty lands in the effects —
    /// never hidden. (Exercised via the planner's bust-booking helper directly,
    /// since none of the levers wired here mutates the prefix.)
    #[test]
    fn planner_books_bust_negative_when_prefix_mutated() {
        let p = tt_shared::pricing::ModelPricing {
            input_per_million: 10.0,
            output_per_million: 20.0,
            cached_input_per_million: Some(1.0),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: Some(8),
            effective_at: chrono::Utc::now(),
        };
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        let mut req = req_with(vec![
            Message::System {
                content: MessageContent::Text("you are a helpful assistant, be concise".into()),
            },
            user("question"),
            Message::Assistant {
                content: Some(MessageContent::Text("answer".into())),
                tool_calls: vec![],
                name: None,
            },
        ]);
        let split = SplitRequest::compute(&mut req, &cx);
        let stable_tokens = split.stable().est_tokens;
        assert!(stable_tokens > 0, "the prefix must be cache-qualified");

        // The planner's accounting helper: book a non-deterministic prefix
        // mutation. The penalty must be non-zero and the warning greppable.
        let mut out = PlanOutcome::default();
        let (_req_mut, bust) = split.mutate_whole_request(
            "compaction-1",
            crate::passes::MutationDeterminism::NonDeterministic,
        );
        AgenticBudgetPlanner::book_bust(&mut out, bust, Some(&p));

        assert!(
            out.effects.cache_bust_penalty_usd > 0.0,
            "a stable-prefix mutation must book a non-zero negative entry"
        );
        assert!(
            out.warnings.iter().any(|w| w == "cache_bust:compaction-1"),
            "the bust must surface as cache_bust:<source>: {:?}",
            out.warnings
        );
    }

    // ── Sub-lever 4: substep-cache marking now runs in plan() (COST-3) ───────

    /// With `semantic_substep_cache` on, `plan()` marks each READ-ONLY sub-step
    /// `substep_cache:<tool>` and NEVER marks a mutating one (fail-closed). This
    /// is the assertion that Sub-lever 4 now runs in `plan()`.
    #[test]
    fn planner_marks_read_only_substeps_for_cache() {
        let mut req = req_with(vec![
            user("scan and then write"),
            assistant_call("c1", "inspect_diff"), // READ-ONLY (allowlisted)
            tool_result("c1", diff_result()),
            assistant_call("c2", "write_file"), // MUTATING (fail-closed)
            tool_result("c2", diff_result()),
        ]);
        let cx = cx();
        let ab = AgenticBudget {
            semantic_substep_cache: true,
            ..budget()
        };
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        assert!(
            out.warnings
                .iter()
                .any(|w| w == "substep_cache:inspect_diff"),
            "a read-only sub-step must be marked cacheable (Sub-lever 4 runs in plan): {:?}",
            out.warnings
        );
        assert!(
            !out.warnings.iter().any(|w| w == "substep_cache:write_file"),
            "a mutating sub-step must NEVER be marked cacheable (fail-closed): {:?}",
            out.warnings
        );
    }

    /// Off by default: no `substep_cache:` marker unless the lever is set.
    #[test]
    fn planner_does_not_mark_substeps_when_disabled() {
        let mut req = req_with(vec![
            user("scan"),
            assistant_call("c1", "inspect_diff"),
            tool_result("c1", diff_result()),
        ]);
        let cx = cx();
        let ab = budget(); // semantic_substep_cache = false
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        assert!(
            !out.warnings.iter().any(|w| w.starts_with("substep_cache:")),
            "no substep_cache marker when the lever is off: {:?}",
            out.warnings
        );
    }

    /// The same read-only tool called twice is marked once (deduped,
    /// deterministic).
    #[test]
    fn planner_dedups_repeated_read_only_substep_markers() {
        let mut req = req_with(vec![
            user("scan twice"),
            assistant_call("c1", "inspect_diff"),
            tool_result("c1", diff_result()),
            assistant_call("c2", "inspect_diff"),
            tool_result("c2", diff_result()),
        ]);
        let cx = cx();
        let ab = AgenticBudget {
            semantic_substep_cache: true,
            ..budget()
        };
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        let count = out
            .warnings
            .iter()
            .filter(|w| *w == "substep_cache:inspect_diff")
            .count();
        assert_eq!(
            count, 1,
            "a repeated read-only tool must be marked once (deduped): {:?}",
            out.warnings
        );
    }

    // ── Sub-lever 2b: summarize is now invocable from the planner (COST-3) ───

    /// A fixed-output summarizer for the planner-seam test.
    struct TestSummarizer {
        summary: &'static str,
        cost_usd: Option<f64>,
    }
    impl Summarizer for TestSummarizer {
        fn summarize(&self, _class: &str, _content: &str) -> SummarizeCall {
            SummarizeCall {
                summary: Some(self.summary.to_string()),
                cost_usd: self.cost_usd,
            }
        }
    }

    /// Sub-lever 2b is invocable via `plan_summary`, gated behind the judge: a
    /// CLOSED gate commits nothing (fail open, byte-identical), an OPEN gate
    /// commits the lossy summary, and the whole step is a no-op unless
    /// `elide_stale_tools` is set.
    #[test]
    fn plan_summary_invocable_behind_judge_gate() {
        // A big tool blob so the fixed short summary is clearly byte-shorter.
        let big = json!({
            "rows": (0..40).map(|i| format!("row-{i}-noisy-payload")).collect::<Vec<_>>(),
        });
        let make_req = || {
            req_with(vec![
                user("call a tool"),
                assistant_call("c1", "mystery_tool"),
                tool_result("c1", big.clone()),
            ])
        };
        let cx = cx();
        let summarizer = TestSummarizer {
            summary: "[s]",
            cost_usd: Some(0.0001),
        };
        // keep_recent_pairs=0 so the single tool result is eligible.
        let ab = AgenticBudget {
            elide_stale_tools: true,
            keep_recent_pairs: 0,
            ..budget()
        };

        // Closed gate → nothing commits (fail open to the un-summarized bytes).
        {
            let mut req = make_req();
            let before = serde_json::to_string(&req).unwrap();
            let closed = NeverCommitGate;
            let out = {
                let mut split = SplitRequest::compute(&mut req, &cx);
                AgenticBudgetPlanner::plan_summary(&ab, &mut split, &closed, &summarizer)
            };
            assert!(
                out.committed.is_empty(),
                "a closed judge gate commits no summary"
            );
            assert_eq!(
                serde_json::to_string(&req).unwrap(),
                before,
                "a gate-refused summary leaves the request byte-identical (fail open)"
            );
        }

        // Open gate → commits the lossy summary THROUGH the planner seam.
        {
            let mut req = make_req();
            let open = AlwaysCommitGate;
            let out = {
                let mut split = SplitRequest::compute(&mut req, &cx);
                AgenticBudgetPlanner::plan_summary(&ab, &mut split, &open, &summarizer)
            };
            assert_eq!(
                out.committed.len(),
                1,
                "an open judge gate commits the lossy summary via plan_summary"
            );
        }

        // Disabled (elide_stale_tools=false) → no-op even with an open gate.
        {
            let mut req = make_req();
            let open = AlwaysCommitGate;
            let disabled = budget(); // elide_stale_tools = false
            let out = {
                let mut split = SplitRequest::compute(&mut req, &cx);
                AgenticBudgetPlanner::plan_summary(&disabled, &mut split, &open, &summarizer)
            };
            assert!(
                out.committed.is_empty(),
                "Sub-lever 2b is gated on elide_stale_tools (no-op when off)"
            );
        }
    }
}
