# Plans

Every non-trivial plan lives here, version-controlled with the repo. The master buildout plan is `00-master-buildout-plan.md`. New plans should be numbered sequentially.

## Pattern

When we make a new plan (via `/plan` in Claude Code or via design discussion):

1. **Save it here.** Filename: `NN-<short-name>.md` where NN is the next two-digit sequence (00, 01, 02, ...). Examples: `01-enterprise-tier-rollout.md`, `02-soc2-readiness.md`.
2. **Extract the checklist.** Every plan must end with a "Backlog items" section listing each actionable item as a one-line entry suitable for paste into `.claude/BACKLOG.md`. Use the BACKLOG format:
   ```
   - [ ] [PRIORITY] [task-id] subagent: brief description (est: $X.XX)
   ```
3. **Sync to BACKLOG.md.** After the plan is approved, append those items to `.claude/BACKLOG.md`. The autopilot will pick them up.
4. **Reference in DECISIONS.md.** If the plan locks in an ADR-worthy decision, add the ADR with a `Pointers:` line back to the plan file.

## What's here

- `00-master-buildout-plan.md` — the 26-week solo-founder roadmap that drives everything (Week 0 → beta). Originally drafted at `~/.claude/plans/please-review-all-files-linked-tulip.md`; this is the in-repo copy that survives session changes.

## Why in-repo, not in `~/.claude/plans/`

The global plans directory is per-machine and per-Claude-Code-install. Plans that govern this project must travel with the project — for autopilot resumability, contributor onboarding, and audit. The pattern here:

- `~/.claude/plans/` — scratch / in-progress / cross-project
- `<repo>/.claude/plans/` — the source of truth for THIS project

When a plan is finalized via `/plan` and `ExitPlanMode`, copy it here.

## Browsing

```
make plans            # list files in this directory
```
