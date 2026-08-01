# `tt` gateway commands

Seven `tt` subcommands talk to a TokenTrimmer Gateway (hosted or self-hosted)
over its OpenAI-compatible API: `chat`, `advise`, `route`, `recipes`, `models`,
`embed`, and `login`. This page documents each — purpose, the flags that exist in
the binary, and a real example.

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

`tt login` is the easiest way to populate the file; it validates key shape but
stores the key as **not verified** so offline and self-hosted setup still works.
`tt whoami` shows what's resolved (masked), `tt whoami --check` verifies a
`tt_live_…` key against authenticated `GET /v1/capabilities`, and `tt logout`
clears the local key. Sandbox `tt_test_…` tokens have no durable server-side
identity, so the CLI reports their locally accepted format without claiming
remote verification.

---

## `tt context`

Surface the most relevant repo files for a coding task — before the agent
starts exploring. `tt context` walks the repo, extracts symbols and imports
(Python, TypeScript, JavaScript), builds an import graph, ranks files by
symbol/lexical match plus import centrality minus size, then assembles a
token-budgeted pack with per-file outlines, reasons, and inlined content.
**Fully local — no network, no embeddings, no key required.**

It shares the ranking engine with the `get_repo_context` MCP tool, so the
same context pack that a coding agent gets through MCP is reproducible with
this command.

### Flags

- `--task <TEXT>` **(required)** — plain-English description of the coding
  task (e.g. `"fix authenticate"`, `"add CSV export to orders"`).
- `[path]` — repo root to index. Default: current directory.
- `--format json|md` — output format. `md` (default) prints a
  human-readable Markdown report with code fences; `json` prints the raw
  `ContextPack` JSON (useful for piping into other tools).
- `--max-files <N>` — maximum number of files to describe. Default `12`.
- `--token-budget <T>` — token cap for inlined file content. Files are
  inlined in rank order until this budget is exhausted; the rest receive
  outlines only. Default `6000`.

### Examples

```bash
# Markdown overview of the most relevant files before starting a task
tt context --task "fix the authenticate flow" ./my-app

# Machine-readable JSON for a CI pre-check step
tt context --task "add CSV export" --format json --max-files 20

# Wider content window for a large refactor
tt context --task "migrate DB client to async" --token-budget 12000
```

> `tt context` is the zero-config way to cut a coding agent's exploration
> before it starts — paste the output into the context window or pipe it
> with `--format json`.

---

## `tt login`

Store an API key (and optionally a gateway base URL) in
`~/.tokentrimmer/credentials.toml` so the other commands and the SDKs can use it.
Success means the key was stored after a local format check, not that the
gateway accepted it. For a live key, run `tt whoami --check` afterward.

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

## `tt agent run`

Drive the gateway's **server-side** agent loop (`POST /v1/agent/runs`) over a
single prompt and print the result. Unlike `tt chat --tools` — where the CLI
drives every model→tool→model round-trip — the gateway owns the loop here
(mid-loop down-routing, judge-gated summarize, substep cache); the CLI just kicks
it off and resumes it if it pauses on a client tool. Requires a key.

The aggregate cost is read from the run's JSON `usage` (the agent endpoint emits
no per-turn `x-tokentrimmer-*` headers): the final answer prints to **stdout**,
the status/cost footer to **stderr**.

### Flags

- `--model <id>` — model to request; the gateway may still route it per turn.
  Default `gpt-4o-mini`.
- `--system <prompt>` — system prompt for the run.
- `--tools` — advertise the four read-only gateway tools (`find_route_for`,
  `preview_cost`, `inspect_diff`, `batch_savings`) so the loop can call them.
  They execute server-side, so the CLI never runs a tool itself.
- `--max-turns <n>` — server-side per-run turn cap (the gateway clamps to
  `1..=32`).
- `--tag <value>` — `X-TokenTrimmer-Tag` cost-attribution tag.
- `--tt-api-key <KEY>` / `--tt-api-base <URL>` — override the resolved key / base.

### Example

```bash
tt agent run "Which model is cheapest for bulk classification?" --tools --max-turns 6
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

## `tt workflow check`

Validate a `WorkflowDefinition` JSON file offline, project its cost, and
optionally diff against a prior estimate baseline. **Fully offline — no network
call, no API key required.**

`tt workflow check` does three things in order:

1. **Validate** — parses and structurally validates the definition (DAG shape,
   node references, model resolution). `ModelSelection::Auto` is always rejected
   at this stage — every model node must pin a concrete model id or a
   `route_ref`. All other pinned model ids are accepted without a registry call.
2. **Estimate** — projects the cost with a per-node breakdown. The estimate is a
   **linear upper-bound projection**: a Branch node counts the cost of *all* its
   arms (not just one); a Loop node is *not* multiplied by `max_iters`. Treat the
   total as a conservative ceiling, not an exact prediction. Nodes using a
   `Route` selection (unresolvable offline) get `cost_usd = null` and surface a
   warning instead of a hard error.
3. **Diff** (optional) — if `--baseline` is given, prints a per-node delta and a
   net `▲`/`▼` summary against a prior estimate dump.

### Synopsis

```
tt workflow check <file.json> [--inputs <json>] [--output <path>] \
                               [--baseline <prior.json>] [--fail-on-cost-increase]
```

### Flags

- `<file.json>` **(required)** — path to the `WorkflowDefinition` JSON file.
- `--inputs <json>` — JSON string substituted for `{{input}}` in node prompts
  during estimation. Use this to simulate a realistic trigger payload and get
  a more accurate token count.
- `--output <path>` — write the current `WorkflowEstimate` as JSON to this
  path. The resulting file is the format consumed by `--baseline`.
- `--baseline <prior.json>` — path to a prior estimate dump (written by
  `--output`). Prints a per-node cost diff and a net delta.
- `--fail-on-cost-increase` — exit non-zero when the projected cost exceeds the
  baseline. Requires `--baseline`. Intended for CI cost-regression gates.
- `--no-color` — disable colored/ANSI output.

### Caveats

- `ModelSelection::Auto` is rejected by validation — this is a hard error. Pin a
  model id (`"type": "model", "model": "gpt-4o-mini"`) or a named route ref
  before running the check.
- The cost estimate is a **linear sum**: a Branch counts all arms; a Loop's body
  is counted once regardless of `max_iters`. Use the result as a per-run ceiling
  when deciding whether a workflow is cost-safe, not as an exact runtime figure.

### CI cost-gate example

```yaml
# .github/workflows/cost-gate.yml
- name: Check workflow cost
  run: |
    tt workflow check flows/summarise.json \
      --baseline flows/summarise.baseline.json \
      --fail-on-cost-increase
```

```bash
# Capture today's estimate as the new baseline after an approved cost increase:
tt workflow check flows/summarise.json --output flows/summarise.baseline.json
git add flows/summarise.baseline.json && git commit -m "chore: update cost baseline"
```

---

## `tt route`

Manage per-org routing rules on the gateway (list / show / add / rm). Routing is
the core cost lever — see **`docs/routing-rules-guide.md`** for the full
condition/action model and worked examples. All four subcommands require a key
and a gateway with a routing store configured.

### Subcommands

```bash
tt route list                    # table of your routes (NAME, ROUTE, PRIO, STATUS)
tt route show <id>               # full JSON for one route
tt route rm <id>                 # delete one route
tt route add ...                 # create a route
tt route catalog enable          # install the curated down-route catalog
tt route catalog disable         # remove all catalog routes (user routes untouched)
tt route catalog status          # show active/paused state of each catalog route
```

`tt route add` needs a target — `--always <model>` (match all) or
`--from <m> --to <m>` — plus optional `--when-*` conditions and `--max-cost` /
`--disable-cache` / `--batch` / `--fallback` / `--priority` / `--name` /
`--disabled` modifiers. `--batch` sets the *advisory* batch-eligibility marker
(`then.batch`): matched traffic is still served and billed normally today, but
the forgone Batch-API discount is attributed for the future async Batch Lane —
never applied to streaming or interactive requests. There is no in-place
update: `rm` and re-`add` to change a route.

### `tt route catalog`

The zero-config way to get model-right-sizing savings. `enable` installs a
curated set of same-provider flagship→mini down-routes — one per major provider
model — so expensive flagship calls that don't require reasoning-class quality
are transparently served by a cheaper mini equivalent:

| From | To |
| --- | --- |
| `gpt-4o` | `gpt-4o-mini` |
| `claude-opus-4-7`, `claude-opus-4-8`, `claude-sonnet-4-6` | `claude-haiku-4-5` |
| `gemini-3.1-pro` | `gemini-3.1-flash-lite` |

Every catalog route is installed with:

- **`not_reasoning_class: true`** — the route only fires when the request is
  *not* classified as Math / Code / Legal / Medical. Reasoning-is-the-work
  traffic always reaches the original flagship.
- **`auto_pause: true` + `pause_floor_pass_rate: 0.92`** — the paired-judge
  circuit breaker watches quality continuously and pauses the route if the
  pass-rate drops below 92 %, reverting that model to its flagship
  automatically.
- **Low priority** — user-defined routes take precedence over catalog routes.

Catalog routes are normal routes (visible in `tt route list`, pausable and
deletable with `tt route rm`). They are named with a reserved `catalog:` prefix
so `disable` can remove exactly them without touching any user-defined routes.

`status` prints the current state (active / paused) of each installed catalog
route so you can tell at a glance whether a circuit breaker has tripped.

### Example

```bash
# Cheap default with cross-provider failover
tt route add --always gpt-4o-mini \
  --fallback claude-haiku-4-5 --fallback gemini-3.5-flash \
  --name cheap-with-failover
```

See `docs/routing-rules-guide.md` for the complete flag-to-field mapping.

## `tt recipes`

Curated, ready-to-apply savings route-sets. Instead of hand-building rules with
`tt route add`, pick a recipe that targets a common cost lane and apply its whole
route-set in one step. The recipe assets ship embedded in the binary, so `list`
and `show` work fully offline; only `apply` talks to the gateway.

### Subcommands

```bash
tt recipes list            # table: RECIPE, OPTIMIZES, LANE
tt recipes show <recipe>   # humanized route-set + savings lane + description
tt recipes apply <recipe>  # create the recipe's routes on the gateway
```

The five curated recipes:

| Recipe | Optimizes |
| --- | --- |
| `cheap-classification` | Short classification-style prompts → a small model. |
| `vision-gate` | Image requests pinned to a vision-capable model. |
| `cost-ceiling` | Downshift expensive calls + a pre-dispatch estimated-cost cap. |
| `outage-fallback` | Provider outages fail over to a backup chain. |
| `long-context-downshift` | Huge-context prompts → a cheaper long-context model. |

`apply` creates each route via the same `POST /v1/routes` endpoint `tt route add`
uses, so it requires a key (`tt login --token <KEY>` or `TT_API_KEY`) and a
gateway with a routing store configured. Without a key it fails with an
actionable message and a non-zero exit — it never silently "applies" nothing.
After applying, inspect or remove the created routes with `tt route list` /
`tt route rm <id>`.

### Example

```bash
tt recipes show cost-ceiling      # preview the rules before committing
tt recipes apply cost-ceiling     # create them on your gateway
```
