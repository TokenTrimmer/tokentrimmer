# Track A — TokenTrimmer MCP Server (`tt mcp`)

**Status:** Draft 1
**Track:** A of six-track expansion
**Date:** 2026-05-28
**Depends on:** Track C (cost preview) for the cheapest tools; Inspect + Plan engines (already shipped)
**Consumed by:** Claude Code, Cursor, Zed, any MCP-compatible client

---

## 1. Problem

Track B (proxy) makes traffic visible. Track A (MCP) makes the **assistant itself** smarter. Instead of waiting for the assistant to call an API and then telling it what that cost, the MCP server exposes tools the assistant can call *during planning* to:

- `preview_cost` — "what does this prompt cost me right now, and what cheaper option exists?"
- `inspect_diff` — "what would `tt inspect` say about this proposed file diff before I write it?"
- `simulate_plan` — "if I apply this routing config to my last 1000 requests, what's the projected savings?"
- `lookup_semantic_cache` — "has the user asked something similar recently? (returns cached summary, not raw response)"
- `find_route_for` — "given this task description, what's the cheapest model with HIGH confidence?"

Plus MCP **resources** for read-only views (dashboard URLs, cost ledger excerpts, inspect baseline JSON) and **prompts** the user can invoke (`@cost-audit`, `@suggest-cheaper-model`).

This is the highest-leverage track: it shifts cost discipline from "after-the-fact" to "during reasoning."

## 2. Goals

1. Compliant with MCP 1.0 spec — works in any MCP client without TokenTrimmer-specific shims.
2. Stateless tools — each call is independent; server can crash and recover with no client-side impact.
3. Tools that need an org context resolve it via the `Authorization: Bearer tt_live_*` header injected by the MCP client config.
4. Latency: tool round-trip < 500ms for non-cache tools, < 100ms for cached ones.
5. Discoverable: `tools/list` returns helpful descriptions so the assistant uses them without prompting.

## 3. Non-goals

- Replacing Track B (the proxy). MCP runs alongside; it doesn't move traffic.
- Custom MCP transports (HTTPS, WebSocket). v1 ships stdio + SSE only — the two transports Claude Code and Cursor support.
- Authoring `@prompts` (the `prompts/` directory in MCP spec). v1 ships `tools/` and `resources/` only; prompts in v1.1.
- Custom auth. We use the same `tt_live_*` key the Gateway uses.

## 4. Architecture

```
crates/mcp/                              [NEW crate]
└── src/
    ├── lib.rs                           [public API for binary + tests]
    ├── server.rs                        [JSON-RPC over stdio loop]
    ├── transport/
    │   ├── stdio.rs                     [default — for Claude Code]
    │   └── sse.rs                       [optional — for Cursor]
    ├── tools/
    │   ├── mod.rs                       [Tool trait + registry]
    │   ├── preview_cost.rs              [calls /v1/preview, returns Track C JSON]
    │   ├── inspect_diff.rs              [runs inspect-core on a synthetic file]
    │   ├── simulate_plan.rs             [calls /v1/admin/plans (uses tt-plan-core)]
    │   ├── lookup_semantic_cache.rs     [embeds prompt, queries L2 cache, returns redacted summary]
    │   └── find_route_for.rs            [cheap LLM-free classifier + pricing table]
    ├── resources/
    │   ├── mod.rs                       [Resource trait + registry]
    │   ├── cost_ledger.rs               [mcp://cost-ledger/last-7d (read-only)]
    │   ├── inspect_baseline.rs          [mcp://inspect/baseline → current findings]
    │   └── plan_history.rs              [mcp://plan/history?last=N → recent plan_runs]
    ├── auth.rs                          [tt_live_* validation reusing crates/auth/]
    ├── error.rs                         [JSON-RPC error mapping]
    └── client.rs                        [reqwest to tt-api]

crates/cli/src/main.rs                   [modified — `tt mcp` subcommand boots the server]
```

## 5. CLI surface

```
tt mcp [OPTIONS]

  Run the TokenTrimmer MCP server. Default transport: stdio (for Claude Code).

OPTIONS:
  --transport <T>              stdio (default) | sse
  --sse-port <PORT>            With --transport sse. Default 31416.
  --tt-api-key <KEY>           API key. Falls back to TT_API_KEY env.
  --tt-api-base <URL>          Hosted API. Default https://tokentrimmer.fly.dev
  --tools <CSV>                Whitelist; default = all. e.g. `preview_cost,find_route_for`
  --read-only                  Disable tools that POST (simulate_plan). Resources still work.
  -h, --help                   Print help.
```

### 5.1 MCP client config snippet (printed by `tt mcp --print-config`)

For Claude Code (`~/.config/claude-code/config.json`):
```json
{
  "mcpServers": {
    "tokentrimmer": {
      "command": "tt",
      "args": ["mcp"],
      "env": { "TT_API_KEY": "tt_live_..." }
    }
  }
}
```

For Cursor:
```json
{
  "mcp.servers": {
    "tokentrimmer": {
      "command": "tt",
      "args": ["mcp", "--transport", "sse", "--sse-port", "31416"],
      "url": "http://localhost:31416"
    }
  }
}
```

## 6. Tool catalog (Day 0)

### 6.1 `preview_cost`

```json
{
  "name": "preview_cost",
  "description": "Estimate the cost of an LLM request before sending it. Returns current-model cost, cheaper-equivalent suggestions with quality risk bands, and cache hit probability. Use before sending an expensive prompt.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "model": { "type": "string" },
      "messages": { "type": "array" },
      "max_tokens": { "type": "integer" }
    },
    "required": ["model", "messages"]
  }
}
```
Implementation: forwards to Track C `POST /v1/preview`. Returns response JSON directly.

### 6.2 `find_route_for`

```json
{
  "name": "find_route_for",
  "description": "Given a task description in plain English, return the cheapest model that historically handles it with HIGH quality confidence. Use when the user is undecided which model to use.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "task_description": { "type": "string" }
    },
    "required": ["task_description"]
  }
}
```
Implementation: cheap regex/keyword classifier (classification / extraction / generation / code / agent), lookup against pricing × quality bands.

### 6.3 `inspect_diff`

```json
{
  "name": "inspect_diff",
  "description": "Run TokenTrimmer Inspect rules against a proposed file diff before writing. Returns findings (severity, rule_id, line, message). Use after composing an edit, before applying.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "file_path": { "type": "string" },
      "proposed_content": { "type": "string" }
    },
    "required": ["file_path", "proposed_content"]
  }
}
```
Implementation: writes proposed_content to a temp file, runs `tt_inspect_core::scan_path` on it, returns findings.

### 6.4 `simulate_plan` (admin tool — `--read-only` disables it)

```json
{
  "name": "simulate_plan",
  "description": "Project savings if a proposed routing config is applied to your historical traffic. Returns projected cost, savings, cache hit rate, quality risk band, with bootstrap CIs.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "proposed_config": { "type": "object" },
      "window_days": { "type": "integer", "default": 7 }
    },
    "required": ["proposed_config"]
  }
}
```
Implementation: forwards to existing `POST /v1/admin/plans`.

### 6.5 `lookup_semantic_cache`

```json
{
  "name": "lookup_semantic_cache",
  "description": "Check if a semantically-similar prompt has been answered recently (within retention window). Returns a redacted summary of the matching response (no raw text) to avoid context leakage.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "prompt": { "type": "string" }
    },
    "required": ["prompt"]
  }
}
```
Implementation: embeds, queries L2 cache table, returns `{"hit": true, "model_used": "...", "tokens_saved": 123, "occurred_at": "..."}` — never the raw cached body.

## 7. Resources catalog (Day 0)

- `mcp://tokentrimmer/cost-ledger/last-7d` — JSONL of last 7 days of `cost-ledger.jsonl` for the org.
- `mcp://tokentrimmer/inspect/baseline` — current inspect findings JSON.
- `mcp://tokentrimmer/plan/history?last=10` — last 10 plan_runs.

Resources are read-only; clients render them via their UI.

## 8. Auth

- `Authorization: Bearer tt_live_*` injected by client (env or config file).
- Server validates on first tool/resource call; caches the org_id for the process lifetime.
- Invalid key → JSON-RPC error code `-32001` with `"unauthorized"` data field.

## 9. Testing

| Layer | Tests |
|---|---|
| Unit (server) | JSON-RPC request → expected response for `initialize`, `tools/list`, `tools/call`. |
| Unit per tool | Mock `tt-api` httpmock; assert request shape sent + response parsing. |
| Integration (stdio) | Spawn `tt mcp`, send JSON-RPC over stdin/stdout, assert protocol compliance. |
| Integration (sse) | Spawn `tt mcp --transport sse`, connect via SSE client, same assertions. |
| Compatibility | Run against the MCP reference test suite (`mcp-test`) for protocol conformance. |

## 10. Rollout

1. Day 0: stdio transport + 4 tools (preview_cost, find_route_for, inspect_diff, lookup_semantic_cache) + 2 resources (cost-ledger, inspect-baseline).
2. Day 7: add `simulate_plan` (admin) + `mcp://plan/history` resource.
3. Day 14: add SSE transport (Cursor).
4. Day 30: add `prompts/` directory (`@cost-audit`, `@suggest-cheaper-model`).

## 11. References

- MCP spec: https://spec.modelcontextprotocol.io/specification/
- Track C spec: `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md`
- Existing inspect engine: `crates/inspect-core/src/lib.rs`
- Existing plan engine: `crates/plan-core/src/lib.rs`
- Existing L2 cache: `cloud/crates/api/src/cache/l2.rs`
