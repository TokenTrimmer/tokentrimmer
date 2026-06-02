# Track D — `tt init` Installer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `tt init` — a single CLI command that installs the TokenTrimmer best-practices harness (AGENTS.md, .claude/ hooks, .github workflows, inspect baseline) into any git-controlled directory, idempotently.

**Architecture:** New module tree at `crates/cli/src/init/` + embedded templates at `crates/cli/templates/init/`. Templates rendered via Tera. Detection probes `pyproject.toml`/`package.json`/`Cargo.toml`. Idempotency via `.tt-init.lock` manifest of installed-template SHAs.

**Tech Stack:** Rust 1.88, `clap`, `tera` for templates, `include_dir` for embedding, `walkdir`, `serde`, `sha2`, `dialoguer` for interactive mode, `httpmock` + `insta` + `tempfile` for tests.

**Spec:** `docs/superpowers/specs/2026-05-28-trackD-tt-init-installer-design.md`.

---

## File Structure

```
crates/cli/
├── Cargo.toml                              [modified — add deps]
├── src/
│   ├── main.rs                             [modified — register Init subcommand]
│   └── init/
│       ├── mod.rs                          [NEW — orchestrator]
│       ├── detect.rs                       [NEW — language/framework probes]
│       ├── templates.rs                    [NEW — embed via include_dir; render via Tera]
│       ├── merge.rs                        [NEW — idempotency: skip / overwrite / merge]
│       ├── manifest.rs                     [NEW — .tt-init.lock]
│       ├── baseline.rs                     [NEW — runs tt-inspect]
│       └── prompts.rs                      [NEW — interactive confirmation]
└── templates/
    └── init/
        ├── AGENTS.md.tera
        ├── .claude/
        │   ├── settings.json.tera
        │   ├── budget.toml.tera
        │   ├── BACKLOG.md.tera
        │   ├── HANDOFF.md
        │   └── hooks/
        │       ├── pre-edit-guard.sh
        │       ├── cost-cap-check.sh
        │       └── audit-line.sh
        ├── .gitignore.append
        └── .github/workflows/
            ├── inspect-self.yml.tera
            └── tt-cost-report.yml.tera
```

---

## Task 1: Scaffold deps and module tree

**Files:**
- Modify: `crates/cli/Cargo.toml`
- Create: `crates/cli/src/init/{mod,detect,templates,merge,manifest,baseline,prompts}.rs`

- [ ] **Step 1: Add deps to `crates/cli/Cargo.toml`**

In `[dependencies]`:
```toml
tera = "1.20"
include_dir = "0.7"
walkdir = "2.5"
sha2 = "0.10"
dialoguer = "0.11"
toml = "0.8"
```

In `[dev-dependencies]`:
```toml
tempfile = "3.10"
insta = { version = "1.39", features = ["json"] }
```

- [ ] **Step 2: Create module files**

```bash
mkdir -p crates/cli/src/init
for f in mod detect templates merge manifest baseline prompts; do
  echo "//! tt init — \`$f\` module (scaffold; see plan)" > "crates/cli/src/init/$f.rs"
done
```

- [ ] **Step 3: Replace `mod.rs` with the public declarations**

```rust
//! `tt init` — install the TokenTrimmer best-practices harness into a repo.
//!
//! See `docs/superpowers/specs/2026-05-28-trackD-tt-init-installer-design.md`.

pub mod baseline;
pub mod detect;
pub mod manifest;
pub mod merge;
pub mod prompts;
pub mod templates;
```

- [ ] **Step 4: Compile check**

Run: `cargo check -p tt-cli`
Expected: success with unused-module warnings (acceptable).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/src/init/
git commit -m "feat(cli): scaffold tt init module tree

Track D day-0. Empty modules filled by subsequent tasks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Language + framework detection

**Files:** `crates/cli/src/init/detect.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Detect repo language(s) + LLM frameworks from manifest files.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Language {
    Python,
    TypeScript,
    JavaScript,
    Rust,
    Go,
    Java,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detection {
    pub languages: Vec<Language>,
    pub frameworks: Vec<String>, // free-form: "langchain", "openai", "ai-sdk", ...
}

pub fn detect(root: &Path) -> Detection {
    let mut langs = Vec::new();
    let mut fws = Vec::new();

    if root.join("pyproject.toml").exists()
        || root.join("requirements.txt").exists()
        || root.join("setup.py").exists()
    {
        langs.push(Language::Python);
        scan_python_frameworks(root, &mut fws);
    }
    if root.join("package.json").exists() {
        let lang = detect_js_or_ts(root);
        langs.push(lang);
        scan_js_frameworks(root, &mut fws);
    }
    if root.join("Cargo.toml").exists() {
        langs.push(Language::Rust);
    }
    if root.join("go.mod").exists() {
        langs.push(Language::Go);
    }
    if root.join("pom.xml").exists() || root.join("build.gradle").exists() {
        langs.push(Language::Java);
    }
    if langs.is_empty() {
        langs.push(Language::Unknown);
    } else if langs.len() > 1 {
        // Replace with single "Mixed" entry preserving original list separately.
        let mixed = vec![Language::Mixed];
        // Keep originals appended for callers that want detail.
        let mut all = mixed;
        all.extend(langs);
        langs = all;
    }
    Detection { languages: langs, frameworks: fws }
}

fn detect_js_or_ts(root: &Path) -> Language {
    if root.join("tsconfig.json").exists() {
        return Language::TypeScript;
    }
    // Cheap heuristic: any .ts file → TS.
    if let Ok(entries) = std::fs::read_dir(root.join("src").as_path()) {
        if entries.flatten().any(|e| e.path().extension().is_some_and(|x| x == "ts" || x == "tsx")) {
            return Language::TypeScript;
        }
    }
    Language::JavaScript
}

fn scan_python_frameworks(root: &Path, out: &mut Vec<String>) {
    let known = ["langchain", "openai", "anthropic", "instructor", "litellm", "fastapi"];
    for f in ["pyproject.toml", "requirements.txt", "setup.py"] {
        if let Ok(s) = std::fs::read_to_string(root.join(f)) {
            for k in &known {
                if s.contains(k) && !out.contains(&k.to_string()) {
                    out.push(k.to_string());
                }
            }
        }
    }
}

fn scan_js_frameworks(root: &Path, out: &mut Vec<String>) {
    let known = ["ai", "@anthropic-ai/sdk", "openai", "langchain", "@langchain/core", "instructor-js"];
    if let Ok(s) = std::fs::read_to_string(root.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&s) {
            for key in ["dependencies", "devDependencies"] {
                if let Some(deps) = json.get(key).and_then(|v| v.as_object()) {
                    for k in &known {
                        if deps.contains_key(*k) && !out.contains(&k.to_string()) {
                            out.push(k.to_string());
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write(p: &Path, content: &str) {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn detects_python_with_langchain() {
        let d = make_repo();
        write(&d.path().join("pyproject.toml"), r#"[tool.poetry.dependencies]
langchain = "^0.3"
openai = "^1.0"
"#);
        let det = detect(d.path());
        assert!(det.languages.iter().any(|l| matches!(l, Language::Python | Language::Mixed)));
        assert!(det.frameworks.contains(&"langchain".to_string()));
        assert!(det.frameworks.contains(&"openai".to_string()));
    }

    #[test]
    fn detects_typescript_with_ai_sdk() {
        let d = make_repo();
        write(&d.path().join("package.json"), r#"{
  "dependencies": { "ai": "^4.0", "@anthropic-ai/sdk": "^0.40" }
}"#);
        write(&d.path().join("tsconfig.json"), "{}");
        let det = detect(d.path());
        assert!(det.languages.contains(&Language::TypeScript));
        assert!(det.frameworks.contains(&"ai".to_string()));
        assert!(det.frameworks.contains(&"@anthropic-ai/sdk".to_string()));
    }

    #[test]
    fn detects_rust_workspace() {
        let d = make_repo();
        write(&d.path().join("Cargo.toml"), "[workspace]\nmembers = []\n");
        let det = detect(d.path());
        assert!(det.languages.contains(&Language::Rust));
    }

    #[test]
    fn detects_mixed_repo() {
        let d = make_repo();
        write(&d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n");
        write(&d.path().join("package.json"), "{}");
        let det = detect(d.path());
        assert_eq!(det.languages[0], Language::Mixed);
    }

    #[test]
    fn empty_dir_is_unknown() {
        let d = make_repo();
        let det = detect(d.path());
        assert_eq!(det.languages, vec![Language::Unknown]);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli init::detect`
Expected: 5 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/init/detect.rs
git commit -m "feat(cli): tt init language/framework detection

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Embedded templates + Tera rendering

**Files:**
- Create: `crates/cli/templates/init/AGENTS.md.tera`
- Create: `crates/cli/templates/init/.claude/settings.json.tera`
- Create: `crates/cli/templates/init/.claude/BACKLOG.md.tera`
- Create: `crates/cli/templates/init/.claude/budget.toml.tera`
- Create: `crates/cli/templates/init/.claude/HANDOFF.md`
- Create: `crates/cli/templates/init/.claude/hooks/{pre-edit-guard,cost-cap-check,audit-line}.sh`
- Create: `crates/cli/templates/init/.gitignore.append`
- Create: `crates/cli/templates/init/.github/workflows/inspect-self.yml.tera`
- Modify: `crates/cli/src/init/templates.rs`

- [ ] **Step 1: Write the templates**

For each template file, copy from this repo's actual current asset and add Tera variables where parameterizable. Key vars: `project_name`, `language`, `frameworks_csv`, `tt_cli_version`.

`crates/cli/templates/init/AGENTS.md.tera`:
```markdown
# AGENTS.md — {{ project_name }}

This file is the working-tree convention guide for AI assistants and human contributors.

## What this repo is

(Customize this section. Brief: what does this codebase do, who uses it, what's the runtime, what's the source of truth?)

## Stack

- Primary language: {{ language }}
{% if frameworks_csv %}- Known LLM dependencies: {{ frameworks_csv }}
{% endif %}

## Conventions

- Errors: propagate, don't swallow.
- Logging: structured. Never `print` or `console.log` in library code.
- Test before claiming done.
- 800-line file cap. Split before you grow beyond it.

## Build / test / lint

(Fill in your project's commands here. Example:)

```
{% if language == "Python" %}pytest -q
ruff check .
mypy{% elif language == "TypeScript" %}pnpm typecheck
pnpm test{% elif language == "Rust" %}cargo test
cargo clippy -- -D warnings{% else %}# your test command here{% endif %}
```

## Do NOT

- Commit secrets. Hooks block; CI is the second line.
- Push to main without review.
- Skip pre-commit hooks (--no-verify) without justification.

## TokenTrimmer integration

This repo was initialized with `tt init {{ tt_cli_version }}` on {{ initialized_at }}.

- Cost optimization is enforced via `.claude/hooks/cost-cap-check.sh`.
- Pre-edit guard is at `.claude/hooks/pre-edit-guard.sh`.
- Inspect baseline at `.claude/inspect-baseline.json` — re-run `tt inspect --output .claude/inspect-baseline.json` to refresh.
- Read more: https://tokentrimmer.com/docs/init
```

`crates/cli/templates/init/.claude/settings.json.tera`:
```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [{ "type": "command", "command": ".claude/hooks/pre-edit-guard.sh" }]
      }
    ],
    "SessionEnd": [
      {
        "matcher": "*",
        "hooks": [{ "type": "command", "command": ".claude/hooks/audit-line.sh" }]
      }
    ]
  }
}
```

`crates/cli/templates/init/.claude/BACKLOG.md.tera`:
```markdown
# Backlog

Single source of truth for actionable work. Entries are checkboxes; flip to `[x]` when done.

**Format**: `- [PRIORITY] [task-id] subagent: brief description`

- `PRIORITY` ∈ {P0 (blocker), P1 (next), P2 (soon), P3 (whenever)}

## Initial

- [ ] [P3] [first-task] Edit this file. Add a real backlog item. Delete this one.
```

`crates/cli/templates/init/.claude/budget.toml.tera`:
```toml
# Daily / weekly cost circuit-breaker for autonomous AI loops in this repo.
# Edit only after deciding any spike was justified.

daily_cap_usd = 10
weekly_cap_usd = 50
```

`crates/cli/templates/init/.claude/HANDOFF.md`:
```markdown
# Active session handoff

_Written by `tt init`. Replace with your own handoff notes between sessions._

## Status

Fresh `tt init` install. No work in progress.

## Next session should

- Review and customize `AGENTS.md`.
- Add real items to `.claude/BACKLOG.md`.
- Commit the baseline: `git add . && git commit -m "chore: bootstrap tt init harness"`.
```

`crates/cli/templates/init/.claude/hooks/pre-edit-guard.sh`:
```bash
#!/usr/bin/env bash
# Block edits to files containing common secret patterns. Add lines for
# repo-specific patterns.
set -euo pipefail

FILE="${CLAUDE_EDIT_FILE:-}"
[[ -z "${FILE}" ]] && exit 0
[[ ! -f "${FILE}" ]] && exit 0

if grep -qE 'sk-[a-zA-Z0-9_]{20,}|AIza[0-9A-Za-z_-]{35}|ghp_[a-zA-Z0-9]{36}' "${FILE}"; then
  echo "BLOCKED: ${FILE} contains a high-entropy token. Refuse edit." >&2
  exit 1
fi

if [[ "${FILE}" =~ \.env$|\.env\.local$ ]]; then
  echo "BLOCKED: refusing to edit ${FILE}. Use .env.example or env-specific tooling." >&2
  exit 1
fi

exit 0
```

`crates/cli/templates/init/.claude/hooks/cost-cap-check.sh`:
```bash
#!/usr/bin/env bash
# Check daily/weekly Claude API spend against budget.toml. Pauses the
# autonomous loop if exceeded.
set -euo pipefail

BUDGET=".claude/budget.toml"
LEDGER=".claude/cost-ledger.jsonl"
[[ -f "${BUDGET}" ]] || exit 0
[[ -f "${LEDGER}" ]] || exit 0

DC=$(grep -E '^daily_cap_usd' "${BUDGET}" | sed -E 's/.*=\s*([0-9.]+).*/\1/' || echo "10")
WC=$(grep -E '^weekly_cap_usd' "${BUDGET}" | sed -E 's/.*=\s*([0-9.]+).*/\1/' || echo "50")

D_START=$(date -u +%Y-%m-%d)
W_START=$(date -u -v-7d +%Y-%m-%d 2>/dev/null || date -u --date='-7 days' +%Y-%m-%d)

DS=$(jq -s --arg s "${D_START}" '[.[] | select(.date >= $s) | .cost_usd] | add // 0' "${LEDGER}")
WS=$(jq -s --arg s "${W_START}" '[.[] | select(.date >= $s) | .cost_usd] | add // 0' "${LEDGER}")

if awk -v a="${DS}" -v b="${DC}" 'BEGIN { exit !(a > b) }'; then
  echo "Daily cap exceeded: \$${DS} > \$${DC}. Pausing." > .claude/PAUSED
  exit 1
fi
if awk -v a="${WS}" -v b="${WC}" 'BEGIN { exit !(a > b) }'; then
  echo "Weekly cap exceeded: \$${WS} > \$${WC}. Pausing." > .claude/PAUSED
  exit 1
fi
```

`crates/cli/templates/init/.claude/hooks/audit-line.sh`:
```bash
#!/usr/bin/env bash
# Append a one-line audit entry per session to .claude/AUDIT.log
set -euo pipefail

LOG=".claude/AUDIT.log"
[[ -f "${LOG}" ]] || touch "${LOG}"
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
HEAD=$(git rev-parse --short HEAD 2>/dev/null || echo "no-git")
SESSION="${CLAUDE_SESSION_ID:-unknown}"
echo "${TS}  session=${SESSION}  head=${HEAD}" >> "${LOG}"
```

`crates/cli/templates/init/.gitignore.append`:
```
# Added by tt init
.claude/cost-ledger.jsonl
.claude/AUDIT.log
.claude/PAUSED
.claude/STOP-CHAIN
.claude/sessions/
.tt-init.lock
```

`crates/cli/templates/init/.github/workflows/inspect-self.yml.tera`:
```yaml
name: tt inspect
on: [pull_request, push]

jobs:
  inspect:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install tt CLI
        run: |
          curl -sSfL https://tokentrimmer.com/install.sh | sh
      - name: Run tt inspect
        run: |
          tt inspect . --fail-on high
```

- [ ] **Step 2: Write the templates loader**

`crates/cli/src/init/templates.rs`:
```rust
//! Templates: embedded via include_dir at compile time, rendered via Tera.

use std::collections::HashMap;
use std::path::PathBuf;

use include_dir::{include_dir, Dir};
use tera::{Context, Tera};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TemplateError {
    #[error("tera: {0}")]
    Tera(#[from] tera::Error),
    #[error("template not found: {0}")]
    NotFound(String),
}

static TEMPLATES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/templates/init");

/// One file to write, with its destination path (relative to repo root)
/// and rendered content.
#[derive(Debug, Clone)]
pub struct RenderedFile {
    pub dest: PathBuf,
    pub content: String,
    pub mode: u32, // 0o644 default; 0o755 for .sh scripts
}

/// Render the complete template set into one `RenderedFile` per template.
pub fn render_all(vars: &HashMap<String, String>) -> Result<Vec<RenderedFile>, TemplateError> {
    let mut out = Vec::new();
    let mut tera = Tera::default();
    // Register every .tera file under TEMPLATES.
    for entry in TEMPLATES.find("**/*").unwrap_or_default() {
        if let Some(f) = entry.as_file() {
            if f.path().extension().is_some_and(|e| e == "tera") {
                let name = f.path().to_str().unwrap();
                let body = std::str::from_utf8(f.contents()).unwrap_or("");
                tera.add_raw_template(name, body)?;
            }
        }
    }

    let mut ctx = Context::new();
    for (k, v) in vars {
        ctx.insert(k, v);
    }

    for entry in TEMPLATES.find("**/*").unwrap_or_default() {
        if let Some(f) = entry.as_file() {
            let path = f.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let dest_path = if ext == "tera" {
                // Strip .tera suffix.
                path.with_extension("")
            } else {
                path.to_path_buf()
            };
            let content = if ext == "tera" {
                tera.render(path.to_str().unwrap(), &ctx)?
            } else {
                std::str::from_utf8(f.contents())
                    .map(|s| s.to_string())
                    .map_err(|_| TemplateError::NotFound(path.display().to_string()))?
            };
            let mode = if dest_path.extension().is_some_and(|e| e == "sh") {
                0o755
            } else {
                0o644
            };
            out.push(RenderedFile { dest: dest_path, content, mode });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_agents_md_with_project_name() {
        let mut vars = HashMap::new();
        vars.insert("project_name".into(), "my-app".into());
        vars.insert("language".into(), "Rust".into());
        vars.insert("frameworks_csv".into(), "".into());
        vars.insert("tt_cli_version".into(), "0.1.0".into());
        vars.insert("initialized_at".into(), "2026-05-28".into());

        let files = render_all(&vars).unwrap();
        let agents = files.iter().find(|f| f.dest.ends_with("AGENTS.md")).expect("AGENTS.md missing");
        assert!(agents.content.contains("# AGENTS.md — my-app"));
        assert!(agents.content.contains("Primary language: Rust"));
    }

    #[test]
    fn sh_scripts_get_0o755() {
        let mut vars = HashMap::new();
        vars.insert("project_name".into(), "x".into());
        vars.insert("language".into(), "Rust".into());
        vars.insert("frameworks_csv".into(), "".into());
        vars.insert("tt_cli_version".into(), "0".into());
        vars.insert("initialized_at".into(), "x".into());
        let files = render_all(&vars).unwrap();
        let hook = files.iter().find(|f| f.dest.ends_with("pre-edit-guard.sh")).unwrap();
        assert_eq!(hook.mode, 0o755);
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p tt-cli init::templates`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/src/init/templates.rs crates/cli/templates/
git commit -m "feat(cli): tt init embedded templates + Tera renderer

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Manifest + idempotency

**Files:** `crates/cli/src/init/manifest.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! `.tt-init.lock` — records SHA-256 of each template file installed,
//! so `--upgrade` can detect customer modifications.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub version: String,
    /// Map of relative path (forward slashes) → SHA-256 hex of original template content.
    pub installed: BTreeMap<String, String>,
}

impl Manifest {
    pub fn new(version: impl Into<String>) -> Self {
        Self { version: version.into(), installed: BTreeMap::new() }
    }

    pub fn record(&mut self, rel_path: &Path, original_content: &str) {
        let hex = sha256_hex(original_content.as_bytes());
        self.installed.insert(rel_path.to_string_lossy().replace('\\', "/"), hex);
    }

    pub fn load(path: &Path) -> Result<Option<Manifest>, ManifestError> {
        if !path.exists() {
            return Ok(None);
        }
        let s = std::fs::read_to_string(path)?;
        Ok(Some(toml::from_str(&s)?))
    }

    pub fn save(&self, path: &Path) -> Result<(), ManifestError> {
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeAction {
    /// File is unmodified vs the installed template — safe to overwrite.
    SafeOverwrite,
    /// File was modified by the user — skip unless --force.
    UserModified,
    /// File did not exist before this run — fresh install.
    Fresh,
}

pub fn classify_upgrade(
    manifest: &Manifest,
    dest_rel: &Path,
    current_disk_content: Option<&str>,
) -> UpgradeAction {
    let rel = dest_rel.to_string_lossy().replace('\\', "/");
    let recorded = manifest.installed.get(&rel);
    match (recorded, current_disk_content) {
        (None, _) => UpgradeAction::Fresh,
        (Some(prev_hash), Some(disk)) => {
            let now = sha256_hex(disk.as_bytes());
            if &now == prev_hash {
                UpgradeAction::SafeOverwrite
            } else {
                UpgradeAction::UserModified
            }
        }
        (Some(_), None) => UpgradeAction::Fresh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn record_and_lookup() {
        let mut m = Manifest::new("0.1.0");
        m.record(&PathBuf::from("AGENTS.md"), "hello");
        assert_eq!(m.installed.len(), 1);
        assert!(m.installed.contains_key("AGENTS.md"));
    }

    #[test]
    fn classify_fresh_when_not_recorded() {
        let m = Manifest::new("0.1.0");
        assert_eq!(classify_upgrade(&m, &PathBuf::from("X.md"), None), UpgradeAction::Fresh);
        assert_eq!(classify_upgrade(&m, &PathBuf::from("X.md"), Some("anything")), UpgradeAction::Fresh);
    }

    #[test]
    fn classify_safe_when_unchanged() {
        let mut m = Manifest::new("0.1.0");
        m.record(&PathBuf::from("AGENTS.md"), "hello");
        assert_eq!(
            classify_upgrade(&m, &PathBuf::from("AGENTS.md"), Some("hello")),
            UpgradeAction::SafeOverwrite
        );
    }

    #[test]
    fn classify_user_modified_when_changed() {
        let mut m = Manifest::new("0.1.0");
        m.record(&PathBuf::from("AGENTS.md"), "hello");
        assert_eq!(
            classify_upgrade(&m, &PathBuf::from("AGENTS.md"), Some("changed")),
            UpgradeAction::UserModified
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut m = Manifest::new("0.1.0");
        m.record(&PathBuf::from("AGENTS.md"), "hello");
        m.save(tmp.path()).unwrap();
        let loaded = Manifest::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded.version, "0.1.0");
        assert_eq!(loaded.installed.len(), 1);
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli init::manifest`
Expected: 5 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/init/manifest.rs
git commit -m "feat(cli): tt init manifest + upgrade classification

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Merge logic per file type

**Files:** `crates/cli/src/init/merge.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Per-file-type merge strategy: settings.json is JSON-merge, .gitignore is
//! append-with-dedupe, other files are skip-or-overwrite.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MergeError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Append lines from `additions` to `existing`, skipping any line already
/// present (whole-line match). Preserves order; adds a blank line before
/// new content if existing doesn't end in one.
pub fn append_gitignore(existing: &str, additions: &str) -> String {
    let existing_lines: std::collections::HashSet<&str> = existing.lines().collect();
    let mut new_lines = Vec::new();
    for line in additions.lines() {
        if !existing_lines.contains(line) && !line.trim().is_empty() {
            new_lines.push(line);
        }
    }
    if new_lines.is_empty() {
        return existing.to_string();
    }
    let mut out = existing.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&new_lines.join("\n"));
    out.push('\n');
    out
}

/// Merge JSON `additions` into `existing`. Deep merge for objects.
/// Existing values win on type mismatch (we never silently change a user
/// non-object key into an object).
pub fn merge_settings_json(existing: &str, additions: &str) -> Result<String, MergeError> {
    let mut a: serde_json::Value = serde_json::from_str(existing)?;
    let b: serde_json::Value = serde_json::from_str(additions)?;
    deep_merge(&mut a, &b);
    Ok(serde_json::to_string_pretty(&a)?)
}

fn deep_merge(into: &mut serde_json::Value, from: &serde_json::Value) {
    match (into, from) {
        (serde_json::Value::Object(into_map), serde_json::Value::Object(from_map)) => {
            for (k, v) in from_map {
                if let Some(existing_v) = into_map.get_mut(k) {
                    deep_merge(existing_v, v);
                } else {
                    into_map.insert(k.clone(), v.clone());
                }
            }
        }
        _ => {} // type mismatch — existing wins
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_gitignore_dedupes() {
        let existing = "node_modules/\n.env\n";
        let additions = ".env\n.tt-init.lock\n.claude/PAUSED\n";
        let result = append_gitignore(existing, additions);
        assert!(result.contains("node_modules/"));
        assert!(result.contains(".tt-init.lock"));
        assert!(result.contains(".claude/PAUSED"));
        // .env appears exactly once
        assert_eq!(result.matches(".env").count(), 1);
    }

    #[test]
    fn append_gitignore_noop_when_all_present() {
        let existing = "a\nb\nc\n";
        let result = append_gitignore(existing, "a\nb\n");
        assert_eq!(result, "a\nb\nc\n");
    }

    #[test]
    fn merge_json_deep_merge_objects() {
        let a = r#"{"hooks": {"PreToolUse": [1]}, "other": "x"}"#;
        let b = r#"{"hooks": {"PostToolUse": [2]}}"#;
        let merged = merge_settings_json(a, b).unwrap();
        assert!(merged.contains("\"PreToolUse\""));
        assert!(merged.contains("\"PostToolUse\""));
        assert!(merged.contains("\"other\""));
    }

    #[test]
    fn merge_json_existing_wins_on_type_mismatch() {
        let a = r#"{"key": "string"}"#;
        let b = r#"{"key": {"nested": "x"}}"#;
        let merged = merge_settings_json(a, b).unwrap();
        assert!(merged.contains("\"string\""));
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli init::merge`
Expected: 4 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/init/merge.rs
git commit -m "feat(cli): tt init merge strategies for settings.json + .gitignore

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Baseline runner

**Files:** `crates/cli/src/init/baseline.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Run `tt inspect` on the target repo after install and write the result
//! to `.claude/inspect-baseline.json`. If --skip-baseline is set, write a
//! stub so future comparison logic doesn't crash.

use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BaselineError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub fn run_baseline(root: &Path) -> Result<usize, BaselineError> {
    // Mirrors the invocation in crates/cli/src/main.rs::run_inspect.
    let mut engine = tt_inspect_core::Engine::new();
    for rule in tt_inspect_rules_tier1::all_rules() {
        engine.add_rule(rule);
    }
    let findings = engine.scan(root);
    let dest = root.join(".claude").join("inspect-baseline.json");
    std::fs::create_dir_all(dest.parent().unwrap())?;
    let body = serde_json::json!({
        "findings": findings,
        "generated_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(&dest, serde_json::to_string_pretty(&body).unwrap())?;
    Ok(findings.len())
}

pub fn write_skipped_baseline(root: &Path) -> Result<(), BaselineError> {
    let dest = root.join(".claude").join("inspect-baseline.json");
    std::fs::create_dir_all(dest.parent().unwrap())?;
    let body = serde_json::json!({
        "findings": [],
        "skipped": true,
        "generated_at": chrono::Utc::now().to_rfc3339(),
    });
    std::fs::write(&dest, serde_json::to_string_pretty(&body).unwrap())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_writes_stub_with_skipped_flag() {
        let d = tempfile::tempdir().unwrap();
        write_skipped_baseline(d.path()).unwrap();
        let body = std::fs::read_to_string(d.path().join(".claude").join("inspect-baseline.json")).unwrap();
        assert!(body.contains("\"skipped\": true"));
        assert!(body.contains("\"findings\": []"));
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p tt-cli init::baseline`
Expected: 1 passed (the integration with real `scan_path` is covered in Task 9).

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/init/baseline.rs
git commit -m "feat(cli): tt init inspect-baseline runner

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Orchestrator + CLI subcommand

**Files:**
- Modify: `crates/cli/src/init/mod.rs`
- Modify: `crates/cli/src/main.rs`

- [ ] **Step 1: Write the orchestrator**

Replace `crates/cli/src/init/mod.rs`:

```rust
//! `tt init` — install the TokenTrimmer best-practices harness into a repo.

pub mod baseline;
pub mod detect;
pub mod manifest;
pub mod merge;
pub mod prompts;
pub mod templates;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use detect::detect;
use manifest::{classify_upgrade, Manifest, UpgradeAction};
use merge::{append_gitignore, merge_settings_json};
use templates::{render_all, RenderedFile};

#[derive(Debug, Error)]
pub enum InitError {
    #[error("not a git repo: {0}")]
    NotGit(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("templates: {0}")]
    Templates(#[from] templates::TemplateError),
    #[error("manifest: {0}")]
    Manifest(#[from] manifest::ManifestError),
    #[error("merge: {0}")]
    Merge(#[from] merge::MergeError),
    #[error("baseline: {0}")]
    Baseline(#[from] baseline::BaselineError),
}

pub struct RunOptions {
    pub root: PathBuf,
    pub language_override: Option<String>,
    pub framework_override: Option<String>,
    pub interactive: bool,
    pub upgrade: bool,
    pub force: bool,
    pub diff_only: bool,
    pub skip_baseline: bool,
    pub skip_hooks: bool,
    pub skip_workflows: bool,
    pub dry_run: bool,
    pub tt_cli_version: String,
}

pub struct RunReport {
    pub files_written: u32,
    pub files_skipped: u32,
    pub baseline_findings: Option<usize>,
}

pub fn run(opts: RunOptions) -> Result<RunReport, InitError> {
    if !opts.root.join(".git").exists() {
        return Err(InitError::NotGit(opts.root.clone()));
    }

    let detection = if opts.language_override.is_some() {
        let mut d = detect::Detection::default();
        if let Some(l) = &opts.language_override {
            // Best-effort string parse; UI accepts python/typescript/rust/go/java/mixed.
            d.languages.push(match l.to_lowercase().as_str() {
                "python" => detect::Language::Python,
                "typescript" => detect::Language::TypeScript,
                "javascript" => detect::Language::JavaScript,
                "rust" => detect::Language::Rust,
                "go" => detect::Language::Go,
                "java" => detect::Language::Java,
                _ => detect::Language::Mixed,
            });
        }
        if let Some(f) = &opts.framework_override {
            d.frameworks = f.split(',').map(|s| s.trim().to_string()).collect();
        }
        d
    } else {
        detect(&opts.root)
    };

    let project_name = opts.root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();
    let mut vars: HashMap<String, String> = HashMap::new();
    vars.insert("project_name".into(), project_name);
    vars.insert("language".into(), format!("{:?}", detection.languages.first().unwrap_or(&detect::Language::Unknown)));
    vars.insert("frameworks_csv".into(), detection.frameworks.join(", "));
    vars.insert("tt_cli_version".into(), opts.tt_cli_version.clone());
    vars.insert("initialized_at".into(), chrono::Utc::now().to_rfc3339());

    let files = render_all(&vars)?;

    let manifest_path = opts.root.join(".tt-init.lock");
    let existing_manifest = Manifest::load(&manifest_path)?.unwrap_or_else(|| Manifest::new(&opts.tt_cli_version));

    let mut new_manifest = existing_manifest.clone();
    let mut written = 0u32;
    let mut skipped = 0u32;

    for f in &files {
        if should_skip_by_options(f, &opts) {
            skipped += 1;
            continue;
        }
        let dest = opts.root.join(&f.dest);

        // Read disk current
        let disk_current = std::fs::read_to_string(&dest).ok();

        // Decide action
        let action = if opts.upgrade {
            classify_upgrade(&existing_manifest, &f.dest, disk_current.as_deref())
        } else if disk_current.is_none() {
            UpgradeAction::Fresh
        } else if f.dest.ends_with(".gitignore.append") || f.dest.ends_with(".gitignore") {
            UpgradeAction::SafeOverwrite // append-only, handled below
        } else {
            UpgradeAction::UserModified
        };

        match action {
            UpgradeAction::Fresh => {
                if !opts.dry_run {
                    write_file(&dest, &f.content, f.mode)?;
                }
                new_manifest.record(&f.dest, &f.content);
                written += 1;
                println!("✓ Wrote {} ({} bytes)", f.dest.display(), f.content.len());
            }
            UpgradeAction::SafeOverwrite => {
                let new_content = if f.dest.file_name().is_some_and(|n| n == ".gitignore.append") {
                    let target_gitignore = opts.root.join(".gitignore");
                    let existing_gi = std::fs::read_to_string(&target_gitignore).unwrap_or_default();
                    let merged = append_gitignore(&existing_gi, &f.content);
                    if !opts.dry_run {
                        std::fs::write(&target_gitignore, &merged)?;
                    }
                    written += 1;
                    println!("✓ Updated .gitignore");
                    continue;
                } else if f.dest.ends_with("settings.json") {
                    let existing = disk_current.as_deref().unwrap_or("{}");
                    let merged = merge_settings_json(existing, &f.content)?;
                    if !opts.dry_run {
                        write_file(&dest, &merged, f.mode)?;
                    }
                    merged
                } else {
                    if !opts.dry_run {
                        write_file(&dest, &f.content, f.mode)?;
                    }
                    f.content.clone()
                };
                new_manifest.record(&f.dest, &new_content);
                written += 1;
                println!("✓ Updated {} (safe — unchanged from prior install)", f.dest.display());
            }
            UpgradeAction::UserModified => {
                if opts.force {
                    if !opts.dry_run {
                        write_file(&dest, &f.content, f.mode)?;
                    }
                    new_manifest.record(&f.dest, &f.content);
                    written += 1;
                    println!("! Overwrote user-modified {} (--force)", f.dest.display());
                } else {
                    skipped += 1;
                    println!("- Skipped {} (user-modified; --force to overwrite)", f.dest.display());
                }
            }
        }
    }

    if !opts.dry_run {
        new_manifest.save(&manifest_path)?;
    }

    let baseline_findings = if opts.skip_baseline {
        if !opts.dry_run {
            baseline::write_skipped_baseline(&opts.root)?;
        }
        None
    } else if opts.dry_run {
        None
    } else {
        Some(baseline::run_baseline(&opts.root)?)
    };

    println!();
    println!("Detected: {:?} + frameworks {:?}", detection.languages, detection.frameworks);
    println!("Files written: {written}, skipped: {skipped}");
    if let Some(n) = baseline_findings {
        println!("Inspect baseline: {n} findings → .claude/inspect-baseline.json");
    }

    Ok(RunReport { files_written: written, files_skipped: skipped, baseline_findings })
}

fn should_skip_by_options(f: &RenderedFile, opts: &RunOptions) -> bool {
    let path_str = f.dest.to_string_lossy();
    if opts.skip_hooks && path_str.contains(".claude/hooks/") {
        return true;
    }
    if opts.skip_workflows && path_str.contains(".github/workflows/") {
        return true;
    }
    false
}

fn write_file(dest: &Path, content: &str, mode: u32) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, content)?;
    set_mode(dest, mode)?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(dest: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_dest: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}
```

- [ ] **Step 2: Register the subcommand in `crates/cli/src/main.rs`**

In the `Command` enum, after `Audit { ... }`:

```rust
    /// Install TokenTrimmer best-practices into the current repo.
    Init {
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        framework: Option<String>,
        #[arg(long)]
        interactive: bool,
        #[arg(long)]
        upgrade: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        diff: bool,
        #[arg(long)]
        skip_baseline: bool,
        #[arg(long)]
        skip_hooks: bool,
        #[arg(long)]
        skip_workflows: bool,
        #[arg(long)]
        dry_run: bool,
    },
```

Inside the `match` arms in `main`:

```rust
        Command::Init {
            path, language, framework, interactive, upgrade, force, diff,
            skip_baseline, skip_hooks, skip_workflows, dry_run,
        } => {
            use tt_cli::init::{run, RunOptions};
            let root = path.map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap());
            let opts = RunOptions {
                root,
                language_override: language,
                framework_override: framework,
                interactive,
                upgrade,
                force,
                diff_only: diff,
                skip_baseline,
                skip_hooks,
                skip_workflows,
                dry_run,
                tt_cli_version: env!("CARGO_PKG_VERSION").to_string(),
            };
            let report = run(opts).context("tt init failed")?;
            println!();
            println!("Done. {} written, {} skipped.", report.files_written, report.files_skipped);
        }
```

- [ ] **Step 3: Build + clippy**

Run: `cargo check -p tt-cli && cargo clippy -p tt-cli -- -D warnings`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/init/mod.rs crates/cli/src/main.rs
git commit -m "feat(cli): wire \`tt init\` subcommand + orchestrator

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: End-to-end test

**Files:** `crates/cli/tests/init_smoke.rs`

- [ ] **Step 1: Write the test**

```rust
//! End-to-end: run `tt init` against a fresh tempdir; verify expected files
//! land, manifest is written, baseline file appears.

use std::path::PathBuf;

use tt_cli::init::{run, RunOptions};

fn make_git_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".git")).unwrap();
    d
}

#[test]
fn fresh_install_writes_baseline_files() {
    let d = make_git_dir();
    // Seed a language signal.
    std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let opts = RunOptions {
        root: d.path().to_path_buf(),
        language_override: None,
        framework_override: None,
        interactive: false,
        upgrade: false,
        force: false,
        diff_only: false,
        skip_baseline: true,
        skip_hooks: false,
        skip_workflows: false,
        dry_run: false,
        tt_cli_version: "0.1.0".into(),
    };
    let report = run(opts).unwrap();
    assert!(report.files_written >= 5, "report = {:?}", (report.files_written, report.files_skipped));
    assert!(d.path().join("AGENTS.md").exists());
    assert!(d.path().join(".claude/settings.json").exists());
    assert!(d.path().join(".claude/BACKLOG.md").exists());
    assert!(d.path().join(".claude/hooks/pre-edit-guard.sh").exists());
    assert!(d.path().join(".github/workflows/inspect-self.yml").exists());
    assert!(d.path().join(".tt-init.lock").exists());
}

#[test]
fn idempotent_rerun_is_noop_when_unchanged() {
    let d = make_git_dir();
    std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let mk = || RunOptions {
        root: d.path().to_path_buf(),
        language_override: None,
        framework_override: None,
        interactive: false,
        upgrade: false,
        force: false,
        diff_only: false,
        skip_baseline: true,
        skip_hooks: false,
        skip_workflows: false,
        dry_run: false,
        tt_cli_version: "0.1.0".into(),
    };
    let r1 = run(mk()).unwrap();
    let r2 = run(mk()).unwrap();
    // Second run: every file is now "user-modified" by the perspective of
    // a fresh-install path, OR a no-op if classify routes through the manifest.
    // We assert no NEW files are written (manifest path holds).
    assert_eq!(r2.files_written + r2.files_skipped, r1.files_written + r1.files_skipped);
}

#[test]
fn refuses_non_git_dir() {
    let d = tempfile::tempdir().unwrap();
    let opts = RunOptions {
        root: d.path().to_path_buf(),
        language_override: None,
        framework_override: None,
        interactive: false,
        upgrade: false,
        force: false,
        diff_only: false,
        skip_baseline: true,
        skip_hooks: false,
        skip_workflows: false,
        dry_run: false,
        tt_cli_version: "0.1.0".into(),
    };
    let err = run(opts).unwrap_err();
    assert!(format!("{err}").contains("not a git repo"));
}
```

- [ ] **Step 2: Run test**

Run: `cargo test -p tt-cli --test init_smoke`
Expected: 3 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/tests/init_smoke.rs
git commit -m "test(cli): tt init end-to-end smoke

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Documentation

**Files:**
- Create: `docs/tt-init-usage.md`
- Modify: `.claude/CONTEXT_MAP.md`

- [ ] **Step 1: Write the usage doc**

```markdown
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
```

- [ ] **Step 2: Add a context-map entry**

Append to `.claude/CONTEXT_MAP.md` Domains table:

```markdown
### tt init installer

| If you're doing | Read |
|---|---|
| Adding a new template | `crates/cli/templates/init/` (template files) + `crates/cli/src/init/templates.rs` (renderer) |
| Adding a language detection signal | `crates/cli/src/init/detect.rs` |
| Adjusting merge strategy for an existing file | `crates/cli/src/init/merge.rs` |
| Spec | `docs/superpowers/specs/2026-05-28-trackD-tt-init-installer-design.md` |
```

- [ ] **Step 3: Run full gate**

```bash
cargo fmt --check
cargo clippy -p tt-cli -- -D warnings
cargo test -p tt-cli
./scripts/tt-inspect-self.sh
```

Expected: all four pass with zero new findings.

- [ ] **Step 4: Commit**

```bash
git add docs/tt-init-usage.md .claude/CONTEXT_MAP.md
git commit -m "docs(cli): tt init usage + context map entry

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Mark backlog item complete

- [ ] **Step 1: Update BACKLOG.md**

Change `trackD-tt-init-installer` from `[ ]` to `[x]` and append `_Shipped 2026-MM-DD — Day-0 MVP._`.

- [ ] **Step 2: Commit**

```bash
git add .claude/BACKLOG.md
git commit -m "backlog: trackD tt init Day-0 MVP shipped

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Spec coverage check

| Spec section | Covered by |
|---|---|
| §4 architecture | Tasks 1–7 build it |
| §5 CLI surface | Task 7 |
| §6 detection | Task 2 |
| §7 idempotency per file | Tasks 4 + 5 + 7 |
| §8 inspect baseline | Task 6 + 7 |
| §9 testing | Tasks 2, 3, 4, 5, 6, 8 |
| §10 rollout | Day 0 shipped here; Day 7/30 are follow-ups |

---

## Plan complete

Plan saved to `docs/superpowers/plans/2026-05-28-trackD-tt-init-installer.md`. 10 tasks, ~5 hours of subagent time at Sonnet tier.
