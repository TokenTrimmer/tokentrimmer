# tt init — install TokenTrimmer best-practices into your repo

`tt init` drops a working AI-assistant harness into any git-controlled
directory. It is idempotent: re-running it doesn't clobber your
customizations.

## Quick start

```bash
cd ~/my-project
tt init
```

## What gets installed

- `AGENTS.md` — convention guide; customize freely.
- `.claude/settings.json` — Claude Code hooks config; merged with any existing.
- `.claude/hooks/{pre-edit-guard,cost-cap-check,audit-line}.sh` — runtime guards.
- `.claude/BACKLOG.md` — empty backlog for your future items.
- `.claude/budget.toml` — daily/weekly cost circuit-breaker.
- `.github/workflows/inspect-self.yml` — CI gate that runs `tt inspect`.
- `.gitignore` — `.tt-init.lock`, audit log, cost ledger, etc. appended.
- `.claude/inspect-baseline.json` — snapshot of current inspect findings.
- `.tt-init.lock` — manifest of installed templates (gitignored).

## Upgrade later

```bash
tt init --upgrade
```

Re-runs the installer, pulling newer template versions for any file you
haven't modified. Files you've modified are skipped with a warning;
`--force` overwrites them.

## Common flags

- `--dry-run` — print planned writes, touch nothing.
- `--skip-baseline` — don't run `tt inspect` after install.
- `--skip-hooks` — don't install `.claude/hooks/`.
- `--skip-workflows` — don't install `.github/workflows/`.
- `--language python|typescript|rust|go|java|mixed` — override auto-detection.
