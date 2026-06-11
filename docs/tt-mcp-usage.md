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

## Write tools (off by default)

Two mutating tools ship in the MCP server, gated behind a write-enabled flag
that is **off by default** — when disabled they are omitted from `tools/list`
entirely and calling them returns `MethodNotFound` (-32601), so a read-only
server stays read-only:

- `add_route` — create a routing rule for your authenticated organization
- `apply_plan` — apply a previously-simulated plan (a `plan_run` UUID from
  `simulate_plan`)

Enable them with `tt mcp --allow-write`. The flag requires `DATABASE_URL`:
write tools are scoped to your verified org, so the CLI verifies the operator
key against the key store at boot and **refuses to start** (a clear error, not
a silent read-only fallback) when the database is unset/unreachable or the key
fails verification. Without the flag, `tt mcp` boots read-only as before.
Embedders of the `tt-mcp` crate opt in via `Server::with_write_enabled(true)`
+ `Server::register_write_tools(...)`.

## Day-0 resources

- `mcp://tokentrimmer/cost-ledger/last-7d`
- `mcp://tokentrimmer/inspect/baseline`

## Transports

| Transport | Flag | Status | Use it for |
| --- | --- | --- | --- |
| stdio | `tt mcp` (default) | current | subprocess launch by an MCP host (Claude Code, etc.) |
| Streamable HTTP | `tt mcp --transport http` | current | networked clients (MCP spec 2025-03-26) |
| HTTP+SSE | `tt mcp --transport sse` | **deprecated** | legacy clients only (MCP spec 2024-11-05) |

The stdio transport (default) requires no token — it communicates over the
parent-process pipe and is not exposed over the network.

### Streamable HTTP (recommended HTTP transport)

`tt mcp --transport http` serves the current MCP **Streamable HTTP** transport
on a single endpoint, `POST/GET/DELETE http://127.0.0.1:<port>/mcp` (default
port `31416`):

- **POST `/mcp`** — the client sends one JSON-RPC message in the body.
  - A *request* (carries `id`) is dispatched; the response is returned as a
    single `application/json` object, or — when the client's `Accept` is
    `text/event-stream` (and not also `application/json`) — streamed as one SSE
    `message` event before the stream closes. Clients should send
    `Accept: application/json, text/event-stream`.
  - The `initialize` response carries an `Mcp-Session-Id` header. The client
    **must** echo that header on every subsequent request.
  - A *notification* / *response* (no `id`) is accepted with `202 Accepted` and
    no body.
- **GET `/mcp`** — opens a server→client SSE stream (`text/event-stream`) for
  out-of-band server messages. Requires `Mcp-Session-Id`.
- **DELETE `/mcp`** — terminates the session named by `Mcp-Session-Id`
  (`204 No Content`; `404` if already gone).

Session rules: a request other than `initialize` that omits `Mcp-Session-Id` is
rejected `400 Bad Request`; an unknown or terminated session id is rejected
`404 Not Found` (the client should then re-`initialize`).

### Network transport security

Both HTTP transports (`--transport http` and the deprecated `--transport sse`)
enforce the following on every request:

- **Bearer auth** — each request must carry `Authorization: Bearer $TT_API_KEY`; missing or incorrect tokens are rejected with 401.
- **Loopback-only** — the `Host` header must resolve to `127.0.0.1`, `localhost`, or `::1`; non-loopback hosts are rejected with 403 (DNS-rebind defense).
- **Origin validation** — a browser-style `Origin` header, if present, must also be a loopback origin; cross-origin requests are rejected with 403.
- **Body size cap** — request bodies are limited to 1 MiB; larger payloads are rejected with 413.

Configure your MCP client's HTTP connection with the `Authorization` header set to `Bearer $TT_API_KEY`.

See `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md` for design.
