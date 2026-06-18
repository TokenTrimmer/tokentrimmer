# Server-side agent loop — slice 1b (stateful hybrid: Redis runs + client round-trip)

**Status:** approved design (2026-06-18) · **Repo:** public OSS core (`crates/core`) · **Origin:** the `server-side-agent-loop` workstream (COST-3(U) reframe). Slice 1a (`POST /v1/agent/runs`, synchronous loop over gateway tools) shipped in PR #190. This slice makes the loop **stateful + hybrid**: it pauses when the model calls a tool the gateway can't execute, persists the run, and resumes when the client submits the tool outputs.

## Problem
The 1a loop completes synchronously over the read-only gateway tools, but when the model calls a **client tool** (anything not in the gateway allowlist) it can only return `Incomplete` — it can't ask the client to run the tool and continue. Real agents use arbitrary client-side tools, so the loop must support a **pause → client executes → resume** round-trip. That requires persisting the paused run.

## Decisions (locked in brainstorm)
1. **Graceful fallback when no Redis.** Redis/L1 (`state.l1: Option<L1Config>`) is optional. With no Redis: `create_run` still works synchronously (1a semantics) — a client-tool pause returns `Incomplete` (can't persist), unchanged from 1a. `GET`/`tool_outputs` return **503** ("agent runs require the L1/Redis store"). Non-regressing; the hybrid round-trip simply isn't available without Redis.
2. **Persist on first pause; 1h TTL.** Only a run that enters `requires_action` gets a Redis record (and stays GETtable through its eventual completed/failed state). An inline-completed run (gateway-tools-only) is returned in the create response and NOT stored (`GET` on it → 404). TTL = 3600s.
3. **No secrets in Redis; re-auth on resume.** The persisted record holds the conversation + non-secret routing config only — **never** the caller's bearer/creds (1a's `RunIdentity` carries `raw_bearer`; that is not persisted). On resume the caller re-authenticates; the handler verifies the resume's `org_id` equals the stored run's `org_id` (else 404), and rebuilds the per-turn completer from the **resume request's** auth + the stored routing config.

## Reused / extended assets (verified)
- **`L1Cache` trait** (`crates/cache/src/lib.rs:96-105`): `async fn get(&str)->Result<Option<Vec<u8>>,_>`, `set(&str,&[u8],ttl_secs:u64)`, `delete(&str)`. Arbitrary bytes + TTL. `AppState.l1: Option<L1Config{ cache: Arc<dyn L1Cache>, ttl_secs }>` (`crates/core/src/state.rs:108-119`). Reused for the run store (own key prefix; do NOT reuse `ttl_secs`/the response codec).
- **1a `agent_run.rs`**: `Run{id,status,messages,turns,usage,note}`, `RunStatus{Completed,Incomplete,Failed}`, `RunUsage`, `run_loop(completer,id,model,messages,tools,max_turns)->Run` (pause seam at the non-gateway-tool check, `:138-151`), `RunIdentity` + `RunIdentity::from_request`, `GatewayCompleter`, `TurnCompleter`, `create_run` (`:448-471`), `DEFAULT_MAX_TURNS`.
- **Patterns**: path param `Path(id): Path<Uuid>` + `require_org(ctx)->Result<Uuid,ApiError>` (404-not-403 on mismatch) from `routes_api.rs:28-33,97-109`; `SingleFlight::try_become_leader(key)` (`single_flight.rs:67-118`) for resume concurrency; `ApiError::{Unauthorized,NotFound,Internal,...}` (`error.rs`). All `Message`/`ToolCall`/`Tool` derive `Serialize+Deserialize` (`shared/src/messages.rs`).

## Design

### Loop-core refactor (`LoopOutcome`)
Extract the loop body so it returns:
```rust
enum LoopOutcome {
    Terminal(Run),                       // completed | failed | incomplete | max_turns
    Paused {                             // a client tool was called
        messages: Vec<Message>,          // transcript incl. the assistant turn with the client tool_calls
        turns_done: u32,
        usage: RunUsage,
        pending_tool_calls: Vec<ToolCall>,  // the non-gateway tool_calls awaiting client output
    },
}
```
- `run_loop_core(completer, model, &mut messages, tools, max_turns, turns_done) -> LoopOutcome` accepts a starting `turns_done` so resume continues the turn count (the `id` is attached by the caller when forming a `Run`/`StoredRun`).
- **Mixed gateway+client tool turn — definitive rule:** when an assistant turn's `tool_calls` contain ANY client (non-gateway) tool, the loop FIRST executes that turn's **gateway** tool_calls inline (appending their `Message::Tool` results, as in 1a's executor path), THEN returns `Paused{ pending_tool_calls = the CLIENT tool_calls only }`. Rationale: OpenAI requires every tool_call of an assistant turn to be answered before the next assistant turn — so after the client submits outputs for the pending (client) calls on resume, every tool_call of that paused assistant turn is answered (gateway results already appended, client results just appended) and the loop continues correctly. The `submit_tool_outputs` validation therefore checks the submitted ids exactly cover the *pending client* tool_calls (the gateway ones are already answered).
- **1a `run_loop` becomes a thin wrapper**: calls `run_loop_core`; `Terminal(run)`→`run`; `Paused{messages,turns_done,usage,..}`→ the old `Incomplete` `Run` with the same note. This keeps 1a's tests + the no-Redis `create_run` path byte-behavior-identical.

### `RunStatus` gains `RequiresAction`
`#[serde(rename_all="snake_case")]` → `"requires_action"` (the other variants stay lowercase single words; confirm serde rename covers it). A `Run` returned with `requires_action` carries `messages` (the assistant turn with the client `tool_calls` is the last message) so the client knows what to run.

### `StoredRun` (Redis)
```rust
struct StoredRun {
    id: Uuid, org_id: Uuid, status: RunStatus,
    model: String, messages: Vec<Message>, tools: Vec<Tool>,
    max_turns: u32, turns_done: u32, usage: RunUsage,
    pending_tool_calls: Vec<ToolCall>,        // empty unless requires_action
    routing: StoredRouting,                   // { provider_pin: Option<String>, forced_route: Option<String>, tag: Option<String> }
}
```
`Serialize+Deserialize`. Key `format!("tt:runs:{org_id}:{run_id}")`. Persist/fetch/delete via small helpers over `state.l1.cache` with `ttl_secs=3600` (a module const, NOT `L1Config.ttl_secs`). A `Run` "view" is derived from a `StoredRun` for the HTTP response.

### Handlers
- **`create_run`** (extend 1a): build `RunIdentity` (1a). Run `run_loop_core`. `Terminal(run)` → `Json(run)` (no persist). `Paused{..}` → if `state.l1.is_some()`: build `StoredRun{requires_action, ..}` (routing from the identity), persist (key+3600s), return `Json(Run{status:requires_action, messages, ..})`; else return `Json(Run{status:incomplete, note})` (1a fallback).
- **`get_run`** `GET /v1/agent/runs/:id`: `let org = require_org(ctx)?;` `let l1 = state.l1.as_ref().ok_or(ServiceUnavailable)?;` fetch `tt:runs:{org}:{id}`; `None`→404; deserialize→ return the `Run` view. (The key embeds org, so a wrong-org caller's key simply misses → 404.)
- **`submit_tool_outputs`** `POST /v1/agent/runs/:id/tool_outputs` body `ToolOutputsRequest { tool_outputs: Vec<ToolOutput{ tool_call_id: String, output: String }> }`: `require_org`; require Redis (503); fetch the run (404); if `status != requires_action` → 409; validate the submitted `tool_call_id`s **exactly cover** `pending_tool_calls` (missing/extra → 400); acquire single-flight on `tt:runs:{org}:{id}` (loser → 409 "run is being resumed"); append each output as `Message::Tool{ tool_call_id, content: Text(output) }` to `messages`; rebuild `GatewayCompleter` from the **resume request's** auth (org already == stored, verified) + `stored.routing`; `run_loop_core(.., &mut messages, stored.tools, stored.max_turns, stored.turns_done)`; `Terminal(run)` → overwrite the stored run with the terminal status (stays GETtable to TTL) + return `Json(run)`; `Paused{..}` → overwrite `requires_action` + return `Json(Run{requires_action,..})`.

### Components
| Unit | Location | Responsibility |
|---|---|---|
| `LoopOutcome` + `run_loop_core` + `run_loop` wrapper | `crates/core/src/routes/agent_run.rs` | the pausable loop; 1a wrapper preserves old behavior |
| `RunStatus::RequiresAction` + `StoredRun`/`StoredRouting` + run-store helpers | `agent_run.rs` (or a small `agent_run_store` submodule) | persist/fetch/delete a run via `L1Cache`; the `Run`-view derivation |
| `get_run` + `submit_tool_outputs` handlers + request types | `agent_run.rs` | the two new endpoints |
| route mounts | `crates/core/src/server.rs` | `GET /v1/agent/runs/:id`, `POST /v1/agent/runs/:id/tool_outputs` (inherited auth) |

### Error handling / edge cases
- no-Redis: `create_run` graceful (1a); `get`/`tool_outputs` → 503.
- missing run / wrong org → 404 (key embeds org; no leak).
- resume on non-`requires_action` (e.g. already completed) → 409.
- `tool_outputs` not exactly covering pending ids → 400 (clear message listing the expected ids).
- concurrent resume → single-flight; loser → 409.
- a resume that itself pauses again (model calls another client tool) → updates `requires_action` (the loop is genuinely multi-round).
- `max_turns` spans the whole run (turns_done persists across pauses) — a long round-tripping run still terminates at the cap.
- TTL expiry mid-pause → `get`/`tool_outputs` → 404 (the run lapsed); acceptable.

### Testing
- **loop-core**: stub completer scripted: [client-tool turn] → `Paused` with the right pending ids; then resume by calling `run_loop_core` with the outputs appended → [final answer] → `Terminal(Completed)`; mixed gateway+client turn → gateway executed + paused; `turns_done` continuity across a resume; max_turns spanning a pause.
- **store**: `StoredRun` serde round-trip; persist→fetch via an in-memory `L1Cache` returns the same record; key format.
- **endpoints** (test `AppState` with an in-memory `L1Cache`): no-Redis 503 on get/resume; org-scoping (a different org's key → 404); 409 on resume of a completed/non-requires_action run; 400 on mismatched tool_outputs; happy path create→pause→tool_outputs→complete.
- **1a preserved**: the `run_loop` wrapper still returns `Incomplete` for a client tool (1a tests unchanged); inline-completion path unchanged.
- Redis-only (no DB gate needed for these tests; use the in-memory `L1Cache`).

### Non-goals (1b)
SSE streaming + cross-turn attestation (slice 3); the cost levers (slice 2); background/async run processing (runs are synchronous to the next pause/terminal); multi-run listing/cancellation endpoints; OpenAI-Assistants API compatibility (TT-native shape).

## Rollout
Single public PR. Public CI gates it (`cargo test (workspace)` — disk-flaky, rerun if needed; `fmt+clippy`; `tt inspect .`; determinism goldens untouched). No DB/cloud changes. The loop-core refactor keeps 1a behavior (the wrapper) — the existing agent_run tests are the regression guard.
