# Autopilot iteration prompt

This file is the single source of truth for what one autonomous iteration does. When the chain is running, each `ScheduleWakeup` fires a short prompt that says "execute the protocol in `.claude/AUTOPILOT_PROMPT.md`" — keeping the actual logic version-controlled here.

---

## Protocol

You are running one autonomous iteration of the TokenTrimmer build loop. Repo root: `/Users/iansimon/Developer/TokenTrimmer`.

Execute the steps in order. Bail on the first failure.

### 1. Early-exit checks

- If `.claude/PAUSED` exists, print its contents, **STOP** — do NOT schedule the next iteration.
- If `.claude/STOP-CHAIN` exists, print "Chain stopped by user request" and delete the file, **STOP** — do NOT schedule the next iteration.
- Run `./scripts/backlog.sh take`. If no open items, run `./scripts/session-end.sh "Backlog drained — chain ending" --task "(idle)" --next "Add items to .claude/BACKLOG.md, then say 'start the autopilot' to resume"` and **STOP**.
- **Skip placeholder items.** If the parsed backlog line body contains `[DEFERRED` or `[NEEDS-SPEC]`, that item is a placeholder (deferred decision or unwritten spec). Do NOT dispatch a subagent against it. Instead, call `./scripts/backlog.sh take` again to advance to the next item. If every open item is a placeholder, treat the same as backlog-drained: write a one-line HANDOFF noting "all open items placeholder" and **STOP** without scheduling the next iteration.

### 2. Find context cheaply

- Parse the backlog line for `task-id`, `priority`, declared subagent (`rust-crate-builder`, `provider-adapter-author`, etc.), and the description.
- Run `./scripts/context-for.sh <main keyword(s)>` to surface relevant pointers. Use that output as the working brief.
- Do NOT load `docs/tokentrimmer-architecture-spec-v1.md` unless the task truly needs system-wide context.

### 3. Dispatch the specialist subagent

- Use the Agent tool with `subagent_type` matching the declared agent.
- Pass the brief from step 2 + the task description + acceptance criteria from the backlog line.
- The subagent's model tier is in `.claude/MODEL_ROUTING.md` — its frontmatter `model:` field encodes the default. Honor it.

### 4. Mandatory gates BEFORE marking work complete

For each gate, run and verify:
- `cargo test -p <changed-crate>` — exit 0
- `cargo clippy -p <changed-crate> -- -D warnings` — exit 0
- `./scripts/tt-inspect-self.sh` — exit 0 (zero NEW high/critical findings vs main)
- Iteration cost so far — if `> $5.00`, bail (raised 2026-05-28 to fit larger plan-driven items; runaway protection still in place via daily/weekly budget.toml caps)

### 5. Resolve outcome

**All gates pass:**
- `./scripts/backlog.sh done <task-id>`
- `./scripts/session-end.sh "<one-line status>" --task "<next backlog task-id>" --next "<one-line direction>"`
- Append a one-line entry to `.claude/AUDIT.log` if the audit-line hook didn't already

**Any gate fails:**
- Do NOT mark item done
- Write a HANDOFF.md noting exactly what blocked (failing test name, clippy lint, inspect finding, etc.)
- If this is the 3rd consecutive failure on the same `task-id`, append ` [BLOCKED — needs human]` to the backlog line so the next iteration skips it
- Continue to step 6 (chain still continues; next iteration will pick a different item)

### 6. Chain the next iteration

- If `.claude/PAUSED` was created mid-iteration (e.g. by `cost-cap-check.sh`), **STOP** — do not chain.
- Otherwise, call `ScheduleWakeup` with:
  - `delaySeconds: 60` (the minimum; keeps cache warm)
  - `prompt: "[AUTOPILOT] Execute the protocol in .claude/AUTOPILOT_PROMPT.md."`
  - `reason: "Chaining autopilot iteration after <task-id> (<status>)"`

### 7. Hard limits

- Max 30 tool calls per iteration before bailing (write HANDOFF, do not chain)
- Never `git push`, `gh pr create`, `git commit --amend`, or destructive ops without the user
- The `pre-edit-guard.sh` hook will enforce file size and secret limits — respect denials
- Cron / wakeup fires only when REPL is idle; never preempt active user work

### 8. Report

Conclude the iteration with a 5-line summary:
```
Iteration: <task-id>
Outcome: <DONE | BLOCKED | PARTIAL>
Files: <count> changed
Gates: <which passed / which failed>
Next: <chained | stopped — reason>
```

---

## How a human stops the chain

| Method | Effect |
|---|---|
| Send any message in the active session | Chain pauses until you yield; next wakeup waits for idle |
| `make loop-pause` (creates `.claude/PAUSED`) | Next iteration self-aborts before any work |
| `touch .claude/STOP-CHAIN` | Next iteration aborts AND deletes the file; one-shot stop |
| Close Claude Code | All scheduled wakeups die — chain ends naturally |
| Ask the assistant to "stop the autopilot" | Assistant deletes `.claude/STOP-CHAIN` after the active iteration, or cancels any pending wakeup |

## How a human starts the chain

| Method | Effect |
|---|---|
| Ask the assistant to "start the autopilot" | Assistant schedules the first wakeup (60s out); chain begins |
| `make loop` | Manual one-shot iteration (does NOT chain — single run only) |
