# Workflow DSL reference (`/v1/workflows`)

TokenTrimmer's workflow engine (`crates/core/src/workflow/`) is an in-gateway,
durable, cost-controlled workflow runtime. A workflow is a JSON graph of nodes
(LLM calls, agent loops, HTTP calls, transforms, branches, sub-workflows) that
the gateway executes server-side, applying routing, caching, cost accounting,
budget caps, and signed receipts to every step — and rolling the cost up into
one `saved_usd` figure per run.

This page documents the DSL (the node types, the `ModelSelection`, edges,
`allowed_hosts`, `secrets`), the CRUD + run API, and the SSE event shape. It's
grounded in `crates/core/src/workflow/types.rs` + `routes/workflows.rs`.

## The workflow definition

A workflow is a `WorkflowDefinition` with nodes, edges, entry/exit, an
allowlist of outbound HTTP hosts, and optionally named secrets. Stored via
`POST /v1/workflows` (CRUD in `routes/workflows.rs`); run via
`POST /v1/workflows/:id/runs`; estimated (offline cost preview) via
`POST /v1/workflows/:id/estimate`.

## Node types (`NodeKind`)

`#[serde(tag = "type", rename_all = "snake_case")]` — each node is a JSON
object with a `"type"` discriminant + the variant's fields inlined via
`#[serde(flatten)]`. The `id` field is the node's graph identifier.

| `type` | What it does | Cost-relevant fields |
|--------|---|---|
| `trigger` | Entry-point; receives the workflow's external input. | — |
| `model` | A single LLM call. `selection` picks the model/route; `prompt` is the template; `max_cost_usd` (optional) caps THIS call's cost. | `max_cost_usd: Option<f64>` |
| `agent` | An agentic multi-turn loop with tool access. `max_turns` (default `DEFAULT_MAX_TURNS = 8`), `max_cost_usd`, `tools`. | `max_turns`, `max_cost_usd` |
| `transform` | A deterministic expression transform (no LLM call). `expr` is the expression string. | — (no LLM cost) |
| `branch` | A conditional branch; exactly one outgoing edge is followed. `cond` (expression), `when_true`/`when_false` (node ids). | — |
| `output` | Terminal output-collection node. | — |
| `http` | An outbound HTTP call to an allowlisted external API. `method`, `url`, `headers`, `body`, `max_response_bytes`. The `url` host MUST be a static literal in `allowed_hosts` (default-deny); only path/query/headers/body may contain `{{template}}` tokens. | `max_response_bytes` (size cap) |
| `sub_workflow` | Execute another stored workflow as a nested child. `workflow_id`, `version` (unused at MVP — always latest). The parent's remaining budget cap is passed to the child; cost + baseline roll up so `saved_usd` derives without double-counting. | inherits parent cap |
| `loop` | A bounded loop — runs the `body_workflow_id` sub-workflow up to `max_iters` times, re-checking `cond` (Branch syntax) before each iteration. Termination is GUARANTEED by `max_iters`; `cond` is early-exit. | `max_iters` |

## Model selection (`ModelSelection`)

`#[serde(tag = "type", rename_all = "snake_case")]` — how a `model` or `agent`
node picks its model.

| `type` | Meaning |
|--------|---|
| `model` | A specific model id (e.g. `"claude-3-5-haiku-20241022"`). |
| `route` | A named TokenTrimmer route (resolved at runtime). |
| `auto` | Let the gateway pick the best model automatically. |

## Edges

Edges connect node `id`s. A node with multiple outgoing edges (e.g. `branch`)
selects the one to follow at runtime; `branch` follows exactly one
(`when_true` or `when_false`). `loop` re-checks `cond` before each iteration
using the `branch` syntax.

## Allowed hosts + secrets

- **`allowed_hosts`** (on `WorkflowDefinition`): a default-DENY allowlist of
  hostnames an `http` node may call. The `url`'s host MUST be a static literal
  in this list — only path/query/headers/body may be templated. This makes the
  outbound allowlist unambiguous (no SSRF-via-template-injection). See
  `workflow/http.rs` for the connect-time private/loopback/link-local IP
  blocking, `Policy::none()` redirects, userinfo-spoofing rejection (all
  adversarially tested).
- **`secrets`**: named secrets referenced by `{{secrets.NAME}}` templates in
  `http` node headers/body. BYO-secrets (the gateway never has OAuth custody);
  see `workflow/secrets.rs` + the workflow-secrets migration.

## API surface

| Method + path | What |
|---|---|
| `POST /v1/workflows` | Create a workflow (CRUD; `id` + `version` optional). |
| `GET /v1/workflows` | List workflow definitions (metadata). |
| `POST /v1/workflows/:id/estimate` | Offline cost preview (no LLM calls; computes the projected cost from the graph + pricing). |
| `POST /v1/workflows/:id/runs` | Run synchronously (returns the run + the rolled-up `saved_usd`). |
| `GET /v1/workflows/:id/...` | (Run status / receipt endpoints — see `routes/workflows.rs`.) |

## SSE events (a run's streaming surface)

A run streams Anthropic-shaped SSE frames? — no: a workflow run's SSE events
are the workflow engine's own typed frames (`workflow/events.rs`), carrying
node-entry/exit, per-node cost + baseline, tool-call blocks, and the rolled-up
`saved_usd` at completion. (Confirm the exact event names against
`workflow/events.rs` before publishing — this reference documents the shape,
the event-name list is in that file.)

## Cost accounting + receipts

Every `model`/`agent` node's LLM call goes through the SAME chat pipeline as
`/v1/chat/completions` — cost accounting, routing, caching, the budget
enforcer, the kill-switch all apply identically. The run rolls the per-node
cost + baseline into a parent total; `saved_usd` derives without
double-counting (sub-workflows pass the parent's remaining cap to the child).

Each completed run is signable as a workflow receipt — `POST /v1/admin/workflow-runs/{run_id}/receipt/sign` returns a frozen, Ed25519-signed receipt + a
shareable verify URL. The receipt's canonical payload is
`wfr:v1|<org>|<workflow_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<status>`
(or `wfr:v2|…|<quality_verdict>` when the run carried a sampled flow-level
quality-gate verdict).

### Verifying a workflow receipt

The mint returns a self-contained `VerifyReceiptResponse` (from the public GET
endpoint `GET /v1/workflow-runs/{run_id}/receipt?expires=&sig=`) exposing every
canonical-payload field — `org_id`, `workflow_id`, `run_id`, `cost_micros`,
`baseline_micros`, `saved_micros`, `status`, `canonical_version`,
`quality_verdict`, `signature_hex`, + the embedded `verifying_key_hex`. Any party
with the share URL can reconstruct the canonical string + check the Ed25519
signature with that key — offline, no TokenTrimmer network call beyond fetching
the receipt.

The `tt verify-receipt` CLI dispatches over the **compression** (`vcr:v1|`) +
**cache-hit** (`l2:v1|`) receipt families (the gateway-signed receipts; the
families customers verify most). Workflow-receipt (`wfr:`) online verify runs
via the GET endpoint above; offline CLI verify of `wfr:` is a follow-up (the
canonical-payload + verify primitives currently live cloud-side; moving them to
the public `tt_telemetry` crate would make the CLI the single offline-verify
entry point across all three families).

## Example

```json
{
  "name": "classify-and-route",
  "nodes": [
    {"id": "in", "type": "trigger"},
    {"id": "classify", "type": "model",
     "selection": {"type": "auto"},
     "prompt": "Classify the user's intent: {{input.text}}",
     "max_cost_usd": 0.01},
    {"id": "branch", "type": "branch",
     "cond": "{{nodes.classify.output}} == 'code'",
     "when_true": "code_route", "when_false": "chat_route"},
    {"id": "code_route", "type": "model",
     "selection": {"type": "route", "route_ref": "coding-down-route"}},
    {"id": "chat_route", "type": "sub_workflow",
     "workflow_id": "00000000-0000-0000-0000-000000000001"},
    {"id": "out", "type": "output"}
  ],
  "edges": [
    {"from": "in", "to": "classify"},
    {"from": "classify", "to": "branch"},
    {"from": "branch", "to": "code_route"},
    {"from": "branch", "to": "chat_route"},
    {"from": "code_route", "to": "out"},
    {"from": "chat_route", "to": "out"}
  ],
  "allowed_hosts": [],
  "secrets": {}
}
```

## Related

- `crates/core/src/workflow/types.rs` — the `Node`/`NodeKind`/`ModelSelection`/edge types (the source of truth for this page).
- `crates/core/src/workflow/engine.rs` / `executor.rs` — the runtime.
- `crates/core/src/routes/workflows.rs` — the CRUD + run + estimate API.
- `crates/core/src/workflow/http.rs` — the `http` node's SSRF posture (the allowlist + IP blocking).
- `crates/core/src/workflow/secrets.rs` — the BYO-secrets surface.
- `docs/coding-agents.md` — the coding-agent wedge (a complement to workflows).
