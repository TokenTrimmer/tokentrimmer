# Contributing to TokenTrimmer

Thank you for your interest. TokenTrimmer is built in the open under Apache 2.0.

## Before you start

- Read the [architecture spec](docs/tokentrimmer-architecture-spec-v1.md) (the high-level overview only — skim).
- Read [`AGENTS.md`](AGENTS.md) — it documents how the codebase is structured and tested.
- Browse open issues with the `good first issue` or `help wanted` label.

## Development setup

```bash
# With Nix (preferred — locked toolchain matches CI)
nix develop

# Or without Nix: install Rust 1.83+, Node 20+, pnpm 9+, Docker
# Then bring up local services:
make dev

# Verify:
make ci
```

## Workflow

1. **Open an issue first.** Describe the problem or proposal. For substantive changes, get a signal before you write code.
2. **Branch from `main`** with a descriptive name: `feat/anthropic-cache-control`, `fix/streaming-disconnect`.
3. **Make scoped commits.** Use scoped cargo commands (`cargo test -p <crate>`) — whole-workspace builds are denied in `.claude/settings.json`.
4. **Pass our own scanner.** Run `./scripts/tt-inspect-self.sh`. New high/critical findings on your branch block merge.
5. **Sign your commits.** GPG or Sigstore. Branch protection requires it.
6. **Open a PR** with a clear summary, a test plan, and links to related issues.

## Code review

- Every PR requires at least one review.
- We optimize for **boring code** over clever code. If a one-line change works, prefer it.
- Comments should explain **why**, never **what**. Well-named identifiers and types should make the "what" obvious.

## Coding conventions

- Errors via `thiserror` enums. No `.unwrap()` outside `#[cfg(test)]`.
- Logging via `tracing`. No `println!` in library code.
- All wire types in `crates/shared/`. Do not redefine in adapters.
- Each provider adapter is its own crate. Stateless beyond HTTP client and pricing table.
- 800-line cap per `.rs` file (hook-enforced).
- No new dependencies without justification in the PR description.

## Provider adapters

If you're adding a new provider, follow `docs/02-provider-adapter-guide.md`. The Anthropic adapter is the worked example.

Required deliverables:
- `Provider` trait implementation in `crates/providers/<name>/`
- Insta snapshot tests of OpenAI ↔ provider translation (min 20 fixtures)
- httpmock tests for success, rate limit, 5xx, network error, partial stream
- Pricing table in `src/pricing.rs` with `effective_at` timestamp
- Documentation in `docs/providers/<name>.md`
- Registration in `crates/core/src/registry.rs`

## Inspect rules

Follow `docs/01-inspect-rule-catalog.md`. Each rule needs:
- Implementation in `crates/inspect-rules-tier1/src/rules/<rule_id>.rs` (or tier 2/3 in the cloud repo)
- Min 5 positive fixtures (rule should fire) and 10 negative (rule should not fire) in `tests/rules/<rule_id>/`
- False-positive rate under 5% on the `corpora/` open-source samples
- Documentation entry in the rule catalog

## DCO

By contributing, you agree to the Developer Certificate of Origin (DCO). Commits should be signed off:

```
git commit -s -m "feat: add Gemini context caching support"
```

## License

Contributions are licensed under Apache 2.0, the same as the project.
