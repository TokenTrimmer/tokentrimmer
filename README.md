# TokenTrimmer

> The cost layer for LLM applications. See, plan, and optimize every token across OpenAI, Anthropic, Google, Mistral, Groq, Together, OpenRouter, and local runtimes (Ollama / vLLM / LM Studio).

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Status: alpha](https://img.shields.io/badge/status-alpha-yellow.svg)](#status)

TokenTrimmer is four products that ship together:

- **Gateway** — OpenAI-compatible HTTP proxy with rule-based routing, two-layer caching (L1 Redis exact-match + L2 pgvector semantic), cross-provider failover with circuit breakers, and per-request cost telemetry via `x-tokentrimmer-*` response headers.
- **Inspect** — Static analyzer that scans Python, TypeScript, and JavaScript codebases for token-waste patterns (19 Tier-1 rules: oversized prompts, missing prompt caching, unbounded agent loops, flagship models on classification work, …).
- **Plan** — Deterministic replay simulator that projects how a proposed config change would have affected cost, savings, and cache hit rate, with bootstrap confidence intervals.
- **Reporting** — Dashboard, weekly digest, monthly PDF (closed-source, hosted-only).

This repo is the **open-source core** (Apache 2.0): the Gateway, the `tt` CLI, the Inspect and Plan engines, an MCP server, a retrieval/context-compression engine, an Ed25519-signed audit ledger, and Python/TypeScript/Rust SDKs. The closed-source hosted product lives in a separate repo.

> **New here?** Start with **[`GETTING_STARTED.md`](GETTING_STARTED.md)** — quickstarts plus a copy-paste example for every surface (gateway, inspect, plan, init, mcp, proxy, retrieval, SDKs).

## Status

Alpha. The open-source core is implemented and tested: the Gateway (routing, L1/L2 caching, failover), all 19 Tier-1 Inspect rules, the Plan replay engine, the full `tt` CLI, the MCP server, retrieval, and the three SDKs all ship from this repo, gated by CI (fmt, clippy `-D warnings`, tests, and a self-inspect run).

Two things are **not** true yet, so you don't find out the hard way:

- **No published packages.** Nothing is on crates.io, PyPI, or npm yet — published packages land at launch. Until then, every install path below (and throughout the docs) is a working git install.
- **The hosted gateway is not live.** Self-hosting is the supported path today.

## Quick start (self-host)

The Docker image is built from [`Dockerfile`](Dockerfile) and pushed to GHCR on every merge to `main`. The image's entrypoint is `tt`, default command `gateway` (binds `:8080`). Configuration is env-only; Postgres and Redis are optional — without them the Gateway runs in a degraded dev mode (no persistence, no caching).

```bash
docker run -p 8080:8080 \
  -e OPENAI_API_KEY=sk-... -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/tokentrimmer/tt-cli:latest

# Then point your existing OpenAI SDK at it — one line:
#   base_url="http://localhost:8080/v1"
```

Or install the `tt` binary directly:

```bash
# (not yet on crates.io — published packages land at launch; install from git)
cargo install --locked --git https://github.com/tokentrimmer/tokentrimmer tt-cli

# Scan a codebase for token waste
tt inspect ./my-project --fail-on=high
```

### Local development

```bash
# Toolchain: rust-toolchain.toml pins 1.88.0 and rustup auto-installs it on
# the first cargo command — no manual install needed.

make dev          # optional services for end-to-end testing:
                  # Postgres+pgvector, Redis, MinIO, mailpit
cargo test --workspace
```

## The `tt` CLI

One binary, every surface:

| Command | What it does |
|---|---|
| `tt gateway` | Run the Gateway proxy server |
| `tt inspect` | Scan a codebase for token-waste patterns; also `--cost-diff` and `--suggest-plan` modes |
| `tt plan` | Replay telemetry against a proposed config; project cost/savings with bootstrap CIs |
| `tt audit verify` | Verify the integrity of a hash-chained, Ed25519-signed audit log export |
| `tt chat` | Interactive chat through a gateway — streaming, savings display, optional tool-calling |
| `tt proxy` | Local OpenAI/Anthropic-compatible proxy for coding agents (port 31415) |
| `tt advise` | AI cost advisor: scans a repo and recommends optimizations (read-only) |
| `tt route` | Manage routing rules on a gateway (list / show / add / rm) |
| `tt models` | List a gateway's model catalog (context windows, capabilities, pricing) |
| `tt embed` | Embeddings through a gateway, with a cost summary |
| `tt retrieval` | RAG corpus management (doc-add, search) |
| `tt init` | Install TokenTrimmer best-practices config into a repo |
| `tt mcp` | Run the MCP server (stdio or SSE transport) |
| `tt login` / `logout` / `whoami` | Manage the locally stored API key |

## Repo layout

```
crates/
├── shared/                    Provider trait, wire types, errors
├── core/                      Gateway: Axum app, routing, failover, middleware
├── cache/                     L1 (Redis exact-match) + L2 (pgvector semantic)
├── routing/                   Rule engine
├── auth/                      API key validation
├── telemetry/                 OTel + hash-chained, Ed25519-signed audit log
├── config/                    Env config loader
├── tokenize/                  Shared token estimator
├── preview/                   Cost-preview engine
├── inspect-core/              Rule trait, tree-sitter harness
├── inspect-rules-tier1/       The 19 Tier-1 rules
├── plan-core/                 Replay engine + bootstrap CIs
├── retrieval/                 RAG / context-compression engine
├── mcp/                       MCP server
├── client/                    Rust SDK (typed gateway client)
├── cli/                       `tt` binary
├── ts-types/                  Rust → TS bindings codegen (placeholder, not yet implemented)
└── providers/
    ├── openai/                The canonical adapter
    ├── anthropic/             Worked reference (separate system field, cache_control)
    ├── gemini/                Native API (systemInstruction, streamGenerateContent)
    ├── compat/                Shared machinery for OpenAI-wire-compatible providers
    ├── mistral/               OpenAI-compatible
    ├── groq/                  OpenAI-compatible
    ├── together/              OpenAI-compatible
    ├── openrouter/            OpenAI-compatible passthrough
    └── local/                 Ollama / vLLM / LM Studio
sdk-python/                    Python SDK (drop-in openai.OpenAI subclass)
sdk-typescript/                TypeScript SDK
examples/                      Runnable Python + TypeScript snippets
docs/                          Architecture spec + per-surface design and usage docs
```

## Architecture

Read [`docs/tokentrimmer-architecture-spec-v1.md`](docs/tokentrimmer-architecture-spec-v1.md) for the full system design. The numbered docs cover the Inspect rule catalog, the provider adapter contract, the Plan replay design, the cost-preview API, and the Gateway API reference; the `docs/tt-*-usage.md` guides cover `init`, `mcp`, `proxy`, and `retrieval`.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). The repo is engineered to pass its own Inspect rules — see [`AGENTS.md`](AGENTS.md) for the developer playbook and [`.claude/`](.claude/) for the autonomous-build harness.

## Security

Report security issues to `security@tokentrimmer.com`. See [`SECURITY.md`](SECURITY.md).

## License

Apache License 2.0. See [`LICENSE`](LICENSE).
