//! Tests for `chat::compact` — split out of `compact.rs` to honor the
//! repo-wide 800-line cap per `.rs` file.

use super::*;
use httpmock::prelude::*;
use serde_json::json;

const FILLER: &str = "the quick brown fox jumps over the lazy dog while discussing model \
    routing budgets caching prefixes and token estimation in considerable detail";

/// A conversation with `turns` whole user+assistant turns of fat text.
fn fat_conv(turns: usize) -> Conversation {
    let mut c = Conversation::new("gpt-4o-mini".into(), Some("be helpful".into()));
    push_fat_turns(&mut c, 0, turns);
    c
}

fn push_fat_turns(c: &mut Conversation, from: usize, n: usize) {
    for i in from..from + n {
        c.push_user(format!("question {i}: {FILLER}"));
        c.push_assistant(format!("answer {i}: {FILLER} {FILLER}"));
    }
}

fn test_ctx() -> ContextState {
    ContextState::new(None, std::collections::HashMap::new())
}

/// A mock gateway whose compact endpoint is pinned on the `chat-compact`
/// tag + interactive headers and returns `summary` with cost headers.
fn mock_compact_endpoint<'a>(server: &'a MockServer, summary: &str) -> httpmock::Mock<'a> {
    let body = json!({
        "id": "c", "object": "chat.completion", "created": 0, "model": "gpt-4o-mini",
        "choices": [{ "index": 0,
            "message": { "role": "assistant", "content": summary } }],
        "usage": { "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120 }
    });
    server.mock(|when, then| {
        when.method(POST)
            .path("/v1/chat/completions")
            .header("x-tokentrimmer-tag", "chat-compact")
            .header("x-tokentrimmer-interactive", "1");
        then.status(200)
            .header("content-type", "application/json")
            .header("x-tokentrimmer-model-used", "gpt-4o-mini")
            .header("x-tokentrimmer-cost-usd", "0.0001")
            .json_body(body);
    })
}

#[test]
fn k_below_two_is_rejected() {
    let mut st = CompactionState::new(true, None);
    let err = st.set_every(1).unwrap_err();
    assert!(err.contains("−145%"), "{err}");
    assert!(err.contains("every turn"), "{err}");
    assert_eq!(st.every(), DEFAULT_COMPACT_EVERY, "K must be unchanged");
    let err0 = st.set_every(0).unwrap_err();
    assert!(err0.contains("rejected"), "{err0}");
    st.set_every(2)
        .expect("K=2 is the minimum and must be accepted");
    assert_eq!(st.every(), 2);
}

#[test]
fn due_fires_only_every_k_and_with_enough_turns() {
    let mut st = CompactionState::new(true, None);
    st.set_every(2).unwrap();
    let conv = fat_conv(6); // > KEEP_RECENT_TURNS whole turns
    assert!(!st.due(&conv), "no turns counted yet");
    st.note_turn();
    assert!(!st.due(&conv), "1 < K");
    st.note_turn();
    assert!(st.due(&conv), "K reached + enough history");
    // not enough history → never due, regardless of the counter
    let small = fat_conv(KEEP_RECENT_TURNS);
    assert!(!st.due(&small));
    // disabled → never due
    st.enabled = false;
    assert!(!st.due(&conv));
    st.enabled = true;
    st.reset_cadence();
    assert!(!st.due(&conv), "counter reset");
}

/// Part D invariant 1+2: between compactions the summary block is
/// byte-identical and sits at the stable-prefix position (right after the
/// system message; first when there is no system).
#[test]
fn summary_block_is_byte_identical_across_non_compaction_turns() {
    let mut conv = fat_conv(2);
    conv.summary = Some(FrozenSummary::replace_at_compaction(
        format!("{SUMMARY_HEADER}\nuser asked fox questions; assistant answered."),
        3,
        "gpt-4o-mini".into(),
    ));
    let block_bytes = |c: &Conversation, pos: usize| -> String {
        let wire = c.wire_messages();
        let Message::System {
            content: MessageContent::Text(t),
        } = &wire[pos]
        else {
            panic!("expected System at wire position {pos}");
        };
        t.clone()
    };
    let first = block_bytes(&conv, 1); // position 1: right after system
    for i in 0..5 {
        conv.push_user(format!("follow-up {i}"));
        conv.push_assistant(format!("reply {i}"));
        assert_eq!(
            block_bytes(&conv, 1),
            first,
            "summary bytes/position must not change between compactions"
        );
    }
    // no system prompt → the summary block is the FIRST wire message
    conv.system = None;
    assert_eq!(block_bytes(&conv, 0), first);
}

#[test]
fn plan_predicate_net_positive_for_fat_history_negative_for_tiny_fold() {
    // 8 fat turns → folding 4 fat turns clearly pays for itself over K=8.
    let plan = build_plan(&fat_conv(8), DEFAULT_COMPACT_EVERY).expect("plan");
    assert_eq!(plan.fold_turns, 4);
    assert!(
        plan.net_positive(),
        "save={} spend={}",
        plan.save_tokens,
        plan.spend_tokens
    );
    // 1 tiny turn folded against a fat kept tail → net-negative.
    let mut tiny = Conversation::new("gpt-4o-mini".into(), None);
    tiny.push_user("hi".into());
    tiny.push_assistant("yo".into());
    push_fat_turns(&mut tiny, 0, KEEP_RECENT_TURNS);
    let plan = build_plan(&tiny, DEFAULT_COMPACT_EVERY).expect("plan");
    assert_eq!(plan.fold_turns, 1);
    assert!(
        !plan.net_positive(),
        "save={} spend={}",
        plan.save_tokens,
        plan.spend_tokens
    );
    // nothing beyond the kept tail → no plan at all
    assert!(build_plan(&fat_conv(KEEP_RECENT_TURNS), 8).is_none());
}

/// Cadence is every K turns, NOT per turn: 16 simulated successful turns
/// with K=8 → exactly 2 `chat-compact` requests.
#[tokio::test]
async fn compaction_fires_only_every_k_turns() {
    console::set_colors_enabled(false);
    let server = MockServer::start_async().await;
    let mock = mock_compact_endpoint(
        &server,
        "The user asked numbered fox questions; the assistant gave numbered answers.",
    );
    let client = tt_client::Client::new(server.base_url(), "k");
    let mut conv = fat_conv(0);
    let mut cstate = CompactionState::new(true, None); // K = 8
    let mut ctx = test_ctx();
    let mut ledger = Ledger::default();

    for i in 0..16 {
        push_fat_turns(&mut conv, i, 1);
        cstate.note_turn();
        maybe_compact(&client, &mut conv, &mut cstate, &mut ctx, &mut ledger).await;
    }
    assert_eq!(
        mock.calls_async().await,
        2,
        "compaction must fire once per K, not per turn"
    );
    let sum = conv.summary.as_ref().expect("summary present");
    // fire 1 folds 8−4=4 turns; fire 2 folds (4+8)−4=8 → cumulative 12
    assert_eq!(sum.folded_turns(), 12);
    assert!(sum.wire_block().starts_with(SUMMARY_HEADER));
    assert_eq!(budget::turn_starts(&conv.messages).len(), KEEP_RECENT_TURNS);
    // HONESTY GUARD: real spend metered, savings figures untouched
    assert_eq!(ledger.compaction_calls, 2);
    assert!((ledger.compaction_spend_usd - 0.0002).abs() < 1e-9);
    assert!((ledger.cost_usd - 0.0002).abs() < 1e-9);
    assert_eq!(ledger.saved_usd, 0.0);
    assert_eq!(ledger.baseline_usd, 0.0);
    assert_eq!(ledger.turns, 0);
    assert!(ledger.compaction_est_tok_per_turn > 0);
}

/// Part D invariant 3 gate: a net-negative plan skips with ZERO gateway
/// calls and no history change; the cadence resets so it isn't retried
/// every turn.
#[tokio::test]
async fn net_negative_compaction_is_skipped_with_warning_and_no_history_change() {
    console::set_colors_enabled(false);
    let server = MockServer::start_async().await;
    let mock = mock_compact_endpoint(&server, "unused");
    let client = tt_client::Client::new(server.base_url(), "k");
    // 1 tiny foldable turn + a fat kept tail → predicate says net-negative.
    let mut conv = Conversation::new("gpt-4o-mini".into(), None);
    conv.push_user("hi".into());
    conv.push_assistant("yo".into());
    push_fat_turns(&mut conv, 0, KEEP_RECENT_TURNS);
    let before = serde_json::to_string(&conv.messages).unwrap();
    let mut cstate = CompactionState::new(true, None);
    for _ in 0..DEFAULT_COMPACT_EVERY {
        cstate.note_turn();
    }
    let mut ctx = test_ctx();
    let mut ledger = Ledger::default();
    assert!(cstate.due(&conv));
    let out = compact_now(&client, &mut conv, &mut cstate, &mut ctx, &mut ledger).await;
    assert_eq!(out, CompactOutcome::SkippedNetNegative);
    assert_eq!(
        mock.calls_async().await,
        0,
        "skip must make zero gateway calls"
    );
    assert!(conv.summary.is_none());
    assert_eq!(serde_json::to_string(&conv.messages).unwrap(), before);
    assert_eq!(ledger.compaction_calls, 0);
    assert_eq!(ledger.cost_usd, 0.0);
    assert!(!cstate.due(&conv), "cadence counter must reset on skip");
}

/// Failed summary call = strict no-op on history; real spend reported on
/// the error response is still metered.
#[tokio::test]
async fn failed_summary_call_is_noop_on_history() {
    console::set_colors_enabled(false);
    let server = MockServer::start_async().await;
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(500)
            .header("x-tokentrimmer-cost-usd", "0.0002")
            .body("boom");
    });
    let client = tt_client::Client::new(server.base_url(), "k");
    let mut conv = fat_conv(8);
    let before = serde_json::to_string(&conv.messages).unwrap();
    let mut cstate = CompactionState::new(true, None);
    let mut ctx = test_ctx();
    let mut ledger = Ledger::default();
    let out = compact_now(&client, &mut conv, &mut cstate, &mut ctx, &mut ledger).await;
    assert_eq!(out, CompactOutcome::Failed);
    assert!(conv.summary.is_none(), "no summary on failure");
    assert_eq!(serde_json::to_string(&conv.messages).unwrap(), before);
    assert_eq!(ledger.turns, 0);
    // spend from the error response's headers is still real money
    assert_eq!(ledger.compaction_calls, 1);
    assert!((ledger.compaction_spend_usd - 0.0002).abs() < 1e-9);
    assert_eq!(ledger.saved_usd, 0.0);
}

/// An empty or non-compressing summary is discarded (no-op on history) but
/// the call's spend is still metered from the headers.
#[tokio::test]
async fn oversized_or_empty_summary_is_discarded() {
    console::set_colors_enabled(false);
    // Case 1: the "summary" is LONGER than the folded turns (est ≥ D).
    let server = MockServer::start_async().await;
    let oversized = FILLER.repeat(40);
    let _m = mock_compact_endpoint(&server, &oversized);
    let client = tt_client::Client::new(server.base_url(), "k");
    let mut conv = fat_conv(8);
    let before = serde_json::to_string(&conv.messages).unwrap();
    let mut cstate = CompactionState::new(true, None);
    let mut ctx = test_ctx();
    let mut ledger = Ledger::default();
    let out = compact_now(&client, &mut conv, &mut cstate, &mut ctx, &mut ledger).await;
    assert_eq!(out, CompactOutcome::Discarded);
    assert!(conv.summary.is_none());
    assert_eq!(serde_json::to_string(&conv.messages).unwrap(), before);
    assert_eq!(ledger.compaction_calls, 1, "spend still metered");
    assert!((ledger.compaction_spend_usd - 0.0001).abs() < 1e-9);
    assert_eq!(ledger.compaction_est_tok_per_turn, 0, "no savings claim");

    // Case 2: empty text → same no-op, spend metered.
    let server2 = MockServer::start_async().await;
    let _m2 = mock_compact_endpoint(&server2, "");
    let client2 = tt_client::Client::new(server2.base_url(), "k");
    let mut conv2 = fat_conv(8);
    let before2 = serde_json::to_string(&conv2.messages).unwrap();
    let out2 = compact_now(&client2, &mut conv2, &mut cstate, &mut ctx, &mut ledger).await;
    assert_eq!(out2, CompactOutcome::Discarded);
    assert!(conv2.summary.is_none());
    assert_eq!(serde_json::to_string(&conv2.messages).unwrap(), before2);
    assert_eq!(ledger.compaction_calls, 2);
}

/// Off-by-default proof: with compaction disabled, no amount of turns ever
/// produces a `chat-compact` request.
#[tokio::test]
async fn no_compact_call_when_disabled() {
    let server = MockServer::start_async().await;
    let mock = mock_compact_endpoint(&server, "unused");
    let client = tt_client::Client::new(server.base_url(), "k");
    let mut conv = fat_conv(0);
    let mut cstate = CompactionState::new(false, None); // the default
    let mut ctx = test_ctx();
    let mut ledger = Ledger::default();
    for i in 0..20 {
        push_fat_turns(&mut conv, i, 1);
        cstate.note_turn();
        maybe_compact(&client, &mut conv, &mut cstate, &mut ctx, &mut ledger).await;
    }
    assert_eq!(mock.calls_async().await, 0);
    assert!(conv.summary.is_none());
    assert_eq!(ledger.compaction_calls, 0);
}

/// HONESTY GUARD pin: `add_compaction` is real spend (headline cost) and
/// NEVER touches turns / saved_usd / baseline_usd; the `/cost` line labels
/// the estimate as unbooked.
#[test]
fn compaction_spend_metered_never_booked_as_savings() {
    console::set_colors_enabled(false);
    let mut l = Ledger {
        turns: 3,
        cost_usd: 0.01,
        saved_usd: 0.02,
        baseline_usd: 0.03,
        ..Ledger::default()
    };
    l.add_compaction(0.005);
    l.compaction_est_tok_per_turn += 400;
    assert!((l.cost_usd - 0.015).abs() < 1e-12, "headline spend grows");
    assert!((l.compaction_spend_usd - 0.005).abs() < 1e-12);
    assert_eq!(l.compaction_calls, 1);
    assert_eq!(l.turns, 3, "compaction is not a turn");
    assert!(
        (l.saved_usd - 0.02).abs() < 1e-12,
        "saved_usd must not move"
    );
    assert!((l.baseline_usd - 0.03).abs() < 1e-12);
    let s = l.summary();
    assert!(s.contains("compaction $0.0050"), "{s}");
    assert!(s.contains("est. −400 tok/turn"), "{s}");
    assert!(s.contains("unbooked"), "{s}");
}

/// `/compact now` with nothing beyond the kept tail is an honest no-op.
#[tokio::test]
async fn compact_now_with_nothing_to_fold_is_noop() {
    console::set_colors_enabled(false);
    let server = MockServer::start_async().await;
    let mock = mock_compact_endpoint(&server, "unused");
    let client = tt_client::Client::new(server.base_url(), "k");
    let mut conv = fat_conv(KEEP_RECENT_TURNS);
    let mut cstate = CompactionState::new(false, None); // manual works even when off
    let mut ctx = test_ctx();
    let mut ledger = Ledger::default();
    let out = compact_now(&client, &mut conv, &mut cstate, &mut ctx, &mut ledger).await;
    assert_eq!(out, CompactOutcome::NothingToFold);
    assert_eq!(mock.calls_async().await, 0);
}
