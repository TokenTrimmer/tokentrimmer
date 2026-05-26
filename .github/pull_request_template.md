## Summary

<!-- 1-3 bullets: what changed and why. The "why" matters more than the "what". -->

-

## Backlog item

<!-- Closes #N (autopilot issue) or task-id from .claude/BACKLOG.md -->

Closes #

## Test plan

<!-- Checkboxes you actually checked locally. Reviewer should be able to repeat. -->

- [ ] `cargo test -p <crate>` green
- [ ] `cargo clippy -p <crate> -- -D warnings` clean
- [ ] `./scripts/tt-inspect-self.sh` clean (no new high/critical)
- [ ] Manual verification: (describe)

## Dogfood check

<!-- Does this change introduce or remove a pattern we'd flag with our own rules? -->

- [ ] No new dependency without justification
- [ ] No new AGENTS.md content over 4K tokens
- [ ] No secret-like literals added
- [ ] Anthropic SDK calls (if any) include `cache_control` on long system prompts

## AI assistance

<!-- For audit trail. Fill if any part of this PR was Claude Code-assisted. -->

- AI-Session(s): (paste session ID(s) from `.claude/AUDIT.log`)
- AI-Cost-USD: (sum from `.claude/cost-ledger.jsonl` for those sessions)
- AI-Subagents-Used: (e.g. `rust-crate-builder`, `provider-adapter-author`)

## Handoff

<!-- If this PR is part of a larger workstream, note what should happen next. -->

Next step:
