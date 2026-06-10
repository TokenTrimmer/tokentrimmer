# `tt` gateway commands

Six `tt` subcommands talk to a TokenTrimmer Gateway (hosted or self-hosted) over
its OpenAI-compatible API: `chat`, `advise`, `route`, `models`, `embed`, and
`login`. This page documents each — purpose, the flags that exist in the binary,
and a real example.

For `tt gateway` (running the gateway itself), `tt inspect`, `tt plan`,
`tt init`, `tt mcp`, `tt proxy`, and `tt retrieval`, see `README.md`,
`GETTING_STARTED.md`, and the other `docs/tt-*-usage.md` guides. The routing
rule model is its own guide: `docs/routing-rules-guide.md`.

> **Hosted gateway launching soon** *(as of 2026-06-10)* — the default base
> `https://api.tokentrimmer.com` is not live yet. Self-host with Docker today
> (`tt gateway`, or the Docker image — see `GETTING_STARTED.md`) and point these
> commands at it with `--tt-api-base http://localhost:8080` or `TT_API_BASE`.

## Authentication

Every command on this page (except `tt models`, which can run logged-out)
resolves an API key with this precedence: **`--tt-api-key` flag → `TT_API_KEY`
env → `~/.tokentrimmer/credentials.toml` → none**. The gateway base URL resolves
the same way (`--tt-api-base` → `TT_API_BASE` → `~/.tokentrimmer/config.toml` →
the built-in default `https://api.tokentrimmer.com`).

`tt login` is the easiest way to populate the file; `tt whoami` shows what's
resolved (masked), and `tt logout` clears it.

---

## `tt login`

Store an API key (and optionally a gateway base URL) in
`~/.tokentrimmer/credentials.toml` so the other commands and the SDKs can use it.

With no `--token`, it runs the **browser-assisted flow**: it opens the dashboard
keys page so you can mint a key, then reads the pasted key from a hidden prompt.
This path needs an interactive terminal.

### Flags

- `--token <KEY>` — store this `tt_live_…` / `tt_test_…` key non-interactively.
  Use `--token -` to read the key from stdin (good for CI / scripts).
- `--base-url <URL>` — persist a gateway base URL alongside the key (e.g. your
  self-hosted gateway).
- `--no-browser` — don't open a browser; just print the URL to visit (headless /
  SSH).

### Examples

```bash
# Interactive: opens the dashboard, paste the key back
tt login

# Non-interactive (CI): read the key from stdin, never echo it
echo "$TT_KEY" | tt login --token -

# Point the CLI at a self-hosted gateway and store the key for it
tt login --token tt_live_... --base-url http://localhost:8080
```

> `tt logout` removes the local key only — it does **not** revoke it
> server-side. Revoke a compromised key in the dashboard.

---

## `tt models`

List the gateway's model catalog: context windows, capabilities, and per-million
pricing. Reads `GET /v1/models`, which is **public** — `tt models` works without
a key (it sends one if you have it).

### Flags

- `--tt-api-key <KEY>` / `--tt-api-base <URL>` — override the resolved key / base.

### Example

```bash
$ tt models
MODEL           PROVIDER   CONTEXT  CAPS         $IN/1M  $OUT/1M
gpt-4o-mini     openai     128k     text,tools   0.15    0.60
claude-haiku-4-5 anthropic 200k     text,vision  -       -
…
N models
```

Context is shown compactly (`128k`, `1M`); a `-` price means the gateway has no
pricing row for that model.

---

## `tt chat`

An interactive chat REPL routed through the gateway. It streams the reply and
prints a per-turn cost footer (and `saved …%` when the gateway routed/cached the
request to something cheaper). Requires a key.

### Flags

- `--model <id>` — model to request; the gateway may still route it. Default
  `gpt-4o-mini`.
- `--system <prompt>` — system prompt for the conversation.
- `--resume <name>` — resume a session saved earlier (see `/save`, `/sessions`).
- `--tools` — enable agentic tool-calling from the start. The model can call
  three read-only tools: `find_route_for`, `preview_cost`, `inspect_diff`.
- `--max-context <tokens>` — token budget for context management. Default is the
  per-model context window (fetched from `tt models`).
- `--tt-api-key <KEY>` / `--tt-api-base <URL>` — override the resolved key / base.

### In-REPL commands

`/help`, `/clear`, `/model [m]`, `/system [s]`, `/editor` (compose in
`$VISUAL`/`$EDITOR`), `/retry`, `/copy` (OSC52 clipboard), `/tools [on|off]`,
`/context [n]`, `/trim`, `/save [name]`, `/resume <name>`, `/sessions`, `/cost`,
`/exit` (or Ctrl-D).

### Example

```bash
tt chat --model gpt-4o --system "Be terse." --tools
```

---

## `tt embed`

Embed text through the gateway's `POST /v1/embeddings` and print a one-line cost
summary — or, with `--json`, the full embeddings response. Requires a key.

One positional arg embeds a single string; multiple args embed a batch; with no
args it reads the text from **stdin**.

### Flags

- `--model <id>` — embedding model. Default `text-embedding-3-small`.
- `--dimensions <n>` — reduce output dimensions (Matryoshka models).
- `--encoding-format <fmt>` — wire encoding (e.g. `float`, `base64`).
- `--cost-limit <usd>` — reject with `402` if the estimated cost exceeds this.
- `--json` — print the full `EmbeddingsResponse` JSON to stdout (the summary then
  goes to stderr).
- `--tt-api-key <KEY>` / `--tt-api-base <URL>` — override the resolved key / base.

### Examples

```bash
# Single string → cost summary
tt embed "the quick brown fox"
# text-embedding-3-small · 1 embedding × 1536 dims · $0.0000

# Batch, reduced dimensions, JSON vectors to stdout
tt embed "first" "second" --dimensions 256 --json

# From stdin
cat notes.txt | tt embed --model text-embedding-3-large
```

---

## `tt advise`

An AI cost/routing advisor. It scans a repo for model-id usage, then runs one
tool-grounded turn that asks the model to recommend optimizations — grounding
every number with the same three tools as `tt chat --tools`
(`preview_cost`, `find_route_for`, `inspect_diff`). **Read-only**: it never edits
your code. Requires a key.

### Flags

- `[path]` — repo path to scan. Default: the current directory.
- `--describe <text>` — describe what the app does, for extra advisor context.
- `--model <id>` — advisor model. Default `gpt-4o-mini`.
- `--tt-api-key <KEY>` / `--tt-api-base <URL>` — override the resolved key / base.

The scanner skips vendor directories (`node_modules`, `target`, `.venv`, …) and
over-large files, and detects model ids across common source extensions.

### Example

```bash
tt advise ./my-app --describe "a customer-support chatbot"
```

---

## `tt route`

Manage per-org routing rules on the gateway (list / show / add / rm). Routing is
the core cost lever — see **`docs/routing-rules-guide.md`** for the full
condition/action model and worked examples. All four subcommands require a key
and a gateway with a routing store configured.

### Subcommands

```bash
tt route list           # table of your routes (NAME, ROUTE, PRIO, STATUS)
tt route show <id>      # full JSON for one route
tt route rm <id>        # delete one route
tt route add ...        # create a route
```

`tt route add` needs a target — `--always <model>` (match all) or
`--from <m> --to <m>` — plus optional `--when-*` conditions and `--max-cost` /
`--disable-cache` / `--fallback` / `--priority` / `--name` / `--disabled`
modifiers. There is no in-place update: `rm` and re-`add` to change a route.

### Example

```bash
# Cheap default with cross-provider failover
tt route add --always gpt-4o-mini \
  --fallback claude-haiku-4-5 --fallback gemini-3.5-flash \
  --name cheap-with-failover
```

See `docs/routing-rules-guide.md` for the complete flag-to-field mapping.
