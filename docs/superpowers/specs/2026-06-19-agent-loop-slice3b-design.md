# Server-side agent loop — slice 3b (SSE streaming of run events)

**Status:** approved design (2026-06-19) · **Repo:** public OSS core (`crates/core`) · **Origin:** the `server-side-agent-loop` workstream — slice 3 decomposed into 3a (cross-turn cost, shipped #196) + **3b (this, the LAST sub-slice)**. After 3b the entire workstream is complete (1a-0,1a,1b,2a,2c-1,2c-2,3a,3b; 2b deferred).

## Problem
`POST /v1/agent/runs` returns synchronously at the next pause/terminal — a caller watching a multi-turn run sees nothing until it finishes. This slice adds an **SSE** variant that emits **turn-level run events** as the loop runs, ending with the terminal/pause event carrying the aggregated cost (3a). The per-turn completion is **non-streaming** (`complete_once` — the loop needs the whole assistant message to decide tool calls + bypasses cache), so events are at **turn granularity** (a turn's full assistant message, tool results, terminal), NOT intra-turn token deltas.

## Decisions (locked in brainstorm)
1. **Trigger = `stream: true` on the request body** (mirrors `/v1/chat/completions` `req.stream`; one route). `create_run` branches: `false` → today's `Json<Run>` (byte-identical); `true` → SSE.
2. **TT-native minimal events** (the runs API is TT-native, not OpenAI-Assistants-compatible): `run.turn`, `run.message`, `run.tool_result`, `run.requires_action`, `run.completed`/`run.failed`/`run.incomplete`, then `[DONE]`.
3. **`create_run` only.** A streamed run that pauses emits `run.requires_action` + `[DONE]` (and is persisted, so GET/resume work); resume (`submit_tool_outputs`) keeps its JSON response (resume-streaming a deferrable follow-up).
4. **Event-sink seam:** `run_loop_core` gains an optional `Option<&UnboundedSender<RunEvent>>` (None in pure tests ⇒ no events + behavior-identical, mirroring the 2c-2 summarizer hook). NOT a trait — a plain sender is simplest + testable via a channel.
5. **Channel→Stream bridge = `futures::stream::unfold`** (no new dep — `futures` is already used by `sse.rs`'s `BoxStream`; `tokio-stream`/`ReceiverStream` is NOT a tt-core dep).

## Verified seams (current code)
- **`create_run`** (`agent_run.rs`): `pub async fn create_run(State, Extension<TraceId>, Option<Extension<ApiKeyContext>>, HeaderMap, Json<CreateRunRequest>) -> ApiResult<Json<Run>>`. Builds `identity`, resolves `summarize_cfg` (`resolve_summarize_config(&state, ...)`), builds the summarizer + `GatewayCompleter { state: &state, identity }`, runs `run_loop_core`, and on `Paused` persists a `StoredRun` (when `state.l1` is `Some`). Mounted `server.rs:83 .route("/v1/agent/runs", post(create_run))`.
- **`run_loop_core`** (`agent_run.rs`): 10-arg `(completer: &dyn TurnCompleter, id, model, mut messages, tools, max_turns, turns_done, mut summarized_upto, mut usage, summarizer: Option<&dyn TranscriptSummarizer>) -> LoopOutcome`. The loop: builds the per-turn req, `completer.complete(...)`, `messages.push(assistant)`, executes gateway tools (`messages.push(Message::Tool{..})`), returns `LoopOutcome::{Terminal(Run), Paused{messages,turns_done,usage,pending_tool_calls,summarized_upto,summarizer_tax_usd}}`. 6 call sites (the `run_loop` 1a wrapper + create_run + submit_tool_outputs + 3 unit tests).
- **SSE primitives** (`sse.rs`): `use axum::response::{sse::{Event, KeepAlive}, IntoResponse, Response, Sse};`. Pattern: `Sse::new(stream).keep_alive(KeepAlive::default()).into_response()` where `stream: impl Stream<Item = Result<Event, Infallible>>`. `[DONE]` is the end-of-stream convention (`Event::default().data("[DONE]")`). `futures` (BoxStream) is a dep.
- **`Run`/`RunUsage`/`StoredRun`** carry `usage` (incl. 3a `cost_usd`) + `summarizer_tax_usd`; `Message`/`ToolCall` are `Serialize`.
- **`AppState: Clone`** (cheap — `Arc` fields), so an owned clone can move into a `'static` spawn (the 2c-2 judge spawn already does `let state = self.state.clone()`).

## Design

### 1. `RunEvent` (`agent_run.rs`)
```rust
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type")]
pub enum RunEvent {
    #[serde(rename = "run.turn")]            Turn { turn: u32 },
    #[serde(rename = "run.message")]         Message { message: Message },
    #[serde(rename = "run.tool_result")]     ToolResult { tool_call_id: String, content: String },
    #[serde(rename = "run.requires_action")] RequiresAction { run: Run, pending_tool_calls: Vec<ToolCall> },
    #[serde(rename = "run.completed")]       Completed { run: Run },
    #[serde(rename = "run.failed")]          Failed { run: Run },
    #[serde(rename = "run.incomplete")]      Incomplete { run: Run },
}
impl RunEvent {
    fn event_name(&self) -> &'static str { /* "run.turn" .. matches the serde rename */ }
    fn to_sse(&self) -> Event { Event::default().event(self.event_name()).data(serde_json::to_string(self).unwrap_or_default()) }
}
```
The terminal/`requires_action` variants embed the `Run` view (so the final event carries `usage.cost_usd` + `summarizer_tax_usd` + the full transcript), matching the JSON response body.

### 2. `run_loop_core` gains an optional event sink
Add a trailing `events: Option<&tokio::sync::mpsc::UnboundedSender<RunEvent>>`. Emit (helper `emit(events, ev)` = `if let Some(tx) = events { let _ = tx.send(ev); }` — unbounded, sync, never blocks; receiver-dropped ⇒ ignored):
- `RunEvent::Turn { turn: turn + 1 }` at the top of each iteration (before `completer.complete`). **1-indexed** to match `Run.turns` (which is `turn + 1` everywhere) — the loop var `turn` is 0-indexed (`let mut turn = turns_done;`).
- `RunEvent::Message { message: assistant.clone() }` right after `messages.push(assistant)`.
- `RunEvent::ToolResult { tool_call_id, content }` after each inline gateway-tool `messages.push` (the `result: String` + `tc.id` are in scope there).
The **terminal/pause** events are emitted by the streaming task from the returned `LoopOutcome` (not the loop) — it owns the `Run`/persist mapping. **Call-site ripple:** `run_loop_core` has **9 direct call sites** (3 production — the `run_loop` 1a wrapper, `create_run`, `submit_tool_outputs` — + **6 unit tests**); ALL pass `None` for `events` EXCEPT `create_run`'s streaming arm (which passes `Some(&tx)`). Use the compiler (`cargo build -p tt-core --tests`) to find every site — do not rely on a count. Keep `#[allow(clippy::too_many_arguments)]`.

### 3. Streaming branch in `create_run`
Widen the return to `ApiResult<Response>`. After the shared setup (identity, `summarize_cfg`, org/routing/model/tools/max_turns — all computed borrowing `&state` before any move), branch on `req.stream`:
- **`false`** → today's logic, returning `Ok(Json(run).into_response())` / `Ok(Json(stored.to_run()).into_response())`.
- **`true`** → (the summarizer is built INLINE — there is no `build_summarizer` helper — and it borrows `identity` fields, so it MUST be built **before** `identity` moves into `GatewayCompleter`, exactly as the JSON `create_run` does today):
  ```rust
  let owned_state = state.clone();                       // cheap Arcs; 'static for the task
  let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<RunEvent>();
  tokio::spawn(async move {
      // Build the summarizer FIRST (it borrows identity.org_id/raw_bearer + base_ctx),
      // mirroring create_run's order; only THEN move identity into the completer.
      let base_ctx = base_request_context(&identity);
      let base_provider_id = owned_state.registry.resolve(&model).map(|p| p.id().to_string());
      let summarizer_model = summarizer_model(&owned_state);
      let summarizer_obj = summarize_cfg.clone().map(|cfg| GatewayTranscriptSummarizer {
          state: &owned_state, org_id: identity.org_id, raw_bearer: identity.raw_bearer.clone(),
          base_ctx, gate: owned_state.summary_gate.clone(), cfg, base_model: model.clone(),
          base_provider_id, summarizer_model, deadline: owned_state.judge_config.baseline_timeout,
      });
      let summ_ref: Option<&dyn TranscriptSummarizer> =
          summarizer_obj.as_ref().map(|s| s as &dyn TranscriptSummarizer);
      let completer = GatewayCompleter { state: &owned_state, identity };
      let outcome = run_loop_core(&completer, id, model.clone(), req_messages, tools.clone(),
                                  max_turns, 0, 0, RunUsage::default(), summ_ref, Some(&tx)).await;
      match outcome {
          LoopOutcome::Terminal(run) => {
              let _ = tx.send(match run.status {
                  RunStatus::Completed => RunEvent::Completed { run },
                  RunStatus::Failed    => RunEvent::Failed { run },
                  _                    => RunEvent::Incomplete { run }, // Incomplete (max_turns)
              });
          }
          LoopOutcome::Paused { messages, turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd } => {
              // `persist_paused` owns BOTH branches (see §4): Redis present ⇒ persists a
              // RequiresAction StoredRun + returns its Run view; no Redis ⇒ returns an
              // Incomplete Run (the 1a fallback). The task maps run.status → the event,
              // so a no-Redis streamed pause correctly emits run.incomplete, not requires_action.
              // No `?` here — a tokio::spawn closure returns (); match the Result.
              match persist_paused(&owned_state, id, org_id, routing, model, messages, tools,
                                   max_turns, turns_done, usage, pending_tool_calls.clone(),
                                   summarized_upto, summarizer_tax_usd, summarize_cfg).await {
                  Ok(run) => {
                      let _ = tx.send(match run.status {
                          RunStatus::RequiresAction => RunEvent::RequiresAction { run, pending_tool_calls },
                          _                         => RunEvent::Incomplete { run }, // no-Redis fallback
                      });
                  }
                  // store_run failure (rare, e.g. Redis down): log + emit run.failed with a
                  // minimal Failed Run (id + turns_done + a note). Plan pins the exact ctor.
                  Err(e) => { tracing::warn!(%id, error=%e, "agent run persist failed"); /* tx.send(RunEvent::Failed{..}) */ }
              }
          }
      }
      // tx dropped here ⇒ stream ends
  });
  use futures::StreamExt; // for .chain
  let stream = futures::stream::unfold(rx, |mut rx| async move {
      rx.recv().await.map(|ev| (Ok::<_, std::convert::Infallible>(ev.to_sse()), rx))
  })
  // append the [DONE] sentinel after the channel closes (explicit error annotation):
  .chain(futures::stream::once(async { Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]")) }));
  Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
  ```
  (`base_request_context`, `summarizer_model`, and the `GatewayTranscriptSummarizer { … }` literal already exist in `create_run` — the streaming task replicates that exact construction borrowing `owned_state`. The spawn closure returns nothing, so `persist_paused`'s `?` must be handled inside the task — propagate by logging + emitting `run.failed`, or make `persist_paused` infallible and log the store error; the plan pins this.)

### 4. Shared `persist_paused` helper (owns BOTH the Redis and no-Redis branches)
Extract the JSON path's ENTIRE `Paused` handling (the current `match state.l1 { Some(l1) => { build StoredRun + store_run + to_run() }, None => { Incomplete Run with the "requires Redis" note } }`) into `async fn persist_paused(state: &AppState, id, org_id, routing, model, messages, tools, max_turns, turns_done, usage, pending_tool_calls, summarized_upto, summarizer_tax_usd, summarize_cfg) -> ApiResult<Run>` so both the JSON `create_run` arm and the streaming task call it (DRY; one `StoredRun` literal). It takes `&state` and branches internally: Redis present ⇒ persist a `RequiresAction` `StoredRun` (consuming `summarize_cfg` into `StoredRun.summarize`) and return its `to_run()` (status `RequiresAction`); no Redis ⇒ return an `Incomplete` `Run` (the 1a fallback note). **The returned `Run` carries the discriminating status**, so callers map it: the JSON arm `Json`s it directly; the streaming task matches `run.status` → `RunEvent::RequiresAction` (when `RequiresAction`) or `RunEvent::Incomplete` (no-Redis). (`persist_paused` returns `ApiResult` because `store_run` can fail; the JSON arm `?`-propagates, the streaming task handles the `Err` by emitting `run.failed` + logging.)

## Components
| Unit | Location | Responsibility |
|---|---|---|
| `RunEvent` + `to_sse`/`event_name` | `agent_run.rs` | TT-native event enum → axum SSE `Event` |
| `run_loop_core` `events: Option<&UnboundedSender<RunEvent>>` + emits | `agent_run.rs` | per-turn `run.turn`/`run.message`/`run.tool_result` (None ⇒ no-op) |
| `CreateRunRequest.stream` (`#[serde(default)]`) + `create_run` branch (`-> ApiResult<Response>`) | `agent_run.rs` | SSE path: clone state, spawn loop w/ sink, map outcome→terminal event, `futures::unfold`→`Sse` |
| `persist_paused` helper | `agent_run.rs` | shared persist-on-pause (JSON + streaming) |

## Error handling / edge cases
- `stream:false` (default) ⇒ byte-identical JSON path. Receiver dropped (client disconnects) ⇒ `tx.send` errors, ignored; the loop runs to completion regardless (its work — incl. persistence on pause — still happens; acceptable, matches a detached run). Loop `Terminal(Failed)` ⇒ `run.failed` event. Pause w/ no Redis ⇒ `run.incomplete` (1a fallback). The spawned task owns its `state` clone ⇒ `'static`, no borrow leak. Per-turn behavior (routing/down-route/summarize/cost) is the SAME `run_loop_core` — streaming only adds emission. `[DONE]` always terminates the stream (the `chain`ed sentinel) even on a loop error (the terminal event is sent, then `[DONE]`).
- The summarizer's detached judge (2c-2) + the per-turn cost (3a) are unaffected — they ride the same loop.

## Testing
- **Pure (no provider):** `run_loop_core` with a test `mpsc::unbounded_channel` sink + a `Stub` scripted `[assistant_toolcall("find_route_for"), assistant_final()]` → drain `rx` and assert the event sequence: `Turn{1}`, `Message`(toolcall), `ToolResult{c1}`, `Turn{2}`, `Message`(final). A `None` sink ⇒ no events sent + the run behaves identically (existing loop tests unchanged).
- **`RunEvent` serialization:** `to_sse`/`serde` → the right `type` tag + `event_name` (`run.message` etc.); the terminal variants embed the `Run` (incl. `usage.cost_usd`).
- **SSE HTTP wiring** (spawn + `futures::unfold` + `Sse` + the `create_run` branch + `persist_paused`): provider-bound ⇒ integration-covered (the 1a/2c-2 pattern); unit tests cover the sink-emission seam + the event serialization.
- **Behavior-preservation:** `cargo test -p tt-core --lib --tests` at baseline — the `stream:false` path is byte-identical and every `run_loop_core` call site passes `None` for `events` EXCEPT `create_run`'s streaming arm (the events=None branch is a pure no-op). The `run_loop_core` 11th param (ripples to all 9 call sites — compiler-listed) + the `create_run` return-type widening (`Json(...).into_response()`) are the only ripples. `cargo fmt -p tt-core -- --check` + `cargo clippy -p tt-core --all-targets` clean (always `fmt --check` before push).

## Non-goals (3b)
Intra-turn token-delta streaming (per-turn completion is non-streaming by design). Resume (`submit_tool_outputs`) streaming (JSON, deferrable follow-up). OpenAI-Assistants event compatibility. A signed/attested stream (3a is unsigned). No change to the loop's per-turn behavior, `/v1/chat/completions`, or the 2a/2c/3a levers.

## Rollout
Single public PR — the workstream's last slice. Default-off (`stream:false` ⇒ current JSON path). Public CI (`cargo test (workspace)`; `fmt + clippy`; `tt inspect .`; determinism untouched). No DB/cloud changes. Redis optional (a streamed pause persists when present, else emits `run.incomplete`).
