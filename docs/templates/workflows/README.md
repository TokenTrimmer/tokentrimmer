# Workflow template pack

Five first-party TokenTrimmer workflow templates — cost-controlled, signable
graphs that run in the in-gateway workflow engine. Each is a `POST /v1/workflows`-
loadable JSON definition (see `docs/05-workflow-dsl-reference.md` for the DSL).

Every template's run is signable as a receipt:

```bash
# After a run completes:
curl -X POST https://api.tokentrimmer.com/v1/admin/workflow-runs/<run_id>/receipt/sign \
  -H "Authorization: Bearer $TT_ADMIN_KEY" -d '{"org_id":"<your-org>"}' > receipt.json

# Verify offline:
tt verify-receipt --receipt receipt.json --key-hex <the-key-you-trust>
```

The receipts are the differentiator: competitors can reproduce the savings
number, they can't sign it.

## Templates

| # | Template | What it demonstrates |
|---|---|---|
| 01 | `classify-and-route` | Classify intent → branch → down-route. The cost-control base pattern. |
| 02 | `doc-summarize-with-fetch` | Http node (allowlisted host) → summarize. The `allowed_hosts` + the `max_response_bytes` size cap. |
| 03 | `agent-with-tool-budget` | An agentic loop with `max_turns` + `max_cost_usd` + the runaway-repeat detector. The cost-controlled agent base. |
| 04 | `cost-capped-translation` | Detect language → branch → translate (or transform-passthrough). The Transform node + per-node `max_cost_usd`. |
| 05 | `retry-with-fallback` | Primary call → branch on empty/errored → fallback. Cost-aware error handling (the retry overhead is visible per the receipt, not hidden). |

## The cost-control levers in every template

- **`max_cost_usd` per Model/Agent node** — a hard ceiling; the loop terminates as `Incomplete` with `stop_reason = budget_exhausted` BEFORE a turn that would breach it.
- **`selection: { type: "route", route_ref: "<route>" }`** — down-routes the call to a cheaper same-family model (the catalog's flagship→mini mappings). Auto-pauses on recall-drop below 0.90.
- **`max_turns` (Agent)** — clamped to `[1, 32]`, default 8.
- **`max_response_bytes` (Http)** — bounds the fetched payload.
- **The runaway-repeat detector** — trips on `RUNAWAY_REPEAT_THRESHOLD` consecutive byte-identical steps (the fastest agent-spend leak; catches a stuck loop before a static cost cap would).

## Loading a template

```bash
curl -X POST https://api.tokentrimmer.com/v1/workflows \
  -H "Authorization: Bearer $TT_LIVE_KEY" \
  -H "content-type: application/json" \
  -d @docs/templates/workflows/01-classify-and-route.json
```

Then `POST /v1/workflows/:id/estimate` for an offline cost preview (no LLM
calls; computes the projected cost from the graph + pricing), or
`POST /v1/workflows/:id/runs` to run + stream the SSE events (`run.turn`,
`run.turn_cost`, `run.message`, `run.completed`).

## Related
- `docs/05-workflow-dsl-reference.md` — the DSL (node types, ModelSelection, edges, allowed_hosts, secrets).
- `docs/07-agent-runs-api-reference.md` — the agent loop + the stop reasons (the `agent` node's runtime).
- The receipt mint endpoint + `tt verify-receipt` — `docs/coding-agents.md`.
