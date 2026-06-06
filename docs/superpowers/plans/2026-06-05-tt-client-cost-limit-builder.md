# tt-client `.cost_limit()` Builder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `.cost_limit(usd)` builder option to `tt-client` that sends `X-TokenTrimmer-Cost-Limit-Usd` on every request path (`send`/`stream`/`run_tools`).

**Architecture:** Mirror the existing `.tag()` option exactly — a `ChatBuilder` field + setter, with header injection at each of the three request-build sites.

**Tech Stack:** Rust, reqwest, httpmock (tests).

Spec: `docs/superpowers/specs/2026-06-05-tt-client-cost-limit-builder-design.md`. Branch `tt-client-cost-limit` (off `main`, spec committed).

**Verified anchors (`crates/client`):**
- `ChatBuilder` struct (lib.rs) — fields `…, tag: Option<String>, tools, tool_choice, max_tool_rounds`.
- `Client::chat()` init (lib.rs) — sets `tag: None, …`.
- `.tag()` setter at lib.rs:331-335.
- Tag header injection: `send` lib.rs:382-384, `stream` lib.rs:425-427 (`if let Some(tag) = &self.tag { req = req.header("X-TokenTrimmer-Tag", tag); }`).
- `send_round` (tools.rs:107-117) ends with `tag: Option<&str>`; tag injected at tools.rs:131-133; called at tools.rs:182 (per-round) and ~230 (forced-final); `run_tools` destructures the builder at tools.rs:166-176 and `let tag = tag.as_deref();` at :177.
- Test helpers in the lib.rs `mod tests`: `sample_response()`, `user(...)`, `Client::new`, `MockServer`, `POST`, `json!`.

---

### Task 1: Builder option + `send`/`stream` injection

**Files:**
- Modify: `crates/client/src/lib.rs` (struct field, `chat()` init, setter, `send`, `stream`, tests)

- [ ] **Step 1: Write the failing tests**

In `crates/client/src/lib.rs` `mod tests`, add:

```rust
    #[tokio::test]
    async fn send_sends_cost_limit_header() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("x-tokentrimmer-cost-limit-usd", "0.05");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(sample_response());
        });
        let client = Client::new(server.base_url(), "k");
        let out = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .cost_limit(0.05)
            .send()
            .await
            .unwrap();
        assert_eq!(out.response.model, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn cost_limit_402_surfaces_as_status() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(402).body("cost limit exceeded");
        });
        let client = Client::new(server.base_url(), "k");
        let result = client
            .chat()
            .model("m")
            .message(user("hi"))
            .cost_limit(0.0001)
            .send()
            .await;
        assert!(matches!(result, Err(Error::Status { status: 402, .. })));
    }

    #[tokio::test]
    async fn stream_sends_cost_limit_header() {
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("x-tokentrimmer-cost-limit-usd", "0.05");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse);
        });
        let client = Client::new(server.base_url(), "k");
        let mut stream = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .cost_limit(0.05)
            .stream()
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(ev) = stream.next().await.unwrap() {
            if let StreamEvent::Delta(t) = ev {
                text.push_str(&t);
            }
        }
        assert_eq!(text, "Hi");
    }
```

- [ ] **Step 2: Run them — confirm they FAIL**

Run: `cargo test -p tt-client cost_limit`
Expected: FAIL to compile — `no method named cost_limit`. (After the impl compiles, `send_sends_cost_limit_header`/`stream_sends_cost_limit_header` would also fail if the header weren't sent, because the mock requires it.)

- [ ] **Step 3: Add the field**

In `crates/client/src/lib.rs`, add `cost_limit: Option<f64>` to the `ChatBuilder` struct (after `tag: Option<String>,`):

```rust
pub struct ChatBuilder<'a> {
    client: &'a Client,
    model: String,
    messages: Vec<Message>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    tag: Option<String>,
    cost_limit: Option<f64>,
    tools: Vec<Tool>,
    tool_choice: Option<ToolChoice>,
    max_tool_rounds: usize,
}
```

- [ ] **Step 4: Initialise it in `chat()`**

In `Client::chat()`, add `cost_limit: None,` after `tag: None,`:

```rust
            tag: None,
            cost_limit: None,
```

- [ ] **Step 5: Add the setter**

After the `tag` setter (lib.rs:331-335), add:

```rust
    /// `X-TokenTrimmer-Cost-Limit-Usd` — the gateway rejects the request with
    /// `402` if its estimated cost exceeds `usd`.
    #[must_use]
    pub fn cost_limit(mut self, usd: f64) -> Self {
        self.cost_limit = Some(usd);
        self
    }
```

- [ ] **Step 6: Inject the header in `send` and `stream`**

In `send` (after the tag injection at lib.rs:382-384) and in `stream` (after the tag injection at lib.rs:425-427), add the same block in both:

```rust
        if let Some(tag) = &self.tag {
            req = req.header("X-TokenTrimmer-Tag", tag);
        }
        if let Some(limit) = self.cost_limit {
            req = req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
        }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p tt-client cost_limit`
Expected: `send_sends_cost_limit_header`, `stream_sends_cost_limit_header`, `cost_limit_402_surfaces_as_status` PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(tt-client): .cost_limit() builder (send + stream)"
```

---

### Task 2: `run_tools` / `send_round` threading

**Files:**
- Modify: `crates/client/src/tools.rs` (`send_round` param + injection; `run_tools` destructure + calls; test)

- [ ] **Step 1: Write the failing test**

In `crates/client/src/tools.rs` `mod tests`, add (the test module already has `Canned`, `text_response`, `tool`, `user`, `Client`, httpmock in scope):

```rust
    #[tokio::test]
    async fn run_tools_sends_cost_limit_header() {
        let server = MockServer::start_async().await;
        // Require the header; return an immediate text answer (no tool calls) so
        // the loop completes in one round.
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .header("x-tokentrimmer-cost-limit-usd", "0.05");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(text_response("done"));
        });
        let client = Client::new(server.base_url(), "k");
        let out = client
            .chat()
            .model("gpt-4o-mini")
            .message(user("hi"))
            .cost_limit(0.05)
            .run_tools(&Canned("x"))
            .await
            .unwrap();
        assert_eq!(out.text(), Some("done"));
    }
```

- [ ] **Step 2: Run it — confirm it FAILS**

Run: `cargo test -p tt-client run_tools_sends_cost_limit_header`
Expected: FAIL — the loop's request lacks the header, so the mock doesn't match → the request errors / the loop returns `Err` and `.unwrap()` panics (or `out.text()` mismatches).

- [ ] **Step 3: Add the `cost_limit` param to `send_round`**

In `crates/client/src/tools.rs`, add `cost_limit: Option<f64>` as the final `send_round` parameter (after `tag: Option<&str>,`), and inject the header after the tag injection:

```rust
#[allow(clippy::too_many_arguments)]
async fn send_round(
    client: &Client,
    model: &str,
    messages: &[Message],
    tools: &[Tool],
    tool_choice: Option<&ToolChoice>,
    force_no_tools: bool,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    tag: Option<&str>,
    cost_limit: Option<f64>,
) -> Result<(ChatCompletionResponse, CostInfo)> {
```

and after the existing tag block (tools.rs:131-133):

```rust
    if let Some(t) = tag {
        req = req.header("X-TokenTrimmer-Tag", t);
    }
    if let Some(limit) = cost_limit {
        req = req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
    }
```

- [ ] **Step 4: Thread `cost_limit` through `run_tools`**

In `run_tools`, add `cost_limit` to the destructure (after `tag,`) and bind it; then pass it to both `send_round` calls.

Destructure (tools.rs:166-177):

```rust
        let ChatBuilder {
            client,
            model,
            mut messages,
            max_tokens,
            temperature,
            tag,
            cost_limit,
            tools,
            tool_choice,
            max_tool_rounds,
        } = self;
        let tag = tag.as_deref();
```

Per-round `send_round` call (tools.rs:182…) — add `cost_limit` as the last argument (after `tag`):

```rust
            let (response, ci) = send_round(
                client,
                &model,
                &messages,
                &tools,
                tool_choice.as_ref(),
                false,
                max_tokens,
                temperature,
                tag,
                cost_limit,
            )
            .await?;
```

Forced-final `send_round` call (the second call, ~tools.rs:230, with `force_no_tools = true`) — add `cost_limit` as the last argument the same way:

```rust
        let (response, ci) = send_round(
            client,
            &model,
            &messages,
            &tools,
            tool_choice.as_ref(),
            true,
            max_tokens,
            temperature,
            tag,
            cost_limit,
        )
        .await?;
```

- [ ] **Step 5: Run the test + the full SDK suite**

Run: `cargo test -p tt-client`
Expected: `run_tools_sends_cost_limit_header` passes alongside the full suite (the other `run_tools_*` tests don't set `.cost_limit()`, so `cost_limit` is `None` and no header is injected — they stay green).

- [ ] **Step 6: Commit**

```bash
git add crates/client/src/tools.rs
git commit -m "feat(tt-client): thread .cost_limit() through run_tools/send_round"
```

---

### Task 3: Gates + finish the branch

**Files:** none (verification + PR)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt --all`
Then: `git diff --quiet || git commit -am "style: cargo fmt"`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0.

- [ ] **Step 2: Tests + advisories + docs**

Run: `cargo test -p tt-client`
Expected: all pass.
Run: `cargo deny check advisories`
Expected: ok.
Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-client --no-deps`
Expected: exit 0.

- [ ] **Step 3: Finish the branch**

Use the **superpowers:finishing-a-development-branch** skill: verify tests, push `tt-client-cost-limit`, create the PR (option 2). PR body: the `.cost_limit()` option mirroring `.tag()`, injected on `send`/`stream`/`run_tools`; pairs with the #41 gateway 402.

- [ ] **Step 4: Adversarial review + CI**

After the PR is open, run a Workflow-based adversarial review (lenses: header parity across all three paths + value formatting; builder/API hygiene). Watch CI; fix confirmed findings before merge. Update roadmap memory (F2 done) when green.

---

## Notes for the implementer

- **Three paths, one header:** `cost_limit` must be injected in `send`, `stream`, AND `send_round` (the `run_tools` loop) — exactly where `tag` already is. Missing one leaves a silent gap (a `.cost_limit()` that doesn't apply to `run_tools`).
- **Value formatting:** `format!("{limit}")` (Rust f64 `Display`, e.g. `0.05`) — the gateway's `parse::<f64>` accepts it. No client-side validation (the gateway ignores non-positive limits).
- **Destructure exhaustiveness:** adding the `cost_limit` field means every `ChatBuilder { … }` destructure must list it — `run_tools` is the only one; the compiler will flag any missed.
