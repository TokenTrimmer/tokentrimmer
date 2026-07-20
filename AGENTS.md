# TokenTrimmer — AGENTS.md

This file is injected once per Claude Code session. Per-crate `AGENTS.md` files cover crate-specific guidance — load them on demand.

## What this repo is

TokenTrimmer is an LLM cost-optimization platform with four pillars:

1. **Gateway** (`crates/core` + `crates/providers/*`) — Rust/Axum proxy in front of 10+ LLM providers. OpenAI-compatible API. Sub-30ms p50 overhead on miss, sub-5ms on hit. Multi-region on Fly.io.
2. **Inspect** (`crates/inspect-core` + `crates/inspect-rules-tier1`) — Rust CLI + tree-sitter scanner for token-waste rules in user codebases.
3. **Plan** (`crates/plan-core`) — Deterministic replay simulator with bootstrap confidence intervals.
4. **Reporting** — Astro 5 + Solid.js dashboard, lives in sibling repo `tokentrimmer-cloud` (private). This repo is public OSS (Apache 2.0).

Full architecture: `docs/tokentrimmer-architecture-spec-v1.md`. Don't read it unless your task needs it.

## Build / test / lint

Always use **scoped** cargo commands. Whole-workspace builds are explicitly denied in `.claude/settings.json`.

```
cargo check -p <crate>                            # fastest feedback
cargo clippy -p <crate> -- -D warnings             # required clean
cargo test -p <crate>                              # run crate tests
cargo fmt                                          # before commit
```

Frontend (in sibling cloud repo, but pnpm patterns same):
```
pnpm --filter <pkg> typecheck
pnpm --filter <pkg> test
pnpm --filter <pkg> build
```

Dogfood our own scanner against the repo:
```
./scripts/tt-inspect-self.sh                       # zero new high/critical = pass
```

Full local CI mirror:
```
./scripts/ci-local.sh                              # fmt + clippy + scoped tests + inspect-self
```

## Conventions

- Errors: `thiserror` enums. NEVER `.unwrap()` outside `#[cfg(test)]`.
- Logging: `tracing` crate. NEVER `println!`/`eprintln!` in library code.
- Async: `tokio`. Stream types: `futures::Stream`.
- Wire types: live in `crates/shared/src/`. Use those, do not redefine.
- Contract TS bindings: `crates/ts-types` derives JSON Schema from the real
  VCR/L2/WFR/ARR/bundle, route, workflow, and gateway-capability Rust wires,
  then emits the receipt and product TypeScript bindings; CI regenerates and
  byte-compares schemas, TypeScript, vectors, compatibility artifacts, and both
  manifests. Other API families remain outside this generator—do not imply
  whole-API binding coverage.
- OpenAPI: emitted via `utoipa` on Axum routes.
- Provider adapters: each in its own crate at `crates/providers/<name>/`. Stateless beyond HTTP client + pricing table.
- File size target: keep new `.rs` modules under 800 lines. Legacy oversized
  files are being split incrementally; do not make them larger unless the edit
  is part of an active extraction.

## Do NOT

- Run `cargo test --workspace`, `cargo build --release`, or `cargo build --workspace`. Hooks deny these.
- Commit secrets. `pre-edit-guard.sh` blocks; gitleaks in CI is the second line.
- Add a new dependency without justification in the PR. `cargo deny` enforces license allowlist.
- Skip pre-commit hooks (`--no-verify`).
- Push to `main` directly. Branch + PR + review required.

## Finding context (path of least resistance)

**Before any `Grep`/`Glob`/exploration, run:**

```
./scripts/context-for.sh <keyword>     # queries CONTEXT_MAP → DECISIONS → INDEX → code
```

That single command consults four layers in cost order and usually answers "where is X". If it returns nothing useful, fall back to Grep — and when you find the answer, **add a one-line entry to `.claude/CONTEXT_MAP.md`** so the next task gets it for free.

Map layers (increasing cost-per-query):
- `.claude/CONTEXT_MAP.md` — curated "if you're doing X, read Y" index
- `.claude/DECISIONS.md` — ADR log of why-it-is-this-way
- `.claude/INDEX.md` — auto-generated structural facts (refresh: `make context-index`)
- `Grep`/`Glob` over `crates/` and `docs/`

## Reference files

- `docs/01-inspect-rule-catalog.md` — when authoring rules
- `docs/02-provider-adapter-guide.md` — when authoring providers (Anthropic worked example)
- `docs/03-plan-replay-design.md` — when changing replay engine
- `docs/04-gateway-api-reference.md` — when changing public API surface
- `docs/tokentrimmer-architecture-spec-v1.md` — system-wide only (anti-context for routine tasks)

## Subagent fleet

Dispatch specialized subagents for scoped work (each has its own context window):

- `rust-crate-builder` — work inside one crate
- `provider-adapter-author` — one provider adapter
- `inspect-rule-author` — one rule
- `astro-page-builder` — one page or island (cloud repo)
- `plan-replay-validator` — Plan engine changes
- `dogfood-inspect-runner` — check repo against our own rules
- `onboarding-context-loader` — build a brief BEFORE dispatching workers

Use `onboarding-context-loader` first when the task crosses files or you lack context. It returns a 500-token brief; the implementation subagent uses that as its starting context.

## Autonomous loop

The autonomous build runs via `scripts/ralph-iteration.sh`, triggered by cron or `/loop`. Each iteration: one backlog item (`.claude/BACKLOG.md` or GitHub issue labeled `autopilot`), one branch, one subagent dispatch, mandatory gates (test green + inspect-self clean + cost under $1), opens a PR. Never auto-merges. See the in-repo plan at `.claude/plans/00-master-buildout-plan.md`.

## Session lifecycle

- `.claude/STATE.md` — durable pointer (current task, branch, status). Read by session-start hook.
- `.claude/HANDOFF.md` — written by `make session-end` when a session ends mid-work. Next session reads it via the session-start hook.
- `.claude/BACKLOG.md` — single source of truth for actionable work. Use `make backlog`, `make backlog-take`, `make backlog-sync`.
- `.claude/AUDIT.log` — append-only line per session. Source of truth for "what did AI do".
- `.claude/cost-ledger.jsonl` — one JSON line per session with model + USD cost. Source for `make review`.
- `.claude/MODEL_ROUTING.md` — which subagent uses which Claude tier (Haiku/Sonnet/Opus). Dogfood: cheap models for routine work.

End a substantive session with:
```
make session-end MSG="OpenAI adapter scaffolded" TASK="w1-openai-trait-impl" NEXT="Wire streaming, run cargo test -p tt-provider-openai"
```

Weekly: `make review` to surface cost drift, rework signals, and tuning candidates.
