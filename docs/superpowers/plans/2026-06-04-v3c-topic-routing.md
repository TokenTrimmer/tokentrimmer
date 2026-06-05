# V3c — Topic/keyword Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `prompt_contains_any_of` route condition that matches when the request's user+system text contains any of a set of keywords (case-insensitive substring), plus `tt route add --when-prompt-contains`.

**Architecture:** One `tt_shared` helper extracts the user+system input text. `tt_routing::RouteConditions` gains the field + a matcher arm (case-insensitive contains-any); `tt_plan_core` mirrors it, matching `RequestLog.body` when present (else conservative no-match). The CLI adds a repeatable flag. Same shape as V3a-1's modality conditions.

**Tech Stack:** Rust workspace — `tt-shared`, `tt-routing`, `tt-plan-core`, `tt-cli`. No new deps.

**Repo / branch:** `/Users/iansimon/Developer/TokenTrimmer/public` on `feat/v3c-topic-routing` (off `main`). Spec: `docs/superpowers/specs/2026-06-04-v3c-topic-routing-design.md`.

**Test note:** `cargo test --workspace` hook-denied — scope with `-p`. Red = compile error referencing a not-yet-defined item.

**Verified anchors:**
- `tt_shared::capability_check` (`crates/shared/src/capability_check.rs`): private `extract_text(&MessageContent) -> String` (`:147`); `Message::{User{content,name?},System{content},Assistant{content:Option,..},Tool{content,..}}`; tests have a `base_req()` helper (`:241`).
- `tt_routing::matches` (`crates/routing/src/lib.rs`): AND-ed arms ending with `has_images`/`has_audio`, then `true`. `RouteConditions` derives `Default`; test helpers `make_route`/`make_req(model)`/`make_ctx`.
- `tt_plan_core::types::RouteConditions` mirror + `matches_conditions` (`crates/plan-core/src/routing.rs`): ends with the modality short-circuit (`if c.has_images.is_some() || c.has_audio.is_some() { return false; }`) then `true`. `RequestLog.body: Option<String>`.
- `tt route add` (`crates/cli/src/route/mod.rs`): `AddArgs` + `build_new_route`; clap `RouteAction::Add` in `main.rs`.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/shared/src/capability_check.rs` (modify) | `request_input_text` (user+system text) + test. |
| `crates/routing/src/lib.rs` (modify) | `prompt_contains_any_of` field + matcher arm + tests. |
| `crates/plan-core/src/types.rs` + `routing.rs` (modify) | mirror field + body-based matcher arm + test. |
| `crates/cli/src/route/mod.rs` + `main.rs` (modify) | `--when-prompt-contains` flag + mapping + tests. |

---

## Task 1: `request_input_text` helper (`tt-shared`)

**Files:** Modify `crates/shared/src/capability_check.rs`

- [ ] **Step 1: Write the failing test** — in the `#[cfg(test)] mod tests` block, add:

```rust
    #[test]
    fn request_input_text_user_and_system_only() {
        let mut req = base_req();
        req.messages = vec![
            Message::System { content: MessageContent::Text("sys ctx".into()) },
            Message::User { content: MessageContent::Text("Confidential matter".into()), name: None },
            Message::Assistant { content: Some(MessageContent::Text("legal advice".into())), tool_calls: vec![], name: None },
        ];
        let t = request_input_text(&req);
        assert!(t.contains("sys ctx"));
        assert!(t.contains("Confidential matter"));
        assert!(!t.contains("legal advice"), "assistant output must be excluded");
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-shared request_input_text` → FAIL (`cannot find function request_input_text`).

- [ ] **Step 3: Implement** — above the `#[cfg(test)]` module (near `request_has_images`), add:

```rust
/// Concatenated text of the **user + system** messages — the caller-controlled
/// input, used for content/topic routing. Assistant/tool turns are excluded so a
/// model's own output can't spuriously trigger a topic route.
pub fn request_input_text(req: &ChatCompletionRequest) -> String {
    req.messages
        .iter()
        .filter_map(|m| match m {
            Message::User { content, .. } | Message::System { content } => Some(extract_text(content)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-shared request_input_text` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/shared/src/capability_check.rs
git commit -m "feat(shared): request_input_text (user+system prompt text) for topic routing"
```

---

## Task 2: `prompt_contains_any_of` condition + matcher (`tt-routing`)

**Files:** Modify `crates/routing/src/lib.rs`

- [ ] **Step 1: Write the failing tests** — in the `#[cfg(test)] mod tests` block, extend the `tt_shared` import to include `MessageContent`/`Message` if not already present (the modality tests already import them), and add a text-request helper + tests:

```rust
    fn make_req_text(model: &str, text: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Text(text.into()),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    #[test]
    fn prompt_contains_matches_case_insensitive_any() {
        let route = Route {
            when: RouteConditions {
                prompt_contains_any_of: vec!["confidential".into(), "salary".into()],
                ..Default::default()
            },
            ..make_route("topic", 10, vec![], "local")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        // "Confidential" (different case) matches.
        assert!(eng
            .evaluate(&make_req_text("gpt-4o", "This is a Confidential memo"), &make_ctx(None), 100)
            .is_some());
        // Second keyword matches.
        assert!(eng
            .evaluate(&make_req_text("gpt-4o", "my SALARY is"), &make_ctx(None), 100)
            .is_some());
        // No keyword → no match.
        assert!(eng
            .evaluate(&make_req_text("gpt-4o", "the weather today"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn prompt_contains_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                prompt_contains_any_of: vec!["confidential".into()],
                ..Default::default()
            },
            ..make_route("both", 10, vec!["gpt-4o"], "local")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req_text("gpt-4o", "confidential"), &make_ctx(None), 100)
            .is_some());
        // model matches but keyword absent → no match.
        assert!(eng
            .evaluate(&make_req_text("gpt-4o", "hello"), &make_ctx(None), 100)
            .is_none());
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-routing prompt_contains` → FAIL (`RouteConditions` has no field `prompt_contains_any_of`).

- [ ] **Step 3: Add the field** — in `RouteConditions` (after `has_audio`):

```rust
    /// Match if the request's user+system text contains ANY of these keywords
    /// (case-insensitive substring). Empty = ignore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_contains_any_of: Vec<String>,
```

- [ ] **Step 4: Add the matcher arm** — in `matches()`, immediately before the final `true`:

```rust
    if !c.prompt_contains_any_of.is_empty() {
        let text = tt_shared::capability_check::request_input_text(req).to_lowercase();
        if !c
            .prompt_contains_any_of
            .iter()
            .any(|kw| text.contains(&kw.to_lowercase()))
        {
            return false;
        }
    }
```

- [ ] **Step 5: Run to verify it passes** — `cargo test -p tt-routing` → PASS (existing + the 2 new). If any non-test `RouteConditions { … }` literal errors (missing field), it uses an explicit construction — add `prompt_contains_any_of: vec![],`; the `..Default::default()` ones are unaffected.

- [ ] **Step 6: Commit**

```bash
git add crates/routing/src/lib.rs
git commit -m "feat(routing): prompt_contains_any_of condition (case-insensitive topic match)"
```

---

## Task 3: Plan-core mirror (`tt-plan-core`)

**Files:** Modify `crates/plan-core/src/types.rs`, `crates/plan-core/src/routing.rs`

- [ ] **Step 1: Write the failing test** — in `crates/plan-core/src/routing.rs`'s `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn prompt_contains_matches_body_else_no_match() {
        let r = route(
            "topic",
            10,
            true,
            RouteConditions { prompt_contains_any_of: vec!["confidential".into()], ..Default::default() },
        );
        // No body → conservative no-match.
        assert!(match_route(&req("m", 1, None), &[r.clone()]).is_none());
        // Body containing the keyword (case-insensitive) → match.
        let mut with_body = req("m", 1, None);
        with_body.body = Some("This is CONFIDENTIAL".into());
        assert!(match_route(&with_body, &[r]).is_some());
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-plan-core prompt_contains` → FAIL (no field `prompt_contains_any_of`).

- [ ] **Step 3: Add the mirror field** — in `crates/plan-core/src/types.rs`, in `RouteConditions` (after `has_audio`):

```rust
    /// Mirror of `tt_routing::RouteConditions::prompt_contains_any_of`. Replay
    /// matches against `RequestLog.body` when present, else conservative no-match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_contains_any_of: Vec<String>,
```

- [ ] **Step 4: Add the matcher arm** — in `crates/plan-core/src/routing.rs`, in `matches_conditions`, immediately before the final `true` (after the modality short-circuit):

```rust
    if !c.prompt_contains_any_of.is_empty() {
        let Some(body) = &req.body else {
            return false;
        };
        let text = body.to_lowercase();
        if !c
            .prompt_contains_any_of
            .iter()
            .any(|kw| text.contains(&kw.to_lowercase()))
        {
            return false;
        }
    }
```

- [ ] **Step 5: Run to verify it passes** — `cargo test -p tt-plan-core` → PASS (incl. the snapshot — empty list omitted, snapshot unchanged). If any explicit `RouteConditions { … }` literal errors, add `prompt_contains_any_of: vec![],`.

- [ ] **Step 6: Commit**

```bash
git add crates/plan-core/src/types.rs crates/plan-core/src/routing.rs
git commit -m "feat(plan-core): mirror prompt_contains_any_of (body-based, conservative no-match)"
```

---

## Task 4: CLI `--when-prompt-contains`

**Files:** Modify `crates/cli/src/route/mod.rs`, `crates/cli/src/main.rs`

- [ ] **Step 1: Write the failing tests** — in `crates/cli/src/route/mod.rs`'s `#[cfg(test)] mod tests`, add (and add `when_prompt_contains: vec![],` to the three existing `AddArgs { … }` literals):

```rust
    #[test]
    fn when_prompt_contains_maps_to_condition() {
        let body = build_new_route(&AddArgs {
            always: Some("ollama/llama3".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_prompt_contains: vec!["confidential".into(), "salary".into()],
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert_eq!(body["when"]["prompt_contains_any_of"], serde_json::json!(["confidential", "salary"]));
    }

    #[test]
    fn when_prompt_contains_omitted_when_empty() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o".into()), from: None, to: None,
            when_has_images: false, when_has_audio: false, when_prompt_contains: vec![],
            priority: 100, name: None, fallback: vec![], disabled: false,
        })
        .unwrap();
        assert!(body["when"].get("prompt_contains_any_of").is_none());
    }
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-cli route` → FAIL (`AddArgs` has no field `when_prompt_contains`).

- [ ] **Step 3: Implement** — in `crates/cli/src/route/mod.rs`, add to `AddArgs` (after `when_has_audio`):

```rust
    pub when_prompt_contains: Vec<String>,
```

In `build_new_route`, after the `when_has_audio` block:

```rust
    if !args.when_prompt_contains.is_empty() {
        when.insert("prompt_contains_any_of".into(), json!(args.when_prompt_contains));
    }
```

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-cli route` → PASS.

- [ ] **Step 5: Wire the clap arg + dispatch** — in `crates/cli/src/main.rs`, in `enum RouteAction`'s `Add { … }` (after `when_has_audio`):

```rust
        /// Match only requests whose prompt contains this keyword (repeatable).
        #[arg(long)]
        when_prompt_contains: Vec<String>,
```

In the `Command::Route` dispatch's `RouteAction::Add { … }` destructure + `AddArgs { … }` construction, add `when_prompt_contains,` to both.

- [ ] **Step 6: Build + smoke + commit**

Run: `cargo build -p tt-cli && ./target/debug/tt route add --help | grep when-prompt-contains`
Expected: the flag is listed. Then:

```bash
git add crates/cli/src/route/mod.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt route add --when-prompt-contains"
```

---

## Task 5: Final verification

**Files:** none.

- [ ] **Step 1: Format** — `cargo fmt -p tt-shared -p tt-routing -p tt-plan-core -p tt-cli`; then `git diff --quiet || git commit -am "style: cargo fmt (v3c)"`.
- [ ] **Step 2: Clippy** — `cargo clippy -p tt-shared -p tt-routing -p tt-plan-core -p tt-cli --all-targets -- -D warnings`. Expected: clean.
- [ ] **Step 3: Tests** — `cargo test -p tt-shared -p tt-routing -p tt-plan-core -p tt-cli`. Expected: all pass.
- [ ] **Step 4: Clean tree** — `git status` + `git log --oneline -8`.

---

## Self-Review (completed by plan author)

**1. Spec coverage:** `request_input_text` (user+system) → Task 1; `prompt_contains_any_of` + case-insensitive contains-any matcher → Task 2; plan-core mirror w/ body-based conservative match → Task 3; `--when-prompt-contains` → Task 4. Out-of-scope (word-boundary/regex/semantic, assistant scanning, dashboard) untouched.

**2. Placeholder scan:** every step has complete code/commands + expected output. Construction-churn handled with an explicit "if an explicit literal errors, add `prompt_contains_any_of: vec![]`" note (the `..Default::default()` literals are unaffected).

**3. Type consistency:** `request_input_text(&ChatCompletionRequest) -> String` (Task 1) called in the `tt_routing` matcher (Task 2). `prompt_contains_any_of: Vec<String>` identical across `tt_routing` (Task 2), `tt_plan_core` (Task 3), and the CLI JSON key (Task 4). `AddArgs.when_prompt_contains: Vec<String>` (Task 4) matches the clap arg + dispatch.
