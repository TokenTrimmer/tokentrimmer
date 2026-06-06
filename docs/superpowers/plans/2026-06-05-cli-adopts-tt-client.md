# CLI Adopts tt-client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route `tt chat` (streaming) and the `tt advise` / tool-calling loop through the `tt-client` SDK, deleting the CLI's duplicate gateway networking + SSE/cost parsing.

**Architecture:** Add `tt-client` to `tt-cli`; thread a single `&tt_client::Client` through the turn functions in place of `(http, base, key)`. `stream_turn` uses `ChatStream`; `tools.rs::send_round` uses `client.send()` and reads the typed response. CLI domain types (`Conversation`/`Ledger`/`UsageInfo`/`format_turn_footer`/`TurnTotals`) and all UX are unchanged.

**Tech Stack:** Rust, `tt-client` (sibling crate), reqwest (kept only for `tt models` catalog fetch), httpmock (tests).

Spec: `docs/superpowers/specs/2026-06-05-cli-adopts-tt-client-design.md`. Branch `cli-adopts-tt-client` (created off `main`, spec committed).

**Sequencing note:** `dispatch_turn` fans out to BOTH the streaming and tool paths, and `advise.rs` calls `run_tool_turn`, so the `&tt_client::Client` threading + both path rewrites must land in ONE compiling commit (Task 2). There is no clean intermediate where only one path is converted.

---

### Task 1: Add the `tt-client` dependency

**Files:**
- Modify: `Cargo.toml` (root `[workspace.dependencies]`, ~line 121)
- Modify: `crates/cli/Cargo.toml` (`[dependencies]`)

- [ ] **Step 1: Add to workspace dependencies**

In the root `Cargo.toml`, in `[workspace.dependencies]`, add after the `tt-tokenize` line (~line 121):

```toml
tt-client = { path = "crates/client" }
```

- [ ] **Step 2: Depend on it from tt-cli**

In `crates/cli/Cargo.toml`, under `[dependencies]`, add after `tt-tokenize.workspace = true`:

```toml
tt-client.workspace = true
```

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p tt-cli`
Expected: builds (the dep is unused for now; that is not an error).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/cli/Cargo.toml Cargo.lock
git commit -m "build(tt-cli): depend on tt-client"
```

---

### Task 2: Route both gateway paths through tt-client

This is the atomic refactor. Apply all edits, then build + test once at the end. Each sub-step shows the complete new code.

**Files:**
- Modify: `crates/cli/src/chat/mod.rs` (`UsageInfo`, `stream_turn`, `do_turn`, `dispatch_turn`, `run()`, deletions, tests)
- Modify: `crates/cli/src/chat/tools.rs` (`Round`, `send_round`, `run_tool_turn`, `registry_tools`, deletions, tests)
- Modify: `crates/cli/src/advise.rs` (build a `tt_client::Client`)

#### 2A — `chat/mod.rs`

- [ ] **Step 1: Add `From<tt_client::StreamUsage> for UsageInfo`**

After the `UsageInfo` struct (ends ~line 30 in `crates/cli/src/chat/mod.rs`), add:

```rust
impl From<tt_client::StreamUsage> for UsageInfo {
    fn from(u: tt_client::StreamUsage) -> Self {
        Self {
            cost_usd: u.cost_usd,
            baseline_cost_usd: u.baseline_cost_usd,
            saved_usd: u.saved_usd,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_tokens: u.cached_tokens,
        }
    }
}
```

- [ ] **Step 2: Delete `parse_sse_frame` and the `StreamEvent` enum**

Remove the `StreamEvent` enum (~lines 32-39) and the `parse_sse_frame` fn (~lines 41-76) entirely.

- [ ] **Step 3: Delete `drain_frames`**

Remove the `drain_frames` fn (~lines 308-319).

- [ ] **Step 4: Rewrite `stream_turn`**

Replace the entire `stream_turn` fn (~lines 321-395) with:

```rust
/// Stream one turn. Prints the assistant text live and the cost footer, and
/// returns the full reply for history. Returns `Err` (turn failed) on a non-2xx
/// gateway response so the caller can drop the unanswered user message.
async fn stream_turn(
    client: &tt_client::Client,
    conv: &Conversation,
) -> anyhow::Result<(String, Option<UsageInfo>)> {
    let mut stream = client
        .chat()
        .model(&conv.model)
        .messages(conv.wire_messages())
        .stream()
        .await
        .context("request to gateway failed")?;

    let served_model = stream
        .header_cost()
        .model_used
        .clone()
        .unwrap_or_else(|| conv.model.clone());

    let mut spinner = Some(ui::spinner("…"));
    let mut reply = String::new();
    let mut usage: Option<UsageInfo> = None;

    while let Some(ev) = stream.next().await.context("stream error")? {
        match ev {
            tt_client::StreamEvent::Delta(t) => {
                spinner.take(); // clear the spinner on the first token
                print!("{t}");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
                reply.push_str(&t);
            }
            tt_client::StreamEvent::Usage(u) => usage = Some(UsageInfo::from(u)),
            _ => {} // StreamEvent is #[non_exhaustive] (external crate) → wildcard required
        }
    }
    drop(spinner);
    println!();
    if let Some(u) = &usage {
        println!(
            "{}",
            format_turn_footer(
                &served_model,
                u.input_tokens,
                u.output_tokens,
                u.cost_usd,
                u.saved_usd,
                u.baseline_cost_usd
            )
        );
    }
    Ok((reply, usage))
}
```

- [ ] **Step 5: Update `do_turn` signature + call**

Replace `do_turn` (~lines 400-420) with:

```rust
/// Stream the current conversation: print live, push the assistant reply, and
/// update the ledger. Returns true on success. The caller decides whether to
/// drop the pending user turn on failure.
async fn do_turn(
    client: &tt_client::Client,
    conv: &mut Conversation,
    ledger: &mut Ledger,
) -> bool {
    match stream_turn(client, conv).await {
        Ok((reply, usage)) => {
            conv.push_assistant(reply);
            if let Some(u) = usage {
                ledger.add(&u);
            }
            true
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            false
        }
    }
}
```

- [ ] **Step 6: Update `dispatch_turn`**

Replace `dispatch_turn` (~lines 422-437) with:

```rust
/// Route a turn to the tool-calling loop (tools on) or the streamed path (off).
async fn dispatch_turn(
    client: &tt_client::Client,
    conv: &mut Conversation,
    ledger: &mut Ledger,
    reg: &tt_mcp::tools::Registry,
    tools_enabled: bool,
) -> bool {
    if tools_enabled {
        tools::run_tool_turn(client, conv, reg, ledger).await
    } else {
        do_turn(client, conv, ledger).await
    }
}
```

- [ ] **Step 7: Build the client in `run()` and update the dispatch calls**

In `run()`, after `let http = reqwest::Client::new();` (~line 510), add:

```rust
    let client = tt_client::Client::new(base.clone(), key.clone());
```

(`http`/`base`/`key` stay — `catalog::fetch_catalog(&http, &base, Some(&key))` at ~line 526 still uses them.)

Then change each of the three `dispatch_turn(&http, &base, &key, ...)` call sites (~lines 549, 645, 664) so the first three args become a single `&client`. Each call goes from:

```rust
                        if !dispatch_turn(
                            &http,
                            &base,
                            &key,
                            &mut conv,
```
to:
```rust
                        if !dispatch_turn(
                            &client,
                            &mut conv,
```
(Leave the remaining args — `&mut ledger`, `&reg`, `tools`/`tools_enabled` — exactly as they are.)

- [ ] **Step 8: Remove now-unused imports**

`stream_turn` no longer uses `futures::StreamExt` or `serde_json::json` (the SDK builds the request and drives the byte stream). Remove these two `use` lines from the top of `crates/cli/src/chat/mod.rs`:

```rust
use futures::StreamExt as _;
use serde_json::json;
```
Keep `use anyhow::Context as _;`, `use serde::Deserialize;` (UsageInfo derive), and `use tt_shared::messages::{Message, MessageContent};`.

#### 2B — `chat/tools.rs`

- [ ] **Step 9: Add `assistant_msg` to `Round`**

Replace the `Round` struct (~lines 146-152) with:

```rust
/// One round's parsed response.
struct Round {
    served_model: String,
    calls: Vec<ToolCall>,
    content: String,
    usage: Option<UsageInfo>,
    /// The typed assistant message the SDK returned (carries content +
    /// tool_calls), pushed to history verbatim when the round requests tools.
    assistant_msg: Option<Message>,
}
```

- [ ] **Step 10: Rewrite `send_round`**

Replace the entire `send_round` fn (~lines 154-208) with:

```rust
/// Send one non-streamed request through the SDK and parse it. `force_no_tools`
/// sets `tool_choice:"none"` so the model must answer with text — used to close
/// out a turn that hit the round cap.
async fn send_round(
    client: &tt_client::Client,
    conv: &Conversation,
    tools: &[tt_client::Tool],
    force_no_tools: bool,
) -> anyhow::Result<Round> {
    let mut builder = client
        .chat()
        .model(&conv.model)
        .messages(conv.wire_messages())
        .tools(tools.to_vec());
    if force_no_tools {
        builder = builder.tool_choice(tt_client::ToolChoice::Auto("none".to_string()));
    }
    let out = builder.send().await.context("request to gateway failed")?;

    let served_model = out
        .cost
        .model_used
        .clone()
        .unwrap_or_else(|| conv.model.clone());
    let calls = out.tool_calls().to_vec();
    let content = out.text().unwrap_or_default().to_string();
    let usage = out.cost.cost_usd.map(|c| {
        usage_from_parts(
            c,
            out.cost.saved_usd.unwrap_or(0.0),
            out.cost.baseline_cost_usd,
            out.response.usage.prompt_tokens,
            out.response.usage.completion_tokens,
        )
    });
    let assistant_msg = out.response.choices.first().map(|ch| ch.message.clone());
    Ok(Round {
        served_model,
        calls,
        content,
        usage,
        assistant_msg,
    })
}
```

- [ ] **Step 11: Add `registry_tools` and delete `tools_json`**

Replace the `tools_json` fn (~lines 30-46) with:

```rust
/// Build the SDK `tools` from the registry's tool definitions.
fn registry_tools(reg: &Registry) -> Vec<tt_client::Tool> {
    reg.list()
        .into_iter()
        .map(|d| tt_client::tool(d.name, d.description, d.input_schema))
        .collect()
}
```

- [ ] **Step 12: Delete `parse_tool_calls` and `header_f64`**

Remove the `parse_tool_calls` fn (~lines 48-72) and the `header_f64` fn (~lines 107-111) entirely.

- [ ] **Step 13: Update `run_tool_turn`**

Change the signature and the two `send_round` calls and the assistant-push. Specifically:

Signature (~lines 244-251) becomes:
```rust
pub async fn run_tool_turn(
    client: &tt_client::Client,
    conv: &mut Conversation,
    reg: &Registry,
    ledger: &mut Ledger,
) -> bool {
    let start_len = conv.messages.len();
    let tools = registry_tools(reg);
    let mut turn = TurnTotals::default();
```

First `send_round` call (~line 257) becomes:
```rust
        let round = match send_round(client, conv, &tools, false).await {
```

The assistant-push block (~lines 278-285) — replace:
```rust
        // assistant turn that requests tools — preserve any accompanying text
        let content =
            (!round.content.is_empty()).then(|| MessageContent::Text(round.content.clone()));
        conv.messages.push(Message::Assistant {
            content,
            tool_calls: round.calls.clone(),
            name: None,
        });
```
with:
```rust
        // push the assistant message the SDK returned verbatim (already typed,
        // carrying any accompanying text + the tool_calls)
        if let Some(m) = round.assistant_msg.clone() {
            conv.messages.push(m);
        }
```

Forced-final `send_round` call (~line 310) becomes:
```rust
    match send_round(client, conv, &tools, true).await {
```

- [ ] **Step 14: Fix tools.rs imports**

At the top of `crates/cli/src/chat/tools.rs`, the `tt_shared` import no longer needs `ToolCallFunction` (only `parse_tool_calls` used it). Change:
```rust
use tt_shared::messages::{Message, MessageContent, ToolCall, ToolCallFunction};
```
to:
```rust
use tt_shared::messages::{Message, MessageContent, ToolCall};
```
(`serde_json::{json, Value}` stays — `json!` is still used for tool-error results and `Value` for parsing tool args.)

#### 2C — `advise.rs`

- [ ] **Step 15: Build a `tt_client::Client` in `advise::run`**

In `crates/cli/src/advise.rs`, replace `let http = reqwest::Client::new();` (~line 214) with:

```rust
    let client = tt_client::Client::new(base.clone(), key.clone());
```

And change the `run_tool_turn` call (~line 235):
```rust
    tools::run_tool_turn(&http, &base, &key, &mut conv, &reg, &mut ledger).await;
```
to:
```rust
    tools::run_tool_turn(&client, &mut conv, &reg, &mut ledger).await;
```

#### 2D — Tests

- [ ] **Step 16: Delete dead SSE unit tests in mod.rs**

In `crates/cli/src/chat/mod.rs` `mod tests`, delete these four test fns: `parse_content_delta` (~730), `parse_usage_event` (~736), `parse_done_and_ignore` (~748), `drain_frames_handles_chunk_split_multibyte` (~780). Keep `command_parse`, `footer_formats_with_savings`, `ledger_accumulates`, and all others.

- [ ] **Step 17: Add a streaming integration test in mod.rs**

In `crates/cli/src/chat/mod.rs` `mod tests`, add (a `use httpmock::prelude::*;` may already be needed — add it at the top of the test module if absent):

```rust
    use httpmock::prelude::*;

    #[tokio::test]
    async fn stream_turn_streams_reply_and_usage() {
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "event: tokentrimmer.usage\n",
            "data: {\"cost_usd\":0.0001,\"baseline_cost_usd\":0.0004,\"saved_usd\":0.0003,\"input_tokens\":10,\"output_tokens\":2,\"cached_tokens\":0}\n\n",
            "data: [DONE]\n\n",
        );
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .body(sse);
        });

        let client = tt_client::Client::new(server.base_url(), "k");
        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("hi".into());

        let (reply, usage) = stream_turn(&client, &conv).await.unwrap();
        assert_eq!(reply, "Hello");
        let u = usage.expect("usage event");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 2);
    }
```

- [ ] **Step 18: Delete `parse_tool_calls` test + retarget `tools_json` test in tools.rs**

In `crates/cli/src/chat/tools.rs` `mod tests`:
- Delete `parse_tool_calls_extracts_and_skips_malformed` (~lines 340-356).
- Replace `tools_json_advertises_three_tools` (~lines 524-545) with:

```rust
    #[test]
    fn registry_tools_advertises_three_tools() {
        let reg = build_registry();
        let t = registry_tools(&reg);
        assert_eq!(t.len(), 3);
        let names: Vec<&str> = t.iter().map(|x| x.function.name.as_str()).collect();
        assert!(names.contains(&"find_route_for"));
        assert!(names.contains(&"preview_cost"));
        assert!(names.contains(&"inspect_diff"));
        let fr = t
            .iter()
            .find(|x| x.function.name == "find_route_for")
            .unwrap();
        assert_eq!(fr.function.parameters["required"][0], "task_description");
    }
```
Keep `usage_baseline_from_header_or_derived` and `format_tool_call_has_name_and_truncates`.

- [ ] **Step 19: Update the httpmock tool-loop tests to use a `tt_client::Client`**

In `crates/cli/src/chat/tools.rs` `mod tests`, in each of `tool_loop_executes_then_answers`, `tool_loop_forces_answer_at_round_cap`, and `tool_loop_rolls_back_on_error`, replace:
```rust
        let http = reqwest::Client::new();
        let ok = run_tool_turn(&http, &server.base_url(), "k", &mut conv, &reg, &mut ledger).await;
```
with:
```rust
        let client = tt_client::Client::new(server.base_url(), "k");
        let ok = run_tool_turn(&client, &mut conv, &reg, &mut ledger).await;
```
(The mocks, conversation setup, and assertions are unchanged — the wire format the SDK sends is identical, so the `body_contains` / header matchers still hold. In `tool_loop_rolls_back_on_error` the variable is named `start` not `ok`; keep its existing assertions, only swap the client construction + call.)

#### 2E — Build, test, commit

- [ ] **Step 20: Build**

Run: `cargo build -p tt-cli`
Expected: compiles. If the compiler flags an unused import (e.g. `reqwest` in `advise.rs` if it was only used for the removed client), remove it.

- [ ] **Step 21: Test**

Run: `cargo test -p tt-cli`
Expected: all pass, including `stream_turn_streams_reply_and_usage`, `registry_tools_advertises_three_tools`, and the three `tool_loop_*` tests.

- [ ] **Step 22: Commit**

```bash
git add crates/cli/src/chat/mod.rs crates/cli/src/chat/tools.rs crates/cli/src/advise.rs
git commit -m "refactor(tt-cli): route chat streaming + tool loop through tt-client"
```

---

### Task 3: Gates + finish the branch

**Files:** none (verification + PR)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `git diff --quiet || git commit -am "style: cargo fmt"`

- [ ] **Step 2: Clippy (workspace, all targets, deny warnings)**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0. Common follow-ups: remove any now-unused imports clippy flags (`futures`/`serde_json::json` in `mod.rs`, `reqwest` in `advise.rs` if applicable). Re-run until clean.

- [ ] **Step 3: Tests + advisories + docs**

Run: `cargo test -p tt-cli`
Expected: pass.
Run: `cargo deny check advisories`
Expected: ok.
Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-cli --no-deps`
Expected: exit 0.

- [ ] **Step 4: Finish the branch**

Use the **superpowers:finishing-a-development-branch** skill: verify tests, push `cli-adopts-tt-client`, create the PR (option 2). PR body: summarise the de-dup (streaming + tool path onto the SDK, deleted parsers) and the test plan.

- [ ] **Step 5: Adversarial review + CI**

After the PR is open, run a Workflow-based adversarial review (lenses: behavior-preservation/parity vs the old code, error-mapping & UX equivalence, dead-code/import hygiene) with per-finding verification against the real source, and watch CI. Fix any confirmed findings on the branch before merge. Update the roadmap memory when green.

---

## Notes for the implementer

- **One compiling commit for Task 2:** because `dispatch_turn` calls both paths and `advise.rs` calls `run_tool_turn`, the tree only compiles once every signature is converted. Apply all of 2A–2D before building in 2E.
- **`Client::new` arg types:** `base`/`key` are `String`; `tt_client::Client::new` takes `impl Into<String>`, so pass `base.clone()`/`key.clone()` (in `run()` they're reused by the catalog fetch; cloning is required there anyway).
- **`out.text()` on a tool-call response** returns `None` (content is null) → `content == ""`, matching the old `parse_tool_calls` behavior where the accompanying text was empty.
- **`ToolChoice::Auto("none".to_string())`** serialises (untagged) to the bare string `"none"`, so the `tool_loop_forces_answer_at_round_cap` mock's `body_contains("\"tool_choice\":\"none\"")` still matches.
- **No UX change:** the spinner, live `print!`, footer, tool-call previews, round cap, and failure roll-back (`truncate(start_len)`) all stay; only the gateway call + parsing moved into the SDK.
