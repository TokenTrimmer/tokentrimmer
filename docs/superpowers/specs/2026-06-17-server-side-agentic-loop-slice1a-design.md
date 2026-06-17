# Server-side agentic loop — slice 1a (+ COST-3(U) accurate closure)

**Status:** approved design (2026-06-17) · **Repo:** public OSS core (`crates/core`, `crates/mcp`, `crates/cli`) · **Origin:** `COMPREHENSIVE_REVIEW_2026-06-15.md` finding `COST-3(U)` Sub-lever 3 — which the brainstorm reframed: the gateway is a per-request proxy, so the agentic cost levers need a server-side loop the gateway owns.

## Problem

`COST-3(U)` Sub-lever 3 was scoped as "dispatch the routed sub-step on a distinct cache lane *in the chat handler*." Investigation showed that seam doesn't exist: the chat handler is a **per-request proxy** — the agent's tool-loop runs **client-side**, so each sub-step arrives as a separate `/v1/chat/completions` request. Two consequences:

1. The "distinct cache lane" is already satisfied — L2 entries are keyed on the served model, so a down-routed sub-step is automatically isolated.
2. The gateway can't *dispatch* a sub-step; today Sub-lever 3 only pushes a `subagent_lane:<target>` string into the `x-tokentrimmer-warnings` response header (an unconsumed/loose signal).

To realize the agentic cost levers server-side (down-route mechanical sub-steps, serve read-only sub-steps from cache, summarize across turns) the gateway must **own the loop**. This spec delivers the honest interim closure of `COST-3(U)` **and** the first foundation slice of that loop.

## Scope of this spec

- **Slice 0 — `COST-3(U)` accurate closure** (small, safe, zero hot-path risk).
- **Slice 1a — stateless server-side agentic loop over gateway-executed tools** (the loop foundation).

Both are public-repo-only, opt-in/additive (no change to the existing `/v1/chat/completions` behavior).

## Decomposition (roadmap — later slices are NON-GOALS here)

- **1b** — stateful hybrid: Redis run store + `GET /v1/agent/runs/{id}` + `POST .../tool_outputs`; when the model calls a tool the gateway can't execute, persist `requires_action` with the remaining calls and resume on submitted outputs.
- **2** — the cost levers across turns: wire the already-built `agentic_budget` modules (Sub-lever 3 down-route of a mechanical turn, Sub-lever 4 `substep_cache` serve, 2b summarize). This is where the loop earns its keep; it *subsumes* the original Sub-lever 3.
- **3** — polish: SSE streaming of run events; cross-turn run-cost aggregation feeding attestation/dashboard.

## Slice 0 — COST-3(U) accurate closure

In `crates/core/src/passes/agentic_budget/mod.rs`, the Sub-lever 3 block currently does:
```rust
if let Some(target) = &ab.route_mechanical_to {
    out.warnings.push(format!("subagent_lane:{target}"));
}
```
Change:
1. **Keep the down-route signal but make it accurate + documented.** The string stays a *client signal* (the gateway can't act on it per-request), but the module doc is corrected to state plainly: the gateway is a per-request proxy; cache-isolation of a down-routed sub-step is already achieved by the model-keyed L2 lane; the routed *dispatch* is the client's (today) or the server-side loop's (slice 2). Update the doc comment at the Sub-lever 3 block + the module header's "the handler's to wire (deferred there)" lines to reference the server-side loop as the realization path, not a missing handler seam.
2. **No behavior change to the warning emission** (clients already consuming `subagent_lane:` keep working); this slice only corrects the misleading "deferred to the handler" framing into an accurate boundary statement, so the codebase stops implying a seam that can't exist in a per-request proxy.

This is documentation + comment accuracy only — no logic change, no hot-path touch. It closes the `COST-3(U)` checklist item honestly; slice 2 later supersedes it with real server-side dispatch.

## Slice 1a — stateless server-side loop over gateway tools

### API
`POST /v1/agent/runs` (authed like `/v1/chat/completions`).
- **Request:** `{ model: String, messages: Vec<Message>, tools?: Vec<ToolDef>, max_turns?: u32 }` (`max_turns` default 8, clamped `[1, 32]`).
- **Response:** a **Run** object:
```
Run {
  id: Uuid,                 // generated; addressable in 1b
  status: RunStatus,        // completed | incomplete | failed
  messages: Vec<Message>,   // the full final transcript (incl. tool turns)
  turns: u32,               // model invocations performed
  usage: RunUsage,          // summed prompt/completion tokens across turns
}
RunStatus = completed       // model returned a final (no tool_calls) answer
          | incomplete      // max_turns hit, OR the model called a tool the
                            //   gateway can't execute (client tool — lands in 1b)
          | failed          // an upstream turn errored
```
The Run shape is forward-designed so 1b adds `requires_action` (a 4th status) + an `id`-addressable Redis store + a `tool_outputs` resume without breaking 1a clients.

### The loop (synchronous, per request)
```
transcript = request.messages
for turn in 0..max_turns:
    completion = dispatch_one_completion(model, transcript, tools, ctx)   // reuses the chat path
    append completion.assistant_message to transcript
    if completion has no tool_calls:
        return Run { completed, transcript, turn+1, usage }
    (known, unknown) = partition(completion.tool_calls) by gateway-executable
    if unknown is non-empty:
        // 1a stops here; 1b turns this into a requires_action pause + resume
        return Run { incomplete, transcript, turn+1, usage,
                     note: "client tool '<name>' requires slice-1b round-trip" }
    for tc in known:
        result = mcp_registry.call(tc, ctx)        // in-process; org-scoped
        append tool_result(tc.id, result) to transcript
    // loop continues with the tool results appended
return Run { incomplete (max_turns), transcript, max_turns, usage }
```

### Components (public)
| Unit | Location | Responsibility | Depends on |
|---|---|---|---|
| run types + loop | new `crates/core/src/routes/agent_run.rs` | `Run`/`RunStatus`/`RunUsage`, the turn loop, the tool partition, `max_turns` bound, the `POST /v1/agent/runs` axum handler | dispatch-core, mcp registry |
| in-process completion dispatch | refactor in `crates/core/src/routes/chat.rs` | extract the per-completion core (route match → model rewrite → provider/cred resolution → upstream call → telemetry/breaker) into a callable `async fn` the loop invokes per turn — **the key integration point** | existing chat internals |
| gateway tool execution | new small executor in `crates/core` (e.g. `agent_run.rs` or a sibling) | map a gateway-known tool name → the **underlying** `preview`/`inspect-core`/analytics call (the same logic the MCP tools wrap), org-scoped; map result → a tool `Message` | `tt-preview`, `tt-inspect-core` (existing core deps) |
| route mount | server router (where `/v1/chat/completions` is mounted) | mount `POST /v1/agent/runs` behind the same auth layer | — |

### Gateway-executable tools (dependency-direction note)
A tool call is gateway-executable iff its name is in a small allowlist mirroring `substep_cache`'s `READ_ONLY_TOOLS` (`find_route_for`, `preview_cost`, `inspect_diff`, `batch_savings`). **Do NOT call `crates/mcp` from `crates/core`** — the MCP tool *implementations* depend on core, so a `core→mcp` edge would be a cycle. Instead the loop dispatches each known tool to the **underlying library call the MCP tool also wraps** (e.g. `preview_cost`→`tt-preview`, `inspect_diff`→`tt-inspect-core`), which core already depends on. The MCP server and this executor are two thin wrappers over the same underlying logic — no duplicated business logic, no dependency cycle. The plan confirms the exact underlying entry point per tool. Unknown/client tools are non-executable in 1a (→ `incomplete`). (If a clean shared executor proves larger than expected, the fallback is to mount the run handler in `crates/cli`, which already depends on both core + mcp — but the no-cycle in-core executor is preferred.)

### The handler refactor (the main risk — its own PR: slice 1a-0)
Investigation found **no clean inner dispatch fn**: the non-streaming completion logic is interwoven across ~900 lines of the 5000-line handler (route → redaction → compression → agentic-budget → provider dispatch w/ failover+breaker → L1/L2 cache → cost → `request_logs` → response). The chosen approach (over an in-process handler-invocation shortcut) is to **extract a reusable `complete_once` core** — the better long-term foundation, since slices 2+ need to rewrite the per-turn request (down-route a mechanical turn) and inspect the typed response, not parse an HTTP body.

Because this is the money + circuit-breaker hot path (the ARCH-1 stuck-breaker bug lived here), the extraction ships as **its own behavior-preserving PR (slice 1a-0), landed + verified before the loop builds on it**:
- Extract the non-streaming completion pipeline into `async fn complete_once(state, ctx, req, <header-derived inputs>) -> Result<CompletionOutcome, ApiError>` where `CompletionOutcome` carries the `ChatCompletionResponse` + the cost/route/cache meta the handler needs for headers.
- `handler` becomes the axum-glue wrapper: extractor parsing + ctx build + sandbox short-circuit + the **streaming branch (unchanged)** + (non-streaming) `complete_once(...)` then Response+header assembly.
- **Gate:** the full `cargo test -p tt-core` suite (309+ cases across `crates/core/tests/` + the inline chat tests) stays green — behavior-preserving by construction. No new feature in this PR.

The loop (slice 1a) then calls `complete_once` per turn through a `TurnCompleter` seam (so loop tests inject a stub and need no live provider).

### Error handling / edge cases
- Upstream turn error → `failed` (carry the turn index + error class); the run does not retry inside the loop (the dispatch core's own failover/breaker still applies per turn).
- A gateway tool that errors → its error is appended as the tool result so the model can react; it does NOT abort the run.
- `max_turns` exhausted → `incomplete` (not `failed`); the partial transcript is returned.
- Unknown/client tool in 1a → `incomplete` + the note (the clean handoff to 1b).
- Empty `tools` / a run whose first completion has no tool_calls → one turn, `completed` (degenerates to a single completion — still correct).
- Auth/org: the run carries the caller's org context; gateway tool execution is org-scoped exactly as the MCP server enforces.

### Billing / telemetry
Each turn is a normal metered completion through the existing telemetry path → one `request_logs` row per turn (billing "just works" per turn). Cross-turn run-cost aggregation is slice 3.

### Testing
- **Loop termination**: a stub dispatch-core returning a no-tool-call completion → `completed` in one turn.
- **Gateway-tool turn**: stub core returns a `find_route_for` tool_call, then (after the tool result is appended) a final answer → the loop executes the tool via the registry, appends the result, re-invokes, and completes in 2 turns; the transcript contains the tool result.
- **max_turns bound**: a core that always returns a tool_call → `incomplete` at exactly `max_turns` model invocations.
- **unknown tool**: a core returning a non-registry tool_call → `incomplete` + the note; no panic.
- **tool error**: a gateway tool that errors → error appended as the tool result, loop continues (not aborted).
- **behavior preservation**: the full existing `chat.rs` test suite stays green after the dispatch-core extraction (the refactor gate).
- The loop tests use a **stub/injectable dispatch-core** so they don't need a live provider; the MCP execution uses the in-process registry. (CLI `cli_spawn_smoke` style end-to-end is out of scope for unit tests.)

## Non-goals (slice 1a)
- Redis run store, `GET /v1/agent/runs/{id}`, `requires_action`, `tool_outputs` resume (→ 1b).
- The cost levers (down-route / substep-cache serve / summarize) (→ 2).
- SSE streaming; cross-turn run-cost aggregation/attestation (→ 3).
- Executing arbitrary client/mutating tools server-side (only registry read-only tools; everything else round-trips in 1b).
- OpenAI-Assistants API compatibility (TT-native minimal surface; compat is a possible later direction, not now).
- Any change to `/v1/chat/completions` request/response behavior (the refactor is behavior-preserving).

## Rollout
Single public PR (slice 0 + slice 1a). Public CI gates it (the 7 required checks, incl. `cargo test (workspace)` as the behavior-preservation gate for the chat refactor, `fmt + clippy`, `tt inspect .`, determinism goldens — untouched). Merge on green. Slices 1b/2/3 follow as separate PRs, each its own spec.
