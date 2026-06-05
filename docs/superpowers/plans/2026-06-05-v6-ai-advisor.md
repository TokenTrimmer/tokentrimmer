# V6 — AI Cost/Routing Advisor (`tt advise`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `tt advise [path]` scans a repo for model usage and runs the V5b-2 tool-grounded loop once to print a read-only cost/routing recommendation.

**Architecture:** New `crates/cli/src/advise.rs`: pure `scan_text_for_models`/`build_context_message`, a `detect_models` walk, and a thin `run` that seeds a `Conversation` and calls `chat::tools::run_tool_turn`. No new model-call code.

**Tech Stack:** Rust, `regex` + `walkdir` (existing deps), the V5b-2 tool loop.

---

### Task 1: `advise.rs` — scan + context (test-first)

**Files:**
- Create: `crates/cli/src/advise.rs`
- Modify: `crates/cli/src/lib.rs` (`pub mod advise;`)

- [ ] **Step 1: Create the module with the pure pieces + tests**

Create `crates/cli/src/advise.rs`:

```rust
//! `tt advise` — scan a repo for model usage and ask a tool-grounded model for
//! cost/routing recommendations. Read-only. Reuses the V5b-2 tool-calling loop.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Context as _;
use regex::Regex;

use crate::chat::{tools, Conversation, Ledger};
use crate::context::ResolvedContext;
use crate::ui;

const ADVISOR_MODEL: &str = "gpt-4o-mini";

const ADVISOR_SYSTEM: &str = "You are a TokenTrimmer cost-optimization advisor. \
Use the provided tools (preview_cost, find_route_for, inspect_diff) to ground EVERY \
recommendation in real numbers — never invent prices, call the tools. Be concrete and \
brief: list specific routing/model changes with their dollar impact, name cheaper \
equivalents, and flag risky or wasteful prompt patterns. End with the single \
highest-impact change.";

/// File extensions worth scanning for model usage.
const SCAN_EXTS: &[&str] = &[
    "py", "js", "ts", "tsx", "jsx", "rs", "go", "rb", "java", "kt", "php", "cs",
];
/// Directories never scanned.
const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", ".venv", "vendor", ".next",
];
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_FILES: usize = 5000;

/// One model id found in the codebase.
pub struct ModelUsage {
    pub id: String,
    pub count: usize,
    pub example_file: String,
}

fn model_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(gpt-[\w.\-]+|claude-[\w.\-]+|gemini-[\w.\-]+|o[1-9]-[\w.\-]+|mistral-[\w.\-]+|llama-?[0-9][\w.\-]*|text-embedding-[\w.\-]+)\b",
        )
        .expect("valid model regex")
    })
}

/// Extract known model-id mentions from `text` (de-duped, first-seen order).
#[must_use]
pub fn scan_text_for_models(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in model_re().find_iter(text) {
        let id = m.as_str().to_string();
        if seen.insert(id.to_ascii_lowercase()) {
            out.push(id);
        }
    }
    out
}

/// A compact brief: detected models + the optional `--describe`, asking for
/// tool-grounded recommendations.
#[must_use]
pub fn build_context_message(detected: &[ModelUsage], describe: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("Review this project's LLM usage and recommend cost/routing optimizations.\n\n");
    if detected.is_empty() {
        s.push_str("No model usage was detected by scanning the code.\n");
    } else {
        s.push_str("Models referenced in the codebase:\n");
        for m in detected {
            s.push_str(&format!(
                "- {} (in {} file(s), e.g. {})\n",
                m.id, m.count, m.example_file
            ));
        }
    }
    if let Some(d) = describe {
        s.push_str(&format!("\nWhat the app does: {d}\n"));
    }
    s.push_str(
        "\nFor each suggestion, call preview_cost / find_route_for to ground the numbers, \
         then give the single highest-impact change.",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_extracts_model_ids() {
        let t = r#"client.chat("gpt-4o-mini"); model = "claude-3-5-sonnet"; use o3-mini;
                   var llamaindex = 1; pick "mistral-large-latest"; llama-3.3-70b"#;
        let ids = scan_text_for_models(t);
        assert!(ids.contains(&"gpt-4o-mini".to_string()), "{ids:?}");
        assert!(ids.iter().any(|s| s.eq_ignore_ascii_case("claude-3-5-sonnet")));
        assert!(ids.iter().any(|s| s.eq_ignore_ascii_case("o3-mini")));
        assert!(ids.iter().any(|s| s.to_ascii_lowercase().starts_with("mistral-large")));
        assert!(ids.iter().any(|s| s.to_ascii_lowercase().starts_with("llama-3.3")));
        assert!(!ids.iter().any(|s| s.to_ascii_lowercase().contains("llamaindex")));
        assert!(scan_text_for_models("no models here").is_empty());
        assert_eq!(scan_text_for_models("gpt-4o gpt-4o").len(), 1); // de-duped
    }

    #[test]
    fn context_message_lists_detected_and_describe() {
        let det = vec![ModelUsage {
            id: "gpt-4o".into(),
            count: 3,
            example_file: "src/llm.py".into(),
        }];
        let msg = build_context_message(&det, Some("a support chatbot"));
        assert!(msg.contains("gpt-4o"));
        assert!(msg.contains("3 file(s)"));
        assert!(msg.contains("src/llm.py"));
        assert!(msg.contains("a support chatbot"));
        let empty = build_context_message(&[], None);
        assert!(empty.contains("No model usage was detected"));
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/cli/src/lib.rs`, add `pub mod advise;` (after `pub mod account;`).

- [ ] **Step 3: Run the tests**

Run: `cargo test -p tt-cli --lib advise 2>&1 | tail -12`
Expected: PASS (`scan_extracts_model_ids`, `context_message_lists_detected_and_describe`). (`detect_models`/`run` aren't defined yet → a `dead_code` warning on the consts is fine until Task 2/3.)

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/advise.rs crates/cli/src/lib.rs
git commit -m "feat(advise): model-id scanner + advisor context builder"
```

---

### Task 2: `detect_models` (walkdir, test over a tempdir)

**Files:**
- Modify: `crates/cli/src/advise.rs`

- [ ] **Step 1: Write the failing test**

Add to `advise.rs` `mod tests`:

```rust
    #[test]
    fn detect_models_walks_and_skips_vendor_dirs() {
        let dir = std::env::temp_dir().join(format!("tt-advise-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/foo")).unwrap();
        std::fs::write(dir.join("src/a.py"), "m = \"gpt-4o\"\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "let m = \"gpt-4o\"; // and claude-3-5-sonnet\n").unwrap();
        std::fs::write(dir.join("node_modules/foo/x.js"), "\"gpt-4o\"\n").unwrap(); // must be skipped
        std::fs::write(dir.join("README.md"), "uses gpt-4o\n").unwrap(); // wrong ext, skipped

        let found = detect_models(&dir);
        let ids: Vec<&str> = found.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"gpt-4o"), "{ids:?}");
        assert!(ids.iter().any(|s| s.eq_ignore_ascii_case("claude-3-5-sonnet")));
        // gpt-4o is in src/a.py + src/b.rs only (node_modules skipped, README wrong ext) → 2 files
        let gpt = found.iter().find(|m| m.id == "gpt-4o").unwrap();
        assert_eq!(gpt.count, 2, "node_modules + README must be skipped");
        std::fs::remove_dir_all(&dir).ok();
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-cli --lib advise::tests::detect_models 2>&1 | tail -8`
Expected: FAIL to compile — `cannot find function detect_models`.

- [ ] **Step 3: Add `detect_models`**

In `advise.rs`, add after `build_context_message`:

```rust
/// Scan `root` for model-id usage across source files, skipping vendor dirs and
/// over-large files. Returns ids aggregated by file count, most-used first.
#[must_use]
pub fn detect_models(root: &Path) -> Vec<ModelUsage> {
    let mut agg: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut files = 0usize;
    let walk = walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        !e.file_name()
            .to_str()
            .is_some_and(|n| SKIP_DIRS.contains(&n))
    });
    for entry in walk.flatten() {
        if files >= MAX_FILES {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !SCAN_EXTS.contains(&ext) {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        files += 1;
        let rel = path.strip_prefix(root).unwrap_or(path).display().to_string();
        for id in scan_text_for_models(&text) {
            let e = agg.entry(id).or_insert((0, rel.clone()));
            e.0 += 1;
        }
    }
    let mut out: Vec<ModelUsage> = agg
        .into_iter()
        .map(|(id, (count, example_file))| ModelUsage {
            id,
            count,
            example_file,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.id.cmp(&b.id)));
    out
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-cli --lib advise 2>&1 | tail -8`
Expected: PASS (all `advise` tests).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/advise.rs
git commit -m "feat(advise): detect_models repo scan (walkdir, skips vendor dirs)"
```

---

### Task 3: `run` + the `tt advise` command

**Files:**
- Modify: `crates/cli/src/advise.rs` (`run`)
- Modify: `crates/cli/src/main.rs` (the `Advise` command + dispatch)

- [ ] **Step 1: Add `run` to `advise.rs`**

After `detect_models`:

```rust
/// `tt advise` entry point: scan the repo, then run one tool-grounded turn.
pub async fn run(
    path: Option<String>,
    describe: Option<String>,
    model: Option<String>,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();

    let root = path.unwrap_or_else(|| ".".to_string());
    let detected = detect_models(Path::new(&root));
    if detected.is_empty() {
        ui::note("no model usage detected in the code — advising from --describe / general guidance");
    } else {
        ui::note(&format!("scanned: {} model(s) referenced", detected.len()));
    }

    let mut conv = Conversation::new(
        model.unwrap_or_else(|| ADVISOR_MODEL.to_string()),
        Some(ADVISOR_SYSTEM.to_string()),
    );
    conv.push_user(build_context_message(&detected, describe.as_deref()));

    let reg = tools::build_registry();
    let mut ledger = Ledger::default();
    ui::heading("TokenTrimmer advisor");
    tools::run_tool_turn(&http, &base, &key, &mut conv, &reg, &mut ledger).await;
    Ok(())
}
```

- [ ] **Step 2: Add the `Advise` command to `main.rs`**

In the `Command` enum (after `Models { … },`):

```rust
    /// AI cost/routing advisor: scan a repo + recommend optimizations (read-only).
    Advise {
        /// Repo path to scan (default: current directory).
        path: Option<String>,
        /// Describe what the app does (adds context for the advisor).
        #[arg(long)]
        describe: Option<String>,
        /// Advisor model (default: gpt-4o-mini).
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
    },
```

- [ ] **Step 3: Add the dispatch arm**

After the `Command::Models { … } => { … }` arm:

```rust
        Command::Advise {
            path,
            describe,
            model,
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::advise::run(path, describe, model, tt_api_key, tt_api_base).await?;
        }
```

- [ ] **Step 4: Build + tests**

Run: `cargo build -p tt-cli 2>&1 | grep -E "^error|never used" | head` then `cargo test -p tt-cli --lib advise 2>&1 | tail -6`
Expected: no errors / no dead-code; `advise` tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/advise.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt advise — AI cost/routing advisor (tool-grounded)"
```

---

### Task 4: Gates + smoke + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt -p tt-cli && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings.

- [ ] **Step 2: Full tt-cli tests**

Run: `cargo test -p tt-cli 2>&1 | grep -E "test result|error\[" | tail -8`
Expected: all pass.

- [ ] **Step 3: cargo-deny**

Run: `cargo deny check advisories 2>&1 | tail -3`
Expected: `advisories ok` (no new deps).

- [ ] **Step 4: Smoke (no network needed)**

Run:
```bash
cargo build -q -p tt-cli --bin tt
echo "--- --help wired ---"
target/debug/tt advise --help 2>&1 | head -3
echo "--- no key → same bail as chat (no model call) ---"
TT_API_KEY= target/debug/tt advise --tt-api-base http://127.0.0.1:1 2>&1 | tail -2 || true
```
Expected: `--help` shows `path`/`--describe`/`--model`; with no key resolvable, the "no API key" bail (the scan runs first, then it needs the key). (The full advisor run is a manual check with a real key.)

- [ ] **Step 5: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** `scan_text_for_models` + `build_context_message` + `ADVISOR_SYSTEM` (T1), `detect_models` (T2), `run` + the `Advise` command (T3), gates/smoke (T4). All spec items covered.
- **Placeholders:** none — full code in every step.
- **Type consistency:** `scan_text_for_models(&str)->Vec<String>`, `detect_models(&Path)->Vec<ModelUsage>`, `build_context_message(&[ModelUsage], Option<&str>)->String`, `run(Option<String>×3, Option<String>×2)`; reuses `chat::tools::{build_registry()->Registry, run_tool_turn(&Client,&str,&str,&mut Conversation,&Registry,&mut Ledger)->bool}` and `Conversation::new(String, Option<String>)` / `Ledger::default()`. `ADVISOR_MODEL` is a local const (chat's `DEFAULT_CHAT_MODEL` is private). `Advise` is a leaf command (plain `#[arg(long)]`, positional `path`).
- **Read-only:** no file writes; the model call goes through the existing tool loop; the scan caps files/bytes so a huge repo can't hang.
