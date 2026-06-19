# Agent-loop slice 3b (SSE streaming of run events) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `stream: true` SSE variant of `POST /v1/agent/runs` that emits TT-native turn-level run events as the loop runs, ending with the terminal/pause event (carrying the aggregated 3a cost). The **last sub-slice** of the server-side-agent-loop workstream.

**Architecture:** `run_loop_core` gains an optional `mpsc::UnboundedSender<RunEvent>` sink (None ⇒ no events, behavior-identical) and emits `run.turn`/`run.message`/`run.tool_result` per turn. Two shared helpers — `drive_run_loop` (builds the summarizer + completer + runs the loop) and `persist_paused` (the Paused both-branches → `Run`) — are used by both the JSON and streaming paths. `create_run` branches on `req.stream`: false → today's `Json<Run>` (now `.into_response()`); true → `state.clone()` + spawn `drive_run_loop(.., Some(&tx))`, map the outcome to a terminal/pause event, and bridge the channel to `Sse` via `futures::stream::unfold`.

**Tech Stack:** Rust, `crates/core/src/routes/agent_run.rs`. Reuses `axum::response::sse::{Sse, Event, KeepAlive}`, `futures::stream::{unfold, once, StreamExt}` (no new dep — `tokio-stream` is NOT a tt-core dep), `tokio::sync::mpsc::unbounded_channel`.

**Spec:** `docs/superpowers/specs/2026-06-19-agent-loop-slice3b-design.md`.

**Gate (after each compiling task + final):** `cargo test -p tt-core --lib --tests`. **`cargo fmt -p tt-core -- --check` before push** (public CI gates rustfmt). `cargo clippy -p tt-core --all-targets`. No DB gate.

---

## File Structure
All changes in `crates/core/src/routes/agent_run.rs` (the loop + handlers + tests). Add imports at the top: `use axum::response::{sse::{Event, KeepAlive}, IntoResponse, Response, Sse};`, `use futures::StreamExt;` (for `.chain`), `use tokio::sync::mpsc;` (verify against the existing `use` block; some may already be present transitively — add only what's missing, `cargo build` will tell you).

---

## Task 1: `RunEvent` enum + serialization

**Files:** Modify `crates/core/src/routes/agent_run.rs` (add the enum near `Run`); Test: tests mod.

- [ ] **Step 1: Write the failing test**
```rust
#[test]
fn run_event_serializes_with_type_tag_and_event_name() {
    // run.message embeds the assistant message; tag is the renamed type; event_name matches.
    let ev = RunEvent::Turn { turn: 1 };
    assert_eq!(ev.event_name(), "run.turn");
    assert_eq!(serde_json::to_value(&ev).unwrap()["type"], "run.turn");

    let m = RunEvent::Message { message: assistant_final() };
    assert_eq!(m.event_name(), "run.message");
    let v = serde_json::to_value(&m).unwrap();
    assert_eq!(v["type"], "run.message");
    assert!(v.get("message").is_some());

    let tr = RunEvent::ToolResult { tool_call_id: "c1".into(), content: "r".into() };
    assert_eq!(tr.event_name(), "run.tool_result");
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-core --lib agent_run::tests::run_event 2>&1 | tail -10` (cannot find type RunEvent).

- [ ] **Step 3: Implement** — add (uses `Message`/`ToolCall` from `tt_shared::messages`, already imported; `Run` is in-file):
```rust
/// One server-sent event from a streaming agent run (slice 3b). TT-native,
/// turn-level (per-turn completion is non-streaming, so no token deltas).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum RunEvent {
    #[serde(rename = "run.turn")]
    Turn { turn: u32 },
    #[serde(rename = "run.message")]
    Message { message: Message },
    #[serde(rename = "run.tool_result")]
    ToolResult { tool_call_id: String, content: String },
    #[serde(rename = "run.requires_action")]
    RequiresAction {
        run: Run,
        pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
    },
    #[serde(rename = "run.completed")]
    Completed { run: Run },
    #[serde(rename = "run.failed")]
    Failed { run: Run },
    #[serde(rename = "run.incomplete")]
    Incomplete { run: Run },
}

impl RunEvent {
    fn event_name(&self) -> &'static str {
        match self {
            RunEvent::Turn { .. } => "run.turn",
            RunEvent::Message { .. } => "run.message",
            RunEvent::ToolResult { .. } => "run.tool_result",
            RunEvent::RequiresAction { .. } => "run.requires_action",
            RunEvent::Completed { .. } => "run.completed",
            RunEvent::Failed { .. } => "run.failed",
            RunEvent::Incomplete { .. } => "run.incomplete",
        }
    }
    /// Render as an axum SSE event (named, JSON data).
    fn to_sse(&self) -> axum::response::sse::Event {
        axum::response::sse::Event::default()
            .event(self.event_name())
            .data(serde_json::to_string(self).unwrap_or_default())
    }
}
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -6`. (`to_sse`/`event_name` are unused until Task 2/4 — if clippy flags `dead_code`, add `#[allow(dead_code)] // used by the streaming path in Task 4` to the `impl` methods; remove in Task 4.)

- [ ] **Step 5: Commit**
```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 3b): RunEvent enum (TT-native turn-level SSE events)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `run_loop_core` event sink + per-turn emits + call-site ripple

**Files:** Modify `agent_run.rs` (`run_loop_core` `:254`, its body, the 9 call sites); Test: tests mod.

- [ ] **Step 1: Write the failing test**
```rust
#[tokio::test]
async fn loop_emits_run_events_to_sink() {
    let stub = Stub { script: std::sync::Mutex::new(vec![
        assistant_toolcall("find_route_for"), // turn 1: gateway tool
        assistant_final(),                     // turn 2: final
    ]) };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RunEvent>();
    let out = run_loop_core(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8, 0, 0,
                            RunUsage::default(), None, Some(&tx)).await;
    assert!(matches!(out, LoopOutcome::Terminal(_)));
    drop(tx); // close so recv drains then ends
    let mut names = vec![];
    while let Some(ev) = rx.recv().await { names.push(ev.event_name()); }
    // turn1: Turn{1}, Message(toolcall), ToolResult(c1); turn2: Turn{2}, Message(final)
    assert_eq!(names, vec!["run.turn", "run.message", "run.tool_result", "run.turn", "run.message"]);
}

#[tokio::test]
async fn loop_with_no_event_sink_emits_nothing() {
    let stub = Stub { script: std::sync::Mutex::new(vec![assistant_final()]) };
    let out = run_loop_core(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8, 0, 0,
                            RunUsage::default(), None, None).await;
    assert!(matches!(out, LoopOutcome::Terminal(_))); // None sink ⇒ behavior-identical, no panic
}
```
Also assert the first `Turn` is `1` (1-indexed): you can capture the events into `Vec<RunEvent>` instead of names and `assert!(matches!(events[0], RunEvent::Turn { turn: 1 }))`.

- [ ] **Step 2: Run to verify it fails** — arity mismatch (`run_loop_core` takes 10 args, test passes 11).

- [ ] **Step 3: Add the param + emits.** Change the signature (add `events` LAST):
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
    events: Option<&tokio::sync::mpsc::UnboundedSender<RunEvent>>,
) -> LoopOutcome {
```
Add a local emit helper at the top of the fn body:
```rust
    let emit = |ev: RunEvent| {
        if let Some(tx) = events {
            let _ = tx.send(ev); // unbounded, sync; receiver-dropped ⇒ ignored
        }
    };
```
Emit at the three points (in the `while turn < max_turns` body):
- First statement of the loop body (before the summarizer hook + before building `req`): `emit(RunEvent::Turn { turn: turn + 1 });` — **1-indexed**, matching `Run.turns`.
- Right after `messages.push(assistant.clone());` (`:309`): `emit(RunEvent::Message { message: assistant.clone() });`.
- Inside the `for tc in &tool_calls` gateway-tool loop, right before `messages.push(Message::Tool { .. })` (`:345`): capture the result for the event before it's moved:
```rust
                emit(RunEvent::ToolResult { tool_call_id: tc.id.clone(), content: result.clone() });
                messages.push(Message::Tool {
                    content: MessageContent::Text(result),
                    tool_call_id: tc.id.clone(),
                });
```

- [ ] **Step 4: Update ALL `run_loop_core` call sites** — every call gets a trailing `None` for `events` EXCEPT none yet (Task 4 adds the `Some(&tx)` site). Use the compiler: `cargo build -p tt-core --tests 2>&1 | grep -E "this function takes|expected .* arguments"` lists them. They are (current line numbers): the `run_loop` 1a wrapper (`:393`), `create_run` (`:877`), `submit_tool_outputs` (`:1118`), and 6 unit tests (`:1737`, `:1778`, `:1810`, `:2282`, `:2310`, `:2388`). Add `None` (or `None /*events*/`) as the final arg to each. (Task 4 changes create_run's site.)

- [ ] **Step 5: Run to verify it passes** — `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -10` (the 2 new tests + all existing loop tests, which now pass `None` and behave identically). `cargo build -p tt-core --tests 2>&1 | grep -E "^error" || echo OK`.

- [ ] **Step 6: Commit**
```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 3b): run_loop_core optional event sink + per-turn RunEvent emits

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: extract `persist_paused` + `drive_run_loop` (behavior-preserving)

**Files:** Modify `agent_run.rs` (`create_run` `:819-944` — extract two helpers + rewire the non-streaming path). No new tests (behavior-preserving; the existing CI/integration + the create_run path guard it; `run_loop_core` is the unit-tested core).

- [ ] **Step 1: Add the two helpers** (place above `create_run`):
```rust
/// Build the production summarizer + completer (borrowing `state` + `identity`)
/// and run the loop with an optional event sink. Shared by the JSON and SSE paths.
#[allow(clippy::too_many_arguments)]
async fn drive_run_loop(
    state: &AppState,
    identity: RunIdentity,
    id: Uuid,
    model: String,
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    summarize_cfg: Option<SummarizeConfig>,
    events: Option<&tokio::sync::mpsc::UnboundedSender<RunEvent>>,
) -> LoopOutcome {
    let base_provider_id = state.registry.resolve(&model).map(|p| p.id().to_string());
    let summarizer_model = summarizer_model(state);
    let base_ctx = base_request_context(&identity);
    let summarizer_obj = summarize_cfg.map(|cfg| GatewayTranscriptSummarizer {
        state,
        org_id: identity.org_id,
        raw_bearer: identity.raw_bearer.clone(),
        base_ctx,
        gate: state.summary_gate.clone(),
        cfg,
        base_model: model.clone(),
        base_provider_id,
        summarizer_model,
        deadline: state.judge_config.baseline_timeout,
    });
    let summ_ref: Option<&dyn TranscriptSummarizer> =
        summarizer_obj.as_ref().map(|s| s as &dyn TranscriptSummarizer);
    let completer = GatewayCompleter { state, identity };
    run_loop_core(
        &completer, id, model, messages, tools, max_turns, 0, 0,
        RunUsage::default(), summ_ref, events,
    )
    .await
}

/// Handle a paused run: Redis present ⇒ persist a `RequiresAction` `StoredRun`
/// (returns its `Run` view); no Redis ⇒ the 1a `Incomplete` fallback `Run`. The
/// returned `Run.status` discriminates the two for the caller.
#[allow(clippy::too_many_arguments)]
async fn persist_paused(
    state: &AppState,
    id: Uuid,
    org_id: Uuid,
    routing: StoredRouting,
    model: String,
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    turns_done: u32,
    usage: RunUsage,
    pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
    summarized_upto: u32,
    summarizer_tax_usd: Option<f64>,
    summarize_cfg: Option<SummarizeConfig>,
) -> ApiResult<Run> {
    match state.l1.as_ref() {
        Some(l1) => {
            let stored = StoredRun {
                id, org_id, status: RunStatus::RequiresAction, model, messages, tools,
                max_turns, turns_done, usage, pending_tool_calls, routing,
                summarized_upto, summarizer_tax_usd, summarize: summarize_cfg,
            };
            store_run(l1.cache.as_ref(), &stored).await?;
            Ok(stored.to_run())
        }
        None => {
            let name = pending_tool_calls
                .first()
                .map(|tc| tc.function.name.clone())
                .unwrap_or_default();
            Ok(Run {
                id,
                status: RunStatus::Incomplete,
                messages,
                turns: turns_done,
                usage,
                note: Some(format!(
                    "client tool '{name}' requires Redis to pause/resume (none configured)"
                )),
                summarizer_tax_usd,
            })
        }
    }
}
```

- [ ] **Step 2: Rewire the non-streaming `create_run`** to use them. Keep the return type `ApiResult<Json<Run>>` FOR NOW (Task 4 widens it). Replace the body from `let base_provider_id = ...` (`:847`) through the end of the `match run_loop_core(...) { ... }` (`:944`) with:
```rust
    let id = Uuid::new_v4();
    let outcome = drive_run_loop(
        &state, identity, id, model.clone(),
        req.messages, tools.clone(), max_turns,
        summarize_cfg.clone(), None,
    )
    .await;
    match outcome {
        LoopOutcome::Terminal(run) => Ok(Json(run)),
        LoopOutcome::Paused {
            messages, turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd,
        } => {
            let run = persist_paused(
                &state, id, org_id, routing, model, messages, tools, max_turns,
                turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd,
                summarize_cfg,
            )
            .await?;
            Ok(Json(run))
        }
    }
```
(`drive_run_loop` consumes `model.clone()`/`tools.clone()`/`summarize_cfg.clone()`; the originals `model`/`tools`/`summarize_cfg` flow to `persist_paused`. The `summarize_cfg`/`base_provider_id`/`summarizer_model`/`base_ctx`/`summarizer_obj`/`summ_ref`/`completer` locals that previously lived inline are now inside `drive_run_loop` — delete them from `create_run`.)

- [ ] **Step 3: Build + run the gate** — `cargo build -p tt-core --tests 2>&1 | grep -E "^error" || echo OK`. `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -8` (all existing tests pass — behavior-preserving). `cargo clippy -p tt-core --lib --tests 2>&1 | tail -8`.

- [ ] **Step 4: Commit**
```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "refactor(agent-loop 3b): extract drive_run_loop + persist_paused (shared by JSON/SSE)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `stream: true` + the SSE branch in `create_run`

**Files:** Modify `agent_run.rs` (`CreateRunRequest`; `create_run` signature + branch; remove any Task-1 `dead_code` allow on `RunEvent::to_sse`/`event_name`). Test: tests mod (the pure pieces are already covered; the SSE HTTP wiring is integration-covered).

- [ ] **Step 1: Add the `stream` field.** In `CreateRunRequest` (`:800`), after `max_turns`:
```rust
    /// When true, `POST /v1/agent/runs` streams run events as SSE (slice 3b)
    /// instead of returning a single JSON `Run`. Default false.
    #[serde(default)]
    pub stream: bool,
```

- [ ] **Step 2: Widen the return type + add the branch.** Change `create_run`'s signature `-> ApiResult<Json<Run>>` → `-> ApiResult<Response>`. After the shared setup (identity/org_id/routing/model/tools/max_turns/`summarize_cfg`/`id`), insert the streaming branch BEFORE the non-streaming `drive_run_loop` call, and wrap the non-streaming returns in `.into_response()`:
```rust
    let id = Uuid::new_v4();

    if req.stream {
        let owned_state = state.clone(); // cheap Arcs; 'static for the spawned task
        let messages = req.messages;
        let (tx, rx) = mpsc::unbounded_channel::<RunEvent>();
        tokio::spawn(async move {
            let outcome = drive_run_loop(
                &owned_state, identity, id, model.clone(),
                messages, tools.clone(), max_turns, summarize_cfg.clone(), Some(&tx),
            )
            .await;
            match outcome {
                LoopOutcome::Terminal(run) => {
                    let _ = tx.send(match run.status {
                        RunStatus::Completed => RunEvent::Completed { run },
                        RunStatus::Failed => RunEvent::Failed { run },
                        _ => RunEvent::Incomplete { run }, // max_turns
                    });
                }
                LoopOutcome::Paused {
                    messages, turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd,
                } => {
                    let pending = pending_tool_calls.clone();
                    match persist_paused(
                        &owned_state, id, org_id, routing, model, messages, tools, max_turns,
                        turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd,
                        summarize_cfg,
                    )
                    .await
                    {
                        Ok(run) => {
                            let _ = tx.send(match run.status {
                                RunStatus::RequiresAction => {
                                    RunEvent::RequiresAction { run, pending_tool_calls: pending }
                                }
                                _ => RunEvent::Incomplete { run }, // no-Redis fallback
                            });
                        }
                        Err(e) => {
                            tracing::warn!(run_id = %id, error = %e, "agent run persist failed");
                            let _ = tx.send(RunEvent::Failed {
                                run: Run {
                                    id, status: RunStatus::Failed, messages: vec![],
                                    turns: turns_done, usage: RunUsage::default(),
                                    note: Some(format!("persist failed: {e}")),
                                    summarizer_tax_usd: None,
                                },
                            });
                        }
                    }
                }
            }
            // tx dropped here ⇒ the stream ends after [DONE]
        });
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|ev| (Ok::<_, std::convert::Infallible>(ev.to_sse()), rx))
        })
        .chain(futures::stream::once(async {
            Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"))
        }));
        return Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response());
    }

    let outcome = drive_run_loop(
        &state, identity, id, model.clone(),
        req.messages, tools.clone(), max_turns, summarize_cfg.clone(), None,
    )
    .await;
    match outcome {
        LoopOutcome::Terminal(run) => Ok(Json(run).into_response()),
        LoopOutcome::Paused {
            messages, turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd,
        } => {
            let run = persist_paused(
                &state, id, org_id, routing, model, messages, tools, max_turns,
                turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd,
                summarize_cfg,
            )
            .await?;
            Ok(Json(run).into_response())
        }
    }
```
NOTE the `Err(e)` Failed-run uses `messages: vec![]` + `usage: default()` because the outcome's `messages`/`usage` were moved into the failed `persist_paused` call; a persist failure is a rare Redis error and the event just needs a terminal signal. (Alternatively clone `messages`/`usage` before the `persist_paused` call if you want the transcript in the failed event — not required.)

- [ ] **Step 3: Remove the Task-1 `dead_code` allow** on `RunEvent::to_sse`/`event_name` if you added one (they're now used). Add the imports (Step in File Structure) if not already present.

- [ ] **Step 4: Build + gate + clippy** — `cargo build -p tt-core --tests 2>&1 | grep -E "^error" || echo OK`. `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -10` (all pass; `stream:false` default keeps existing create_run tests green). `cargo clippy -p tt-core --lib --tests 2>&1 | tail -8` (clean — no borrow/'static error on the spawn, no dead_code).

- [ ] **Step 5: Commit**
```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 3b): stream:true SSE variant of POST /v1/agent/runs

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: full gate + fmt/clippy

**Files:** none (verification).

- [ ] **Step 1: fmt** — `cargo fmt -p tt-core` then `cargo fmt -p tt-core -- --check` (clean). `git diff --stat` touched only `agent_run.rs`.
- [ ] **Step 2: Full gate** — `cargo test -p tt-core --lib --tests 2>&1 | tail -15` — ALL green (additive; the `events:None` + `stream:false` defaults are byte-identical). Record counts.
- [ ] **Step 3: Clippy** — `cargo clippy -p tt-core --all-targets 2>&1 | tail -12` — no warnings. (Do NOT `cargo test --all-targets` — benches hang.)
- [ ] **Step 4: Commit (if fmt changed anything)** — `git add -A && git commit -m "style(agent-loop 3b): rustfmt" || echo "nothing to commit"`.

---

## Notes for the implementer
- **Default-off:** `stream:false` (default) ⇒ the JSON path (now via `drive_run_loop`/`persist_paused`, behavior-preserving); every `run_loop_core` call passes `events: None` except the streaming spawn ⇒ no events, byte-identical loop. `/v1/chat/completions` untouched.
- **The `'static` spawn** captures only owned values (`owned_state = state.clone()`, `identity`, `messages`, `model`, `tools`, `org_id`, `routing`, `summarize_cfg`, `id`); the completer + summarizer are built INSIDE the task borrowing `owned_state` (via `drive_run_loop`). Never reference the handler's `&state` in the `async move`.
- **`drive_run_loop` consumes** `model`/`tools`/`summarize_cfg` (clones passed in); the ORIGINALS go to `persist_paused`. The Paused arm's `messages`/`usage` come from the outcome (the evolved transcript), not the originals.
- **1-indexed `Turn`** (`turn + 1`) matches `Run.turns`.
- **CI:** public `cargo test (workspace)` is disk-flaky → `gh run rerun <run-id> --failed`. ALWAYS `cargo fmt -p tt-core -- --check` before push (it caught a miss in 2c-1).
- After this merges, the **server-side-agent-loop workstream is complete** (1a-0,1a,1b,2a,2c-1,2c-2,3a,3b; 2b deferred) — update the `server-side-agent-loop` memory + the COMPREHENSIVE_REVIEW tracker.
