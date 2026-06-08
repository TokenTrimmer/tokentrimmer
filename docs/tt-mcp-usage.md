# tt mcp

MCP server exposing TokenTrimmer intelligence to MCP-compatible clients.

## Quick start with Claude Code

```json
// ~/.config/claude-code/config.json
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

## Day-0 tools

- `preview_cost` — cost projection (Track C engine)
- `find_route_for` — cheapest model for a plain-English task
- `inspect_diff` — run Inspect rules on a proposed file diff
- `lookup_semantic_cache` — check if a similar prompt was answered recently

## Day-0 resources

- `mcp://tokentrimmer/cost-ledger/last-7d`
- `mcp://tokentrimmer/inspect/baseline`

## SSE transport security

The SSE transport (`tt mcp --transport sse`) enforces the following on every request:

- **Bearer auth** — each request must carry `Authorization: Bearer $TT_API_KEY`; missing or incorrect tokens are rejected with 401.
- **Loopback-only** — the `Host` header must resolve to `127.0.0.1`, `localhost`, or `::1`; non-loopback hosts are rejected with 403 (DNS-rebind defense).
- **Origin validation** — a browser-style `Origin` header, if present, must also be a loopback origin; cross-origin requests are rejected with 403.
- **Body size cap** — POST `/messages` bodies are limited to 1 MiB; larger payloads are rejected with 413.

Configure your MCP client's SSE connection with the `Authorization` header set to `Bearer $TT_API_KEY`. The stdio transport (default) requires no token — it communicates over the parent-process pipe and is not exposed over the network.

See `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md` for design.
