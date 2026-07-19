# Getting Started with TokenTrimmer

> The cost layer for LLM applications. See, plan, and optimize every token across OpenAI, Anthropic, Google, and 7+ other providers.

This guide takes you from zero to a working integration in five minutes (hosted) or one `docker run` (self-host), then shows a copy-paste example for every TokenTrimmer surface.

---

## What is TokenTrimmer?

TokenTrimmer sits between your application and the LLM providers you already use. It speaks the **OpenAI-compatible API**, so adopting it is usually a one-line change: point your existing SDK at the TokenTrimmer base URL. In return you get intelligent routing, multi-layer caching, and per-request cost telemetry — without changing your application code.

This repository is the **open-source core** (Apache 2.0). The closed-source hosted dashboard and reporting product live in a separate repo.

### The four products

| Product | What it does | Where it lives |
|---|---|---|
| **Gateway** | OpenAI-compatible HTTP proxy: routing, exact + semantic caching, per-request cost headers | `crates/core` + `crates/providers/*`, run via `tt gateway` |
| **Inspect** | Static analyzer that scans your codebase for token-waste patterns | `crates/inspect-*`, run via `tt inspect` |
| **Plan** | Deterministic replay simulator that projects how a config change would have affected cost/latency, with bootstrap confidence intervals | `crates/plan-core`, run via `tt plan` |
| **Reporting** | Dashboard, weekly digest, monthly PDF | Hosted-only (closed source) |

Everything in the OSS core ships as a single binary called **`tt`** (built from the `tt-cli` crate). One binary, many subcommands.

---

## Prerequisites

Pick the path that matches what you want to do.

| You want to… | You need |
|---|---|
| Use the **hosted** Gateway | A TokenTrimmer API key (`tt_live_…`) from your dashboard. Nothing to install except your existing OpenAI SDK (or our thin SDK). |
| **Self-host** the Gateway via Docker | Docker. Optionally Postgres + Redis for persistence/caching (the gateway runs without them, in degraded "dev mode"). Provider API keys: per-org in the credential store, or callers bring their own as the Bearer token (operator env keys like `OPENAI_API_KEY` are only served behind `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1`). |
| Use **`tt inspect`**, **`tt plan`**, **`tt init`** locally | The `tt` binary. Build from source (Rust 1.88) or use the Docker image. |
| **Build from source / contribute** | Rust **1.88** (pinned in `rust-toolchain.toml`; `rustup` auto-installs it). For local end-to-end services: Docker (for `make dev`). |

> **Toolchain note:** the canonical pinned version is **1.88.0** (`rust-toolchain.toml`). If you have `rustup` installed, it picks this up automatically — you do not need to install a toolchain by hand.

---

## 5-minute hosted quickstart (one-line base_url swap)

> **Hosted gateway launching soon** *(as of 2026-06-10)* — `https://api.tokentrimmer.com` is not live yet. Self-host with Docker today (see the [Self-host path (Docker)](#self-host-path-docker) section below) and point your SDK at `http://localhost:8080/v1`. This section is the integration you will use once the hosted endpoint goes live.

You already have OpenAI-style code. Change the base URL and the key. That's it.

### Option A — your existing OpenAI SDK (no new dependency)

```python
from openai import OpenAI

client = OpenAI(
    api_key="tt_live_...",                       # your TokenTrimmer key
    base_url="https://api.tokentrimmer.com/v1",  # the only change
)

resp = client.chat.completions.create(
    model="claude-sonnet-4-6",                   # any model your Gateway routes
    messages=[{"role": "user", "content": "Hello"}],
)
print(resp.choices[0].message.content)
```

The Gateway authenticates via `Authorization: Bearer tt_live_…` and returns cost/cache metadata on `X-TokenTrimmer-*` response headers (`x-tokentrimmer-cost-usd`, `x-tokentrimmer-saved-usd`, `x-tokentrimmer-cache`, `x-tokentrimmer-trace-id`, …).

### Option B — the TokenTrimmer SDK (surfaces cost metadata for you)

The SDK is a drop-in `openai.OpenAI` subclass; the default base URL is already the hosted Gateway, and it parses the cost headers onto a `.tt` attribute.

> **Not yet on PyPI** — published packages land at launch. Until then, install from git:

```bash
pip install "git+https://github.com/TokenTrimmer/tokentrimmer.git#subdirectory=sdk-python"
```

```python
from tokentrimmer import TokenTrimmer

client = TokenTrimmer(api_key="tt_live_...")

resp = client.chat.completions.create(
    model="claude-sonnet-4-6",
    messages=[{"role": "user", "content": "Hello"}],
    tt_tag="feature=chat-support",   # optional: per-feature cost attribution
)

print(resp.choices[0].message.content)
print(f"cost  ${resp.tt.cost_usd:.4f}")
print(f"saved ${resp.tt.saved_usd:.4f}")
print(f"cache {resp.tt.cache}        # hit-l1 | hit-l2 | miss | none")
print(f"trace {resp.tt.trace_id}")
```

**Requires:** a `tt_live_…` key. Nothing else.

---

## Self-host path (Docker)

The Gateway runs as `tt gateway` and listens on **port 8080**. Configuration is read entirely from **environment variables** — there is no YAML config file.

### Build and run

```bash
# Build the image (produces the `tt` binary; default CMD is `gateway`)
docker build -t tokentrimmer/tt-cli:dev .

# Run the Gateway — env-only config. Without DATABASE_URL there is no key
# store: the gateway binds loopback by default and refuses a non-loopback
# bind unless you explicitly opt in. A container port mapping needs a
# non-loopback bind, hence the two extra vars:
docker run --rm -p 8080:8080 \
  -e TT_BIND_ADDR=0.0.0.0 \
  -e TT_ALLOW_UNAUTHENTICATED_PUBLIC_BIND=1 \
  tokentrimmer/tt-cli:dev gateway
```

> **Security:** the opt-in runs the gateway as an *unauthenticated BYO-key
> passthrough* — anyone who can reach the port can use it, but each caller
> must supply their **own** upstream provider key as the Bearer token. The
> operator's `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` env vars are **never**
> served without a key store — and even with one, only behind the explicit
> `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1` opt-in (see the table below). For
> verified `tt_live_*` keys and per-org provider credentials, set
> `DATABASE_URL` (and `TT_MASTER_KEY`).

Smoke-test it:

```bash
curl http://localhost:8080/health
```

### What the Gateway uses (all optional, all best-effort at boot)

Every external dependency is best-effort: if it's missing or unreachable, the Gateway logs a warning and continues in a degraded mode rather than crash-looping.

| Env var | Purpose | If unset |
|---|---|---|
| `PORT` | Bind port | Defaults to `8080` |
| `TT_BIND_ADDR` | Bind IP | Defaults to `0.0.0.0` with `DATABASE_URL`, `127.0.0.1` without (fail-closed) |
| `TT_ALLOW_UNAUTHENTICATED_PUBLIC_BIND` | Set to `1` to allow a non-loopback bind **without** a key store (unauthenticated BYO-key passthrough) | A non-loopback `TT_BIND_ADDR` without `DATABASE_URL` refuses to start |
| `DATABASE_URL` | Postgres: API-key verification, request logs, routing rules, provider-credential store | Runs without persistence; `tt_live_*` keys pass through **unverified** (dev mode), binds loopback by default, and env provider keys are never served |
| `REDIS_URL` | L1 exact-match cache (use `rediss://` native, **not** an HTTP REST URL) | L1 cache disabled |
| `TT_MASTER_KEY` | XChaCha20-Poly1305 root key for the Postgres provider-credential store. Generate with `openssl rand -hex 32` | Postgres credential store disabled; **no** provider credentials are served unless `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1` restores the env-only dogfood mode |
| `TT_ALLOW_ENV_CREDENTIAL_FALLBACK` | Set to `1` to serve the operator's env provider keys (below) to orgs with no stored credential — **single-tenant self-host / dogfood only**. On a multi-tenant gateway this lets every org spend on the operator's keys (provider-ToS / resale exposure) | **BYO-only (default):** with `DATABASE_URL` + `TT_MASTER_KEY` configured, an org with no stored credential for the requested provider gets an actionable `missing_provider_credential` error — never the operator's env keys |
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `MISTRAL_API_KEY`, `GROQ_API_KEY`, `TOGETHER_API_KEY`, `OPENROUTER_API_KEY` | Upstream provider credentials (single-tenant fallback; only served when `DATABASE_URL` configures a key store **and** `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1` is set) | That provider can't be reached (callers can still pass their own key as the Bearer token) |
| `OTEL_EXPORTER_OTLP_ENDPOINT`, `SENTRY_DSN` | Observability | Disabled |
| `RUST_LOG` | Log filter, e.g. `info,tt_core=debug` | Default tracing filter |

Copy `.env.example` to `.env.local` and fill in what you need. **Never commit real secrets** — `.env.*` (except `.env.example`) is gitignored.

### Point your app at your self-hosted Gateway

```python
from tokentrimmer import TokenTrimmer
client = TokenTrimmer(
    api_key="sk-...",                       # your provider key, passed through
    base_url="http://localhost:8080/v1",    # your self-hosted Gateway
)
```

---

## Building the `tt` binary from source

You need `tt` locally for `inspect`, `plan`, and `init`.

```bash
# rustup auto-installs the pinned toolchain (1.88) from rust-toolchain.toml
git clone https://github.com/TokenTrimmer/tokentrimmer.git
cd tokentrimmer

# Build just the CLI (binary is named `tt`)
cargo build --release -p tt-cli
./target/release/tt --help
```

To bring up local services (Postgres + pgvector, Redis, MinIO, Mailpit) for end-to-end work:

```bash
make dev      # Postgres :5432, Redis :6379, MinIO console :9001, Mailpit UI :8025
make dev-down # tear down
```

---

## Surface-by-surface examples

Every example below uses real commands and flags from the `tt` CLI.

### 1. Gateway — `tt gateway`

Run the OpenAI-compatible proxy.

```bash
PORT=8080 ./target/release/tt gateway
```

Endpoints exposed: `GET /health`, `GET /v1/models`, `POST /v1/chat/completions`, `POST /v1/preview`, and `POST /v1/embeddings` *(fully implemented — routed, billing-correct provider dispatch; sandbox `tt_test_*` keys return deterministic vectors with zero cost)*.

**Requires:** nothing to start (degraded dev mode: loopback-only, callers pass their own provider key as the Bearer token). For full features: `DATABASE_URL`, `REDIS_URL`, and provider keys. See the self-host table above.

### 2. Inspect — `tt inspect`

Static-scan a codebase for token-waste patterns. Exits non-zero when a finding meets or exceeds `--fail-on`, which makes it a drop-in CI gate.

```bash
# Human-readable markdown to stdout
tt inspect ./my-project

# Fail CI on any high+ finding (default fail-on is `high`)
tt inspect ./my-project --fail-on=high

# Write JSON (path ending in .json switches format automatically)
tt inspect ./my-project --output findings.json --fail-on=critical
```

Two extra modes share the subcommand (both offline, no cloud dependency):

```bash
# Cost-diff: project the per-call cost change of model ids added/removed in a git diff
tt inspect ./my-project --cost-diff --base origin/main --fail-on-cost-increase

# Suggest-plan: scan for model strings and emit a PlanInput skeleton for `tt plan`
tt inspect ./my-project --suggest-plan --output plan_input.json
```

**Requires:** the `tt` binary and a path. No keys, no network, no services.

### 3. `tt` proof loop — discover → simulate → realize → prove

For a self-hosted gateway (your own `DATABASE_URL`), the whole loop runs locally:

```bash
# Discover + freeze a real telemetry window into a runnable PlanInput
#   --org is auto-detected when the window has exactly one org.
tt inspect --suggest-plan . --from-db --window-days 7 --output plan.json

# Simulate — deterministic replay of the frozen window (text summary)
tt plan --input plan.json

# (optional) full PlanResult as JSON
tt plan --input plan.json --output result.json

# Realize — write the proposed routes to the gateway's routes table.
#   Dry-runs + prompts for confirmation; the gateway applies them within ~60s.
#   Use --yes in CI to skip the prompt.
tt plan --input plan.json --apply

# Prove — verify the signed plan.applied entry recorded by --apply
tt audit verify
```

Without `--from-db`, `tt inspect --suggest-plan` still emits a skeleton you fill in
by hand. `--from-db` and `--apply` both require `DATABASE_URL` (the gateway's
Postgres). `--apply` records a signed `plan.applied` entry to
`.claude/AUDIT-CHAIN.jsonl` using a per-machine key at
`~/.tokentrimmer/audit-signing-key`, and prints the verifying key for `tt audit verify`.

**Requires:** the `tt` binary. Offline simulation needs only a `PlanInput` JSON file; `--from-db` and `--apply` require the gateway's `DATABASE_URL`.

### 4. `tt init` — install best-practices into your repo

Drops a working AI-assistant harness (AGENTS.md, `.claude/` hooks + budget, an Inspect CI workflow, and an inspect baseline) into any git directory. Idempotent — re-running won't clobber your customizations.

```bash
cd ~/my-project
tt init

# Preview without writing anything
tt init --dry-run

# Re-run later to pull newer template versions for files you haven't edited
tt init --upgrade
```

Useful flags: `--path <dir>` (target a directory other than cwd), `--language <name>` and `--framework <name>` (override auto-detect), `--interactive` (prompt through choices), `--diff` (preview changes as a diff), `--skip-baseline`, `--skip-hooks`, `--skip-workflows`, `--force`.

**Requires:** a git-controlled directory. `--skip-baseline` avoids running an Inspect scan during install.

### 5. `tt mcp` — MCP server for AI clients

Exposes TokenTrimmer intelligence (cost preview, cheapest-route lookup, inspect-on-diff, semantic-cache lookup, plan simulation) to MCP-compatible clients over stdio (default) or Streamable HTTP.

```bash
# stdio (default), e.g. for Claude Desktop / Claude Code
tt mcp --tt-api-key tt_live_...

# Streamable HTTP transport for Cursor / Zed (default port 31416)
tt mcp --transport http --tt-api-key tt_live_... --sse-port 31416
```

Register it with an MCP client:

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

Tools (always available): `preview_cost` (estimate cost before sending), `find_route_for` (cheapest route for a model), `inspect_diff` (Inspect scan on a diff), `get_repo_context` (repo context for prompts), `lookup_semantic_cache` (check the semantic cache), `simulate_plan` (simulate a routing plan). With `DATABASE_URL` + verified key: `get_spend_today` (current-day org spend), `check_budget_remaining` (remaining monthly budget), `set_cost_limit` (set org or per-key monthly cap). With `--allow-write` + `DATABASE_URL`: `add_route` (add a gateway route), `apply_plan` (apply an optimization plan). With `--query-config`: `run_query` (run a named dataset query), `list_datasets` (list configured datasets). Resources: `mcp://tokentrimmer/cost-ledger/last-7d`, `mcp://tokentrimmer/inspect/baseline`, `mcp://tokentrimmer/plan/history?last=10`.

**Requires:** a `tt_live_…` key — run `tt login --token <KEY>`, or pass `--tt-api-key` / set `TT_API_KEY`. The hosted API base defaults to `https://api.tokentrimmer.com` (override with `--tt-api-base` or `TT_API_BASE`).

### 6. `tt proxy` — local listener for coding agents

A local OpenAI/Anthropic-compatible listener (default **port 31415**) that routes OpenAI-wire traffic through the hosted Gateway and writes per-session cost rollups. Handy for tools like Claude Code, Codex, or anything that reads `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`. Note: Anthropic-wire requests (`/v1/messages`) forward directly to the Anthropic upstream in every mode, with your client's own credentials passed through — the Gateway has no Anthropic ingress yet.

```bash
tt proxy --mode gateway --tt-api-key tt_live_...

# Then point your agent at it:
export ANTHROPIC_BASE_URL=http://localhost:31415
# or, for OpenAI-style tools:
export OPENAI_BASE_URL=http://localhost:31415
```

Modes: `gateway` (default; OpenAI-wire endpoints forward to hosted TT with your TokenTrimmer key injected, requires a key), `bypass` (forward straight to the upstream, logging only), `hybrid` (OpenAI-wire endpoints to the gateway, but your client's own credentials pass through). In all three modes `/v1/messages` goes directly to the Anthropic upstream. Flags: `--port`, `--bind` (default `127.0.0.1`), `--no-tui`, `--no-preview`, `--session-log <path>`.

Session log is JSONL at `~/.tokentrimmer/sessions/YYYY-MM-DD.jsonl`; on Ctrl-C the proxy prints a session summary.

**Requires:** for `gateway`/`hybrid` modes, a `tt_live_…` key via `--tt-api-key` or `TT_API_KEY`. `bypass` needs no key.

### 7. Retrieval (RAG / context compression) — `tt retrieval`

Ingest docs, retrieve relevant chunks, and (server-side) splice them into prompts via `<retrievable corpus="X" k="N">…</retrievable>` tags.

> **EXPERIMENTAL.** `tt retrieval doc-add` and `tt retrieval search` run against an **in-process store**: the corpus lives only inside the running process and is **discarded when the command exits**. A `doc-add` does **not** persist, and a later `search` (a separate process) always starts from an empty store — so the two commands **cannot see each other's data**. The example below illustrates the chunking/embedding path, not a durable ingest→query workflow; persistent corpora require the Postgres-backed store + cloud endpoints (follow-up). Each invocation prints a one-line `note:` to stderr restating this.

```bash
# EXPERIMENTAL in-process CLI (dev only; NOT persisted — doc-add and search
# run in separate processes and cannot see each other's data)
export OPENAI_API_KEY=sk-...
tt retrieval doc-add my-docs ./docs/architecture.md
tt retrieval search my-docs "How does the gateway dispatch?" --k 5
```

To activate the Gateway middleware that rewrites `<retrievable>` tags, set both env vars on the Gateway process:

```bash
TT_RETRIEVAL_STORE=memory TT_OPENAI_EMBED_KEY=sk-... tt gateway
```

> **Status:** the engine + middleware annotation ship today; full runtime activation (persistent corpus + cloud corpus endpoint) is a follow-up. CLI ingestion is in-process and not persisted.

**Requires:** an OpenAI key for embeddings (`OPENAI_API_KEY`, or `--openai-key`). For Gateway-side substitution: `TT_RETRIEVAL_STORE` + `TT_OPENAI_EMBED_KEY`.

### 8. SDKs (Python & TypeScript)

Thin OpenAI subclasses that route through the Gateway and surface cost/cache metadata on `.tt`.

**Try it first with no account, no provider key, $0.** Point either SDK at a local
Gateway (the one-line `docker run … ghcr.io/tokentrimmer/tt-cli:latest` from the
[Self-host path](#self-host-path-docker) above) and use any `tt_test_*` **sandbox
key** — the Gateway short-circuits to a deterministic synthetic response without
contacting a provider or verifying against a key store. In both examples below,
swap `api_key="tt_live_..."` → `api_key="tt_test_demo"` and add
`base_url="http://localhost:8080/v1"` (`baseURL` in TS); the call returns
`[sandbox] TokenTrimmer test response …` at `cost_usd=0`, `cache=sandbox`.

> **Not yet on PyPI/npm** — published packages land at launch. Until then, install from git:
> `pip install "git+https://github.com/TokenTrimmer/tokentrimmer.git#subdirectory=sdk-python"`.
> For TypeScript (npm cannot install a git subdirectory directly): clone the repo, run
> `npm install && npm run build` in `sdk-typescript/`, then `npm install <path-to-clone>/sdk-typescript`
> from your project.

**Python** (package `tokentrimmer`, requires Python ≥ 3.9, `openai>=1.70.0,<3.0.0`):

```python
from tokentrimmer import TokenTrimmer
client = TokenTrimmer(api_key="tt_live_...")
resp = client.chat.completions.create(
    model="claude-sonnet-4-6",
    messages=[{"role": "user", "content": "Hello"}],
    tt_tag="feature=chat-support",
)
print(resp.tt.cost_usd, resp.tt.cache, resp.tt.trace_id)
```

**TypeScript** (package `@tokentrimmer/client`; `openai ^6.45.0` is a **peer dependency** — install it alongside: `npm i @tokentrimmer/client openai`):

```ts
import { TokenTrimmer } from '@tokentrimmer/client';
const client = new TokenTrimmer({ apiKey: 'tt_live_...' });
const response = await client.chat.completions.create({
  model: 'claude-sonnet-4-6',
  messages: [{ role: 'user', content: 'Hello' }],
  ttTag: 'feature=chat-support',
});
console.log(response.tt.costUsd, response.tt.cache, response.tt.traceId);
```

Both default to the hosted Gateway. For self-host, pass `base_url` / `baseURL` = `http://localhost:8080/v1` and your provider key. Every other OpenAI method (streaming, tools, vision, async, `embeddings`) works unchanged — the Gateway's `/v1/embeddings` is a routed, billing-correct endpoint.

---

## Audit log verification — `tt audit verify`

Verify the integrity of a hash-chained audit log (Ed25519-signed).

```bash
# Default path: .claude/AUDIT-CHAIN.jsonl; key sourced from export preamble if present
tt audit verify

# Or supply a verifying key explicitly
tt audit verify path/to/chain.jsonl --key-hex <64-hex-chars>
```

---

## Which local endpoint? (8080 vs 31415)

These are two different things, easy to confuse:

- **`http://localhost:8080`** — your **self-hosted Gateway** (`tt gateway`). Point your app's `base_url` here when you run TokenTrimmer yourself.
- **`http://localhost:31415`** — the **`tt proxy`** that forwards OpenAI-wire traffic to the *hosted* Gateway (`/v1/messages` goes directly to the Anthropic upstream — no gateway Anthropic ingress yet) and tracks per-session cost. Point coding agents (`ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL`) here.

---

## Where to go next

- **Architecture:** `docs/tokentrimmer-architecture-spec-v1.md`
- **Gateway API reference:** `docs/04-gateway-api-reference.md`
- **Integrations (n8n, LangChain, LangGraph, Dify):** `docs/integrations.md`
- **Cost Preview API:** `docs/04-cost-preview-api-reference.md`
- **Inspect rule catalog:** `docs/01-inspect-rule-catalog.md`
- **Provider adapter guide:** `docs/02-provider-adapter-guide.md`
- **Plan / replay design:** `docs/03-plan-replay-design.md`
- **Contributing:** `CONTRIBUTING.md`
- **Developer playbook:** `AGENTS.md`
