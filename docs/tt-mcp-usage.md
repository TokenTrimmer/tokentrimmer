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
- `get_repo_context` — given a coding task, return the most relevant repo files
  (ranked by symbol/import-graph + lexical match) with per-file symbol outlines,
  reasons, and token-budgeted inlined content — so a coding agent can skip
  codebase exploration entirely. **Read-only, fully local (no network).**

### `get_repo_context`

| Input | Type | Required | Default | Description |
| --- | --- | --- | --- | --- |
| `task` | string | **yes** | — | Plain-English description of the coding task. |
| `repo_path` | string | no | `.` (current dir) | Repo root to index. |
| `max_files` | integer | no | `12` | Maximum files to describe. |
| `token_budget` | integer | no | `6000` | Token cap for inlined file content. |

**Returns** a `ContextPack` object:

```json
{
  "files": [
    {
      "path": "src/auth.py",
      "summary": "symbols: authenticate",
      "reasons": ["matches 1 task term(s)", "imported by 3 file(s)"],
      "content": "def authenticate():\n    ..."
    }
  ],
  "token_estimate": 420,
  "note": "3 files ranked; 2 inlined within the 6000-token budget."
}
```

`content` is present for files that fit within the token budget (inlined in
rank order); remaining files carry `null` for `content` but always have an
outline in `summary` and their `reasons`. The engine is identical to
`tt context --format json` on the CLI — the two produce the same pack.

**How a coding agent uses it:** call `get_repo_context` with the task before
starting exploration. The returned outlines + inlined snippets eliminate most
`Read`/`grep` round-trips — Claude Code and Codex can be pre-prompted to call
this tool at session start (e.g. `"always call get_repo_context with the user's
first task before reading any files"`). The tool is read-only and local, so it
adds no latency beyond the indexing scan itself.

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

## Query-offload tools (gated by an operator config)

A model asked to analyze data pays input tokens for every row it reads. The
query-offload tools keep the DATA external and return only the COMPUTED
RESULT, deleting the data token class. (The trap is the opposite: pasting
rows into the prompt and then "offloading" compute over them is *worse* than
doing nothing — you pay the rows AND the tool round-trip. The tool's input
types leave that no useful channel: data can only be *named* by alias, and
every caller string is size-capped, so at most 4 KiB of literals can ever
reach an executor — useless as a data channel. See below.)

- `run_query` — run a bounded query against an operator-registered dataset:
  a single read-only `SELECT` for `postgres` datasets, or a structured
  aggregation (`count`/`sum`/`avg`/`min`/`max`/`count_distinct` with bounded
  `where`/`group_by`) for `file` (CSV/JSONL) datasets. Only the computed
  result enters context. Aggregation semantics worth knowing:
  `aggregate.limit` returns the **first N groups in group-key (alphabetical)
  order** — not top-N by aggregate value; an empty match without `group_by`
  still returns one row (`count` 0, `sum` 0, `avg`/`min`/`max` `null` —
  note `sum` keeps the mathematical empty-sum-is-0 convention, where SQL
  would return `NULL`).
- `list_datasets` — the registered aliases (kind, format, description, CSV
  columns). Never returns paths, roots, or DSN material.

### Enabling: the config file is the gate

There is **no `--allow-query` flag**. The tools register only when you pass
an operator dataset config: `tt mcp --query-config <path>` (or
`TT_MCP_QUERY_CONFIG`). No config → the tools are absent from `tools/list`
and calling them is `MethodNotFound` (-32601). An unreadable or invalid
config **refuses to start** (fail closed) — same posture as `--allow-write`.
Unlike the write tools, no `DATABASE_URL` is needed: query datasets are
local and have no org binding.

```toml
# query-datasets.toml — every entry is an explicit operator act.
[limits]                      # optional; defaults shown; hard ceilings 60000/10000/1048576
statement_timeout_ms = 5000
max_result_rows = 100
max_result_bytes = 65536

[files]
root = "/abs/data/dir"        # REQUIRED if any file dataset exists; no default

[[dataset]]
alias = "orders"              # [a-z0-9_-]{1,64}; what the model names
kind = "file"
path = "orders.csv"           # RELATIVE to files.root; absolute or `..` fails boot
format = "csv"                # or "jsonl"
description = "2025 orders export"

[[dataset]]
alias = "warehouse"
kind = "postgres"
dsn_env = "TT_QUERY_DSN_WAREHOUSE"   # DSN read from the env at boot — never inline
```

### Security model

The MCP caller is an LLM agent; its arguments are attacker-influencable
(prompt injection). The defenses, in order:

- **Alias indirection** — no tool parameter is a path, URL, or DSN. The
  model can only *name* operator-registered aliases, so path traversal and
  SSRF are unrepresentable at the tool boundary.
- **Type-level inline-data gate** — dataset handles implement no
  deserialization (pinned by a `compile_fail` test) and every caller string
  is size-capped (`query` ≤ 4 KiB, whole arguments object ≤ 8 KiB), so the
  parameters are useless as a data channel.
- **File datasets** — paths live only in the config, relative to the
  required `files.root`, canonicalized at boot AND at every call with a
  containment check (symlink-swap defense).
- **Postgres datasets** — a sqlparser allowlist (exactly one
  `SELECT`/`WITH..SELECT`; data-modifying CTE bodies like
  `WITH t AS (INSERT/UPDATE/DELETE/MERGE .. RETURNING ..)` and `SELECT INTO`
  rejected; escape/recon function families — `dblink*`, `pg_read_*`,
  `pg_ls_*`, `pg_stat_file`, `lo_*`/`loread`/`lowrite` — rejected anywhere
  in the AST; that denylist is defense in depth, **not exhaustive**: `READ
  ONLY` does not block reads, so unlisted privileged reads are stopped only
  by the role), then a `READ ONLY` transaction with `SET LOCAL
  statement_timeout`, streamed row/byte caps (every cell's raw wire size is
  charged against the byte budget before it is even decoded), and an
  unconditional `ROLLBACK`. **The real boundary is the role behind
  `dsn_env`**: point it at a dedicated read-only role
  (`default_transaction_read_only = on`, minimal grants) — the parser and
  transaction layers are defense in depth, not a substitute.
- **Hard result caps, never truncation** — a result over `max_result_rows`
  or `max_result_bytes` is an error telling the model to aggregate tighter.
  Truncation would be a silent wrong number and a bulk-export channel.
- **No code execution** — deliberately out of scope: an arbitrary code
  primitive reachable from model-controlled tool calls on an operator
  machine is an unacceptable escalation. The savings case holds with
  query-offload alone.

### Data residency

Execution is **local by design**: dataset rows never leave your machine.
Only the computed aggregate enters model context — and that aggregate IS
then sent to your model provider like any other tool result, so size the
result caps accordingly. This is exactly why the MVP is local-only: a
hosted/provider-side execution variant would route raw data through provider
infrastructure and is **not ZDR-eligible** (recorded as a hard constraint on
any future hosted variant).

### Verification (`verify: true`)

Re-executes the query a second time (cache bypassed for both runs), compares
results structurally, and returns `verified: true|false` plus per-run
`blake3` result hashes. A mismatch is surfaced with a warning and the first
run's result — never swallowed. This is the honest catch for
silent-wrong-number failures; note that nondeterministic SQL (`now()`,
`random()`, un-`ORDER`ed `LIMIT`) legitimately mismatches.

### Execution ledger + result cache

Every `run_query` call appends one JSONL line to the query ledger
(`TT_MCP_QUERY_LEDGER_PATH`, default `.claude/query-ledger.jsonl`):
`{ts, tool, dataset, kind, wall_ms, rows_scanned, result_rows, result_bytes,
cache, verify, verified, unit: "execution", priced_as_tokens: false}`.
Execution cost is a **distinct non-token cost class** — nothing converts it
to USD or token counts; `rows_scanned` is exact for file scans and `null`
for Postgres (no honest number exists in the MVP, so none is fabricated).
Cache-hit lines also carry `rows_scanned: null` — a hit scans nothing (the
file is only re-hashed to prove byte-identity), so summing `rows_scanned`
over ledger lines equals actual scan work. `cache` is
`hit`/`miss`/`bypass` for file datasets and `none` for Postgres (no cache
exists for live databases — `none` means *uncacheable*, not a 0% hit rate).
Failed executions (e.g. a statement-timeout abort) currently write **no**
ledger line — execution cost is under-counted for errored calls, never
over-counted (recorded follow-up). Ledger write failures are logged and
never fail the call. The ledger is readable via the
`mcp://tokentrimmer/query-ledger/recent` resource.

Results for **file** datasets are cached in-process, keyed on the blake3
hash of the file content plus the canonical query spec — any byte change to
the file misses. **Postgres results are never cached**: without an honest
content fingerprint a cache would serve stale wrong numbers.

## Day-0 resources

- `mcp://tokentrimmer/cost-ledger/last-7d`
- `mcp://tokentrimmer/inspect/baseline`
- `mcp://tokentrimmer/query-ledger/recent` (only with `--query-config`)

## Transports

| Transport | Flag | Status | Use it for |
| --- | --- | --- | --- |
| stdio | `tt mcp` (default) | current | subprocess launch by an MCP host (Claude Code, etc.) |
| Streamable HTTP | `tt mcp --transport http` | current | networked clients (MCP spec 2025-03-26) |
| HTTP+SSE | `tt mcp --transport sse` | **deprecated** | legacy clients only (MCP spec 2024-11-05) |

The stdio transport (default) requires no token — it communicates over the
parent-process pipe and is not exposed over the network. Each stdio request
line is capped at 1 MiB (mirroring the HTTP body cap): an oversized line is
drained without being buffered or parsed and answered with a parse error.

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
