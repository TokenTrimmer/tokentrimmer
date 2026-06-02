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

See `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md` for design.
