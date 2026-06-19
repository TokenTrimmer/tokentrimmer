# Agent-loop slice 2c-2 (live judge degrade-ratchet — operator-opens, judge-shuts) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the existing blind paired judge to the server-side summarize path as a safety ratchet — a sampled committed summary is judged (original-vs-summary recall-of-baseline); a windowed pass-rate dip shuts the class (cooldown half-open recovery), in-process.

**Architecture:** The operator allowlist (2c-1's `TT_SUMMARIZE_TRUSTED_CLASSES`) still OPENS a class; a new `RatchetSummaryGate` adds a per-class judge-fed shut-state (`committable = allowlisted && !shut`). After the loop commits a summary (in-place `messages[idx]` mutation), it samples (~`JudgeConfig.sample_rate`) and spawns a detached judge (`judge_paired`, zero added latency) whose verdict feeds `gate.record_summary_verdict` via a new default-no-op trait method. Default-off (empty allowlist ⇒ no commits ⇒ no judge spawns ⇒ byte-identical to 2c-1). Mirrors 2a (operator configures, judge self-reverts).

**Tech Stack:** Rust, `crates/core` (tt-core). `summarize_judge.rs` (the gate + trait method), `agent_run.rs` (sampling + the detached judge spawn, wired into 2c-1's `GatewayTranscriptSummarizer::summarize_before_turn`), `state.rs` (prod wiring), `agentic_budget/mod.rs` (re-export). Reuses `quality_sample::{judge_paired, GatewayLlmJudge, should_sample, ab_order_for, verdict_str}` + `chat::resolve_credentials_for`.

**Spec:** `docs/superpowers/specs/2026-06-19-agent-loop-slice2c2-design.md` (read it; this plan implements it).

**Behavior-preservation gate (run after each compiling task + as the final check):**
```
cargo test -p tt-core --lib --tests
```
Default-off (empty `TT_SUMMARIZE_TRUSTED_CLASSES` / `NeverCommitGate`) must stay byte-identical to slice 2c-1/1b. **`cargo fmt -p tt-core -- --check` before every push** (2c-1 lesson: public CI gates rustfmt). Clippy: `cargo clippy -p tt-core --all-targets`. No DB gate (no DB/pgvector in this slice).

---

## File Structure

| File | Change |
|---|---|
| `crates/core/src/passes/agentic_budget/summarize_judge.rs` | Add `record_summary_verdict` default-no-op to the `SummaryGate` trait; add `RatchetSummaryGate` + `RatchetConfig` + `from_env`. |
| `crates/core/src/passes/agentic_budget/mod.rs` | `pub use` `RatchetSummaryGate` (+ `RatchetConfig`) alongside the sibling gates. |
| `crates/core/src/routes/agent_run.rs` | `sample_key` + `latest_user_text` pure helpers; `maybe_spawn_summary_judge` (detached judge) on `GatewayTranscriptSummarizer`; wire it into the `summarize_before_turn` commit site. |
| `crates/core/src/state.rs` | `with_default_providers` builds `RatchetSummaryGate::from_env()` (replacing `ConfigSummaryGate`). |

---

## Task 1: `SummaryGate::record_summary_verdict` (default no-op)

**Files:**
- Modify: `crates/core/src/passes/agentic_budget/summarize_judge.rs` (the `SummaryGate` trait, `:64-69`)
- Test: same file's `tests` mod

`summarize_judge.rs` already has `use tt_plan_core::JudgeVerdict;` (`:50`) and `pub trait SummaryGate: Send + Sync { fn is_committable(&self, class: &str) -> bool; }` (`:64-69`).

- [ ] **Step 1: Write the failing test** — add to the `tests` mod:
```rust
#[test]
fn record_summary_verdict_default_is_noop_and_object_safe() {
    use tt_plan_core::JudgeVerdict;
    // Object-safe: callable on a dyn gate; the default impl ignores the verdict.
    let gate: std::sync::Arc<dyn SummaryGate> = std::sync::Arc::new(NeverCommitGate);
    gate.record_summary_verdict("inspect_diff", JudgeVerdict::Degraded); // no-op, must not panic
    assert!(!gate.is_committable("inspect_diff")); // unchanged by the no-op
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-core --lib summarize_judge:: 2>&1 | tail -15` — expect FAIL (`no method named record_summary_verdict`).

- [ ] **Step 3: Add the default method** to the `SummaryGate` trait (after `fn is_committable`):
```rust
    /// Feed one blind-paired judge verdict for a committed summary of `class`
    /// (the detached judge write-side, slice 2c-2). Default no-op — only the
    /// ratchet gate acts on it; `NeverCommitGate`/`ConfigSummaryGate`/
    /// `AdaptiveSummaryGate` ignore it. (`AdaptiveSummaryGate` has its own
    /// inherent `record_verdict` under a different name — no collision.)
    fn record_summary_verdict(&self, _class: &str, _verdict: JudgeVerdict) {}
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-core --lib summarize_judge:: 2>&1 | tail -15` — expect PASS (the new test + all existing summarize_judge tests).

- [ ] **Step 5: Commit**
```bash
git add crates/core/src/passes/agentic_budget/summarize_judge.rs
git commit -m "feat(agent-loop 2c-2): SummaryGate::record_summary_verdict default no-op

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `RatchetSummaryGate` + `RatchetConfig` + `from_env`

**Files:**
- Modify: `crates/core/src/passes/agentic_budget/summarize_judge.rs` (add after `AdaptiveSummaryGate`; reuse `parse_trusted_classes`)
- Modify: `crates/core/src/passes/agentic_budget/mod.rs` (`pub use` list, `:53-56`)
- Test: `summarize_judge.rs` `tests` mod

Model the `&self`-mutates-`Mutex` idiom on `AdaptiveSummaryGate` (which already does `self.tallies.lock().unwrap_or_else(|e| e.into_inner())`). `JudgeVerdict` is `Copy`.

- [ ] **Step 1: Write the failing tests** — add to the `tests` mod:
```rust
use std::time::Duration;

fn ratchet(trusted: &[&str], cooldown_secs: u64) -> RatchetSummaryGate {
    let set = trusted.iter().map(|s| s.to_string()).collect();
    RatchetSummaryGate::new(set, RatchetConfig {
        floor: 0.90, window: 20, min_samples: 5, cooldown: Duration::from_secs(cooldown_secs),
    })
}

#[test]
fn ratchet_allowlist_gates_commit() {
    let g = ratchet(&["inspect_diff"], 3600);
    assert!(g.is_committable("inspect_diff"));   // allowlisted, no verdicts → open
    assert!(!g.is_committable("write_file"));    // not allowlisted → closed
}

#[test]
fn ratchet_shuts_below_floor_after_min_samples() {
    use tt_plan_core::JudgeVerdict::*;
    let g = ratchet(&["inspect_diff"], 3600);
    // 4 verdicts (3 Degraded) — below min_samples(5) ⇒ NOT shut yet (robust to noise)
    for v in [Degraded, Degraded, Degraded, Acceptable] { g.record_summary_verdict("inspect_diff", v); }
    assert!(g.is_committable("inspect_diff"));
    // a 5th (Degraded): window=5, pass-rate=1/5=0.20 < 0.90 ⇒ shut
    g.record_summary_verdict("inspect_diff", Degraded);
    assert!(!g.is_committable("inspect_diff"));
}

#[test]
fn ratchet_clean_window_stays_open() {
    use tt_plan_core::JudgeVerdict::*;
    let g = ratchet(&["inspect_diff"], 3600);
    for _ in 0..10 { g.record_summary_verdict("inspect_diff", Acceptable); }
    assert!(g.is_committable("inspect_diff")); // pass-rate 1.0 ≥ floor
}

#[test]
fn ratchet_unclear_excluded() {
    use tt_plan_core::JudgeVerdict::*;
    let g = ratchet(&["inspect_diff"], 3600);
    for _ in 0..10 { g.record_summary_verdict("inspect_diff", Unclear); } // no valence
    assert!(g.is_committable("inspect_diff")); // window has 0 acc/0 deg ⇒ never shut
}

#[test]
fn ratchet_cooldown_half_open_clears_window() {
    use tt_plan_core::JudgeVerdict::*;
    // cooldown 0 ⇒ a shut class is immediately half-open on the next read, window cleared.
    let g = ratchet(&["inspect_diff"], 0);
    for _ in 0..5 { g.record_summary_verdict("inspect_diff", Degraded); } // shuts (rate 0 < floor)
    // cooldown elapsed (0s) ⇒ half-open: committable again + window cleared (fresh trial)
    assert!(g.is_committable("inspect_diff"));
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p tt-core --lib summarize_judge:: 2>&1 | tail -20` — expect FAIL (`cannot find type RatchetSummaryGate`/`RatchetConfig`).

- [ ] **Step 3: Implement the gate** — add after `AdaptiveSummaryGate`'s impl. Add imports at the top of the file if absent: `use std::collections::VecDeque;` and `use std::time::{Duration, Instant};` (`HashMap`/`HashSet`/`Mutex` are already imported for `AdaptiveSummaryGate`/`ConfigSummaryGate` — verify and add only what's missing).
```rust
/// Tunables for the [`RatchetSummaryGate`] (slice 2c-2). Defaults mirror
/// `route_autopause` (floor 0.90) + a conservative window.
#[derive(Debug, Clone)]
pub struct RatchetConfig {
    /// Windowed pass-rate floor; below it (over >= `min_samples`) the class shuts.
    pub floor: f64,
    /// Sliding verdict-window length (per class).
    pub window: usize,
    /// Minimum judged verdicts in the window before the floor can shut a class.
    pub min_samples: usize,
    /// How long a shut class stays shut before a half-open re-trial.
    pub cooldown: Duration,
}

impl Default for RatchetConfig {
    fn default() -> Self {
        Self { floor: 0.90, window: 20, min_samples: 5, cooldown: Duration::from_secs(3600) }
    }
}

#[derive(Default)]
struct ClassRatchet {
    /// Recent verdicts: `true` = Acceptable, `false` = Degraded (Unclear excluded).
    window: VecDeque<bool>,
    /// `Some(t)` while the class is shut; cleared on the half-open re-trial.
    shut_at: Option<Instant>,
}

/// A [`SummaryGate`] that the OPERATOR opens (allowlist) and the JUDGE shuts
/// (slice 2c-2): `committable = allowlisted && !shut`. The judge feeds
/// [`record_summary_verdict`]; a windowed pass-rate dip below `floor` (over
/// >= `min_samples`) shuts the class for `cooldown`, after which a half-open
/// re-trial clears the window and re-opens it (recovers on fresh real verdicts).
/// In-process per replica (resets on restart) — like `AdaptiveSummaryGate`'s
/// tally. Empty allowlist trusts nothing (== `NeverCommitGate`).
pub struct RatchetSummaryGate {
    trusted: HashSet<String>,
    cfg: RatchetConfig,
    classes: Mutex<HashMap<String, ClassRatchet>>,
}

impl RatchetSummaryGate {
    #[must_use]
    pub fn new(trusted: HashSet<String>, cfg: RatchetConfig) -> Self {
        Self { trusted, cfg, classes: Mutex::new(HashMap::new()) }
    }

    /// Build from `TT_SUMMARIZE_TRUSTED_CLASSES` (the 2c-1 allowlist) +
    /// `TT_SUMMARIZE_JUDGE_{FLOOR,WINDOW,MIN_SAMPLES,COOLDOWN_SECS}` (defaults
    /// from [`RatchetConfig::default`]).
    #[must_use]
    pub fn from_env() -> Self {
        let trusted = std::env::var("TT_SUMMARIZE_TRUSTED_CLASSES")
            .ok()
            .as_deref()
            .map(parse_trusted_classes)
            .unwrap_or_default();
        let d = RatchetConfig::default();
        let cfg = RatchetConfig {
            floor: env_parse("TT_SUMMARIZE_JUDGE_FLOOR", d.floor),
            window: env_parse("TT_SUMMARIZE_JUDGE_WINDOW", d.window),
            min_samples: env_parse("TT_SUMMARIZE_JUDGE_MIN_SAMPLES", d.min_samples),
            cooldown: Duration::from_secs(env_parse(
                "TT_SUMMARIZE_JUDGE_COOLDOWN_SECS",
                d.cooldown.as_secs(),
            )),
        };
        Self::new(trusted, cfg)
    }
}

/// Parse an env var to `T`, falling back to `default` on unset/empty/malformed.
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

impl SummaryGate for RatchetSummaryGate {
    fn is_committable(&self, class: &str) -> bool {
        if !self.trusted.contains(class) {
            return false;
        }
        let mut g = self.classes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cr) = g.get_mut(class) {
            if let Some(shut_at) = cr.shut_at {
                if shut_at.elapsed() >= self.cfg.cooldown {
                    // Half-open re-trial: clear the window (so stale Degradeds
                    // don't instantly re-shut) and re-open.
                    cr.window.clear();
                    cr.shut_at = None;
                } else {
                    return false; // shut within cooldown
                }
            }
        }
        true
    }

    fn record_summary_verdict(&self, class: &str, verdict: JudgeVerdict) {
        let acceptable = match verdict {
            JudgeVerdict::Acceptable => true,
            JudgeVerdict::Degraded => false,
            JudgeVerdict::Unclear => return, // no valence — excluded
        };
        let mut g = self.classes.lock().unwrap_or_else(|e| e.into_inner());
        let cr = g.entry(class.to_string()).or_default();
        cr.window.push_back(acceptable);
        while cr.window.len() > self.cfg.window {
            cr.window.pop_front();
        }
        if cr.window.len() >= self.cfg.min_samples {
            let acc = cr.window.iter().filter(|&&b| b).count();
            let rate = acc as f64 / cr.window.len() as f64;
            if rate < self.cfg.floor {
                cr.shut_at = Some(Instant::now());
            }
        }
    }
}
```

- [ ] **Step 4: Re-export from `mod.rs`** — add `RatchetConfig, RatchetSummaryGate` to the `pub use summarize_judge::{ ... }` list (`mod.rs:53-56`), next to the other gates.

- [ ] **Step 5: Run to verify it passes** — `cargo test -p tt-core --lib summarize_judge:: 2>&1 | tail -20` — expect PASS (5 new tests + existing). `cargo clippy -p tt-core --lib 2>&1 | tail -8` — clean.

- [ ] **Step 6: Commit**
```bash
git add crates/core/src/passes/agentic_budget/summarize_judge.rs crates/core/src/passes/agentic_budget/mod.rs
git commit -m "feat(agent-loop 2c-2): RatchetSummaryGate (operator allowlist + judge degrade-ratchet, cooldown half-open)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `sample_key` + `latest_user_text` pure helpers

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` (add near the other top-level helpers, e.g. after `token_true_ok`)
- Test: `agent_run.rs` `tests` mod

`agent_run.rs` already imports `use tt_shared::{messages::{ChatCompletionRequest, Message, MessageContent}, ...}` and `use uuid::Uuid;`.

- [ ] **Step 1: Write the failing tests**
```rust
#[test]
fn sample_key_is_deterministic_and_spreads() {
    let t = uuid::Uuid::from_u128(42);
    assert_eq!(sample_key(t, "c1"), sample_key(t, "c1"));      // deterministic
    assert_ne!(sample_key(t, "c1"), sample_key(t, "c2"));      // distinct tool_call_ids differ
    assert_ne!(sample_key(uuid::Uuid::from_u128(1), "c1"), sample_key(uuid::Uuid::from_u128(2), "c1"));
}

#[test]
fn latest_user_text_takes_the_most_recent_user_message() {
    let msgs = vec![
        Message::User { content: MessageContent::Text("first".into()), name: None },
        Message::Assistant { content: Some(MessageContent::Text("a".into())), tool_calls: vec![], name: None },
        Message::User { content: MessageContent::Text("second".into()), name: None },
        tool_result("c1"),
    ];
    assert_eq!(latest_user_text(&msgs), "second");
    assert_eq!(latest_user_text(&[]), ""); // no user message → empty
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -15` — expect FAIL (`cannot find function sample_key`/`latest_user_text`).

- [ ] **Step 3: Implement**
```rust
/// Deterministic per-edit sampling key: a `Uuid` digest of `(trace_id, tool_call_id)`,
/// so `should_sample`/`ab_order_for` (which hash an opaque `Uuid`) give a stable,
/// uniform per-edit decision. No RNG.
fn sample_key(trace_id: Uuid, tool_call_id: &str) -> Uuid {
    use std::hash::{Hash, Hasher};
    let mut hi = std::collections::hash_map::DefaultHasher::new();
    trace_id.hash(&mut hi);
    tool_call_id.hash(&mut hi);
    let mut lo = std::collections::hash_map::DefaultHasher::new();
    tool_call_id.hash(&mut lo);
    trace_id.hash(&mut lo);
    lo.write_u8(0x9e); // distinct salt so hi != lo
    Uuid::from_u64_pair(hi.finish(), lo.finish())
}

/// The run's task context for the summary judge's `input`: the most-recent
/// `Message::User` text, or `""` (the judge still compares A/B info-preservation).
fn latest_user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User { content: MessageContent::Text(t), .. } => Some(t.clone()),
            _ => None,
        })
        .unwrap_or_default()
}
```
(If `Uuid::from_u64_pair` isn't available in the pinned `uuid` version, use `Uuid::from_u128(((hi.finish() as u128) << 64) | lo.finish() as u128)` — verify with `grep -n '^uuid' crates/core/Cargo.toml` / the uuid docs.)

- [ ] **Step 4: Run to verify they pass** — `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -15` — expect PASS.

- [ ] **Step 5: Commit**
```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 2c-2): sample_key + latest_user_text helpers for the summary judge

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: detached summary judge + wire into the commit site

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` — add `maybe_spawn_summary_judge` on `GatewayTranscriptSummarizer`; call it from `summarize_before_turn` right after the in-place commit.
- Test: `agent_run.rs` `tests` mod (default-off no-spawn + the gate-feedback contract)

The provider-bound dispatch is integration-covered (like 2c-1's `dispatch_summary`); the gate-feedback (`record_summary_verdict` → shut) is unit-covered in Task 2. This task covers the **default-off no-spawn** invariant and the wiring compiles + the existing loop tests stay green.

- [ ] **Step 1: Write the failing test** (default-off: a `NeverCommitGate` summarizer commits nothing, so no judge is ever reached — assert via the existing seam style that an empty/closed gate yields no committed edits):
```rust
#[tokio::test]
async fn summarize_judge_not_reached_when_gate_closed() {
    // A closed gate (NeverCommit) ⇒ summarize_before_turn commits nothing ⇒ the
    // judge sampling is never reached. Drive the loop with summarizer=None-equivalent:
    // here assert the commit-gate short-circuit by constructing the eligible path
    // with a NeverCommit gate via the pure decision (mirrors 2c-1's default-off seam).
    use crate::passes::agentic_budget::summarize_judge::{NeverCommitGate, SummaryGate};
    let gate = NeverCommitGate;
    // The loop guard is `if !gate.is_committable(&class) { continue; }` BEFORE any
    // dispatch/commit/judge — so a closed gate never commits and never judges.
    assert!(!gate.is_committable("inspect_diff"));
}
```
> NOTE: `maybe_spawn_summary_judge` itself is provider-bound (resolves a judge provider + dispatches), so it is exercised by integration, not a unit test — consistent with 2c-1's `dispatch_summary`. This task's unit assertion is the default-off short-circuit; the gate-feedback (`record_summary_verdict` shuts a class) is Task 2's tests. The real guarantee is the behavior-preservation gate in Step 5.

- [ ] **Step 2: Run to verify it fails/compiles** — `cargo test -p tt-core --lib agent_run::tests::summarize_judge_not_reached_when_gate_closed 2>&1 | tail -10` — it should PASS immediately (it asserts existing behavior); its purpose is to lock the invariant. Proceed.

- [ ] **Step 3: Add the imports + the spawn helper.** At the top of `agent_run.rs`, add `use crate::quality_sample::{self, GatewayLlmJudge};` (verify `quality_sample` is reachable as `crate::quality_sample` — it is, `pub mod` in `crates/core/src/lib.rs`). Add the method in the `impl GatewayTranscriptSummarizer` block (or a new `impl` block on it):
```rust
impl GatewayTranscriptSummarizer<'_> {
    /// Sample (~`judge_config.sample_rate`) a freshly-committed summary and, if
    /// sampled, spawn a DETACHED blind judge (zero added latency) comparing the
    /// original vs the summary (recall-of-baseline). Its verdict feeds the gate's
    /// ratchet for FUTURE turns/runs. Fail-open: any resolve/dispatch failure ⇒
    /// no verdict recorded (a flaky judge must never shut a class). Owned clones
    /// only — never captures `&self`/`&self.state` (the spawn is 'static + Send).
    fn maybe_spawn_summary_judge(
        &self,
        tool_call_id: &str,
        input: &str,
        class: &str,
        original: String,
        summary: String,
    ) {
        let key = sample_key(self.base_ctx.trace_id, tool_call_id);
        if !quality_sample::should_sample(key, self.state.judge_config.sample_rate) {
            return;
        }
        let state = self.state.clone(); // AppState: Clone — owned, not the borrow
        let gate = self.gate.clone();
        let org_id = self.org_id;
        let raw_bearer = self.raw_bearer.clone();
        let base_ctx = self.base_ctx.clone();
        let class = class.to_string();
        let input = input.to_string();
        tokio::spawn(async move {
            let Some(provider) = state.registry.resolve(&state.judge_config.judge_model) else {
                return;
            };
            let Some(creds) =
                chat::resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, true).await
            else {
                return;
            };
            let judge_ctx = RequestContext { credentials: creds, ..base_ctx };
            let judge = GatewayLlmJudge::new(
                provider,
                state.judge_config.judge_model.clone(),
                judge_ctx,
            )
            .with_call_timeout(state.judge_config.baseline_timeout);
            // summary is the OPTIMIZED arg + the matching `order` ⇒ the returned
            // verdict reads as the SUMMARY's recall-of-baseline verdict.
            match quality_sample::judge_paired(
                &judge,
                &input,
                &original,
                &summary,
                quality_sample::ab_order_for(key),
                false,
            )
            .await
            {
                Ok(outcome) => {
                    tracing::info!(
                        target: "tokentrimmer.summary_judge",
                        class = %class,
                        verdict = quality_sample::verdict_str(outcome.verdict),
                        cost_usd = ?outcome.judge_cost_usd,
                        "summary judge verdict"
                    );
                    gate.record_summary_verdict(&class, outcome.verdict);
                }
                Err(_failure) => {
                    tracing::debug!(
                        target: "tokentrimmer.summary_judge",
                        class = %class,
                        "summary judge failed; no verdict recorded (fail-open)"
                    );
                }
            }
        });
    }
}
```

- [ ] **Step 4: Wire it into the commit site** in `summarize_before_turn` (`agent_run.rs:1296-1307`). Replace the token-true-gate-then-commit block:
```rust
            if !token_true_ok(
                provider_id,
                &self.base_model,
                &original,
                &summary,
                self.cfg.clear_at_least_tokens,
            ) {
                continue;
            }
            if let Message::Tool { content, .. } = &mut messages[idx] {
                *content = MessageContent::Text(summary);
            }
```
with (capture `tool_call_id` + `input` BEFORE the in-place overwrite; clone `summary` for the commit so the owned `summary` goes to the judge):
```rust
            if !token_true_ok(
                provider_id,
                &self.base_model,
                &original,
                &summary,
                self.cfg.clear_at_least_tokens,
            ) {
                continue;
            }
            // Capture the judge inputs BEFORE the in-place overwrite of messages[idx].
            let tool_call_id = match &messages[idx] {
                Message::Tool { tool_call_id, .. } => tool_call_id.clone(),
                _ => String::new(),
            };
            let input = latest_user_text(messages);
            if let Message::Tool { content, .. } = &mut messages[idx] {
                *content = MessageContent::Text(summary.clone());
            }
            // Sample + detached-judge the committed summary (feeds the ratchet for
            // future turns/runs). No-op unless this commit is sampled.
            self.maybe_spawn_summary_judge(&tool_call_id, &input, &class, original, summary);
```
(`original` and `summary` are moved into the call; they are not used afterward in the loop body. `class` is borrowed.)

- [ ] **Step 5: Run the gate + clippy** — `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -12` (PASS — all existing loop tests + the new assert; the `None`/closed-gate paths never spawn). `cargo build -p tt-core --tests 2>&1 | grep -E "^error" || echo OK`. `cargo clippy -p tt-core --lib --tests 2>&1 | tail -10` (clean — no `'static`/borrow errors on the spawn).

- [ ] **Step 6: Commit**
```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 2c-2): detached summary judge wired into the commit site

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: prod wiring — `with_default_providers` → `RatchetSummaryGate`

**Files:**
- Modify: `crates/core/src/state.rs` (`with_default_providers`, `:391-397`)
- Test: `state.rs` `tests` mod

- [ ] **Step 1: Write the failing test** — add to `state.rs` `tests`:
```rust
#[test]
fn with_default_providers_uses_ratchet_gate() {
    // An allowlisted, never-judged class is committable (operator opens);
    // a non-allowlisted class is not. (Set the allowlist via env for the test,
    // then clear it.) Use a UNIQUE class to avoid cross-test env races.
    // Simpler: assert the gate type behaves as a ratchet by constructing one
    // directly (env-free) — the wiring swap itself is covered by compilation +
    // the default-gate test below.
    use crate::passes::agentic_budget::summarize_judge::{RatchetSummaryGate, RatchetConfig, SummaryGate};
    let g = RatchetSummaryGate::new(
        ["inspect_diff".to_string()].into_iter().collect(),
        RatchetConfig::default(),
    );
    assert!(g.is_committable("inspect_diff"));
    assert!(!g.is_committable("write_file"));
}
```
(The existing `default_summary_gate_never_commits` test already guards `AppState::new` ⇒ `NeverCommitGate`; that must stay green. Avoid env-var manipulation in tests — it races across the test binary's threads.)

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-core --lib state:: 2>&1 | tail -12` — expect FAIL (`cannot find type RatchetSummaryGate` import) until Step 3 makes it nameable (it's re-exported from `mod.rs` in Task 2, so this is mostly an import check).

- [ ] **Step 3: Swap the prod gate.** In `with_default_providers` (`state.rs:391-397`), change `.with_summary_gate(Arc::new(crate::passes::agentic_budget::summarize_judge::ConfigSummaryGate::from_env()))` to:
```rust
        .with_summary_gate(Arc::new(
            crate::passes::agentic_budget::summarize_judge::RatchetSummaryGate::from_env(),
        ))
```
(Leave `AppState::new`'s `NeverCommitGate` default unchanged. `ConfigSummaryGate` stays a public type — still used by its own tests + the re-export — just no longer the prod gate.)

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-core --lib state:: 2>&1 | tail -12` — expect PASS (new test + `default_summary_gate_never_commits` + `with_summary_gate_overrides_default` still green).

- [ ] **Step 5: Commit**
```bash
git add crates/core/src/state.rs
git commit -m "feat(agent-loop 2c-2): prod summary_gate = RatchetSummaryGate::from_env (replaces ConfigSummaryGate)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: full behavior-preservation gate + fmt/clippy

**Files:** none (verification only; fix any fmt/clippy fallout).

- [ ] **Step 1: Format check + fix** — `cargo fmt -p tt-core` then `cargo fmt -p tt-core -- --check` (expect clean). **This is load-bearing: public CI gates rustfmt (the 2c-1 fmt miss).** Confirm `cargo fmt` only touched the slice's files (`git diff --stat`).

- [ ] **Step 2: Full gate** — `cargo test -p tt-core --lib --tests 2>&1 | tail -15` — expect ALL green (lib + test targets). Default-off (empty allowlist / `NeverCommitGate`) is byte-identical to 2c-1; record the pass/fail/ignored counts.

- [ ] **Step 3: Clippy** — `cargo clippy -p tt-core --all-targets 2>&1 | tail -15` — expect no warnings on tt-core. (NOTE: do NOT run `cargo test --all-targets` — it builds criterion benches that hang ~36min; `--lib --tests` is the test RUN.)

- [ ] **Step 4: Commit (if fmt changed anything)**
```bash
git add -A && git commit -m "style(agent-loop 2c-2): rustfmt the slice (CI fmt gate)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>" || echo "nothing to commit"
```

---

## Notes for the implementer

- **Default-off is the invariant.** Empty `TT_SUMMARIZE_TRUSTED_CLASSES` ⇒ `RatchetSummaryGate.is_committable` always false ⇒ the loop `continue`s before dispatch/commit/judge ⇒ byte-identical to 2c-1. `NeverCommitGate` stays the `AppState::new` default (tests).
- **The judge only SHUTS** (operator opens). A judge dispatch error/timeout records NO verdict (fail-open) — a flaky judge must never shut a class. The verdict affects FUTURE turns/runs (detached, post-commit), like 2a's autopause.
- **`'static` spawn:** the detached judge captures only owned clones (`state = self.state.clone()`, `gate = self.gate.clone()`, owned Strings) — never `&self`/`&self.state`. `AppState`/`RequestContext` are `Clone`; `state.registry.resolve` returns an owned `Arc<dyn Provider>`.
- **`order` correctness:** pass `summary` as `judge_paired`'s `optimized_answer` with the matching `ab_order_for(key)` as `order`, so the returned `verdict` is the summary's recall-of-baseline verdict (no separate `map_summary_verdict` call).
- **CI:** public `cargo test (workspace)` is disk-flaky (`No space left on device` linking test binaries) → `gh run rerun <run-id> --failed`. ALWAYS `cargo fmt -p tt-core -- --check` before push.
