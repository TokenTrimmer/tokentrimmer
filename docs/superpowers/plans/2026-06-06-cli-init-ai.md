# `tt init --ai` (F11) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in `tt init --ai` pass that scans the repo, makes one grounded model call, and tailors `.claude/budget.toml` caps + a marker-delimited `AGENTS.md` section.

**Architecture:** New `crates/cli/src/init/ai.rs` with pure helpers (`parse_ai_config`, `apply_budget_caps`, `render_ai_section`, `upsert_marked_section`) + an async `ai_tailor` orchestrator reusing `advise::detect_models` and `tt_client`. The deterministic `init::run` stays sync/unchanged; the `Init` dispatch arm `.await`s `ai_tailor` only under `--ai`.

**Tech Stack:** Rust, tt_client SDK, serde_json, tempfile + httpmock (tests).

---

### Task 1: Pure helpers + `AiConfig` (in `init/ai.rs`)

**Files:**
- Create: `crates/cli/src/init/ai.rs`
- Modify: `crates/cli/src/init/mod.rs` (add `pub mod ai;` after the other `pub mod` lines; add `pub use ai::ai_tailor;`)

- [ ] **Step 1: Register the module + scaffold helpers with failing tests**

In `crates/cli/src/init/mod.rs`, after `pub mod templates;` add:
```rust
pub mod ai;
pub use ai::ai_tailor;
```

Create `crates/cli/src/init/ai.rs` with the types, helper signatures (`unimplemented!()` bodies), and the test module:

```rust
//! AI pass for `tt init --ai`: one grounded model call → tailored `budget.toml`
//! caps + a marker-delimited `AGENTS.md` section. The deterministic init runs
//! first and unchanged; this layer is additive and opt-in.

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

use crate::advise::{detect_models, ModelUsage};
use crate::context::ResolvedContext;
use crate::ui;

const AI_START: &str = "<!-- tt:ai:start -->";
const AI_END: &str = "<!-- tt:ai:end -->";
const DEFAULT_MODEL: &str = "gpt-4o-mini";

const INIT_AI_SYSTEM: &str = "You are a TokenTrimmer setup assistant. Respond with ONLY a single \
JSON object — no prose, no markdown, no code fences. Shape: \
{\"daily_cap_usd\": <number>, \"weekly_cap_usd\": <number>, \
\"routes\": [{\"from\": \"<model>\", \"to\": \"<cheaper-equivalent model>\", \"reason\": \"<short>\"}], \
\"notes\": \"<one sentence>\"}. Recommend a cheaper equivalent for each detected model where one \
exists (use real model names), and sensible daily/weekly USD caps for a small team.";

#[derive(Debug, Deserialize)]
pub(crate) struct AiConfig {
    #[serde(default)]
    pub(crate) daily_cap_usd: Option<f64>,
    #[serde(default)]
    pub(crate) weekly_cap_usd: Option<f64>,
    #[serde(default)]
    pub(crate) routes: Vec<AiRoute>,
    #[serde(default)]
    pub(crate) notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AiRoute {
    pub(crate) from: String,
    pub(crate) to: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

/// Extract the JSON object (first `{` … last `}`) and parse it. Tolerates code
/// fences / surrounding prose. `None` on any failure.
fn parse_ai_config(text: &str) -> Option<AiConfig> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

/// Replace the `daily_cap_usd` / `weekly_cap_usd` value lines with the rounded
/// whole-dollar AI values (only those provided); preserve all other lines +
/// comments. Whole integers keep the bash cost-cap hook parsing them.
fn apply_budget_caps(content: &str, cfg: &AiConfig) -> String {
    let round = |v: f64| -> u64 { v.max(0.0).round() as u64 };
    content
        .lines()
        .map(|line| {
            let key = line.split('=').next().unwrap_or("").trim();
            match key {
                "daily_cap_usd" => cfg
                    .daily_cap_usd
                    .map(|v| format!("daily_cap_usd = {}", round(v)))
                    .unwrap_or_else(|| line.to_string()),
                "weekly_cap_usd" => cfg
                    .weekly_cap_usd
                    .map(|v| format!("weekly_cap_usd = {}", round(v)))
                    .unwrap_or_else(|| line.to_string()),
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the AI section (wrapped in the markers).
fn render_ai_section(detected: &[ModelUsage], cfg: &AiConfig) -> String {
    let mut s = String::new();
    s.push_str(AI_START);
    s.push_str("\n## TokenTrimmer (AI) recommendations\n\n");
    if detected.is_empty() {
        s.push_str("No model usage was detected in the codebase.\n\n");
    } else {
        s.push_str("Detected models:\n");
        for m in detected {
            s.push_str(&format!("- `{}` ({} file(s), e.g. {})\n", m.id, m.count, m.example_file));
        }
        s.push('\n');
    }
    if !cfg.routes.is_empty() {
        s.push_str("Recommended routes (apply in your TokenTrimmer dashboard):\n");
        for r in &cfg.routes {
            match &r.reason {
                Some(reason) => s.push_str(&format!("- `{}` → `{}` — {}\n", r.from, r.to, reason)),
                None => s.push_str(&format!("- `{}` → `{}`\n", r.from, r.to)),
            }
        }
        s.push('\n');
    }
    if let Some(notes) = &cfg.notes {
        s.push_str(notes);
        s.push('\n');
    }
    s.push_str(AI_END);
    s
}

/// Insert (append) the section, or replace the existing marked block. Idempotent.
fn upsert_marked_section(content: &str, section: &str) -> String {
    if let (Some(s), Some(e)) = (content.find(AI_START), content.find(AI_END)) {
        let e_end = e + AI_END.len();
        format!("{}{}{}", &content[..s], section, &content[e_end..])
    } else {
        let mut out = content.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(section);
        out.push('\n');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(daily: Option<f64>, weekly: Option<f64>) -> AiConfig {
        AiConfig {
            daily_cap_usd: daily,
            weekly_cap_usd: weekly,
            routes: vec![AiRoute {
                from: "gpt-4o".into(),
                to: "gpt-4o-mini".into(),
                reason: Some("cheaper".into()),
            }],
            notes: Some("Start with these caps.".into()),
        }
    }

    #[test]
    fn parse_ai_config_plain_and_fenced_and_bad() {
        let plain = r#"{"daily_cap_usd": 12, "weekly_cap_usd": 60, "routes": [], "notes": "n"}"#;
        let c = parse_ai_config(plain).expect("plain");
        assert_eq!(c.daily_cap_usd, Some(12.0));
        let fenced = "```json\n{\"daily_cap_usd\": 5}\n```";
        assert_eq!(parse_ai_config(fenced).unwrap().daily_cap_usd, Some(5.0));
        assert!(parse_ai_config("no json here").is_none());
        assert!(parse_ai_config("{not valid}").is_none());
    }

    #[test]
    fn apply_budget_caps_replaces_only_provided_and_preserves_comments() {
        let orig = "# comment\n\ndaily_cap_usd = 10\nweekly_cap_usd = 50\n";
        let out = apply_budget_caps(orig, &cfg(Some(24.7), None));
        assert!(out.contains("# comment"), "{out}");
        assert!(out.contains("daily_cap_usd = 25"), "rounds: {out}");
        assert!(out.contains("weekly_cap_usd = 50"), "untouched: {out}");
        let both = apply_budget_caps(orig, &cfg(Some(3.0), Some(20.0)));
        assert!(both.contains("daily_cap_usd = 3") && both.contains("weekly_cap_usd = 20"));
    }

    #[test]
    fn upsert_marked_section_appends_then_replaces() {
        let base = "# AGENTS\n\nbody\n";
        let sec1 = render_ai_section(&[], &cfg(Some(1.0), Some(2.0)));
        let once = upsert_marked_section(base, &sec1);
        assert_eq!(once.matches(AI_START).count(), 1);
        assert!(once.contains("# AGENTS"));
        // Second upsert with a different section replaces, not appends.
        let sec2 = render_ai_section(&[], &cfg(Some(9.0), Some(9.0)));
        let twice = upsert_marked_section(&once, &sec2);
        assert_eq!(twice.matches(AI_START).count(), 1, "idempotent: {twice}");
        assert_eq!(twice.matches(AI_END).count(), 1);
    }

    #[test]
    fn render_ai_section_has_models_routes_notes_and_markers() {
        let detected = vec![ModelUsage {
            id: "gpt-4o".into(),
            count: 2,
            example_file: "src/a.rs".into(),
        }];
        let s = render_ai_section(&detected, &cfg(Some(1.0), Some(2.0)));
        assert!(s.starts_with(AI_START) && s.trim_end().ends_with(AI_END));
        assert!(s.contains("gpt-4o"));
        assert!(s.contains("`gpt-4o` → `gpt-4o-mini`"));
        assert!(s.contains("Start with these caps."));
    }
}
```

(The helper bodies are written here directly — they're short; the test module is the failing-first surface.)

- [ ] **Step 2: Run the unit tests**

Run: `cargo test -p tt-cli init::ai 2>&1 | tail -20`
Expected: PASS (4 tests). If you scaffold the bodies as `unimplemented!()` first, run once to see them fail, then fill in the bodies shown above and re-run to green.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/init/ai.rs crates/cli/src/init/mod.rs
git commit -m "feat(cli): init/ai pure helpers (parse/caps/section)"
```

---

### Task 2: `ai_tailor` orchestrator

**Files:**
- Modify: `crates/cli/src/init/ai.rs` (add `build_init_context` + `pub async fn ai_tailor`)

- [ ] **Step 1: Add the context builder + `ai_tailor`**

Add to `crates/cli/src/init/ai.rs` (above the test module):

```rust
/// The user message: the detected models + the JSON-shape ask.
fn build_init_context(detected: &[ModelUsage]) -> String {
    let mut s = String::from(
        "Tailor a TokenTrimmer starter config for this repo. Return the JSON object only.\n\n",
    );
    if detected.is_empty() {
        s.push_str("No model usage was detected by scanning the code.\n");
    } else {
        s.push_str("Models referenced in the codebase:\n");
        for m in detected {
            s.push_str(&format!("- {} (in {} file(s), e.g. {})\n", m.id, m.count, m.example_file));
        }
    }
    s
}

/// AI pass for `tt init --ai`. Scans the repo, makes one grounded model call, and
/// tailors `.claude/budget.toml` caps + a marked `AGENTS.md` section. Deterministic
/// init must have already run (these files exist). Degrades gracefully: an
/// unparseable model reply or a missing artifact is a warning, not a failure.
///
/// # Errors
/// Missing API key, or a transport/gateway error from the SDK.
pub async fn ai_tailor(
    root: &Path,
    model: Option<String>,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base, key);

    let detected = detect_models(root);
    ui::note(&format!("AI pass: scanned {} model reference(s)", detected.len()));

    let out = client
        .chat()
        .model(model.unwrap_or_else(|| DEFAULT_MODEL.to_string()))
        .message(tt_client::system(INIT_AI_SYSTEM))
        .message(tt_client::user(build_init_context(&detected)))
        .send()
        .await?;

    let Some(cfg) = out.text().and_then(parse_ai_config) else {
        ui::warn("AI pass: could not parse the model's JSON — skipped (deterministic init is intact)");
        return Ok(());
    };

    // Tailor .claude/budget.toml caps.
    let budget_path = root.join(".claude/budget.toml");
    if let Ok(content) = std::fs::read_to_string(&budget_path) {
        let updated = apply_budget_caps(&content, &cfg);
        std::fs::write(&budget_path, updated).context("writing .claude/budget.toml")?;
        ui::ok(&format!(
            "tailored .claude/budget.toml (daily={:?}, weekly={:?})",
            cfg.daily_cap_usd, cfg.weekly_cap_usd
        ));
    } else {
        ui::warn("AI pass: .claude/budget.toml not found — skipped");
    }

    // Tailor AGENTS.md (marked section).
    let agents_path = root.join("AGENTS.md");
    if let Ok(content) = std::fs::read_to_string(&agents_path) {
        let updated = upsert_marked_section(&content, &render_ai_section(&detected, &cfg));
        std::fs::write(&agents_path, updated).context("writing AGENTS.md")?;
        ui::ok(&format!(
            "tailored AGENTS.md ({} recommended route(s))",
            cfg.routes.len()
        ));
    } else {
        ui::warn("AI pass: AGENTS.md not found — skipped");
    }

    Ok(())
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p tt-cli 2>&1 | tail -15`
Expected: clean build (`ai_tailor` is re-exported from Task 1; not yet wired into `main.rs` — that's Task 4).

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/init/ai.rs
git commit -m "feat(cli): init::ai_tailor (one-shot grounded call → tailored artifacts)"
```

---

### Task 3: Integration test (end-to-end via mock gateway)

**Files:**
- Create: `crates/cli/tests/init_ai_smoke.rs`
- Modify (if needed): `crates/cli/Cargo.toml` (`[dev-dependencies]`)

- [ ] **Step 0: Ensure dev-dependencies**

Check `crates/cli/Cargo.toml` `[dev-dependencies]` for `httpmock` and `serde_json` (the test needs both; `tempfile` + `tokio` are already there for `init_smoke`/async). If `httpmock` is missing, add it (match the version tt-client uses — `grep '^httpmock' crates/client/Cargo.toml`):

Run: `grep -E "httpmock|serde_json|tokio|tempfile" crates/cli/Cargo.toml`
Add any missing under `[dev-dependencies]` (e.g. `httpmock = "0.7"`, `serde_json = "1"`). The `#[tokio::test]` macro needs tokio with `macros` + `rt-multi-thread` — already enabled (the crate is async). Re-run after adding: `cargo build -p tt-cli --tests 2>&1 | tail -5`.

- [ ] **Step 1: Write the test**

Create `crates/cli/tests/init_ai_smoke.rs`:

```rust
//! `tt init --ai`: deterministic init + an AI pass (mock gateway) tailors
//! budget.toml caps + an AGENTS.md section; idempotent across re-runs.

use httpmock::prelude::*;
use serde_json::json;
use tt_cli::init::{ai_tailor, run, RunOptions};

fn git_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".git")).unwrap();
    std::fs::write(d.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(d.path().join("main.rs"), "// uses gpt-4o\n").unwrap();
    d
}

fn base_opts(root: std::path::PathBuf) -> RunOptions {
    RunOptions {
        root,
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
    }
}

fn mock_chat(server: &MockServer, content: &str) {
    server.mock(|when, then| {
        when.method(POST).path("/v1/chat/completions");
        then.status(200)
            .header("content-type", "application/json")
            .json_body(json!({
                "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
                "model": "gpt-4o-mini",
                "choices": [{ "index": 0, "finish_reason": "stop",
                    "message": { "role": "assistant", "content": content } }],
                "usage": { "prompt_tokens": 5, "completion_tokens": 5, "total_tokens": 10 }
            }));
    });
}

#[tokio::test]
async fn init_ai_tailors_budget_and_agents_idempotently() {
    let d = git_dir();
    run(base_opts(d.path().to_path_buf())).unwrap();

    let server = MockServer::start_async().await;
    mock_chat(
        &server,
        r#"{"daily_cap_usd": 7, "weekly_cap_usd": 35,
            "routes": [{"from": "gpt-4o", "to": "gpt-4o-mini", "reason": "10x cheaper"}],
            "notes": "Conservative starter caps."}"#,
    );

    ai_tailor(d.path(), None, Some("k".into()), Some(server.base_url()))
        .await
        .unwrap();

    let budget = std::fs::read_to_string(d.path().join(".claude/budget.toml")).unwrap();
    assert!(budget.contains("daily_cap_usd = 7"), "{budget}");
    assert!(budget.contains("weekly_cap_usd = 35"), "{budget}");

    let agents = std::fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
    assert!(agents.contains("<!-- tt:ai:start -->"));
    assert!(agents.contains("`gpt-4o` → `gpt-4o-mini`"), "{agents}");
    assert!(agents.contains("Conservative starter caps."));

    // Re-run → exactly one AI section (idempotent).
    ai_tailor(d.path(), None, Some("k".into()), Some(server.base_url()))
        .await
        .unwrap();
    let agents2 = std::fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
    assert_eq!(agents2.matches("<!-- tt:ai:start -->").count(), 1, "{agents2}");
}
```

- [ ] **Step 2: Run**

Run: `cargo test -p tt-cli --test init_ai_smoke 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/cli/tests/init_ai_smoke.rs
git commit -m "test(cli): init --ai tailors budget + AGENTS (mock gateway, idempotent)"
```

---

### Task 4: Wire `--ai` into the `Init` command

**Files:**
- Modify: `crates/cli/src/main.rs` (`Init` variant + dispatch arm)

- [ ] **Step 1: Add the flags**

In the `Init { … }` clap variant (`main.rs:192`), add after `dry_run`:

```rust
        /// Tailor the generated config with an AI pass over the repo (needs an API key).
        #[arg(long)]
        ai: bool,
        /// Model for the --ai pass (default: gpt-4o-mini).
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
```

- [ ] **Step 2: Wire the dispatch arm**

In the `Command::Init { … }` arm (`main.rs:570`), add the new fields to the destructure and, after `run(opts).context("tt init failed")?;`, the AI step:

```rust
        Command::Init {
            path,
            language,
            framework,
            interactive,
            upgrade,
            force,
            diff,
            skip_baseline,
            skip_hooks,
            skip_workflows,
            dry_run,
            ai,
            model,
            tt_api_key,
            tt_api_base,
        } => {
            use tt_cli::init::{run, RunOptions};
            let root = path
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap());
            let opts = RunOptions {
                root: root.clone(),
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
            run(opts).context("tt init failed")?;
            if ai && !dry_run {
                tt_cli::init::ai_tailor(&root, model, tt_api_key, tt_api_base)
                    .await
                    .context("tt init --ai pass failed")?;
            }
        }
```

(`root.clone()` because `root` is reused by `ai_tailor`. `&& !dry_run` skips the AI writes on a dry run.)

- [ ] **Step 3: Verify build + help**

Run: `cargo build -p tt-cli 2>&1 | tail -5`
Then: `cargo run -q -p tt-cli --bin tt -- init --help 2>&1 | grep -E "\-\-ai|\-\-model"`
Expected: clean build; help shows `--ai` and `--model`.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/main.rs
git commit -m "feat(cli): wire tt init --ai"
```

---

### Task 5: Docs + gates

**Files:**
- Modify: `docs/` if a CLI reference lists `tt init` flags (optional — check); otherwise gates only.

- [ ] **Step 1: Format**

Run: `cargo fmt`
Then: `git diff --quiet || (git add -A && git commit -m "style: cargo fmt")`

- [ ] **Step 2: Clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: no warnings. Fix any, re-run.

- [ ] **Step 3: Test the crate**

Run: `cargo test -p tt-cli 2>&1 | grep -E "test result:" | tail`
Expected: all pass (incl. `init::ai` units + `init_ai_smoke`).

- [ ] **Step 4: Advisories**

Run: `cargo deny check advisories 2>&1 | tail -5`
Expected: ok.

- [ ] **Step 5: Commit any residual fixes**

```bash
git status --porcelain
```
```
