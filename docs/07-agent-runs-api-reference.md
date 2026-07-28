# Agent Runs API reference (`/v1/agent/runs`)

The in-gateway agentic loop: run `model → tool → model` over the **read-only
gateway tools** (the MCP tools — `get_spend_today`, `check_budget_remaining`,
`set_cost_limit`, etc.) until a final answer, `max_turns`, the cost cap, or
the runaway-detector trips. Server-side, synchronous (slice 1a) with a pausable
slice 1b for client (non-gateway) tools. Source: `crates/core/src/routes/agent_run.rs`.

A run is the canonical surface for "an agent that cost-controls itself": each
turn's cost is accounted + the loop stops on three honest signals (turn cap,
cost cap, runaway-repeat detection). Eligible retained terminal runs can be
minted on demand as signed estimates (`POST /v1/admin/agent-runs/{run_id}/receipt/sign`
in cloud) and verified offline with `tt verify-receipt`; this is not universal
receipt or provider-invoice proof.

## Endpoints

| Method + path | What |
|---|---|
| `POST /v1/agent/runs` | Create + run. Non-streaming returns a JSON `Run`; `stream: true` returns SSE run events. |
| `GET /v1/agent/runs` | List the caller-org's runs. |
| `GET /v1/agent/runs/:id` | Fetch a run by id. |
| `POST /v1/agent/runs/:id/tool_outputs` | Resume a paused (`requires_action`) run with the client tool outputs. |

Auth: the `tt_live_` key (org derived; never caller-supplied).

## `CreateRunRequest` body

| Field | Type | Meaning |
|---|---|---|
| `model` | `String` | Model id for every turn (routing may rewrite it per turn). |
| `messages` | `Vec<Message>` | Initial transcript (system/user messages). |
| `tools` | `Vec<Tool>` | Tool definitions advertised to the model (defaults to none). Gateway (read-only) tool calls are executed inline; client tool calls pause the run. |
| `max_turns` | `Option<u32>` | Turn cap; clamped to `[1, 32]`. Default `DEFAULT_MAX_TURNS = 8`. |
| `max_cost_usd` | `Option<f64>` | Admission guard on accumulated served cost (USD). Before a new turn, the loop terminates as `Incomplete` with `stop_reason = budget_exhausted` when accrued cost—plus a best-effort estimate for priced models—reaches the cap. A started turn can settle beyond it; this is not reservation/settlement or provider-invoice proof. `None` ⇒ no cost cap. |
| `stream` | `bool` | When true, `POST /v1/agent/runs` streams run events as SSE (slice 3b) instead of returning a single JSON `Run`. Default `false`. |

## Run status (`RunStatus`)

`snake_case` serialized: `completed` / `incomplete` / `failed` / `requires_action`.

| Status | Meaning |
|---|---|
| `completed` | The model returned a final (tool-call-free) answer. |
| `incomplete` | The loop stopped without a final answer — `max_turns` reached, OR (for non-persisting/1a callers) a client tool surfaced. |
| `failed` | A completion turn errored. |
| `requires_action` | The loop paused on a client (non-gateway) tool + the run was persisted awaiting the caller's tool outputs (slice 1b). Resume via `POST /v1/agent/runs/:id/tool_outputs`. |

## Stop reasons (`StopReason`)

Carried on the `Run`/terminal events when the loop stopped short of `completed`:

| `stop_reason` | Meaning |
|---|---|
| `max_turns` | The loop hit `max_turns` (clamped `[1, 32]`). |
| `budget_exhausted` | The run's accumulated served cost reached `max_cost_usd`. |
| `runaway` | The loop made no progress — `RUNAWAY_REPEAT_THRESHOLD` consecutive byte-identical (tool-call + tool-result) steps. The model saw the same result and re-issued the same call with no new information; left alone it would burn the budget turn after turn (the *fastest* way an agent loop leaks money — it trips well before a static cost cap). |

## SSE events (when `stream: true`)

Named typed SSE frames; the event name is the frame discriminant. The
`run.turn_cost` event carries the per-turn served cost (the live cost signal:

| Event | Carries | Meaning |
|---|---|---|
| `run.turn` | `turn` (1-indexed) | A turn started. |
| `run.turn_cost` | `turn_cost_usd` | The previous turn's served cost (re-named `run.turn_cost`). |
| `run.message` | the assistant message | An assistant turn (no terminal tool calls, or pre-tool). |
| `run.tool_result` | the gateway tool result | A read-only gateway tool was executed inline. |
| `run.requires_action` | `pending_tool_calls` | A client (non-gateway) tool was called; the run paused (`requires_action`). Resume via `POST /v1/agent/runs/:id/tool_outputs`. |
| `run.completed` | final `Run` | Terminal: `completed`. |
| `run.failed` | final `Run` | Terminal: `failed`. |
| `run.incomplete` | final `Run` (with `stop_reason`) | Terminal: `incomplete` (carries which `stop_reason` — `max_turns` / `budget_exhausted` / `runaway`). |

## Cost accounting + receipts

Each turn builds a non-streaming `ChatCompletionRequest`, calls the completer,
appends the assistant message, + accumulates usage (`per_turn_cost_usd`). The
run's `RunUsage` aggregates the per-turn served costs into a run total.
Gateway (read-only) tool calls execute inline; their results append as
`Message::Tool`. The run rolls into a catalog-priced `saved_usd` estimate; an
eligible terminal record may be minted on demand as a signed receipt estimate.

### Agent-run receipt (ARR)

The mint endpoint returns a share URL whose public response has the top-level
agent-run receipt (`ARR`) shape. Current mints use this Ed25519 canonical
payload:

```
arr:v2|<org_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<signed_request_delta_micros>|<formula>|<eligible_requests>|<measured_requests>|<status>
```

ARR deliberately has no `workflow_id`: it attests a top-level agent run rather
than a workflow child. The `*_micros` values are signed integer micro-USD
inputs. The formula is exactly `tt.request-delta-estimate.v1`; coverage must be
nonempty and complete; and `saved_micros` equals
`max(signed_request_delta_micros, 0)`, preserving regressions as negative signed
deltas. An incomplete or empty cohort does not mint. Already-frozen `arr:v1`
receipts retain their historical canonical bytes. Convenience USD fields and
`signed_at` are not signed. The generated
[machine-readable contract index](receipt-spec/receipt-contracts.manifest.json),
structural contract, and checked-in
[v1](receipt-spec/arr-v1.golden.json) and
[v2](receipt-spec/arr-v2.golden.json) vectors live under `docs/receipt-spec`.
Use
`tt verify-receipt --receipt receipt.json --key-hex <a-key-obtained-out-of-band>`
to verify the signature offline. Signature validity does not establish issuer
identity, current mint eligibility, savings math, provider usage, or invoice
reconciliation.

## Related

- `crates/core/src/routes/agent_run.rs` — the source of truth (run status, SSE events, the pausable loop, `CreateRunRequest`).
- `crates/core/src/routes/agent_run_budget.rs` — `StopReason`, the `would_exceed`/`estimate_next_turn_cost`/`NoProgressTracker` (runaway detection) budget logic.
- `crates/core/src/routes/agent_run_store.rs` — the persisted run store (the pausable slice 1b).
- The cloud receipt endpoint — `POST /v1/admin/agent-runs/{run_id}/receipt/sign` — mints an ARR signed estimate on demand for an eligible retained terminal run.
- `docs/coding-agents.md` — the coding-agent wedge (the runtime `$` cap + kill-switch the loop rides).
- `docs/05-workflow-dsl-reference.md` — the `agent` workflow node (a workflow DSL equivalent of this loop, with `max_turns` / `max_cost_usd` / `tools`).
