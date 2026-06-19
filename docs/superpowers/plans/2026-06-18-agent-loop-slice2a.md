# Slice 2a: down-route mechanical sub-step turns Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When an agent-loop turn is a "mechanical" read-only continuation, down-route it to the route's `route_mechanical_to` model — opt-in, pause-respecting, and quality-sampled so the existing `route_autopause` self-reverts on regression.

**Architecture:** `run_loop_core` computes `is_mechanical` (the prior assistant turn called only read-only tools) and threads it through `TurnCompleter::complete` → `GatewayCompleter::complete` → `chat::prepare`. `prepare` gains an `is_mechanical` param; on a mechanical turn it down-routes `req.model` to `route_mechanical_to` (when set + not auto-paused), keeping `matched_route_id` so the existing paired-quality judge + `route_autopause` treat it as a routed serving. The chat handler passes `is_mechanical=false` (behavior-preserving).

**Tech Stack:** Rust, `tt_shared::messages`, `substep_cache::classify_substep`, the existing routing/quality/auto_pause machinery.

**Spec:** `docs/superpowers/specs/2026-06-18-agent-loop-slice2a-design.md`.

**Gate:** `TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p tt-core --lib --tests` = **753 passed, 0 failed** baseline (a Postgres gate is up). Use `--lib --tests` (NOT `--all-targets` for the test run — benches hang). The 2 `middleware/trace.rs` doctests are pre-existing-broken; ignore.

**Verified call sites:**
- `crates/core/src/routes/agent_run.rs`: `TurnCompleter::complete(&self, req: ChatCompletionRequest) -> Result<(Message,RunUsage),ApiError>` (:76); `run_loop_core` calls `completer.complete(req).await` (:140); `impl TurnCompleter for GatewayCompleter` `complete` (:514-515) builds the per-turn request + calls `chat::prepare(...)` (:563) then `chat::complete_once` (:584); test `impl TurnCompleter for Stub` `complete` (:915-916). `substep_cache::{classify_substep, SubstepKind}` in scope (the loop already uses `is_gateway_tool`).
- `crates/core/src/routes/chat.rs`: `pub(crate) async fn prepare(...)` (:2218); the handler's `prepare(` call (:2106); inside `prepare`: `let route_match = apply_routing(state, ctx, req, forced_route.as_deref()).await?; let matched_route_id = ...; let route_paused = route_match.as_ref().is_some_and(|m| m.paused); let route_matched_name = ...; let route_agentic_budget = route_match.as_ref().and_then(|m| m.agentic_budget.clone()); let mut model_was_rewritten = matched_route_id.is_some() && !route_paused;` then later the provider (re)resolve for `req.model`. `tt_routing::AgenticBudget.route_mechanical_to: Option<String>`.
- `Message::Assistant { tool_calls: Vec<ToolCall>, .. }`, `Message::Tool { tool_call_id, .. }` (`tt_shared::messages`).

---

### Task 1: `is_mechanical_continuation` detection helper

**Files:** Modify `crates/core/src/routes/agent_run.rs` (add the pure helper + tests).

- [ ] **Step 1: Add the helper.**
```rust
/// A turn is "mechanical" when the model is about to digest ONLY read-only tool
/// output: scanning back over the trailing `Message::Tool` results to the
/// assistant turn that produced them, that assistant turn called >=1 tool and
/// EVERY tool_call is read-only (`classify_substep == ReadOnly`). Conservative:
/// any client/mutating tool in that turn — or no preceding assistant tool turn
/// (e.g. the first turn, or a plain user/assistant message) — => not mechanical.
fn is_mechanical_continuation(messages: &[tt_shared::messages::Message]) -> bool {
    use tt_shared::messages::Message;
    use crate::passes::agentic_budget::substep_cache::{classify_substep, SubstepKind};
    // Walk back over trailing Tool results.
    let mut i = messages.len();
    let mut saw_tool_result = false;
    while i > 0 {
        match &messages[i - 1] {
            Message::Tool { .. } => { saw_tool_result = true; i -= 1; }
            Message::Assistant { tool_calls, .. } if saw_tool_result => {
                // The assistant turn whose tool results we just appended.
                return !tool_calls.is_empty()
                    && tool_calls.iter().all(|tc| classify_substep(&tc.function.name) == SubstepKind::ReadOnly);
            }
            _ => return false, // a non-Tool, non-producing message (or no tool results) => not mechanical
        }
    }
    false
}
```
(NOTE: confirm `classify_substep`/`SubstepKind` are reachable at that path — they're `pub`/`pub(crate)` in `crates/core/src/passes/agentic_budget/substep_cache.rs`; if the module isn't `pub(crate)` from `passes`, adjust the `use` to the correct path. Confirm `ToolCallFunction.name` is `tc.function.name`.)

- [ ] **Step 2: Detection tests** (pure, reuse the existing `assistant_toolcall`/`assistant_two_toolcalls`/`assistant_final` helpers + a `Message::Tool` builder). Add to `#[cfg(test)] mod tests`:
```rust
    fn tool_result(id: &str) -> Message {
        Message::Tool { content: tt_shared::messages::MessageContent::Text("r".into()), tool_call_id: id.into() }
    }
    #[test]
    fn mechanical_after_readonly_tool_continuation() {
        // assistant called a read-only gateway tool, its result appended → next turn is mechanical
        let msgs = vec![assistant_toolcall("find_route_for"), tool_result("c1")];
        assert!(is_mechanical_continuation(&msgs));
    }
    #[test]
    fn not_mechanical_after_client_tool() {
        let msgs = vec![assistant_toolcall("write_file"), tool_result("c1")];
        assert!(!is_mechanical_continuation(&msgs));
    }
    #[test]
    fn not_mechanical_mixed_prior_turn() {
        // a turn with a read-only AND a client tool → not mechanical
        let msgs = vec![assistant_two_toolcalls("find_route_for", "write_file"), tool_result("c1"), tool_result("c2")];
        assert!(!is_mechanical_continuation(&msgs));
    }
    #[test]
    fn not_mechanical_first_turn() {
        assert!(!is_mechanical_continuation(&[]));
        assert!(!is_mechanical_continuation(&[Message::User { content: tt_shared::messages::MessageContent::Text("hi".into()), name: None }]));
    }
    #[test]
    fn not_mechanical_after_final_answer() {
        // last message is an assistant final (no tool results trailing) → not mechanical
        let msgs = vec![assistant_final()];
        assert!(!is_mechanical_continuation(&msgs));
    }
```
(Confirm the `Message::User` variant fields match `tt_shared::messages` — `{content, name}`; adjust if the real shape differs.)

- [ ] **Step 3: Run + commit.**
```bash
cargo test -p tt-core --lib agent_run
cargo clippy -p tt-core --lib -- -D warnings && cargo fmt -p tt-core
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(core): is_mechanical_continuation detection for the agent loop (slice 2a)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Thread `is_mechanical` + the `prepare` down-route block

**Files:** Modify `crates/core/src/routes/agent_run.rs` (trait + run_loop_core + GatewayCompleter + Stub) and `crates/core/src/routes/chat.rs` (`prepare` param + block + handler call).

- [ ] **Step 1: `TurnCompleter::complete` gains the flag** (`agent_run.rs:76`):
```rust
#[async_trait]
pub trait TurnCompleter: Send + Sync {
    async fn complete(&self, req: ChatCompletionRequest, is_mechanical: bool) -> Result<(Message, RunUsage), ApiError>;
}
```

- [ ] **Step 2: `run_loop_core` computes + passes it** (at the `completer.complete` call, ~:140):
```rust
        let is_mechanical = is_mechanical_continuation(&messages);
        let (assistant, turn_usage) = match completer.complete(req, is_mechanical).await {
            Ok(x) => x,
            Err(e) => { /* unchanged Failed return */ }
        };
```
(`messages` is the transcript BEFORE pushing this turn's request — confirm `is_mechanical` is computed from the messages that will be sent, i.e. before the assistant response is pushed; the prior turn's tool results are already in `messages` at this point.)

- [ ] **Step 3: `GatewayCompleter::complete` forwards it** (:515): add `is_mechanical: bool` to the signature and pass it as the new trailing arg to `chat::prepare(...)` (:563).

- [ ] **Step 4: The test `Stub` gains the param** (:916):
```rust
        async fn complete(&self, _req: ChatCompletionRequest, _is_mechanical: bool) -> Result<(Message, RunUsage), ApiError> {
            // unchanged body
        }
```
If any test wants to assert the flag, add a recording stub variant; otherwise ignore it. ALSO add a recording stub + test (Step 7) that asserts `run_loop_core` passes `is_mechanical=true` exactly on a mechanical turn.

- [ ] **Step 5: `prepare` gains `is_mechanical` + the down-route block** (`chat.rs:2218`). Add a trailing `is_mechanical: bool` param. After the route capture lines (`route_agentic_budget`/`route_paused`/`matched_route_id`/`model_was_rewritten`) and BEFORE the provider (re)resolve for `req.model`, insert:
```rust
    // Sub-lever 3 (agent-loop only): down-route a mechanical sub-step turn to
    // the route's route_mechanical_to model, IF the route opted in AND is not
    // auto-paused. Keeping matched_route_id set => the existing paired-quality
    // judge + route_autopause treat it as a routed serving and self-revert.
    if is_mechanical && !route_paused {
        if let Some(target) = route_agentic_budget.as_ref().and_then(|ab| ab.route_mechanical_to.clone()) {
            if target != req.model {
                if state.registry.resolve(&target).is_some() {
                    req.model = target;
                    model_was_rewritten = true; // baseline priced vs the original model
                    // provider is (re)resolved below for the new req.model
                } else {
                    warnings.push(format!("mechanical_route_unresolved:{target}"));
                }
            }
        }
    }
```
(NOTE: confirm `warnings` is the mutable `Vec<String>` in scope in `prepare` (it is — the pre-dispatch warnings vec). Confirm `state.registry.resolve(&str) -> Option<...>`. Place this so the existing provider (re)resolve runs AFTER it for the possibly-new `req.model` — verify the resolve isn't already captured into a local above this point; if the provider was resolved before route capture, ensure the down-route happens before the FINAL provider used by `Prepared`/dispatch, or re-resolve here. The cleanest: this block sits immediately after the route capture and before the provider-pin/agentic-effects section; the model-keyed provider resolution for `req.model` must reflect the override.)

- [ ] **Step 6: The handler passes `false`** (`chat.rs:2106`): update the handler's `prepare(` call to pass `false` as the trailing `is_mechanical` arg. The streaming/non-streaming branch is unaffected (the block is a no-op when `is_mechanical=false`).

- [ ] **Step 7: Down-route + default-off tests** (`agent_run.rs`). Add a recording stub to assert the flag threading:
```rust
    struct RecordingStub { mech: std::sync::Mutex<Vec<bool>>, script: std::sync::Mutex<Vec<Message>> }
    #[async_trait]
    impl TurnCompleter for RecordingStub {
        async fn complete(&self, _req: ChatCompletionRequest, is_mechanical: bool) -> Result<(Message, RunUsage), ApiError> {
            self.mech.lock().unwrap().push(is_mechanical);
            Ok((self.script.lock().unwrap().remove(0), RunUsage { prompt_tokens: 1, completion_tokens: 1 }))
        }
    }
    #[tokio::test]
    async fn loop_passes_is_mechanical_on_readonly_continuation() {
        // turn1: assistant calls a read-only gateway tool → loop executes it, appends result;
        // turn2: the digest turn → is_mechanical should be true. turn2 returns a final answer.
        let stub = RecordingStub {
            mech: std::sync::Mutex::new(vec![]),
            script: std::sync::Mutex::new(vec![assistant_toolcall("find_route_for"), assistant_final()]),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        let mech = stub.mech.lock().unwrap().clone();
        assert_eq!(mech, vec![false, true]); // turn1 not mechanical (fresh), turn2 mechanical (read-only continuation)
    }
```
(Adjust `run_loop`'s arity if it's the 1a wrapper — it is `run_loop(completer,id,model,messages,tools,max_turns)`. The stub's `complete` now takes the flag.) The `prepare` down-route block itself: add a focused test if a provider-free path exists, else rely on the existing prepare/routing test harness + the behavior-preservation gate; at minimum assert (via reading) that `is_mechanical=false` (chat path) leaves the block inert.

- [ ] **Step 8: Build + gate + commit.**
```bash
cargo build -p tt-core
cargo clippy --workspace --all-targets -- -D warnings
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p tt-core --lib --tests   # 753 baseline + new tests, 0 failed
cargo fmt -p tt-core
git add crates/core/src/routes/agent_run.rs crates/core/src/routes/chat.rs
git commit -m "feat(core): thread is_mechanical + prepare down-routes a mechanical turn to route_mechanical_to (slice 2a)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (mirror required CI checks)
```bash
cargo fmt --check -p tt-core
cargo clippy --workspace --all-targets -- -D warnings
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p tt-core --lib --tests   # 753 + new, 0 failed
cargo test --workspace --no-run
cargo run -q -p tt-cli -- inspect .
cargo test -p tt-plan-core    # determinism goldens untouched
```
> `cargo test (workspace)` CI check is disk-flaky → `gh run rerun <id> --failed`. The chat-path is behavior-preserving (handler passes `is_mechanical=false`; the new block is inert) — the 753 baseline is the regression guard.

## Self-Review (against the spec)
**Spec coverage:** conservative detection (Task 1 `is_mechanical_continuation`: all-read-only prior tool turn; first/mixed/client → false) ✓ · thread `is_mechanical` (Task 2 Steps 1-4,6) ✓ · `prepare` down-route block: opt-in (`route_mechanical_to`), pause-respecting (`!route_paused`), route-attributed (keeps `matched_route_id` + sets `model_was_rewritten`), unresolvable→warning+original (Task 2 Step 5) ✓ · auto_pause reuse (no new code; the routed-serving attribution makes the existing judge/auto_pause apply) ✓ · default-off + chat-path behavior-preserving (Task 2 Step 6 + gate) ✓.
**Placeholder scan:** none — complete code; the Step-5/7 "confirm/verify" notes are guards against the prepare body's exact provider-resolve ordering (real risk to check against the code), not deferrals.
**Type consistency:** `is_mechanical_continuation(&[Message])->bool` (T1) used in `run_loop_core` (T2); `TurnCompleter::complete(req, is_mechanical)` consistent across trait/run_loop_core/GatewayCompleter/Stub/RecordingStub; `prepare(..., is_mechanical: bool)` consistent across its 2 callers; `route_mechanical_to`/`route_paused`/`matched_route_id`/`model_was_rewritten`/`state.registry.resolve` match the verified seam.
