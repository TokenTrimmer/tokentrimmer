# Agent-loop slice 2c-1 (judge-gated summarize — mechanism, operator-promoted gate) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `SummarizeStep`-style lossy summarization into the server-side agent loop — an *aging* tool-result block is summarized **once**, persisted into the accumulated transcript, token-true gated, default-off, behind an operator-promoted per-class gate.

**Architecture:** Summarize runs inside `run_loop_core` over the loop's accumulated `messages` (the only place a transcript persists across turns). An optional `TranscriptSummarizer` hook is injected into `run_loop_core` (`None` in the pure tests + the 1a wrapper → preserves the provider-free test seam + 1b byte-identical behavior). The production hook (`GatewayTranscriptSummarizer`) resolves eligible aging tool blocks, checks the gate, dispatches a cheap-model summary via `measured_single_dispatch`, applies a token-true gate, commits in place, and advances a persisted `summarized_upto` watermark. The run's summarize policy is resolved once at run creation via `apply_routing` (pinned to the turn-0 route) and persisted in `StoredRun` for resume. The summarizer cost is a measurement tax surfaced on a new `summarizer_tax_usd` field, never folded into served cost.

**Tech Stack:** Rust, `crates/core` (tt-core). `summarize_judge.rs` (gates + policy helpers), `agent_run.rs` (loop + handlers + the summarizer hook), `state.rs` (the process-wide gate), `measurement.rs`/`chat.rs`/`tt_tokenize` (reused seams). `async_trait`, `serde`, `tokio`.

**Spec:** `docs/superpowers/specs/2026-06-19-agent-loop-slice2c1-design.md` (read it; this plan implements it).

**Behavior-preservation gate (run after every task that compiles, and as the final check):**
```
cargo test -p tt-core --lib --tests
```
Default-off paths (no `elide_stale_tools` / gate Never / `summarizer: None`) must stay byte-identical to slice 1b; `/v1/chat/completions` is untouched. The DB gate is NOT needed for this slice (no DB/pgvector). Clippy: `cargo clippy -p tt-core --all-targets`.

---

## File Structure

| File | Change |
|---|---|
| `crates/core/src/passes/agentic_budget/summarize_judge.rs` | Add `ConfigSummaryGate` + `parse_trusted_classes`; make `resolve_summary_class`/`is_error_blob` `pub(crate)`; update the stale token-gate doc. |
| `crates/core/src/state.rs` | Add `AppState.summary_gate: Arc<dyn SummaryGate>` (default `NeverCommitGate`), `with_summary_gate`, and `from_env` wiring in `with_default_providers`. |
| `crates/core/src/routes/agent_run.rs` | `SummarizeConfig`; `Run`/`StoredRun` tax + watermark fields; `eligible_tool_ordinals` + `token_true_ok` pure helpers; `build_summary_request` + `summary_call_from_result` + `dispatch_summary`; `TranscriptSummarizer` trait + `GatewayTranscriptSummarizer`; `run_loop_core` hook param + `LoopOutcome::Paused` fields; `create_run`/`submit_tool_outputs` budget resolution + wiring. |
| `crates/core/src/passes/agentic_budget/substep_cache.rs`, `crates/core/src/passes/agentic_budget/mod.rs` | Sub-lever 4 deferral doc-closure. |

All new summarize code lives in `agent_run.rs` (already the loop's home) — no new module, no `agent_run` split this slice.

---

## Task 1: `ConfigSummaryGate` + expose policy helpers

**Files:**
- Modify: `crates/core/src/passes/agentic_budget/summarize_judge.rs` (add gate after `AlwaysCommitGate` ~`:94`; change `fn resolve_summary_class` `:394` and `fn is_error_blob` `:416` to `pub(crate) fn`)
- Test: same file's `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `summarize_judge.rs`:
```rust
#[test]
fn parse_trusted_classes_trims_and_drops_empties() {
    let got = parse_trusted_classes("inspect_diff, preview_cost ,, ");
    assert!(got.contains("inspect_diff"));
    assert!(got.contains("preview_cost"));
    assert_eq!(got.len(), 2);
    assert!(parse_trusted_classes("").is_empty());
    assert!(parse_trusted_classes("   ").is_empty());
}

#[test]
fn config_gate_empty_trusts_nothing() {
    let gate = ConfigSummaryGate::new(std::collections::HashSet::new());
    assert!(!gate.is_committable("inspect_diff"));
}

#[test]
fn config_gate_trusts_listed_classes_case_sensitively() {
    let gate = ConfigSummaryGate::new(parse_trusted_classes("inspect_diff"));
    assert!(gate.is_committable("inspect_diff"));
    assert!(!gate.is_committable("Inspect_Diff")); // case-sensitive (raw tool name)
    assert!(!gate.is_committable("preview_cost"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p tt-core --lib summarize_judge:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type ConfigSummaryGate` / `cannot find function parse_trusted_classes`.

- [ ] **Step 3: Add `ConfigSummaryGate` + `parse_trusted_classes`**

Insert after `AlwaysCommitGate`'s impl (after `summarize_judge.rs:94`):
```rust
/// A [`SummaryGate`] promoted by operator config: a class is committable iff it
/// is in the env allowlist `TT_SUMMARIZE_TRUSTED_CLASSES` (comma-separated tool
/// names, each TRIMMED, matched CASE-SENSITIVELY against the raw tool name from
/// [`resolve_summary_class`]). An empty/unset allowlist trusts nothing — behaving
/// exactly like [`NeverCommitGate`]. This is slice 2c-1's operator trust surface;
/// slice 2c-2 replaces it with the live [`AdaptiveSummaryGate`].
#[derive(Debug, Clone, Default)]
pub struct ConfigSummaryGate {
    trusted: std::collections::HashSet<String>,
}

/// Parse `TT_SUMMARIZE_TRUSTED_CLASSES` value: comma-separated, each entry
/// trimmed, empty entries dropped. NOT lowercased — matched case-sensitively
/// against the raw tool name.
pub(crate) fn parse_trusted_classes(raw: &str) -> std::collections::HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

impl ConfigSummaryGate {
    /// Build from the `TT_SUMMARIZE_TRUSTED_CLASSES` env var (unset/empty ⇒
    /// trusts nothing).
    #[must_use]
    pub fn from_env() -> Self {
        let trusted = std::env::var("TT_SUMMARIZE_TRUSTED_CLASSES")
            .ok()
            .as_deref()
            .map(parse_trusted_classes)
            .unwrap_or_default();
        Self { trusted }
    }

    /// Build from an explicit class set (tests / embedded use).
    #[must_use]
    pub fn new(trusted: std::collections::HashSet<String>) -> Self {
        Self { trusted }
    }
}

impl SummaryGate for ConfigSummaryGate {
    fn is_committable(&self, class: &str) -> bool {
        self.trusted.contains(class)
    }
}
```

- [ ] **Step 4: Make the two policy helpers `pub(crate)`**

At `summarize_judge.rs:394` change `fn resolve_summary_class(` → `pub(crate) fn resolve_summary_class(`.
At `summarize_judge.rs:416` change (the line is `#[must_use]\nfn is_error_blob(`) `fn is_error_blob(` → `pub(crate) fn is_error_blob(`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p tt-core --lib summarize_judge:: 2>&1 | tail -20`
Expected: PASS (the 3 new tests + the existing `summarize_judge` tests).

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/passes/agentic_budget/summarize_judge.rs
git commit -m "feat(agent-loop 2c-1): ConfigSummaryGate + expose summary policy helpers

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `AppState.summary_gate` (process-wide gate, NeverCommit by default)

**Files:**
- Modify: `crates/core/src/state.rs` (struct field block ending `:299`; `AppState::new` literal `:315-343`; `with_default_providers` `:374-378`; add a `with_summary_gate` builder)
- Test: `state.rs` `#[cfg(test)]` (or inline test mod)

- [ ] **Step 1: Write the failing test**

Add to a `#[cfg(test)] mod tests` in `state.rs` (create one if absent, `use super::*;`):
```rust
#[test]
fn default_summary_gate_never_commits() {
    let st = AppState::new(crate::registry::ProviderRegistry::new());
    assert!(!st.summary_gate.is_committable("inspect_diff"));
}

#[test]
fn with_summary_gate_overrides_default() {
    use crate::passes::agentic_budget::summarize_judge::{AlwaysCommitGate, SummaryGate};
    let st = AppState::new(crate::registry::ProviderRegistry::new())
        .with_summary_gate(std::sync::Arc::new(AlwaysCommitGate));
    assert!(st.summary_gate.is_committable("anything"));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --lib state:: 2>&1 | tail -20`
Expected: FAIL — `no field summary_gate` / `no method with_summary_gate`.

- [ ] **Step 3: Add the field, default, and builder**

In the `AppState` struct (after `pub telemetry_tracker: ...` at `:299`), add:
```rust
    /// Per-class trust gate for the agent-loop summarize lever (slice 2c-1).
    /// `NeverCommitGate` by default ⇒ summarize is a total no-op until an
    /// operator promotes classes via `TT_SUMMARIZE_TRUSTED_CLASSES`.
    pub summary_gate: Arc<dyn crate::passes::agentic_budget::summarize_judge::SummaryGate>,
```
In `AppState::new`'s struct literal (after `telemetry_tracker: None,` at `:342`), add:
```rust
            summary_gate: Arc::new(
                crate::passes::agentic_budget::summarize_judge::NeverCommitGate,
            ),
```
Add the builder (next to the other `with_*` builders, e.g. after `with_telemetry_tracker` `:368`):
```rust
    /// Builder-style attach: set the process-wide summary trust gate (slice
    /// 2c-1). Defaults to `NeverCommitGate`; production wires
    /// `ConfigSummaryGate::from_env()`.
    #[must_use]
    pub fn with_summary_gate(
        mut self,
        gate: Arc<dyn crate::passes::agentic_budget::summarize_judge::SummaryGate>,
    ) -> Self {
        self.summary_gate = gate;
        self
    }
```
Wire production from env in `with_default_providers` (`:374-378`), changing its body to:
```rust
    pub fn with_default_providers() -> Self {
        let mut registry = ProviderRegistry::new();
        register_default_providers(&mut registry);
        Self::new(registry).with_summary_gate(Arc::new(
            crate::passes::agentic_budget::summarize_judge::ConfigSummaryGate::from_env(),
        ))
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-core --lib state:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/state.rs
git commit -m "feat(agent-loop 2c-1): AppState.summary_gate (NeverCommit default, env-promoted in prod)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Run-state fields — `SummarizeConfig`, tax + watermark, serde back-compat

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` — add `SummarizeConfig`; `Run` gains `summarizer_tax_usd`; `StoredRun` gains 3 `#[serde(default)]` fields; `to_run` maps the tax; update all `Run {…}` / `StoredRun {…}` literals (6 / 5 sites).
- Test: `agent_run.rs` tests mod

- [ ] **Step 1: Write the failing tests**

Add to `agent_run.rs`'s `tests` mod:
```rust
#[test]
fn stored_run_deserializes_without_new_fields() {
    // A run persisted BEFORE this deploy has no summarized_upto/summarizer_tax_usd
    // /summarize keys; #[serde(default)] must let it deserialize (resumes unsummarized).
    let json = r#"{
        "id":"00000000-0000-0000-0000-000000000001",
        "org_id":"00000000-0000-0000-0000-000000000002",
        "status":"requires_action","model":"m","messages":[],"tools":[],
        "max_turns":8,"turns_done":1,
        "usage":{"prompt_tokens":0,"completion_tokens":0},
        "pending_tool_calls":[],
        "routing":{"provider_pin":null,"forced_route":null,"tag":null}
    }"#;
    let sr: StoredRun = serde_json::from_str(json).expect("back-compat deserialize");
    assert_eq!(sr.summarized_upto, 0);
    assert_eq!(sr.summarizer_tax_usd, None);
    assert!(sr.summarize.is_none());
}

#[test]
fn to_run_maps_summarizer_tax() {
    let sr = StoredRun {
        id: uuid::Uuid::nil(),
        org_id: uuid::Uuid::nil(),
        status: RunStatus::RequiresAction,
        model: "m".into(),
        messages: vec![],
        tools: vec![],
        max_turns: 8,
        turns_done: 1,
        usage: RunUsage::default(),
        pending_tool_calls: vec![],
        routing: StoredRouting { provider_pin: None, forced_route: None, tag: None },
        summarized_upto: 3,
        summarizer_tax_usd: Some(0.0004),
        summarize: None,
    };
    assert_eq!(sr.to_run().summarizer_tax_usd, Some(0.0004));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -25`
Expected: FAIL — `no field summarized_upto` / `missing field summarizer_tax_usd` (and the existing `StoredRun {…}` test literals won't compile yet — that's expected; Step 3 fixes all literals).

- [ ] **Step 3: Add the type + fields + update all literals**

Add the `SummarizeConfig` type (near `StoredRouting`, after `:331`):
```rust
/// Non-secret summarize policy resolved once from the run's (turn-0) route and
/// persisted with the run so resume drives the same policy. Tiny config only.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SummarizeConfig {
    pub keep_recent_pairs: u32,
    pub clear_at_least_tokens: u32,
}
```
In `Run` (`:60-69`), add after `pub note: Option<String>,`:
```rust
    /// Summarizer measurement tax (USD) accrued across the run's turns (slice
    /// 2c-1). `None` ⇒ unmetered or no summarization. Never folded into served
    /// cost — a measurement tax, like the quality-judge tax.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarizer_tax_usd: Option<f64>,
```
In `StoredRun` (`:336-348`), add after `pub routing: StoredRouting,`:
```rust
    /// Tool-block watermark: count of leading `Message::Tool` blocks already
    /// summarized. Restored on resume so each block is summarized at most once.
    #[serde(default)]
    pub summarized_upto: u32,
    /// Accrued summarizer measurement tax (USD). `#[serde(default)]` for
    /// cross-deploy resume back-compat (a pre-2c-1 record has no key).
    #[serde(default)]
    pub summarizer_tax_usd: Option<f64>,
    /// The run's pinned summarize policy (turn-0 route). `None` ⇒ summarize off.
    #[serde(default)]
    pub summarize: Option<SummarizeConfig>,
```
Update `to_run` (`:358-367`) to map the tax (add the field to its `Run {…}`):
```rust
    pub(crate) fn to_run(&self) -> Run {
        Run {
            id: self.id,
            status: self.status,
            messages: self.messages.clone(),
            turns: self.turns_done,
            usage: self.usage.clone(),
            note: None,
            summarizer_tax_usd: self.summarizer_tax_usd,
        }
    }
```
Now add the new fields to **every other** `Run {…}` and `StoredRun {…}` literal so the crate compiles:
- `Run {…}` literals at `run_loop_core` Terminal `Failed` (`:183`), `Completed` (`:202`), `Incomplete`/max_turns (`:251`), `run_loop` wrapper Incomplete (`:300`), `create_run` no-Redis Incomplete (`:744`): add `summarizer_tax_usd: None,` (Task 6 replaces the loop-core ones with the accumulated tax).
- `StoredRun {…}` literals: `create_run` persist (`:722`) and the 4 test literals (`:1324`, `:1371`, `:1409`, `:1462`): add `summarized_upto: 0, summarizer_tax_usd: None, summarize: None,` (Task 7 replaces the `create_run` one with the real values).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -25`
Expected: PASS (the 2 new tests + all existing agent_run tests still green).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 2c-1): SummarizeConfig + Run/StoredRun tax & watermark fields (serde back-compat)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Pure summarize helpers — eligible set + token-true decision

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` (add the two pure fns near the top-level fns, e.g. after `is_mechanical_continuation` `:112`)
- Test: `agent_run.rs` tests mod

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn eligible_ordinals_keeps_recent_and_respects_watermark() {
    // messages: A(tc) T0 A(tc) T1 A(tc) T2 A(tc) T3  (4 tool blocks)
    let msgs = vec![
        assistant_toolcall("find_route_for"), tool_result("c1"),
        assistant_toolcall("find_route_for"), tool_result("c2"),
        assistant_toolcall("find_route_for"), tool_result("c3"),
        assistant_toolcall("find_route_for"), tool_result("c4"),
    ];
    // keep_recent_pairs=2 → eligible tool blocks are T0,T1 (the 2 oldest); their
    // MESSAGE indices are 1 and 3. watermark=0 → both.
    assert_eq!(eligible_tool_ordinals(&msgs, 0, 2), vec![1, 3]);
    // watermark=1 → T0 already done → only T1 (index 3).
    assert_eq!(eligible_tool_ordinals(&msgs, 1, 2), vec![3]);
    // keep_recent_pairs >= tool count → nothing eligible.
    assert!(eligible_tool_ordinals(&msgs, 0, 4).is_empty());
    assert!(eligible_tool_ordinals(&msgs, 0, 9).is_empty());
}

#[test]
fn token_true_ok_requires_real_reduction() {
    // openai/gpt-4o-mini tokenizer; a clearly shorter summary reduces tokens.
    let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu";
    let short = "alpha";
    assert!(token_true_ok("openai", "gpt-4o-mini", long, short, 0));
    // a non-reduction (same text) must be rejected even at floor 0 (>=1 required).
    assert!(!token_true_ok("openai", "gpt-4o-mini", long, long, 0));
    // a reduction below the clear_at_least_tokens floor is rejected.
    assert!(!token_true_ok("openai", "gpt-4o-mini", long, short, 9999));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function eligible_tool_ordinals` / `token_true_ok`.

- [ ] **Step 3: Implement the helpers**

Add after `is_mechanical_continuation` (`:112`):
```rust
/// Message indices of the tool-result blocks eligible for summarization: the
/// tool blocks OLDER than the last `keep_recent_pairs` (caveat C1 — recent tail
/// verbatim) AND beyond the `summarized_upto` watermark (count of leading tool
/// blocks already summarized, so each block is processed at most once).
fn eligible_tool_ordinals(
    messages: &[Message],
    summarized_upto: u32,
    keep_recent_pairs: u32,
) -> Vec<usize> {
    let tool_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::Tool { .. }))
        .map(|(i, _)| i)
        .collect();
    let n = tool_idxs.len();
    let cutoff = n.saturating_sub(keep_recent_pairs as usize); // ordinals [0, cutoff) are old
    let start = (summarized_upto as usize).min(cutoff);
    tool_idxs[start..cutoff].to_vec()
}

/// Token-true gate: a summary commits only when it reduces the served-model
/// token count by at least `clear_at_least_tokens.max(1)` (≥1 ⇒ never a
/// token-neutral/-inflating commit; the floor is the R1 cache-thrash guard).
/// Mirrors the pipeline gate's discipline (`passes/mod.rs`): even on the
/// `Confidence::Low` (`chars/4`) fallback a non-reduction is rejected.
fn token_true_ok(
    provider_id: &str,
    model: &str,
    original: &str,
    summary: &str,
    clear_at_least_tokens: u32,
) -> bool {
    let orig = tt_tokenize::estimate_input_tokens_for_model(provider_id, model, original).tokens;
    let new = tt_tokenize::estimate_input_tokens_for_model(provider_id, model, summary).tokens;
    orig.saturating_sub(new) >= clear_at_least_tokens.max(1)
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -20`
Expected: PASS.
(If `tt_tokenize` is not yet a direct dependency of `tt-core`, add `tt-tokenize.workspace = true` to `crates/core/Cargo.toml` `[dependencies]` — verify with `grep tt-tokenize crates/core/Cargo.toml`; `passes/mod.rs:283` already uses it, so it is present.)

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 2c-1): pure summarize helpers (eligible-set + token-true gate)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Summarizer dispatch — request builder, result mapping, `dispatch_summary`

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` (add the request builder + the `MeasuredDispatch`→`SummarizeCall` mapping + the async dispatch)
- Test: `agent_run.rs` tests mod

The async `dispatch_summary` is provider-bound (integration-covered). Its two PURE pieces are unit-tested here.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn build_summary_request_shapes_a_cheap_call() {
    let req = build_summary_request("inspect_diff", "big tool output", "gpt-4o-mini");
    assert_eq!(req.model, "gpt-4o-mini");
    assert!(!req.stream);
    // a system instruction + the original content as a user message
    assert_eq!(req.messages.len(), 2);
    assert!(matches!(req.messages[0], Message::System { .. }));
    assert!(matches!(req.messages[1], Message::User { .. }));
}

#[test]
fn summary_call_maps_dispatch_and_fails_open_on_err() {
    use crate::measurement::MeasuredDispatch;
    // Err → fail open (no summary, no cost).
    let call = summary_call_from_result(Err("deadline exceeded".into()));
    assert!(call.summary.is_none());
    assert!(call.cost_usd.is_none());

    // Ok with text → summary + cost passed through. NOTE: ChatCompletionResponse
    // does NOT derive Default (Usage does) — construct all 6 fields explicitly.
    let resp = tt_shared::ChatCompletionResponse {
        id: String::new(),
        object: String::new(),
        created: 0,
        model: "gpt-4o-mini".into(),
        choices: vec![tt_shared::messages::Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text("short".into())),
                tool_calls: vec![],
                name: None,
            },
            finish_reason: Some("stop".into()),
        }],
        usage: tt_shared::Usage::default(),
    };
    let call = summary_call_from_result(Ok(MeasuredDispatch { response: resp, cost_usd: Some(0.0001) }));
    assert_eq!(call.summary.as_deref(), Some("short"));
    assert_eq!(call.cost_usd, Some(0.0001));
}
```
> Verified shapes (no Default on `ChatCompletionResponse`): `ChatCompletionResponse { id, object, created, model, choices, usage }` (`tt_shared`), `Choice { index, message, finish_reason }` (`tt_shared::messages`), `Usage` derives `Default` (`tt_shared::Usage`), `SummarizeCall { pub summary, pub cost_usd }`. If `tt_shared::Usage` isn't re-exported at the crate root, use the full path (`grep -rn "pub use.*Usage\|pub struct Usage" crates/shared/src/lib.rs crates/shared/src/usage.rs`).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function build_summary_request` / `summary_call_from_result`.

- [ ] **Step 3: Implement the builder, mapping, and dispatch**

Add near the other agent-run helpers (e.g. after Task 4's helpers). Import `SummarizeCall`:
```rust
use crate::passes::agentic_budget::summarize_judge::SummarizeCall;

/// Build the cheap-model summarize request for one tool-result blob.
fn build_summary_request(class: &str, original: &str, model: &str) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message::System {
                content: MessageContent::Text(format!(
                    "Summarize this `{class}` tool result. Preserve every fact a later \
                     step might need; drop only redundancy and formatting. Output only \
                     the summary, no preamble."
                )),
            },
            Message::User {
                content: MessageContent::Text(original.to_string()),
                name: None,
            },
        ],
        stream: false,
        ..Default::default()
    }
}

/// Map a measured dispatch result into a `SummarizeCall` (fail-open): `Err` →
/// decline (`summary: None`); `Ok` → the first choice's assistant text (empty/
/// missing ⇒ decline) + the dispatch's metered cost.
fn summary_call_from_result(
    res: Result<crate::measurement::MeasuredDispatch, String>,
) -> SummarizeCall {
    match res {
        Err(_) => SummarizeCall { summary: None, cost_usd: None },
        Ok(d) => {
            let cost_usd = d.cost_usd;
            let text = d
                .response
                .choices
                .into_iter()
                .next()
                .and_then(|c| match c.message {
                    Message::Assistant { content: Some(MessageContent::Text(t)), .. } => Some(t),
                    _ => None,
                })
                .filter(|t| !t.trim().is_empty());
            SummarizeCall { summary: text, cost_usd }
        }
    }
}

/// Dispatch one cheap-model summarize call on the SUMMARIZER model's own
/// provider+creds (NOT the turn's served provider), bounded by `deadline`.
/// Fail-open: any resolution/dispatch failure ⇒ a declined `SummarizeCall`.
async fn dispatch_summary(
    state: &AppState,
    org_id: Uuid,
    raw_bearer: &str,
    base_ctx: &RequestContext,
    summarizer_model: &str,
    class: &str,
    original: &str,
    deadline: std::time::Duration,
) -> SummarizeCall {
    let Some(provider) = state.registry.resolve(summarizer_model) else {
        return SummarizeCall { summary: None, cost_usd: None };
    };
    // The summarizer model may live on a different provider than the run's
    // source; resolve ITS OWN credential (fail closed on a verified-org miss →
    // declined call), mirroring the live judge (`chat.rs:5433-5453`).
    let ctx = match chat::resolve_credentials_for(state, org_id, provider.id(), raw_bearer, true).await {
        Some(credentials) => RequestContext { credentials, ..base_ctx.clone() },
        None => return SummarizeCall { summary: None, cost_usd: None },
    };
    let req = build_summary_request(class, original, summarizer_model);
    let res = crate::measurement::measured_single_dispatch(&provider, req, &ctx, deadline).await;
    summary_call_from_result(res)
}
```
> Verify `chat::resolve_credentials_for` arity/visibility: `grep -n "fn resolve_credentials_for" crates/core/src/routes/chat.rs` — expected `pub(crate) async fn resolve_credentials_for(state: &AppState, org_id: Uuid, provider_id: &str, raw_bearer: &str, <bool>) -> Option<ProviderCredentials>` (5 args, used at `chat.rs:5437`). Adjust the last bool arg's meaning if the signature differs.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -20`
Expected: PASS (the 2 pure tests; `dispatch_summary` compiles, exercised in Task 7 + integration).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 2c-1): summarizer dispatch (request builder, result mapping, dispatch_summary)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `TranscriptSummarizer` hook + `run_loop_core` threading

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` — add the trait; add 2 params to `run_loop_core`; add 2 fields to `LoopOutcome::Paused`; set `summarizer_tax_usd` on Terminal `Run`s; update all 6 call sites; update the `run_loop` wrapper's Paused mapping.
- Test: `agent_run.rs` tests mod (a stub `TranscriptSummarizer`)

- [ ] **Step 1: Write the failing test**

```rust
/// A stub summarizer that records calls, mutates the transcript (truncates the
/// first eligible tool block), advances the watermark, and reports a fixed tax.
struct StubSummarizer {
    calls: std::sync::Mutex<u32>,
}
#[async_trait]
impl TranscriptSummarizer for StubSummarizer {
    async fn summarize_before_turn(
        &self,
        messages: &mut Vec<Message>,
        summarized_upto: &mut u32,
    ) -> Option<f64> {
        *self.calls.lock().unwrap() += 1;
        // mark progress: advance the watermark past all-but-1 tool blocks
        let tools = messages.iter().filter(|m| matches!(m, Message::Tool { .. })).count() as u32;
        *summarized_upto = tools.saturating_sub(1);
        Some(0.0002)
    }
}

#[tokio::test]
async fn loop_calls_summarizer_each_turn_and_accrues_tax() {
    let stub = Stub { script: std::sync::Mutex::new(vec![
        assistant_toolcall("find_route_for"),
        assistant_final(),
    ]) };
    let summ = StubSummarizer { calls: std::sync::Mutex::new(0) };
    let out = run_loop_core(
        &stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8, 0, 0,
        RunUsage::default(), Some(&summ),
    ).await;
    match out {
        LoopOutcome::Terminal(run) => {
            assert_eq!(run.status, RunStatus::Completed);
            // hook ran before each of the 2 completion turns
            assert_eq!(*summ.calls.lock().unwrap(), 2);
            // tax accrued (0.0002 * 2), metered
            assert_eq!(run.summarizer_tax_usd, Some(0.0004));
        }
        _ => panic!("expected Terminal Completed"),
    }
}

#[tokio::test]
async fn loop_with_no_summarizer_is_unchanged() {
    // None hook ⇒ no tax, behavior identical to 1b.
    let stub = Stub { script: std::sync::Mutex::new(vec![assistant_final()]) };
    let out = run_loop_core(
        &stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8, 0, 0,
        RunUsage::default(), None,
    ).await;
    match out {
        LoopOutcome::Terminal(run) => assert_eq!(run.summarizer_tax_usd, None),
        _ => panic!("expected Terminal"),
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -25`
Expected: FAIL — `run_loop_core` arity mismatch / `cannot find trait TranscriptSummarizer`.

- [ ] **Step 3: Add the trait + thread the hook**

Add the trait (near `TurnCompleter` `:74`):
```rust
/// Optional per-turn transcript summarizer injected into [`run_loop_core`].
/// `summarize_before_turn` may rewrite aging tool-result blocks in `messages`
/// in place and advance `summarized_upto`; it returns this call's metered tax
/// (`Some(0.0)` when nothing was summarized; `None` only when a dispatch was
/// billed but unpriced). Fail-open: it never errors the run. The pure loop
/// tests + the 1a `run_loop` wrapper pass `None` (no summarization, no provider).
#[async_trait]
pub(crate) trait TranscriptSummarizer: Send + Sync {
    async fn summarize_before_turn(
        &self,
        messages: &mut Vec<Message>,
        summarized_upto: &mut u32,
    ) -> Option<f64>;
}
```
Add two fields to `LoopOutcome::Paused` (`:129-134`):
```rust
    Paused {
        messages: Vec<Message>,
        turns_done: u32,
        usage: RunUsage,
        pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
        summarized_upto: u32,
        summarizer_tax_usd: Option<f64>,
    },
```
Change `run_loop_core`'s signature (`:155-164`) — add `summarized_upto` after `turns_done`, and `summarizer` last:
```rust
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_loop_core(
    completer: &dyn TurnCompleter,
    id: uuid::Uuid,
    model: String,
    mut messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    turns_done: u32,
    mut summarized_upto: u32,
    mut usage: RunUsage,
    summarizer: Option<&dyn TranscriptSummarizer>,
) -> LoopOutcome {
```
In the body: add a tax accumulator before the loop (`use crate::passes::agentic_budget::summarize_judge::sum_metered;` at the top of the fn or module):
```rust
    let max_turns = max_turns.clamp(1, MAX_MAX_TURNS);
    let mut turn = turns_done;
    let mut summarizer_tax: Option<f64> = summarizer.map(|_| 0.0); // None ⇒ no summarizer ⇒ tax stays None (1b byte-identical); Some(0.0) when present
    while turn < max_turns {
        // Summarize aging tool blocks BEFORE building this turn's request, so
        // the shrunk transcript is sent (and persisted) from here on.
        if let Some(s) = summarizer {
            let tax = s.summarize_before_turn(&mut messages, &mut summarized_upto).await;
            summarizer_tax = sum_metered(summarizer_tax, tax);
        }
        let req = ChatCompletionRequest { /* unchanged */ };
        // ... unchanged through the assistant push ...
```
Set `summarizer_tax_usd: summarizer_tax` on the three Terminal `Run {…}` built in `run_loop_core` (`Failed` `:183`, `Completed` `:202`, max-turns `Incomplete` `:251`) — replace the `summarizer_tax_usd: None` placeholders from Task 3 with `summarizer_tax_usd: summarizer_tax,`. Add `summarized_upto, summarizer_tax_usd: summarizer_tax,` to the `LoopOutcome::Paused { … }` return (`:242-247`).

Update the `run_loop` wrapper (`:267-310`): pass the new args to `run_loop_core` (`…, 0 /*turns_done*/, 0 /*summarized_upto*/, RunUsage::default(), None /*summarizer*/`); destructure the extra Paused fields (`summarized_upto: _, summarizer_tax_usd,`) and set `summarizer_tax_usd` on the Incomplete `Run` it builds (`:300`).

- [ ] **Step 4: Update the remaining call sites**

`create_run` (`:700`): args become `…, 0, 0, RunUsage::default(), summarizer_ref` — for now pass `None` (Task 7 builds the real summarizer). Destructure the 2 new Paused fields in the `LoopOutcome::Paused` arm.
`submit_tool_outputs` (`:905`): args become `…, stored.turns_done, stored.summarized_upto, stored.usage.clone(), None` (Task 7 swaps in the rebuilt summarizer). Destructure the 2 new Paused fields.
The 3 unit-test call sites (`:1178`, `:1217`, `:1247`): add `0 /*summarized_upto*/` after `turns_done` and `None /*summarizer*/` as the final arg; in `core_*` tests that `match` on `Paused { … }`, add `summarized_upto: _, summarizer_tax_usd: _,` (or `..`) to the pattern.

> Tip: after editing, `cargo build -p tt-core --tests 2>&1 | grep -E "error|run_loop_core"` to find any missed call site / pattern.

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -25`
Expected: PASS — the 2 new tests + all existing agent_run/loop tests (default `None` path byte-identical).

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 2c-1): TranscriptSummarizer hook threaded through run_loop_core

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `GatewayTranscriptSummarizer` + create/resume wiring (budget resolution)

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` — `GatewayTranscriptSummarizer` (composes Tasks 1/4/5 + the gate + commit); `resolve_summarize_config` (apply_routing once); `create_run` + `submit_tool_outputs` build and pass the summarizer + persist `summarize`/`summarized_upto`/`summarizer_tax_usd`.
- Test: `agent_run.rs` tests mod (the gate-decision seam, provider-free)

- [ ] **Step 1: Write the failing test (the commit-decision seam, no provider)**

```rust
#[test]
fn summarize_commit_decision_gates_and_token_checks() {
    // The production hook's per-block decision is: trusted class AND not an
    // error blob AND token-true reduction. Assert that composed predicate
    // directly (provider-free), mirroring how 1a/1b assert provider-bound
    // wiring at the pure seam.
    use crate::passes::agentic_budget::summarize_judge::{ConfigSummaryGate, SummaryGate, is_error_blob};
    let gate = ConfigSummaryGate::new(
        crate::passes::agentic_budget::summarize_judge::parse_trusted_classes("inspect_diff"),
    );
    // trusted, non-error, real reduction → commit
    assert!(gate.is_committable("inspect_diff"));
    assert!(!is_error_blob("a long verbose tool result with lots of words"));
    assert!(token_true_ok("openai", "gpt-4o-mini",
        "a long verbose tool result with lots of words to remove", "short", 0));
    // untrusted class → no commit
    assert!(!gate.is_committable("write_file"));
    // error blob → never summarized even if trusted
    assert!(is_error_blob(r#"{"error":"boom"}"#));
}
```

- [ ] **Step 2: Run to verify it fails / compiles against new visibility**

Run: `cargo test -p tt-core --lib agent_run::tests::summarize_commit_decision 2>&1 | tail -20`
Expected: FAIL to compile only if `parse_trusted_classes`/`is_error_blob` aren't `pub(crate)` — they were exposed in Task 1, so this should PASS once the imports resolve. (If it already passes, that confirms the seam; proceed.)

- [ ] **Step 3: Implement `GatewayTranscriptSummarizer` + `resolve_summarize_config`**

```rust
/// Production transcript summarizer: for each eligible aging tool block, if its
/// class is gate-trusted and it is not an error blob, dispatch a cheap-model
/// summary and commit it when the token-true gate passes. Advances the
/// watermark unconditionally (a rejected/declined block is dispatched — and
/// taxed — at most once, never retried). Fail-open throughout.
struct GatewayTranscriptSummarizer<'a> {
    state: &'a AppState,
    org_id: Uuid,
    raw_bearer: String,
    base_ctx: RequestContext,        // minimal ctx (creds replaced per dispatch)
    gate: std::sync::Arc<dyn crate::passes::agentic_budget::summarize_judge::SummaryGate>,
    cfg: SummarizeConfig,
    base_model: String,
    base_provider_id: Option<String>,
    summarizer_model: String,
    deadline: std::time::Duration,
}

#[async_trait]
impl TranscriptSummarizer for GatewayTranscriptSummarizer<'_> {
    async fn summarize_before_turn(
        &self,
        messages: &mut Vec<Message>,
        summarized_upto: &mut u32,
    ) -> Option<f64> {
        use crate::passes::agentic_budget::summarize_judge::{is_error_blob, resolve_summary_class};
        let Some(provider_id) = self.base_provider_id.as_deref() else { return Some(0.0); };
        let tool_count = messages.iter().filter(|m| matches!(m, Message::Tool { .. })).count() as u32;
        let eligible = eligible_tool_ordinals(messages, *summarized_upto, self.cfg.keep_recent_pairs);
        let mut tax: Option<f64> = Some(0.0);
        for idx in eligible {
            let class = resolve_summary_class(messages, idx);
            if !self.gate.is_committable(&class) {
                continue;
            }
            // read the original text (skip non-text / error blobs)
            let original = match &messages[idx] {
                Message::Tool { content: MessageContent::Text(t), .. } if !is_error_blob(t) => t.clone(),
                _ => continue,
            };
            let call = dispatch_summary(
                self.state, self.org_id, &self.raw_bearer, &self.base_ctx,
                &self.summarizer_model, &class, &original, self.deadline,
            ).await;
            tax = crate::passes::agentic_budget::summarize_judge::sum_metered(tax, call.cost_usd);
            let Some(summary) = call.summary else { continue };
            if !token_true_ok(provider_id, &self.base_model, &original, &summary, self.cfg.clear_at_least_tokens) {
                continue;
            }
            if let Message::Tool { content, .. } = &mut messages[idx] {
                *content = MessageContent::Text(summary);
            }
        }
        // Each eligible block was dispatched at most once → advance past them all.
        *summarized_upto = tool_count.saturating_sub(self.cfg.keep_recent_pairs);
        tax
    }
}

/// Resolve the run's summarize policy ONCE from the turn-0 route (pinned for the
/// run). Builds a minimal `RequestContext` (apply_routing + the route engine
/// read only org_id + tag) and reads `agentic_budget`. `None` ⇒ summarize off
/// (no route / nil-org / `elide_stale_tools` unset).
async fn resolve_summarize_config(
    state: &AppState,
    identity: &RunIdentity,
    model: &str,
    messages: &[Message],
) -> Option<SummarizeConfig> {
    let ctx = RequestContext {
        trace_id: identity.trace_id,
        org_id: identity.org_id,
        api_key_id: identity.api_key_id,
        credentials: ProviderCredentials {
            api_key: SecretString::new(String::new()),
            base_url: None,
            extra_headers: Vec::new(),
        },
        tag: identity.tag.clone(),
        deadline: identity.request_timeout,
    };
    let mut req_clone = ChatCompletionRequest {
        model: model.to_string(),
        messages: messages.to_vec(),
        ..Default::default()
    };
    let route_match = chat::apply_routing(state, &ctx, &mut req_clone, identity.forced_route.as_deref())
        .await
        .ok()
        .flatten()?;
    let ab = route_match.agentic_budget?;
    if !ab.elide_stale_tools {
        return None;
    }
    Some(SummarizeConfig {
        keep_recent_pairs: ab.keep_recent_pairs,
        clear_at_least_tokens: ab.clear_at_least_tokens,
    })
}

/// The cheap summarizer model: `TT_SUMMARIZER_MODEL` (this slice's new env) or
/// the already-resolved judge model (default `gpt-4o-mini`).
fn summarizer_model(state: &AppState) -> String {
    std::env::var("TT_SUMMARIZER_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| state.judge_config.judge_model.clone())
}
```

- [ ] **Step 4: Wire `create_run` (build the summarizer, persist the config)**

In `create_run` (`:675-757`), AFTER `let identity = RunIdentity::from_request(...)` and BEFORE `identity` is moved into the completer, resolve the policy and build the summarizer (note: `resolve_summarize_config` borrows `&req.messages`, so do it before `req.messages` is moved into `run_loop_core`):
```rust
    let summarize_cfg = resolve_summarize_config(&state, &identity, &req.model, &req.messages).await;
    let base_provider_id = state.registry.resolve(&req.model).map(|p| p.id().to_string());
    let summarizer_model = summarizer_model(&state);
    let base_ctx = RequestContext {
        trace_id: identity.trace_id,
        org_id: identity.org_id,
        api_key_id: identity.api_key_id,
        credentials: ProviderCredentials {
            api_key: SecretString::new(identity.raw_bearer.clone()),
            base_url: None,
            extra_headers: Vec::new(),
        },
        tag: identity.tag.clone(),
        deadline: identity.request_timeout,
    };
    let summarizer_obj = summarize_cfg.clone().map(|cfg| GatewayTranscriptSummarizer {
        state: &state,
        org_id: identity.org_id,
        raw_bearer: identity.raw_bearer.clone(),
        base_ctx,
        gate: state.summary_gate.clone(),
        cfg,
        base_model: req.model.clone(),
        base_provider_id,
        summarizer_model,
        deadline: state.judge_config.baseline_timeout,
    });
```
Then (keeping the existing `org_id`/`routing`/`model`/`tools`/`max_turns`/`completer`/`id` setup) pass the summarizer into `run_loop_core`:
```rust
    let summ_ref: Option<&dyn TranscriptSummarizer> =
        summarizer_obj.as_ref().map(|s| s as &dyn TranscriptSummarizer);
    match run_loop_core(
        &completer, id, model.clone(), req.messages, tools.clone(),
        max_turns, 0, 0, RunUsage::default(), summ_ref,
    ).await {
        LoopOutcome::Terminal(run) => Ok(Json(run)),
        LoopOutcome::Paused { messages, turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd } =>
            match state.l1.as_ref() {
                Some(l1) => {
                    let stored = StoredRun {
                        id, org_id, status: RunStatus::RequiresAction, model,
                        messages, tools, max_turns, turns_done, usage, pending_tool_calls, routing,
                        summarized_upto,
                        summarizer_tax_usd,
                        summarize: summarize_cfg,
                    };
                    store_run(l1.cache.as_ref(), &stored).await?;
                    Ok(Json(stored.to_run()))
                }
                None => Ok(Json(Run {
                    id, status: RunStatus::Incomplete, messages, turns: turns_done, usage,
                    note: Some(format!("client tool '{}' requires Redis to pause/resume (none configured)",
                        pending_tool_calls.first().map(|tc| tc.function.name.clone()).unwrap_or_default())),
                    summarizer_tax_usd,
                })),
            },
    }
```

- [ ] **Step 5: Wire `submit_tool_outputs` (rebuild the summarizer from the persisted config)**

In `submit_tool_outputs` (`:825-944`), after rebuilding `identity` and BEFORE `run_loop_core`, rebuild the summarizer from `stored.summarize`:
```rust
    let summ_obj = stored.summarize.clone().map(|cfg| {
        let base_ctx = RequestContext {
            trace_id: identity.trace_id, org_id: identity.org_id, api_key_id: identity.api_key_id,
            credentials: ProviderCredentials {
                api_key: SecretString::new(identity.raw_bearer.clone()), base_url: None, extra_headers: Vec::new(),
            },
            tag: identity.tag.clone(), deadline: identity.request_timeout,
        };
        GatewayTranscriptSummarizer {
            state: &state, org_id: identity.org_id, raw_bearer: identity.raw_bearer.clone(), base_ctx,
            gate: state.summary_gate.clone(), cfg,
            base_model: stored.model.clone(),
            base_provider_id: state.registry.resolve(&stored.model).map(|p| p.id().to_string()),
            summarizer_model: summarizer_model(&state),
            deadline: state.judge_config.baseline_timeout,
        }
    });
    // (the completer is built AFTER this — `identity` is moved into it there)
```
> Borrow note: the completer takes `identity` by value. Build `summ_obj` (which clones the identity fields it needs) BEFORE `let completer = GatewayCompleter { state: &state, identity };`. Then pass both into `run_loop_core`.
Update the `run_loop_core` call + both outcome arms:
```rust
    let summ_ref: Option<&dyn TranscriptSummarizer> = summ_obj.as_ref().map(|s| s as &dyn TranscriptSummarizer);
    let outcome = run_loop_core(
        &completer, stored.id, stored.model.clone(),
        std::mem::take(&mut stored.messages), stored.tools.clone(),
        stored.max_turns, stored.turns_done, stored.summarized_upto, stored.usage.clone(), summ_ref,
    ).await;
    // The summarizer tax is CUMULATIVE across pause/resume segments: each
    // resume's run_loop_core starts its own accumulator at Some(0.0), so add it
    // to the tax already persisted from prior segments. `sum_metered` is in
    // scope (imported in Task 6).
    use crate::passes::agentic_budget::summarize_judge::sum_metered;
    match outcome {
        LoopOutcome::Terminal(mut run) => {
            let cumulative = sum_metered(stored.summarizer_tax_usd, run.summarizer_tax_usd);
            stored.status = run.status;
            stored.messages = run.messages.clone();
            stored.turns_done = run.turns;
            stored.usage = run.usage.clone();
            stored.summarizer_tax_usd = cumulative;
            stored.pending_tool_calls = Vec::new();
            store_run(l1.cache.as_ref(), &stored).await?;
            // Return the cumulative tax to the client (total across all segments).
            run.summarizer_tax_usd = cumulative;
            Ok(Json(run))
        }
        LoopOutcome::Paused { messages, turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd } => {
            stored.status = RunStatus::RequiresAction;
            stored.messages = messages;
            stored.turns_done = turns_done;
            stored.usage = usage;
            stored.summarized_upto = summarized_upto;
            stored.summarizer_tax_usd = sum_metered(stored.summarizer_tax_usd, summarizer_tax_usd);
            stored.pending_tool_calls = pending_tool_calls;
            store_run(l1.cache.as_ref(), &stored).await?;
            Ok(Json(stored.to_run()))
        }
    }
```
> **Cumulative-tax note (Task 6 review I-1/M-4):** the watermark (`summarized_upto`) is REPLACED with the resume segment's value (it's an absolute high-water mark, already restored into `run_loop_core` from `stored.summarized_upto`), while the tax is ACCUMULATED via `sum_metered` (each segment's `run_loop_core` accumulator starts fresh at `Some(0.0)`). `create_run` is the first segment, so its persisted `summarized_upto`/`summarizer_tax_usd` come straight from its Paused outcome (no prior segment to add).

- [ ] **Step 6: Run the full agent_run + build the whole crate**

Run: `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -25` → Expected: PASS.
Run: `cargo build -p tt-core --tests 2>&1 | grep -E "^error" || echo OK` → Expected: `OK`.

- [ ] **Step 7: Commit**

```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 2c-1): GatewayTranscriptSummarizer + create/resume wiring (turn-0 budget)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Doc-closures (Sub-lever 4 deferral + summarize token-gate promise) + full gate

**Files:**
- Modify: `crates/core/src/passes/agentic_budget/substep_cache.rs` (module doc), `crates/core/src/passes/agentic_budget/mod.rs` (Sub-lever 4 comment), `crates/core/src/passes/agentic_budget/summarize_judge.rs` (the stale "pipeline token-true gate" promise in the module/`SummarizeStep`/`SummaryOutcome` docs).

- [ ] **Step 1: Sub-lever 4 deferral note**

At the top of `substep_cache.rs`'s module doc, append a paragraph:
```rust
//! **Slice 2c-1 status — intentionally deferred (not wired into the agent loop).**
//! Sub-lever 4 stays a tested building block but is NOT served in the loop: the
//! only read-only/cacheable tools today are the 4 near-free gateway tools
//! (`find_route_for`/`preview_cost`/`inspect_diff`/`batch_savings`), so caching
//! their results saves ~nothing while adding an embedding tax + a persistent
//! pgvector store. It earns its keep only once an *expensive* read-only gateway
//! tool (e.g. retrieval) exists. Mirrors the COST-3(U) per-request-proxy
//! doc-closure. (Slice 2c-1 wired the *summarize* lever instead.)
```
In `agentic_budget/mod.rs`, find the Sub-lever 4 comment (the `semantic_substep_cache` / `mark_substep_cacheable` region, ~`:151`/`:213`) and add a one-line cross-reference: `// NOTE: the substep-cache SERVE path is intentionally deferred — see substep_cache.rs module doc (slice 2c-1).`

- [ ] **Step 2: Update the stale summarize token-gate promise**

In `summarize_judge.rs`, the module/`SummarizeStep`/`SummaryOutcome` docs say the byte heuristic is "pending the deferred wiring that wraps this step inside the pipeline's token-true gate." Append to the `SummarizeStep` doc (the "Byte-length commit is a placeholder" paragraph, ~`:269`):
```rust
/// **Slice 2c-1 update:** the server-side agent loop (`routes::agent_run`)
/// applies its OWN loop-level token-true gate around an out-of-band summarizer
/// dispatch (it operates on the loop's flat `Vec<Message>`, not a `VolatileTail`,
/// since old tool blocks are never in a cache-stable prefix). This `apply`/
/// `VolatileTail` byte path is unchanged and remains for any future pipeline use.
```

- [ ] **Step 3: Run the full behavior-preservation gate**

Run: `cargo test -p tt-core --lib --tests 2>&1 | tail -15`
Expected: PASS — all lib + test-target tests green (the loop's default-off path is byte-identical to 1b; only the new 2c-1 tests are added).
Run: `cargo clippy -p tt-core --all-targets 2>&1 | tail -15`
Expected: no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/core/src/passes/agentic_budget/substep_cache.rs \
        crates/core/src/passes/agentic_budget/mod.rs \
        crates/core/src/passes/agentic_budget/summarize_judge.rs
git commit -m "docs(agent-loop 2c-1): Sub-lever 4 deferral + summarize token-gate doc-closures

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Notes for the implementer

- **Default-off is the invariant.** With no `elide_stale_tools` on the route, `resolve_summarize_config` returns `None` ⇒ `summarizer_obj` is `None` ⇒ `run_loop_core` gets `None` ⇒ byte-identical to 1b. With the route opted in but `TT_SUMMARIZE_TRUSTED_CLASSES` unset, the gate is `NeverCommitGate` (via `with_default_providers`) ⇒ every `is_committable` is `false` ⇒ dispatch still runs per eligible block? NO — re-read Task 7 Step 3: the gate check (`if !self.gate.is_committable(&class) { continue; }`) happens BEFORE `dispatch_summary`, so an untrusted class is skipped with zero dispatch/tax. Verify this ordering — it is load-bearing for "gate Never ⇒ no summarizer calls / no tax".
- **Watermark advance is unconditional** (a token-rejected/declined block is never retried → dispatched at most once). This is the deliberate no-retry / no-re-tax tradeoff from the spec.
- **The summarizer's own tokens never inflate `RunUsage`** (only the USD tax is surfaced, on `summarizer_tax_usd`). Do not add the summarize dispatch's prompt/completion tokens to `usage`.
- **Provider-bound paths** (`dispatch_summary`, the full `GatewayTranscriptSummarizer` run, `create_run`/`submit` end-to-end) are integration-covered, like 1a/1b — the unit tests assert the pure helpers + the gate/token decision seam; the behavior-preservation gate is the regression guard.
- **CI:** public `cargo test (workspace)` is disk-flaky (`No space left on device` linking test binaries) → `gh run rerun <run-id> --failed`. No DB gate needed (no DB/pgvector in this slice).
