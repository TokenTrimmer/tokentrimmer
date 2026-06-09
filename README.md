# TokenTrimmer

> The cost layer for LLM applications. See, plan, and optimize every token across OpenAI, Anthropic, Google, and 7+ other providers.

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![Status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-orange.svg)](docs/tokentrimmer-architecture-spec-v1.md)

TokenTrimmer is four products that ship together:

- **Gateway** — OpenAI-compatible HTTP proxy with intelligent routing, multi-layer caching (exact + semantic), and per-request cost telemetry. Sub-30 ms p50 overhead.
- **Inspect** — Static analyzer that scans your codebase for token-waste patterns (oversized prompts, missing cache_control, unbounded agent loops, etc.).
- **Plan** — Deterministic replay simulator that projects how a proposed config change would have affected cost, latency, and quality, with bootstrap confidence intervals.
- **Reporting** — Dashboard, weekly digest, monthly PDF (closed-source, hosted-only).

This repo is the **open-source core** (Apache 2.0): Gateway, Inspect CLI, Plan engine, and SDKs. The closed-source hosted product lives at `tokentrimmer/cloud`.

> **New here?** Start with **[`GETTING_STARTED.md`](GETTING_STARTED.md)** — a 5-minute hosted quickstart plus a copy-paste example for every surface (gateway, inspect, plan, init, mcp, proxy, retrieval, SDKs).

## Status

Pre-alpha. Week 0 pre-flight is complete; Gateway implementation starts Week 1. See [`/Users/iansimon/.claude/plans/please-review-all-files-linked-tulip.md`](../../.claude/plans/please-review-all-files-linked-tulip.md) for the 26-week solo-founder buildout plan.

### Prerequisites for local build

```bash
# Toolchain: rust-toolchain.toml pins 1.88.0 and rustup auto-installs it on
# the first cargo command — no manual install needed. (Manual: rustup install 1.88.0)

# Services for end-to-end testing
make dev                     # brings up Postgres+pgvector, Redis, MinIO, mailpit
```

## Quick start (when ready)

```bash
# Hosted (single-line integration)
export OPENAI_API_KEY=$(echo $TT_KEY)  # tt_live_... key
# Then change one line in your OpenAI SDK init:
#   base_url="https://api.tokentrimmer.com/v1"

# Self-host — the image's entrypoint is `tt`, default command `gateway`
# (binds :8080). Configuration is env-only; there is no YAML config file.
docker run -p 8080:8080 \
  -e OPENAI_API_KEY=sk-... -e ANTHROPIC_API_KEY=sk-ant-... \
  ghcr.io/tokentrimmer/tt-cli:latest

# Inspect a codebase
# (not yet on crates.io — published packages land at launch; install from git)
cargo install --locked --git https://github.com/tokentrimmer/tokentrimmer tt-cli
tt inspect ./my-project --fail-on=high
```

## Repo layout

```
crates/
├── shared/                    Provider trait, wire types, errors
├── core/                      Axum app, routing, middleware
├── cache/                     L1 (Redis exact-match)
├── routing/                   Rule engine
├── auth/                      API key validation
├── telemetry/                 OTel + hash-chained audit log
├── config/                    Layered config loader
├── inspect-core/              Rule engine, tree-sitter harness
├── inspect-rules-tier1/       10 P0 launch rules
├── plan-core/                 Replay engine + bootstrap CIs
├── cli/                       `tt` binary
├── ts-types/                  Rust → TS bindings codegen
└── providers/
    ├── openai/                The canonical adapter
    ├── anthropic/             Worked reference (separate system field, cache_control)
    ├── gemini/                Native API (systemInstruction, streamGenerateContent)
    ├── mistral/               OpenAI-compatible
    ├── groq/                  OpenAI-compatible
    ├── together/              OpenAI-compatible
    ├── openrouter/            OpenAI-compatible passthrough
    └── local/                 Ollama / vLLM / LM Studio
sdk-python/                    Python SDK
sdk-typescript/                TypeScript SDK
```

## Architecture

Read [`docs/tokentrimmer-architecture-spec-v1.md`](docs/tokentrimmer-architecture-spec-v1.md) for the full system design. The other four spec docs cover the rule catalog, provider adapter contract, plan/replay design, and gateway API reference.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). The repo is engineered to pass its own Inspect rules — see [`AGENTS.md`](AGENTS.md) for the developer playbook and [`.claude/`](.claude/) for the autonomous-build harness.

## Security

Report security issues to `security@tokentrimmer.com`. See [`SECURITY.md`](SECURITY.md).

## License

Apache License 2.0. See [`LICENSE`](LICENSE).
