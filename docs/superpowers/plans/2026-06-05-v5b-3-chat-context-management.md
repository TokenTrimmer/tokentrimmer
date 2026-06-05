# V5b-3 — `tt chat` Context Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Track estimated context usage in `tt chat`, warn at 75% of a per-model budget, auto-trim oldest turns at 95% (coherently, never orphaning a tool exchange), with `/context`, `/trim`, and `--max-context`.

**Architecture:** New submodule `crates/cli/src/chat/budget.rs` (window table, token estimate via `tt_tokenize`, coherent trim, `ContextState`). `mod.rs` calls `ctx.manage(&mut conv)` right after the user turn is pushed (Chat/Editor), adds `/context` + `/trim`, and threads a `--max-context` budget. `dispatch_turn` is unchanged.

**Tech Stack:** Rust, `tt-tokenize` (cl100k estimate, new dep), `tt-shared` `Message`, existing `ui`.

---

### Task 1: `budget.rs` — window table + token estimate

**Files:**
- Modify: `crates/cli/Cargo.toml` (add `tt-tokenize`)
- Create: `crates/cli/src/chat/budget.rs`
- Modify: `crates/cli/src/chat/mod.rs` (`pub mod budget;`)

- [ ] **Step 1: Add the dependency**

In `crates/cli/Cargo.toml` `[dependencies]`, after `tt-mcp.workspace = true`:

```toml
tt-tokenize.workspace = true
```

- [ ] **Step 2: Create `budget.rs` with the window table + estimate + tests**

Create `crates/cli/src/chat/budget.rs`:

```rust
//! Context-window management for `tt chat`: estimate token usage, warn as the
//! conversation fills a per-model budget, and trim the oldest turns before the
//! limit. The gateway remains the authoritative gate; this is advisory.

use tt_shared::messages::{Message, MessageContent};

use crate::chat::Conversation;
use crate::ui;

/// Fallback budget when the model is unknown.
pub const DEFAULT_CONTEXT_BUDGET: u32 = 128_000;
const WARN_FRAC: f64 = 0.75;
const TRIM_FRAC: f64 = 0.95;
const TRIM_TARGET_FRAC: f64 = 0.70;
/// cl100k is a high-quality general estimator; the chat doesn't reliably know
/// the routed provider, so we estimate with the OpenAI tokenizer for all models.
const ESTIMATE_PROVIDER: &str = "openai";

/// Best-effort context window (input tokens) for a model id, by prefix. These
/// are approximate defaults — overridable via `--max-context` / `/context <n>`;
/// exact live windows are the live-catalog's (V4) job.
#[must_use]
pub fn model_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    let s = |p: &str| m.starts_with(p);
    if s("gpt-5") {
        256_000
    } else if s("gpt-4o") || s("gpt-4.1") || s("gpt-4-turbo") {
        128_000
    } else if s("o1") || s("o3") || s("o4") {
        200_000
    } else if s("claude") {
        200_000
    } else if s("gemini") {
        1_000_000
    } else {
        DEFAULT_CONTEXT_BUDGET
    }
}

/// All message text (system prompt + each `Text` content), for estimation.
fn conversation_text(conv: &Conversation) -> String {
    let mut out = String::new();
    if let Some(sys) = &conv.system {
        out.push_str(sys);
        out.push('\n');
    }
    for m in &conv.messages {
        let text = match m {
            Message::System {
                content: MessageContent::Text(t),
            } => Some(t),
            Message::User {
                content: MessageContent::Text(t),
                ..
            } => Some(t),
            Message::Assistant {
                content: Some(MessageContent::Text(t)),
                ..
            } => Some(t),
            Message::Tool {
                content: MessageContent::Text(t),
                ..
            } => Some(t),
            _ => None,
        };
        if let Some(t) = text {
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

/// Estimated tokens for the whole conversation.
#[must_use]
pub fn estimate_conversation_tokens(conv: &Conversation, provider: &str) -> u32 {
    tt_tokenize::estimate_tokens(provider, &conversation_text(conv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_table_by_prefix() {
        assert_eq!(model_window("gpt-4o-mini"), 128_000);
        assert_eq!(model_window("GPT-4o"), 128_000); // case-insensitive
        assert_eq!(model_window("claude-3-5-sonnet"), 200_000);
        assert_eq!(model_window("o1-preview"), 200_000);
        assert_eq!(model_window("gemini-2.0-pro"), 1_000_000);
        assert_eq!(model_window("some-unknown-model"), DEFAULT_CONTEXT_BUDGET);
    }

    #[test]
    fn estimate_grows_with_messages() {
        let mut c = Conversation::new("gpt-4o-mini".into(), None);
        let empty = estimate_conversation_tokens(&c, ESTIMATE_PROVIDER);
        c.push_user("the quick brown fox jumps over the lazy dog".into());
        let one = estimate_conversation_tokens(&c, ESTIMATE_PROVIDER);
        c.push_assistant("a reply with several more words in it".into());
        let two = estimate_conversation_tokens(&c, ESTIMATE_PROVIDER);
        assert!(one > empty, "{one} > {empty}");
        assert!(two > one, "{two} > {one}");
    }
}
```

- [ ] **Step 3: Register the submodule**

In `crates/cli/src/chat/mod.rs`, after `pub mod tools;`:

```rust
pub mod budget;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p tt-cli --lib chat::budget 2>&1 | tail -12`
Expected: PASS (`window_table_by_prefix`, `estimate_grows_with_messages`).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/Cargo.toml crates/cli/src/chat/budget.rs crates/cli/src/chat/mod.rs Cargo.lock
git commit -m "feat(chat): context window table + token estimate"
```

---

### Task 2: coherent trim (`trim_to_budget`, `manual_trim`)

**Files:**
- Modify: `crates/cli/src/chat/budget.rs`

> `manual_trim` needs `ContextState` (Task 3). To keep tasks independent, this task adds `trim_to_budget` (which is self-contained) and its tests; `manual_trim` lands in Task 3 with `ContextState`.

- [ ] **Step 1: Write the failing tests**

Add to `budget.rs` `mod tests`:

```rust
    #[test]
    fn trim_reduces_and_preserves_system_and_last() {
        let mut c = Conversation::new("gpt-4o-mini".into(), Some("be terse".into()));
        for i in 0..8 {
            c.push_user(format!("user message number {i} with some words"));
            c.push_assistant(format!("assistant reply number {i} with some words"));
        }
        let before = c.messages.len();
        let dropped = trim_to_budget(&mut c, 20, ESTIMATE_PROVIDER); // tiny target
        assert!(dropped > 0 && c.messages.len() < before);
        assert_eq!(c.system.as_deref(), Some("be terse")); // system kept
        // a clean boundary: first kept message is a User
        assert!(matches!(c.messages.first(), Some(Message::User { .. })));
        // the most recent message (assistant reply #7) is preserved
        assert!(matches!(
            c.messages.last(),
            Some(Message::Assistant { content: Some(MessageContent::Text(t)), .. }) if t.contains("number 7")
        ));
    }

    #[test]
    fn trim_does_not_orphan_a_tool_exchange() {
        use tt_shared::messages::{ToolCall, ToolCallFunction};
        let mut c = Conversation::new("gpt-4o-mini".into(), None);
        c.push_user("old question".into());
        c.messages.push(Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "find_route_for".into(),
                    arguments: "{}".into(),
                },
            }],
            name: None,
        });
        c.messages.push(Message::Tool {
            content: MessageContent::Text("{\"model\":\"x\"}".into()),
            tool_call_id: "c1".into(),
        });
        c.push_assistant("old answer".into());
        c.push_user("new question".into());
        c.push_assistant("new answer".into());
        trim_to_budget(&mut c, 5, ESTIMATE_PROVIDER); // force aggressive trim
        // never start the window on a Tool or tool-call Assistant
        assert!(!matches!(
            c.messages.first(),
            Some(Message::Tool { .. })
        ));
        assert!(!matches!(
            c.messages.first(),
            Some(Message::Assistant { tool_calls, .. }) if !tool_calls.is_empty()
        ));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tt-cli --lib chat::budget::tests::trim 2>&1 | tail -12`
Expected: FAIL to compile — `cannot find function trim_to_budget`.

- [ ] **Step 3: Add `trim_to_budget`**

In `budget.rs`, add after `estimate_conversation_tokens`:

```rust
/// Drop oldest messages until the estimate is `<= target` (or only the most
/// recent message remains), keeping the system prompt (stored separately) and
/// never leaving the window starting on a `Tool` or tool-call `Assistant`
/// (which would orphan a tool exchange). Returns the number of messages removed.
#[must_use]
pub fn trim_to_budget(conv: &mut Conversation, target: u32, provider: &str) -> usize {
    let original = conv.messages.len();
    while conv.messages.len() > 1 && estimate_conversation_tokens(conv, provider) > target {
        conv.messages.remove(0);
        // re-establish a clean turn boundary at the front
        while conv.messages.len() > 1
            && matches!(
                conv.messages.first(),
                Some(Message::Assistant { .. } | Message::Tool { .. })
            )
        {
            conv.messages.remove(0);
        }
    }
    original - conv.messages.len()
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p tt-cli --lib chat::budget 2>&1 | tail -12`
Expected: PASS (all `budget` tests).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/chat/budget.rs
git commit -m "feat(chat): coherent context trim (never orphans a tool exchange)"
```

---

### Task 3: `ContextState` (budget resolution, warn/trim, manual_trim)

**Files:**
- Modify: `crates/cli/src/chat/budget.rs`

- [ ] **Step 1: Write the failing tests**

Add to `budget.rs` `mod tests`:

```rust
    #[test]
    fn budget_uses_override_then_model() {
        let with = ContextState::new(Some(64_000));
        assert_eq!(with.budget("claude-3-5-sonnet"), 64_000); // override wins
        let auto = ContextState::new(None);
        assert_eq!(auto.budget("claude-3-5-sonnet"), 200_000); // model window
        assert_eq!(auto.budget("mystery"), DEFAULT_CONTEXT_BUDGET);
    }

    #[test]
    fn manage_warns_once_then_trims() {
        console::set_colors_enabled(false);
        // Tiny budget so a couple of messages cross the bands.
        let mut st = ContextState::new(Some(40));
        let mut c = Conversation::new("gpt-4o-mini".into(), None);
        c.push_user("a few words here to use some of the budget".into());
        st.manage(&mut c); // ~ crosses 75% (warn) — should set warned
        assert!(st.warned);
        // Push a lot more so we exceed 95% and force a trim.
        for i in 0..10 {
            c.push_user(format!("more and more filler text number {i} to overflow"));
            c.push_assistant(format!("and a reply number {i} adding yet more tokens"));
        }
        let before = c.messages.len();
        st.manage(&mut c);
        assert!(c.messages.len() < before, "manage should have trimmed");
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p tt-cli --lib chat::budget::tests 2>&1 | tail -12`
Expected: FAIL to compile — `cannot find type ContextState`.

- [ ] **Step 3: Add `ContextState` + `manual_trim`**

In `budget.rs`, add after `trim_to_budget`:

```rust
/// Per-session context budget + warn/trim state.
pub struct ContextState {
    /// Explicit budget from `--max-context` / `/context <n>`; otherwise the
    /// per-model window is used.
    pub override_budget: Option<u32>,
    warned: bool,
}

impl ContextState {
    #[must_use]
    pub fn new(override_budget: Option<u32>) -> Self {
        Self {
            override_budget,
            warned: false,
        }
    }

    /// Effective budget for `model`: the override if set, else the window table.
    #[must_use]
    pub fn budget(&self, model: &str) -> u32 {
        self.override_budget.unwrap_or_else(|| model_window(model))
    }

    /// Estimated tokens for the conversation.
    #[must_use]
    pub fn estimate(&self, conv: &Conversation) -> u32 {
        estimate_conversation_tokens(conv, ESTIMATE_PROVIDER)
    }

    /// Warn at 75%, auto-trim at 95% (down to ~70%). Call after the user turn is
    /// pushed and before sending. Prints via `ui`; may mutate `conv` (trim).
    pub fn manage(&mut self, conv: &mut Conversation) {
        let budget = self.budget(&conv.model);
        if budget == 0 {
            return;
        }
        let est = self.estimate(conv);
        let frac = f64::from(est) / f64::from(budget);
        if frac > TRIM_FRAC {
            let target = (f64::from(budget) * TRIM_TARGET_FRAC) as u32;
            let dropped = trim_to_budget(conv, target, ESTIMATE_PROVIDER);
            if dropped > 0 {
                ui::note(&format!(
                    "(context ~{est}/{budget} tok — trimmed {dropped} old message(s))"
                ));
            }
            self.warned = false;
        } else if frac > WARN_FRAC {
            if !self.warned {
                let pct = (frac * 100.0) as u32;
                ui::warn(&format!(
                    "context ~{est}/{budget} tok ({pct}%) — oldest turns auto-trim near the limit"
                ));
                self.warned = true;
            }
        } else {
            self.warned = false;
        }
    }
}

/// Manual `/trim`: drop oldest turns to ~70% of the effective budget.
#[must_use]
pub fn manual_trim(conv: &mut Conversation, ctx: &ContextState) -> usize {
    let target = (f64::from(ctx.budget(&conv.model)) * TRIM_TARGET_FRAC) as u32;
    trim_to_budget(conv, target, ESTIMATE_PROVIDER)
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p tt-cli --lib chat::budget 2>&1 | tail -12`
Expected: PASS (all `budget` tests).

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/chat/budget.rs
git commit -m "feat(chat): ContextState warn/auto-trim + manual_trim"
```

---

### Task 4: Wire `/context` + `/trim` + `--max-context` into the REPL

**Files:**
- Modify: `crates/cli/src/chat/mod.rs`
- Modify: `crates/cli/src/main.rs`

- [ ] **Step 1: Add the `Command` variants + parse**

In `enum Command`, after `Tools(Option<bool>),`:

```rust
    Context(Option<u32>),
    Trim,
```

In `Command::parse`, after the `"tools" => …` arm:

```rust
            "context" => Command::Context(arg.as_deref().and_then(|a| a.parse().ok())),
            "trim" => Command::Trim,
```

- [ ] **Step 2: Add the `--max-context` param to `run` + the `ContextState`**

Change `run`'s signature to add `max_context` after `tools`:

```rust
pub async fn run(
    model: Option<String>,
    system: Option<String>,
    resume: Option<String>,
    tools: bool,
    max_context: Option<u32>,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
```

After `let mut tools_enabled = tools;`, add:

```rust
    let mut ctx = budget::ContextState::new(max_context);
```

- [ ] **Step 3: Manage context after each user turn (Chat + Editor)**

Replace both occurrences of `conv.push_user(t);` (the Chat arm and the Editor `Ok(Some(t))` arm) with:

```rust
conv.push_user(t);
ctx.manage(&mut conv);
```

Run: `cargo fmt -p tt-cli` afterward to reindent.

- [ ] **Step 4: Add the `/context` + `/trim` run arms**

After the `Command::Cost => ui::info(&ledger.summary()),` arm, add:

```rust
                    Command::Context(set) => {
                        if let Some(n) = set {
                            ctx.override_budget = Some(n);
                            ui::info(&format!("context budget → {n} tokens"));
                        } else {
                            let budget = ctx.budget(&conv.model);
                            let est = ctx.estimate(&conv);
                            let pct = if budget > 0 { est * 100 / budget } else { 0 };
                            ui::info(&format!(
                                "context: ~{est} / {budget} tokens ({pct}%) [{}]",
                                conv.model
                            ));
                        }
                    }
                    Command::Trim => {
                        let dropped = budget::manual_trim(&mut conv, &ctx);
                        ui::info(&format!("trimmed {dropped} old message(s)"));
                    }
```

- [ ] **Step 5: Add help rows**

In `print_help`, after the `("/tools [on|off]", …)` row:

```rust
        ("/context [n]", "show or set the token budget"),
        ("/trim", "drop oldest turns to fit the budget"),
```

- [ ] **Step 6: Thread `--max-context` through `main.rs`**

In the `Chat { … }` command, after the `tools` arg:

```rust
        /// Token budget for context management (default: the per-model window).
        #[arg(long)]
        max_context: Option<u32>,
```

In the `Command::Chat { … }` match arm, add `max_context,` to the destructure and pass it:

```rust
        Command::Chat {
            model,
            system,
            resume,
            tools,
            max_context,
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::chat::run(model, system, resume, tools, max_context, tt_api_key, tt_api_base)
                .await?;
        }
```

- [ ] **Step 7: Build + chat tests**

Run: `cargo build -p tt-cli 2>&1 | grep -E "^error|never used" | head` then `cargo test -p tt-cli --lib chat 2>&1 | tail -6`
Expected: no errors / no dead-code; all chat tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/cli/src/chat/mod.rs crates/cli/src/main.rs
git commit -m "feat(chat): /context + /trim + --max-context wiring"
```

---

### Task 5: Gates + smoke + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt -p tt-cli && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | grep -vE "rgb-0.8.52|Permission denied|failed to (remove|clean|auto-clean)" | tail -15`
Expected: no warnings.

- [ ] **Step 2: Full tests**

Run: `cargo test -p tt-cli 2>&1 | grep -E "test result|error\[" | tail -8`
Expected: all pass.

- [ ] **Step 3: cargo-deny**

Run: `cargo deny check advisories 2>&1 | tail -3`
Expected: `advisories ok` (`tt-tokenize` is a workspace crate — pulls `tiktoken-rs`, already in the tree).

- [ ] **Step 4: Smoke (piped, no network)**

Run:
```bash
cargo build -q -p tt-cli --bin tt
printf '/context\n/context 50000\n/context\n/trim\n/help\n/exit\n' | TT_API_KEY=test target/debug/tt chat 2>&1 | grep -E "context:|budget|trimmed|/context|/trim"
```
Expected: `/context` shows `context: ~N / 128000 tokens …`; `/context 50000` → `context budget → 50000 tokens`; the next `/context` shows `/ 50000`; `/trim` → `trimmed N old message(s)`; `/help` lists `/context [n]` and `/trim`.

- [ ] **Step 5: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** window table (T1), estimate (T1), coherent trim (T2), `ContextState` warn/trim + `manual_trim` (T3), `/context` show/set + `/trim` + `ctx.manage` integration + help (T4), `--max-context` (T4), gates/smoke (T5). All spec items covered.
- **Placeholders:** none — every step has complete code.
- **Type consistency:** `model_window(&str)->u32`, `estimate_conversation_tokens(&Conversation,&str)->u32`, `trim_to_budget(&mut Conversation,u32,&str)->usize`, `ContextState::{new(Option<u32>), budget(&str)->u32, estimate(&Conversation)->u32, manage(&mut Conversation)}`, `manual_trim(&mut Conversation,&ContextState)->usize` are used consistently in mod.rs; `run` gains `max_context: Option<u32>` matched by main.rs's destructure/call.
- **Deviation from spec:** `ctx.manage` is called at the two `push_user` sites (Chat/Editor) rather than inside `dispatch_turn` — same "manage before sending new content" effect, but keeps `dispatch_turn` at 7 args (no `too_many_arguments` allow) and avoids editing 3 fmt-wrapped call sites. Retry re-sends already-managed history, so it is intentionally not re-managed.
