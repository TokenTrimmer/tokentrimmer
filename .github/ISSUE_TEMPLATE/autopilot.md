---
name: Autopilot task
about: Queue a scoped task for the autonomous build loop to pick up
title: '[task-id] Brief description'
labels: autopilot
assignees: ''
---

## Task ID

<!-- short kebab-case, e.g. w1-axum-skeleton. Used as branch name. -->

## Subagent

<!-- one of: rust-crate-builder, provider-adapter-author, inspect-rule-author, astro-page-builder, plan-replay-validator -->

## Scope

<!-- exactly what files/crates this touches. Narrow scope is mandatory for autopilot. -->

- Crate: `crates/...`
- Files: `...`

## Success criteria

<!-- the agent will not return until these are met -->

- [ ] `cargo test -p <crate>` green
- [ ] `cargo clippy -p <crate> -- -D warnings` clean
- [ ] (subagent-specific: e.g. snapshot count, FP rate, fixture coverage)

## Cost estimate

USD: <!-- from BACKLOG.md -->

## Context pointers

<!-- file paths + line numbers, NOT full text. The onboarding-context-loader subagent expands these. -->

- `docs/02-provider-adapter-guide.md` (section: ...)
- `crates/providers/anthropic/src/translate.rs:42`

## Hard limits

- Max 50 tool calls inside the subagent before returning.
- Must end with the subagent's mandatory 5-line summary in the PR description.
