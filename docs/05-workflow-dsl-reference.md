# Workflow DSL reference (`/v1/workflows`)

TokenTrimmer's workflow engine (`crates/core/src/workflow/`) is an in-gateway,
durable, cost-controlled workflow runtime. A workflow is a JSON graph of nodes
(LLM calls, agent loops, HTTP calls, transforms, branches, sub-workflows) that
the gateway executes server-side, applying routing, caching, cost accounting,
and budget caps while rolling the cost up into one `saved_usd` estimate per
run. Eligible terminal records can be minted on demand as signed estimates;
they are not automatic per-step receipts or provider-invoice reconciliation.

This page documents the DSL (the node types, the `ModelSelection`, edges,
`allowed_hosts`, `secrets`), the CRUD + run API, and the SSE event shape. It's
grounded in `crates/core/src/workflow/types.rs` + `routes/workflows.rs`.

## The workflow definition

A workflow is a `WorkflowDefinition` with nodes, edges, entry/exit, an
allowlist of outbound HTTP hosts, optional out-of-band triggers, and optionally
named secrets. Stored via
`POST /v1/workflows` (CRUD in `routes/workflows.rs`); run via
`POST /v1/workflows/:id/runs`; estimated (offline cost preview) via
`POST /v1/workflows/:id/estimate`.

Definition updates are strict at the top level: unknown fields are rejected.
Clients that read and then update a workflow must preserve `metadata` and
`triggers` even when they do not render controls for them.

## Node types (`NodeKind`)

`#[serde(tag = "type", rename_all = "snake_case")]` — each node is a JSON
object with a `"type"` discriminant + the variant's fields inlined via
`#[serde(flatten)]`. The `id` field is the node's graph identifier.

| `type` | What it does | Cost-relevant fields |
|--------|---|---|
| `trigger` | Entry-point; receives the workflow's external input. | — |
| `model` | A single LLM call. `selection` picks the model/route; `prompt` is the template; optional `max_output_tokens` is forwarded as the provider completion limit. | `max_output_tokens`, `max_cost_usd` |
| `agent` | An agentic multi-turn loop with tool access. `max_turns` (default `DEFAULT_MAX_TURNS = 8`), optional per-turn `max_output_tokens`, `max_cost_usd`, `tools`. | `max_turns`, `max_output_tokens`, `max_cost_usd` |
| `transform` | A deterministic expression transform (no LLM call). `expr` is the expression string. | — (no LLM cost) |
| `branch` | A conditional branch; exactly one outgoing edge is followed. `cond` (expression), `when_true`/`when_false` (node ids). | — |
| `output` | Terminal output-collection node. | — |
| `http` | An outbound HTTP call to an allowlisted external API. `method`, `url`, `headers`, `body`, `max_response_bytes`. The `url` host MUST be a static literal in `allowed_hosts` (default-deny); only path/query/headers/body may contain `{{template}}` tokens. | `max_response_bytes` (size cap) |
| `sub_workflow` | Execute another stored workflow as a nested child. `workflow_id`, `version` (unused at MVP — always latest). Cost + baseline roll up so `saved_usd` derives without double-counting. Nested workflows remain compatible for uncapped runs, but capped static admission rejects them because their future graph is not fully known. | nested graph (not capped-admission eligible) |
| `loop` | A bounded loop — runs the `body_workflow_id` sub-workflow up to `max_iters` times, re-checking `cond` (Branch syntax) before each iteration. Termination is GUARANTEED by `max_iters`; `cond` is early-exit. It remains compatible for uncapped runs but is not capped-admission eligible. | `max_iters` |

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

## Out-of-band triggers

`triggers` is optional. An omitted or empty array means a workflow can only be
started by a human/API run. The two current invokers are:

```json
[
  { "type": "schedule", "interval": "6h" },
  { "type": "webhook", "token_id": "ops_sync_1" }
]
```

- `schedule.interval` uses bounded duration components (`1h`, `6h`, `1d`, or
  `1d6h`), with a one-hour minimum and 30-day maximum for new or updated
  definitions. The hosted dispatcher normally picks due work up on an
  approximate hourly sweep rather than at an exact wall-clock time; startup,
  leader acquisition, and the configured sweep profile can add pickup jitter.
  One schedule is allowed per workflow. Existing persisted sub-hour schedules
  are not rewritten or disabled by this validation change; operators must
  explicitly inventory and migrate them to one hour or longer.
- `webhook.token_id` is a non-empty URL-safe identifier. The server derives and
  verifies the signed webhook URL; secrets never live in the definition.

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
  see `workflow/secrets.rs` + the workflow-secrets migration. Invalid names are
  rejected with the definition; missing or unusable references fail closed
  before the current definition executes any node. Recursive child definitions
  preflight when the child begins, after any earlier parent work.

## API surface

| Method + path | What |
|---|---|
| `POST /v1/workflows` | Create a workflow (CRUD; `id` + `version` optional). |
| `GET /v1/workflows` | List workflow definitions (metadata). |
| `POST /v1/workflows/:id/estimate` | Offline cost preview (no LLM calls; computes the projected cost from the graph + pricing). |
| `POST /v1/workflows/:id/runs` | Run synchronously (returns the run + the rolled-up `saved_usd`). |
| `GET /v1/workflows/:id/runs` | List recent durable runs for exactly that org-owned workflow. |
| `GET /v1/workflows/runs/:run_id` | Read one org-scoped durable run and its immutable definition version. |
| `GET /v1/workflows/runs/:run_id/nodes` | Read up to 500 best-effort node-journal rows, labeled from the exact executed definition. New rows include gateway node-envelope timing; legacy rows expose only post-run persistence time. Neither is provider-attempt timing or replay. |
| `GET /v1/workflows/secrets` | List up to 500 org-scoped secret names, decryptability states, and timestamps; never returns values/ciphertext and is `private, no-store`. |
| `POST /v1/workflows/secrets` | Store or rotate a 1–65,536-byte org-scoped secret value. |
| `DELETE /v1/workflows/secrets/:name` | Idempotently delete one org-scoped secret; stored versions remain intact and fail closed if they still reference it. |

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
double-counting. Nested graphs retain their legacy uncapped cost rollup, while
capped static admission rejects loops and sub-workflows before dispatch.

An eligible retained terminal run can be minted on demand as a workflow receipt —
`POST /v1/admin/workflow-runs/{run_id}/receipt/sign` returns a frozen,
Ed25519-signed estimate + a shareable verify URL. It is not an automatic receipt
for every completed run or provider-invoice reconciliation. Current mints use
`wfr:v3` without a sampled quality verdict and `wfr:v4` with one:

```
wfr:v3|<org>|<workflow_id>|<run_id>|<cost_micros>|<baseline_micros>|<saved_micros>|<signed_request_delta_micros>|<formula>|<eligible_requests>|<measured_requests>|<status>
wfr:v4|<the same v3 fields>|<quality_verdict>
```

The formula is exactly `tt.request-delta-estimate.v1`; coverage must be
nonempty and complete (`measured_requests == eligible_requests`), and
`saved_micros` is the positive-only projection
`max(signed_request_delta_micros, 0)`. A regression therefore remains visible
as a negative signed delta instead of becoming an apparent zero-savings run.
An incomplete or empty cohort does not mint. Already-frozen `wfr:v1`/`wfr:v2`
receipts retain their historical canonical bytes and remain verifiable.

### Bounded budget admission

`max_output_tokens` is an optional positive integer on `model` and `agent`
nodes. When present, it is sent as the completion-token limit on each provider
request (and used by the offline cost estimate). Omitting it keeps historical
provider-default output behavior, so existing stored definitions remain
compatible.

When a run requests `max_cost_usd` (from its request or definition budget),
the gateway performs a fail-closed static admission check before creating a run
record or dispatching a provider request. Every `model`/`agent` node must then
have an explicit positive `max_output_tokens`; prompts may contain only
`{{input}}` references; selections must be statically priceable; agents must
have `max_turns: 1` and no tools; and loops/sub-workflows are not admissible.
Tool-bearing agents fail closed because the preview does not price serialized
tool schemas or gateway-tool work. The directional projection must be within
the requested cost value. A rejected definition can still run without
`max_cost_usd`, preserving the legacy uncapped contract.

After admission, ready `model`/`agent` siblings in a capped wave run in stable
sequence rather than concurrently. Before each launch, the engine reserves its
priceable single-turn preview against the budget remaining after prior actual
node cost, then settles that reservation to the returned cost before considering
the next sibling. The node receives the lesser of its own cap and the run value
remaining. After normal route selection and request shaping, the gateway prices
that final request again and fails closed before provider work if it is unknown
or no longer fits. Capped workflow nodes make one provider attempt: route
fallbacks, retries, shadow/panel/workflow fan-out, quality judging, and diff
re-emission are disabled because they are not represented by the single-turn
reservation. This remains an in-memory directional reservation, not a hard
runtime or provider-invoice ceiling: a provider call already started can settle
above its estimate. Uncapped waves retain concurrent execution.

### Verifying a workflow receipt

The mint returns a self-contained `VerifyReceiptResponse` (from the public GET
endpoint `GET /v1/workflow-runs/{run_id}/receipt?expires=&sig=`) exposing every
canonical-payload field — `org_id`, `workflow_id`, `run_id`, `cost_micros`,
`baseline_micros`, `saved_micros`, the signed request-delta formula, result, and
coverage fields, `status`, `canonical_version`, `quality_verdict`,
`signature_hex`, + the embedded `verifying_key_hex`. New evidence fields are
null/absent on legacy receipts. Any party with the share URL can reconstruct the
canonical string + check the Ed25519 signature with that key — offline, no
TokenTrimmer network call beyond fetching the receipt.

`tt verify-receipt` verifies all four currently supported families offline:
**compression** (`vcr:v1|`), **cache-hit** (`l2:v1|`), **workflow-run**
(`wfr:v1|` through `wfr:v4|`), and top-level **agent-run** (`arr:v1|` /
`arr:v2|`). ARR
deliberately has no `workflow_id`; see
[`07-agent-runs-api-reference.md`](07-agent-runs-api-reference.md) for its
canonical fields and mint boundary. Supply a verifying key obtained and trusted
out of band; the embedded key can establish only self-consistency. A successful
signature check establishes that the supplied key signed an unchanged receipt,
not issuer identity, provider usage, or invoice reconciliation.

The generated machine-readable contract index is
[`receipt-spec/receipt-contracts.manifest.json`](receipt-spec/receipt-contracts.manifest.json),
with the WFR structural schema at
[`receipt-spec/wfr-receipt.schema.json`](receipt-spec/wfr-receipt.schema.json).
Its checked-in [v1](receipt-spec/wfr-v1.golden.json),
[v2](receipt-spec/wfr-v2.golden.json),
[v3](receipt-spec/wfr-v3.golden.json), and
[v4](receipt-spec/wfr-v4.golden.json) golden vectors are verified by both the
public canonical builder and `tt verify-receipt`; they pin JSON field names,
canonical bytes, and Ed25519 encoding without asserting anything about the
issuer or provider-invoice evidence. Rust generation drift and an independent
JavaScript forged-fixture verifier are blocking CI checks.

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
