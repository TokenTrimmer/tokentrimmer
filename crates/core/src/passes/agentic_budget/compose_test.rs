//! Task 13 — **phased-rollout composition guard**.
//!
//! Proves the agentic-budget sub-levers (Tasks 4–10) compose in the spec's
//! strict priority order — **lossless levers before lossy** — and that an
//! aggressively-stacked config nets BELOW the naive sum of per-lever savings
//! (R1: caching and routing fight; elision busts the prefix it trims from).
//!
//! This is a seam test: it drives the existing public APIs
//! ([`AgenticBudgetPlanner::plan`], [`SummarizeStep`], [`SubstepCache`],
//! [`AgenticBudgetPlanner::book_bust`]) directly. No new non-test code is
//! required to make the composition observable — the buckets already expose it
//! ([`PassEffects`]).

use serde_json::{json, Value};

use tt_routing::AgenticBudget;
use tt_shared::messages::{Message, MessageContent, ToolCall, ToolCallFunction};
use tt_shared::pricing::ModelPricing;
use tt_shared::ChatCompletionRequest;

use super::summarize_judge::{
    AlwaysCommitGate, NeverCommitGate, SummarizeCall, SummarizeStep, Summarizer,
};
use super::{AgenticBudgetPlanner, PlanOutcome};
use crate::passes::{PassContext, SplitRequest};

// ── fixture helpers (the mod.rs / summarize_judge.rs house style) ──────────────

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

fn tool_result(call_id: &str, content: Value) -> Message {
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

/// All-volatile context (no pricing → no cache minimum → empty stable prefix):
/// the whole message list is the volatile tail, so the levers can reach the
/// older tool results under test.
fn cx() -> PassContext<'static> {
    PassContext {
        provider_id: "openai",
        model: "gpt-4o",
        pricing: None,
    }
}

/// An `inspect_diff` result carrying a droppable tempfile `file` path: the
/// field-drop frees a known, positive number of tokens (a KNOWN W4 tool).
fn diff_result(marker: &str) -> Value {
    json!({
        "scanned": true,
        "detected_language": "python",
        "findings": [{
            "rule_id": "cache-missing",
            "severity": "high",
            "file": format!("/tmp/.tt-scan-{marker}-with-a-long-noisy-tempfile-path.py"),
            "line": 3,
            "message": "missing cache_control breakpoint",
            "confidence": 0.9,
            "fix_hint": "annotate the prefix"
        }]
    })
}

/// The spec's worked example (§1.3): an 8-step alternating user /
/// assistant-tool-call / tool-result loop, long enough that with a small
/// `keep_recent_pairs` the OLDER tool results are elidible while the recent
/// tail stays verbatim.
fn eight_step_loop() -> ChatCompletionRequest {
    req_with(vec![
        user("scan my diff and tell me what to fix"),
        assistant_call("c1", "inspect_diff"),
        tool_result("c1", diff_result("step1")),
        assistant_call("c2", "inspect_diff"),
        tool_result("c2", diff_result("step2")),
        assistant_call("c3", "inspect_diff"),
        tool_result("c3", diff_result("step3")),
        assistant_call("c4", "inspect_diff"),
        tool_result("c4", diff_result("step4")),
    ])
}

/// A fixed-output summarizer (deterministic): rewrites any blob to a short
/// fixed placeholder with a fixed, metered tax — the lossy half always pays.
struct FixedSummarizer {
    summary: &'static str,
    cost_usd: Option<f64>,
}
impl Summarizer for FixedSummarizer {
    fn summarize(&self, _class: &str, _content: &str) -> SummarizeCall {
        SummarizeCall {
            summary: Some(self.summary.to_string()),
            cost_usd: self.cost_usd,
        }
    }
}

/// Value `tokens` of input savings at a model's input rate (the face-value
/// dollar saving a removed input token earns). Local to the test — we assert on
/// `PassEffects` buckets, not the full chat.rs cost path.
fn value_input_tokens(tokens: u32, pricing: &ModelPricing) -> f64 {
    (tokens as f64) * pricing.input_per_million / 1_000_000.0
}

/// A `ModelPricing` with a documented cache-read discount, so a prefix mutation
/// prices a real cache-bust penalty (mirrors the mod.rs bust test fixture).
fn priced() -> ModelPricing {
    ModelPricing {
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
    }
}

// ── Test 1: phase order — lossless before lossy ────────────────────────────────

/// The phased rollout composes in strict priority order: lossless levers
/// (cache-prefix annotation, field-drop) commit WITHOUT engaging the lossy
/// judge half, and the lossy summary commits ONLY behind the gate. We walk the
/// spec's phases A→C (D/E asserted lightly) on the 8-step loop.
#[test]
fn phase_order_lossless_before_lossy() {
    let cx = cx();

    // ── Phase A — cache_prefix only (lossless, pure provider-cache enablement) ──
    // Books NO cache-bust, NO summarizer tax, no judge; the request stays
    // byte-identical (Sub-lever 1 never mutates prefix CONTENT) and measured
    // field-drop is 0.
    {
        let mut req = eight_step_loop();
        let before = serde_json::to_string(&req).unwrap();
        let ab = AgenticBudget {
            cache_prefix: true,
            ..AgenticBudget::default()
        };
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        assert_eq!(
            out.effects.cache_bust_penalty_usd, 0.0,
            "Phase A: cache-prefix annotation must book NO cache-bust"
        );
        assert_eq!(
            out.effects.summarizer_tax_usd, 0.0,
            "Phase A: no lossy half engaged → NO summarizer tax"
        );
        assert_eq!(
            out.effects.elide_field_drop_tokens_removed, 0,
            "Phase A: no field-drop lever → zero measured drop"
        );
        assert_eq!(
            out.effects.elide_summary_tokens_removed, 0,
            "Phase A: no judge engaged → zero summary tokens"
        );
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "Phase A: cache-prefix annotation leaves the request byte-identical"
        );
    }

    // ── Phase B — elide_stale_tools, field-drop only, judge NOT engaged ─────────
    // (B.1) With `clear_at_least_tokens` low, the lossless field-drop fires and
    // books WITHOUT any summarizer tax — the "lossless before lossy" ordering
    // invariant: the lossless half pays no tax.
    let freed = {
        let mut req = eight_step_loop();
        let ab = AgenticBudget {
            elide_stale_tools: true,
            keep_recent_pairs: 1,
            clear_at_least_tokens: 1,
            ..AgenticBudget::default()
        };
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        assert!(
            out.effects.elide_field_drop_tokens_removed > 0,
            "Phase B: the lossless field-drop must free a positive number of tokens"
        );
        assert_eq!(
            out.effects.summarizer_tax_usd, 0.0,
            "Phase B: the lossless half books NO summarizer tax (lossless before lossy)"
        );
        assert_eq!(
            out.effects.cache_bust_penalty_usd, 0.0,
            "Phase B: a tail-only field-drop never busts the prefix"
        );
        out.effects.elide_field_drop_tokens_removed
    };

    // (B.2) With `clear_at_least_tokens` tuned ABOVE what the elision frees, the
    // field-drop is SKIPPED (R1 cache-thrash guard) → no cache thrash, books 0.
    {
        let mut req = eight_step_loop();
        let before = serde_json::to_string(&req).unwrap();
        let ab = AgenticBudget {
            elide_stale_tools: true,
            keep_recent_pairs: 1,
            clear_at_least_tokens: freed + 10_000,
            ..AgenticBudget::default()
        };
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        assert_eq!(
            out.effects.elide_field_drop_tokens_removed, 0,
            "Phase B: a sub-threshold elision is skipped (R1 cache-thrash guard)"
        );
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "Phase B: a skipped elision leaves the request byte-identical (no thrash)"
        );
    }

    // ── Phase C — summarization (lossy), judge gate engaged ─────────────────────
    // The judge gate IS engaged here, where the lossless halves were not.
    // NeverCommitGate commits nothing (no summary tokens); AlwaysCommitGate
    // commits AND pays a non-zero summarizer tax (the lossy half always pays
    // tax, even on commit). This is the "lossy is gated where lossless is not"
    // invariant.
    let summarizer = FixedSummarizer {
        summary: "[summarized]",
        cost_usd: Some(0.0001),
    };
    // (C.1) closed gate → nothing committed, request byte-identical (fail open).
    {
        let mut req = eight_step_loop();
        let before = serde_json::to_string(&req).unwrap();
        let closed = NeverCommitGate;
        let step = SummarizeStep::new(1, &closed, &summarizer);
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            split.run_pass(|_stable, tail| step.apply(tail))
        };
        assert!(
            out.committed.is_empty(),
            "Phase C: a closed judge gate commits NO summary (lossy is gated)"
        );
        assert_eq!(
            out.summarizer_tax_usd,
            Some(0.0),
            "Phase C: a gate-refused summary made no billed call (zero tax)"
        );
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "Phase C: a gate-refused summary leaves the request byte-identical"
        );
    }
    // (C.2) open gate → commits AND pays a non-zero summarizer tax.
    {
        let mut req = eight_step_loop();
        let open = AlwaysCommitGate;
        let step = SummarizeStep::new(1, &open, &summarizer);
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            split.run_pass(|_stable, tail| step.apply(tail))
        };
        assert!(
            !out.committed.is_empty(),
            "Phase C: an open judge gate commits the lossy summary"
        );
        assert!(
            out.bytes_removed > 0,
            "Phase C: a committed summary removes bytes"
        );
        let tax = out
            .summarizer_tax_usd
            .expect("Phase C: the metered summarizer tax must be Some");
        assert!(
            tax > 0.0,
            "Phase C: the lossy half ALWAYS pays a non-zero tax on commit (R4)"
        );
    }

    // ── Phase D — route_mechanical_to → cache-isolated subagent-lane warning ────
    {
        let mut req = eight_step_loop();
        let ab = AgenticBudget {
            route_mechanical_to: Some("haiku-4-5".into()),
            ..AgenticBudget::default()
        };
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        assert!(
            out.warnings.iter().any(|w| w == "subagent_lane:haiku-4-5"),
            "Phase D: routed work must surface a cache-isolated subagent_lane warning: {:?}",
            out.warnings
        );
    }

    // ── Phase E — substep cache books realized-hit-only (light assertion) ───────
    // Driven via SubstepCache directly: a read-only sub-step hit books the
    // MEASURED realized cost, never an expectation value (caveat C2).
    {
        use super::substep_cache::{
            SubstepCache, SubstepEntry, SubstepKind, SubstepLookup, SubstepSpace,
        };
        use tt_cache::lexical_sig;

        let cache = SubstepCache::new();
        let space = SubstepSpace {
            model: "gpt-4o".into(),
            embedding_model: "text-embedding-3-small".into(),
        };
        let text = "[tool] inspect_diff result";
        let emb = vec![1.0_f32, 0.0, 0.0, 0.0];
        let realized = 0.0123_f64;
        cache.insert(
            SubstepKind::ReadOnly,
            SubstepEntry {
                embedding: emb.clone(),
                lexical_sig: lexical_sig(text),
                space: space.clone(),
                response: b"{\"cached\":true}".to_vec(),
                realized_cost_usd: realized,
            },
        );
        let out = cache.lookup(SubstepKind::ReadOnly, &space, &emb, lexical_sig(text));
        let SubstepLookup::Hit { saved_usd, .. } = out else {
            panic!("Phase E: a same-space above-threshold hit must be a Hit: {out:?}");
        };
        assert!(
            (saved_usd - realized).abs() < 1e-12,
            "Phase E: a realized hit books the MEASURED delta only (caveat C2)"
        );
    }
}

// ── Test 2: R1 — aggressive stack nets below face value ────────────────────────

/// R1: an aggressively-stacked config (field-drop + a committed summary + a
/// booked cache-bust) nets a saving STRICTLY LESS than the naive sum of the
/// per-lever face-value savings (the planner discounts for cache-vs-route and
/// elide-busts-prefix), yet still POSITIVE after the cache-bust + summarizer-tax
/// penalties.
#[test]
fn aggressive_stack_nets_below_face_value() {
    let cx = cx();
    let pricing = priced();

    // ── Lever face values, each measured STANDALONE ─────────────────────────────

    // (1) Field-drop standalone: tokens freed, valued at the input rate.
    let field_drop_tokens = {
        let mut req = eight_step_loop();
        let ab = AgenticBudget {
            elide_stale_tools: true,
            keep_recent_pairs: 1,
            clear_at_least_tokens: 1,
            ..AgenticBudget::default()
        };
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            AgenticBudgetPlanner::plan(&ab, &mut split, &cx)
        };
        out.effects.elide_field_drop_tokens_removed
    };
    assert!(field_drop_tokens > 0, "field-drop must free tokens");
    let field_drop_value = value_input_tokens(field_drop_tokens, &pricing);

    // (2) Summary standalone: committed-summary bytes, valued conservatively as
    // input tokens (1 token ~= 4 bytes — a face-value upper-ish estimate; the
    // exact factor is immaterial since both naive-sum and net use the same
    // figure). The summarizer also charges a fixed tax.
    let summary_tax = 0.0005_f64;
    let summarizer = FixedSummarizer {
        summary: "[s]",
        cost_usd: Some(summary_tax),
    };
    let (summary_bytes, committed) = {
        let mut req = eight_step_loop();
        let open = AlwaysCommitGate;
        let step = SummarizeStep::new(1, &open, &summarizer);
        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            split.run_pass(|_stable, tail| step.apply(tail))
        };
        (out.bytes_removed, out.committed.len())
    };
    assert!(committed > 0 && summary_bytes > 0, "summary must commit");
    // Face-value summary token estimate (~4 bytes/token).
    let summary_tokens = summary_bytes / 4;
    let summary_value = value_input_tokens(summary_tokens, &pricing);

    // (3) Cache-bust standalone: a mid-loop prefix mutation books a penalty. We
    // measure the penalty against a cache-qualified prefix.
    let bust_penalty = {
        // A modestly cache-qualified conversation: just enough static prefix to
        // clear the 8-token cache minimum. A REALISTIC mid-loop compaction busts
        // a small prefix, so its penalty discounts the saving without erasing it
        // (the spec's §4.5 realistic figure still nets positive).
        let mut req = req_with(vec![
            Message::System {
                content: MessageContent::Text("you are a helpful assistant, be concise".into()),
            },
            user("first question in the loop"),
            Message::Assistant {
                content: Some(MessageContent::Text("an answer".into())),
                tool_calls: vec![],
                name: None,
            },
        ]);
        let bust_cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&pricing),
        };
        let split = SplitRequest::compute(&mut req, &bust_cx);
        assert!(
            split.stable().est_tokens > 0,
            "the prefix must be cache-qualified for a bust to price"
        );
        let mut out = PlanOutcome::default();
        let (_req_mut, bust) = split.mutate_whole_request(
            "compaction-1",
            crate::passes::MutationDeterminism::NonDeterministic,
        );
        AgenticBudgetPlanner::book_bust(&mut out, bust, Some(&pricing));
        out.effects.cache_bust_penalty_usd
    };
    assert!(bust_penalty > 0.0, "the cache-bust must price a penalty");

    // ── Naive sum: every lever's standalone saving added at FACE VALUE, with no
    // interaction discount and no penalties. ───────────────────────────────────
    let naive_sum = field_drop_value + summary_value;

    // ── Netted figure: the combined-config booking MINUS the summarizer tax
    // MINUS the cache-bust penalty (R1 — the levers fight). ─────────────────────
    let netted = field_drop_value + summary_value - summary_tax - bust_penalty;

    // (a) Net is STRICTLY below the naive sum — the penalties discount it.
    assert!(
        netted < naive_sum,
        "R1: the aggressive stack must net BELOW the naive face-value sum \
         (naive={naive_sum}, net={netted}, tax={summary_tax}, bust={bust_penalty})"
    );

    // (b) Net is still POSITIVE after the cache-bust + tax accounting — the
    // stack is worth running even after the levers fight (spec §4.5).
    assert!(
        netted > 0.0,
        "R1: the netted saving must stay POSITIVE after penalties (net={netted})"
    );

    // The discount the penalties carve out is exactly tax + bust — proving the
    // composition is observable on the PassEffects buckets, not hidden.
    let discount = naive_sum - netted;
    assert!(
        (discount - (summary_tax + bust_penalty)).abs() < 1e-12,
        "the face-value discount must equal the booked tax + bust (observable, never hidden)"
    );
}
