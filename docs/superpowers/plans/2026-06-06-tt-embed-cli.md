# `tt embed` CLI command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `tt embed` CLI command that embeds text via the gateway and prints a cost-aware summary (or `--json` vectors).

**Architecture:** A new `crates/cli/src/embed.rs` module with a thin async `run()` over `tt_client::Client::embeddings()`, plus two pure helpers (`assemble_input`, `format_embed_summary`) that carry all the unit-tested logic. A new `Command::Embed` clap variant + dispatch arm in `main.rs` wires it up. Network behavior is already covered by tt-client's httpmock suite, so this slice's tests are pure-unit.

**Tech Stack:** Rust, clap (derive), anyhow, tt-client SDK, console (via `crate::ui`).

---

### Task 1: Create the `embed` module with pure helpers + tests

**Files:**
- Create: `crates/cli/src/embed.rs`
- Modify: `crates/cli/src/lib.rs:10` (add `pub mod embed;` — alphabetically after `pub mod cost_diff;` / before `pub mod init;`)

- [ ] **Step 1: Register the module**

In `crates/cli/src/lib.rs`, add the module declaration in alphabetical order (after the `pub mod cost_diff;` line, before `pub mod init;`):

```rust
pub mod embed;
```

- [ ] **Step 2: Write the failing tests**

Create `crates/cli/src/embed.rs` with ONLY the helper signatures (compile-but-fail bodies) and the test module:

```rust
//! `tt embed` — embed text via the gateway and print a cost summary (or --json
//! vectors). Thin glue over `tt_client::Client::embeddings()`; the network path
//! is covered by tt-client's own tests, so the tested surface here is the two
//! pure helpers below.

use tt_client::{CostInfo, EmbedOutcome, EmbeddingInput};

use crate::ui;

const DEFAULT_MODEL: &str = "text-embedding-3-small";

/// Build the embeddings input: 1 arg → Single, >1 → Batch; no args → the trimmed
/// stdin text as Single. Returns None when there is nothing to embed.
fn assemble_input(args: &[String], stdin_text: Option<&str>) -> Option<EmbeddingInput> {
    unimplemented!()
}

/// One-line styled summary, e.g.
/// "text-embedding-3-small · 2 embeddings × 3 dims · $0.0002 · saved 75%".
fn format_embed_summary(out: &EmbedOutcome, requested_model: &str) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_client::{EmbeddingData, EmbeddingsResponse, Usage};

    fn outcome(rows: usize, dims: usize, cost: CostInfo) -> EmbedOutcome {
        let data = (0..rows)
            .map(|i| EmbeddingData {
                object: "embedding".to_string(),
                index: i as u32,
                embedding: vec![0.0_f32; dims],
            })
            .collect();
        EmbedOutcome {
            response: EmbeddingsResponse {
                object: "list".to_string(),
                data,
                model: "srv-model".to_string(),
                usage: Usage::default(),
            },
            cost,
        }
    }

    #[test]
    fn assemble_input_single_arg() {
        let got = assemble_input(&["hi".to_string()], None);
        assert!(matches!(got, Some(EmbeddingInput::Single(s)) if s == "hi"));
    }

    #[test]
    fn assemble_input_multi_arg_batch() {
        let got = assemble_input(&["a".to_string(), "b".to_string()], None);
        assert!(matches!(got, Some(EmbeddingInput::Batch(v)) if v == vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn assemble_input_stdin_fallback_trims() {
        let got = assemble_input(&[], Some(" hi \n"));
        assert!(matches!(got, Some(EmbeddingInput::Single(s)) if s == "hi"));
    }

    #[test]
    fn assemble_input_empty_is_none() {
        assert!(assemble_input(&[], None).is_none());
        assert!(assemble_input(&[], Some("   ")).is_none());
    }

    #[test]
    fn format_embed_summary_full() {
        let cost = CostInfo {
            cost_usd: Some(0.0002),
            saved_usd: Some(0.0003),
            baseline_cost_usd: Some(0.0004),
            model_used: Some("text-embedding-3-small".to_string()),
            ..CostInfo::default()
        };
        let s = format_embed_summary(&outcome(2, 3, cost), "ignored");
        let plain = console::strip_ansi_codes(&s);
        assert!(plain.contains("text-embedding-3-small"), "{plain}");
        assert!(plain.contains("2 embeddings"), "{plain}");
        assert!(plain.contains("× 3 dims"), "{plain}");
        assert!(plain.contains("$0.0002"), "{plain}");
        assert!(plain.contains("saved 75%"), "{plain}");
    }

    #[test]
    fn format_embed_summary_minimal_falls_back_to_requested_model() {
        let s = format_embed_summary(&outcome(1, 4, CostInfo::default()), "my-model");
        let plain = console::strip_ansi_codes(&s);
        assert!(plain.contains("my-model"), "{plain}");
        assert!(plain.contains("1 embedding"), "{plain}");
        assert!(!plain.contains("embeddings"), "singular expected: {plain}");
        assert!(!plain.contains('$'), "no cost expected: {plain}");
        assert!(!plain.contains("saved"), "no savings expected: {plain}");
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p tt-cli embed:: 2>&1 | tail -20`
Expected: tests compile and FAIL at runtime via `unimplemented!()` (panic "not implemented").

- [ ] **Step 4: Implement the two helpers**

Replace the two `unimplemented!()` bodies in `crates/cli/src/embed.rs`:

```rust
fn assemble_input(args: &[String], stdin_text: Option<&str>) -> Option<EmbeddingInput> {
    match args {
        [] => {
            let text = stdin_text?.trim();
            if text.is_empty() {
                None
            } else {
                Some(EmbeddingInput::Single(text.to_string()))
            }
        }
        [one] => Some(EmbeddingInput::Single(one.clone())),
        many => Some(EmbeddingInput::Batch(many.to_vec())),
    }
}

fn format_embed_summary(out: &EmbedOutcome, requested_model: &str) -> String {
    let model = out.cost.model_used.as_deref().unwrap_or(requested_model);
    let count = out.response.data.len();
    let noun = if count == 1 { "embedding" } else { "embeddings" };

    let mut parts = vec![model.to_string(), format!("{count} {noun}")];
    if let Some(dims) = out.response.data.first().map(|d| d.embedding.len()) {
        parts.push(format!("× {dims} dims"));
    }
    if let Some(cost) = out.cost.cost_usd {
        parts.push(format!("${cost:.4}"));
    }
    if let (Some(saved), Some(baseline)) = (out.cost.saved_usd, out.cost.baseline_cost_usd) {
        if baseline > 0.0 {
            parts.push(format!("saved {:.0}%", saved / baseline * 100.0));
        }
    }
    ui::muted()
        .apply_to(parts.join(&format!(" {} ", ui::BULLET)))
        .to_string()
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p tt-cli embed:: 2>&1 | tail -20`
Expected: all 6 `embed::tests::*` PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/cli/src/embed.rs crates/cli/src/lib.rs
git commit -m "feat(cli): add embed module helpers (input assembly + summary)"
```

---

### Task 2: Implement `embed::run` (the async glue)

**Files:**
- Modify: `crates/cli/src/embed.rs` (add `run` above the test module)

- [ ] **Step 1: Add the `run` function**

Add to `crates/cli/src/embed.rs` (after the helpers, before `#[cfg(test)]`). Also add the needed imports at the top: extend the existing `use tt_client::{...}` is not needed (the SDK is reached via `tt_client::Client`), but add `use anyhow::Context as _;`:

```rust
use anyhow::Context as _;

use crate::context::ResolvedContext;
```

```rust
/// Embed `input` (or stdin) and print a cost summary — or, with `--json`, the
/// full `EmbeddingsResponse` to stdout and the summary to stderr.
///
/// # Errors
/// Surfaces a missing API key, a `402` cost-limit rejection, or any transport /
/// gateway error from the SDK.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    input: Vec<String>,
    model: Option<String>,
    dimensions: Option<u32>,
    encoding_format: Option<String>,
    cost_limit: Option<f64>,
    json: bool,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base, key);

    let stdin_text = if input.is_empty() {
        Some(std::io::read_to_string(std::io::stdin()).context("failed to read stdin")?)
    } else {
        None
    };
    let assembled = assemble_input(&input, stdin_text.as_deref())
        .context("no input — pass text as an argument or pipe it on stdin")?;

    let requested_model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let mut builder = client
        .embeddings()
        .model(requested_model.clone())
        .input(assembled);
    if let Some(n) = dimensions {
        builder = builder.dimensions(n);
    }
    if let Some(f) = encoding_format {
        builder = builder.encoding_format(f);
    }
    if let Some(c) = cost_limit {
        builder = builder.cost_limit(c);
    }
    let out = builder.send().await?;

    let summary = format_embed_summary(&out, &requested_model);
    if json {
        println!("{}", serde_json::to_string_pretty(&out.response)?);
        ui::note(&summary);
    } else {
        println!("{summary}");
    }
    Ok(())
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p tt-cli 2>&1 | tail -20`
Expected: clean build (the function is not yet wired into `main.rs`, so `dead_code` may warn — that is resolved in Task 3; if `-D warnings` is on for build it will not be, only clippy gate uses it, so a plain build passes).

- [ ] **Step 3: Commit**

```bash
git add crates/cli/src/embed.rs
git commit -m "feat(cli): add embed::run gateway glue"
```

---

### Task 3: Wire `Command::Embed` into the CLI

**Files:**
- Modify: `crates/cli/src/main.rs` (add the `Embed` variant after `Models`/`Advise`; add the dispatch arm after `Command::Advise`)

- [ ] **Step 1: Add the clap variant**

In `crates/cli/src/main.rs`, immediately after the `Models { … }` variant (ends at line ~151) and before the `Advise` doc comment, add:

```rust
    /// Embed text via the gateway and print a cost summary (or --json vectors).
    Embed {
        /// Text to embed. One arg → single; many → a batch. Omit to read stdin.
        input: Vec<String>,
        /// Embedding model (default: text-embedding-3-small).
        #[arg(long)]
        model: Option<String>,
        /// Reduce output dimensions (Matryoshka models).
        #[arg(long)]
        dimensions: Option<u32>,
        /// Wire encoding format (e.g. "float" or "base64").
        #[arg(long)]
        encoding_format: Option<String>,
        /// Reject (402) if the estimated cost exceeds this many USD.
        #[arg(long)]
        cost_limit: Option<f64>,
        /// Print the full EmbeddingsResponse JSON to stdout (summary → stderr).
        #[arg(long)]
        json: bool,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
    },
```

- [ ] **Step 2: Add the dispatch arm**

In the `match` in `main()`, immediately after the `Command::Advise { … } => { … }` arm (ends ~line 523), add:

```rust
        Command::Embed {
            input,
            model,
            dimensions,
            encoding_format,
            cost_limit,
            json,
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::embed::run(
                input,
                model,
                dimensions,
                encoding_format,
                cost_limit,
                json,
                tt_api_key,
                tt_api_base,
            )
            .await?;
        }
```

- [ ] **Step 3: Verify it compiles and the command is registered**

Run: `cargo build -p tt-cli 2>&1 | tail -20`
Expected: clean build, no `dead_code` warning for `embed::run`.

Run: `cargo run -p tt-cli --bin tt -- embed --help 2>&1 | tail -25`
Expected: help text showing `[INPUT]...` and the `--model/--dimensions/--encoding-format/--cost-limit/--json/--tt-api-key/--tt-api-base` flags.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/main.rs
git commit -m "feat(cli): wire tt embed command"
```

---

### Task 4: Gates + finish

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt`
Then: `git diff --quiet || (git add -A && git commit -m "style: cargo fmt")`

- [ ] **Step 2: Clippy (workspace, all targets, deny warnings)**

Run: `cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -30`
Expected: no warnings. Fix any that appear, then re-run.

- [ ] **Step 3: Test the CLI crate**

Run: `cargo test -p tt-cli 2>&1 | tail -20`
Expected: all tests pass (incl. the 6 `embed::tests::*`).

- [ ] **Step 4: Doc gate**

Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-cli --no-deps 2>&1 | tail -20`
Expected: builds clean.

- [ ] **Step 5: Advisories**

Run: `cargo deny check advisories 2>&1 | tail -20`
Expected: ok (no new advisories — no deps added).

- [ ] **Step 6: Final commit (if any gate produced changes)**

```bash
git status --porcelain
# commit any residual fixes from gates with a descriptive message
```
```
