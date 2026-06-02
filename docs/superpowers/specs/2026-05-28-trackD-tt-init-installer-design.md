# Track D — `tt init` Best-Practices Installer

**Status:** Draft 1
**Track:** D of six-track expansion
**Date:** 2026-05-28
**Owner:** solo founder

---

## 1. Problem

TokenTrimmer's own repo has a strong harness — `AGENTS.md` / `CLAUDE.md` conventions, hooks (`pre-edit-guard`, `cost-cap-check`, `audit-line`, `post-edit-scoped-check`), specialist subagents, an Inspect baseline, a `.claude/MODEL_ROUTING.md`, a `.claude/BACKLOG.md` + autopilot loop. None of that exists in customer repos. We've built the pieces for ourselves and never shipped them as a product.

`tt init` is a single CLI command that drops the same harness into any repo, customized for that repo's language / framework.

## 2. Goals

1. One command, zero-configuration default: `cd ~/my-repo && tt init` produces a working baseline.
2. Idempotent. Re-running is safe — existing files are detected and either skipped or selectively merged.
3. Detects language/framework so the installed assets match the repo (Python + LangChain vs TypeScript + Vercel AI SDK get different Inspect rule recommendations).
4. The installed baseline runs `tt inspect` on the repo and writes a fresh inspect baseline file so future inspect-self CI is meaningful.
5. Customer can re-run `tt init --upgrade` to pull in newer template versions without losing local customization.

## 3. Non-goals

- Wiring CI pipelines (the customer chooses GitHub Actions / GitLab / etc.). We provide the templates; they activate.
- Installing into a non-git directory. `tt init` requires a `.git/` to exist; otherwise it asks the user to `git init` first.
- Doing anything in `cloud/` repos. This is the public OSS CLI surface.

## 4. Architecture

```
crates/cli/
└── src/
    ├── main.rs                          [modified — register Init subcommand]
    └── init/
        ├── mod.rs                       [new — orchestrator + CLI args]
        ├── detect.rs                    [new — language/framework probes]
        ├── templates.rs                 [new — embedded template fetch + render]
        ├── merge.rs                     [new — idempotency: skip / overwrite / merge AGENTS.md]
        ├── manifest.rs                  [new — .tt-init.lock with installed template versions]
        ├── baseline.rs                  [new — runs `tt inspect` and writes baseline]
        └── prompts.rs                   [new — interactive confirmation for --interactive]

crates/cli/
└── templates/                           [new — embedded via include_dir]
    └── init/
        ├── AGENTS.md.tera               [tera template; {{ project_name }} / {{ language }}]
        ├── .claude/
        │   ├── hooks/
        │   │   ├── pre-edit-guard.sh
        │   │   ├── cost-cap-check.sh
        │   │   └── audit-line.sh
        │   ├── settings.json.tera
        │   ├── budget.toml.tera
        │   ├── BACKLOG.md.tera
        │   └── HANDOFF.md
        ├── .gitignore.append            [lines to append to existing .gitignore]
        └── .github/
            └── workflows/
                ├── inspect-self.yml.tera
                └── tt-cost-report.yml.tera

docs/
└── tt-init-usage.md                     [new — how customers run + customize]
```

### 4.1 Templates engine

Templates use **Tera** (Rust template engine, similar to Jinja2). Embedded into the binary via `include_dir!`. Variables resolved from the detect pass.

### 4.2 Idempotency manifest

`.tt-init.lock` (gitignored by the install) records per-installed-template SHA-256 of the source. On `--upgrade`:
- If installed file matches its lock entry → safe to overwrite with new template version.
- If installed file differs → customer modified it → skip with warning, suggest `--force` to overwrite or `--diff` to review.

## 5. CLI surface

```
tt init [OPTIONS]

  Install TokenTrimmer best-practices into the current repo.

OPTIONS:
  --path <PATH>                Target directory. Default: current working dir.
  --language <LANG>            Override auto-detection. {python|typescript|rust|go|java|mixed}
  --framework <FW>             Override detection. Comma-separated. e.g. langchain,fastapi
  --interactive                Prompt before each file write.
  --upgrade                    Re-run on existing install; pull newer template versions.
  --force                      Overwrite locally-modified templates on --upgrade.
  --diff                       With --upgrade, show diff per modified template; do not write.
  --skip-baseline              Don't run `tt inspect` after install.
  --skip-hooks                 Don't install .claude/hooks/.
  --skip-workflows             Don't install .github/workflows/.
  --dry-run                    Print planned writes; touch nothing.
  -h, --help                   Print help.
```

### 5.1 Output

On success:
```
✓ Detected: Python + LangChain
✓ Wrote AGENTS.md (847 bytes)
✓ Wrote .claude/settings.json (412 bytes)
✓ Wrote .claude/hooks/pre-edit-guard.sh + 2 more (mode 755)
✓ Wrote .claude/BACKLOG.md (216 bytes)
✓ Wrote .github/workflows/inspect-self.yml (1.1 KB)
✓ Ran `tt inspect` baseline: 3 findings (1 high, 2 medium) → .claude/inspect-baseline.json
✓ Appended 4 lines to .gitignore

Next steps:
  1. Review AGENTS.md and customize the "Conventions" section.
  2. Commit: git add . && git commit -m "chore: bootstrap tt init harness"
  3. Push and verify the inspect-self workflow runs green.
  4. Set up an account at tokentrimmer.com to unlock cost monitoring.
```

## 6. Language / framework detection

| Signal | Conclusion |
|---|---|
| `pyproject.toml` / `requirements.txt` / `setup.py` | python |
| `package.json` | typescript (default; `.ts` files raise confidence) or javascript |
| `Cargo.toml` | rust |
| `go.mod` | go |
| `pom.xml` / `build.gradle` | java |
| Mix of multiple | mixed (installs union of relevant rules) |

Framework detection (additive, never replaces language):
- Python: search `pyproject.toml` + `requirements.txt` for `langchain`, `openai`, `anthropic`, `instructor`, `litellm`, `fastapi`
- TS: search `package.json` deps for `ai` (Vercel AI SDK), `@anthropic-ai/sdk`, `openai`, `langchain`, `@langchain/*`
- Rust: scan workspace `Cargo.toml` for known LLM crates

## 7. Idempotency strategy per file

| File | Strategy |
|---|---|
| `AGENTS.md` | If exists and unchanged from a prior template → safe to upgrade. If user-modified → prompt or `--force`. If exists from non-`tt init` source → skip with note: "AGENTS.md exists, not modified by `tt init`. Merge manually or `--force` to overwrite." |
| `.claude/hooks/*.sh` | Same as AGENTS.md. |
| `.claude/settings.json` | Merge-aware: parse JSON, add `tt`-prefixed keys, never touch other keys. |
| `.claude/BACKLOG.md` | If exists → skip entirely. New template only on first install. Customer's own backlog is theirs. |
| `.gitignore` | Append-only. Lines deduped against current contents. Never reordered. |
| `.github/workflows/*.yml` | Idempotent by filename. Won't touch workflows we didn't create. |

## 8. Inspect baseline

After files land, run `tt inspect --output .claude/inspect-baseline.json`. This:
- Sets a known-state snapshot of current findings.
- Future `tt inspect` calls compare against this — only delta findings fail CI.
- Customer can manually edit the baseline to suppress accepted findings.

If `--skip-baseline` is passed, write a stub `.claude/inspect-baseline.json` with `{"findings": [], "skipped": true}` so the comparison logic still works on first CI run.

## 9. Testing

| Layer | Tests |
|---|---|
| Unit (detect) | Given fixture `pyproject.toml` snippets → returns correct language + frameworks. |
| Unit (templates) | Render Tera template with fixture vars → matches insta snapshot. |
| Unit (merge) | Existing `settings.json` + new keys → expected merged JSON. |
| Integration (orchestrator) | Run `tt init` against `tempfile` dir containing fixture `package.json` → assert expected files written with expected content. |
| Integration (upgrade) | Pre-seed `.tt-init.lock` + modified file → verify skip+warning behavior. |
| Integration (idempotent) | Run `tt init` twice → second run should be no-op (zero writes). |
| Smoke (dry-run) | `tt init --dry-run` → assert zero filesystem writes. |

## 10. Rollout

1. Day 0: ship with templates for Python + TypeScript + Rust. Java + Go skip with "language not yet supported, file an issue" message.
2. Day 7: add `tt init --upgrade` path after live-firing on TokenTrimmer's own repo.
3. Day 30: add Java + Go templates if anyone files issues.

## 11. Out of scope

- Interactive customization of AGENTS.md "Conventions" section (post-install manual step).
- Auto-detecting which Inspect rules to include based on detected framework (Day 0 includes all Tier-1 rules; framework-tuned selection is a follow-up).
- `tt init` for the cloud-repo style (`crates/api`, `apps/dashboard`) — that's a different shape.

## 12. References

- Existing assets being bundled: `AGENTS.md`, `.claude/hooks/`, `.claude/agents/`, `.claude/MODEL_ROUTING.md`, `.claude/budget.toml`, `.github/workflows/inspect-self.yml`, `.github/workflows/plan-self.yml` in this repo
- Tera engine docs (templates)
- include_dir crate (embedding template files into the binary)
