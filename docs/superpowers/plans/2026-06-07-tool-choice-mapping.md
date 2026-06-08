# tool_choice mapping fix (Anthropic + Gemini) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Honor `tool_choice` `"required"` and `"none"` on the Anthropic and Gemini adapters (today both collapse to auto / required→AUTO), so an explicit caller instruction to force or suppress tool use is faithfully translated.

**Architecture:** Add a `None` variant to `AnthropicToolChoice` and rewrite both adapters' `translate_tool_choice` to match `"none"`/`"required"`/`"auto"` explicitly (Anthropic: `None`/`Any`/`Auto`; Gemini: `NONE`/`ANY`/`AUTO`). Request-translation only; the canonical `ToolChoice` type and the compat adapter (already correct) are untouched.

**Tech Stack:** Rust (`crates/providers/anthropic` = `tt-provider-anthropic`, `crates/providers/gemini` = `tt-provider-gemini`), serde.

Spec: `docs/superpowers/specs/2026-06-07-tool-choice-mapping-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`** — run it before committing (recurring miss). One cohesive fix across two small adapter files; do not restructure. Anthropic-docs-verified: `tool_choice {"type":"none"}` disables tool use, `{"type":"any"}` forces a tool.

---

### Task 1: Honor required/none in Anthropic + Gemini tool_choice translation

**Files:**
- Modify: `crates/providers/anthropic/src/translate.rs` (`AnthropicToolChoice` enum + `translate_tool_choice` + in-file tests)
- Modify: `crates/providers/gemini/src/translate.rs` (`translate_tool_choice`)
- Modify: `crates/providers/gemini/tests/translate.rs` (add a `required` test)

- [ ] **Step 1: Write the failing Anthropic tests**

In `crates/providers/anthropic/src/translate.rs`, inside `#[cfg(test)] mod tests`, after `tool_choice_specific_translates` (~line 682), add:
```rust
    #[test]
    fn tool_choice_required_translates_to_any() {
        use tt_shared::messages::ToolChoice;
        let mut req = base_request("claude-sonnet-4-6");
        req.tool_choice = Some(ToolChoice::Auto("required".to_string()));
        let body = translate_request(req).expect("translate ok");
        assert!(matches!(body.tool_choice, Some(AnthropicToolChoice::Any)));
        // Wire format: required → {"type":"any"}.
        let v = serde_json::to_value(body.tool_choice.unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({ "type": "any" }));
    }

    #[test]
    fn tool_choice_none_translates_to_none() {
        use tt_shared::messages::ToolChoice;
        let mut req = base_request("claude-sonnet-4-6");
        req.tool_choice = Some(ToolChoice::Auto("none".to_string()));
        let body = translate_request(req).expect("translate ok");
        assert!(matches!(body.tool_choice, Some(AnthropicToolChoice::None)));
        // Wire format: none → {"type":"none"} (Anthropic "disable all tool use").
        let v = serde_json::to_value(body.tool_choice.unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({ "type": "none" }));
    }
```

- [ ] **Step 2: Run to confirm they fail to compile**

Run: `cargo test -p tt-provider-anthropic tool_choice 2>&1 | tail -15`
Expected: FAIL — no variant `AnthropicToolChoice::None` (and `Any` is not yet produced).

- [ ] **Step 3: Add the `None` variant + rewrite Anthropic `translate_tool_choice`**

In `crates/providers/anthropic/src/translate.rs`, add a `None` variant to `AnthropicToolChoice` (the enum at ~line 143, `#[serde(tag = "type", rename_all = "snake_case")]`):
```rust
pub enum AnthropicToolChoice {
    /// Let the model decide.
    Auto,
    /// Force any available tool.
    Any,
    /// Disable all tool use.
    None,
    /// Force a specific tool by name.
    Tool { name: String },
}
```
Then replace `translate_tool_choice` (~line 403) with:
```rust
/// Convert a canonical [`ToolChoice`] to an [`AnthropicToolChoice`].
fn translate_tool_choice(choice: ToolChoice) -> AnthropicToolChoice {
    match choice {
        ToolChoice::Auto(s) if s == "none" => AnthropicToolChoice::None,
        ToolChoice::Auto(s) if s == "required" => AnthropicToolChoice::Any,
        ToolChoice::Auto(_) => AnthropicToolChoice::Auto, // "auto" + any unknown string
        ToolChoice::Specific { function, .. } => AnthropicToolChoice::Tool {
            name: function.name,
        },
    }
}
```

- [ ] **Step 4: Run the Anthropic tests (expect PASS)**

Run: `cargo test -p tt-provider-anthropic tool_choice 2>&1 | tail -15`
Expected: PASS — `tool_choice_auto_string_translates`, `tool_choice_specific_translates`, `tool_choice_required_translates_to_any`, `tool_choice_none_translates_to_none` all green.

- [ ] **Step 5: Write the failing Gemini test**

In `crates/providers/gemini/tests/translate.rs`, after `translate_tool_choice_none` (~line 412), add (NOTE: no `insta` snapshot — assert the config fields directly so no `.snap` acceptance is needed):
```rust
#[test]
fn translate_tool_choice_required() {
    let mut req = make_request("gemini-3.1-pro", vec![user_text("You must use a tool.")]);
    req.tool_choice = Some(ToolChoice::Auto("required".to_string()));

    let body = translate_request(req).expect("translate ok");

    let tc = body
        .tool_config
        .as_ref()
        .expect("toolConfig should be present");
    assert_eq!(tc.function_calling_config.mode, "ANY");
    assert!(tc.function_calling_config.allowed_function_names.is_empty());
}
```

- [ ] **Step 6: Run to confirm it fails**

Run: `cargo test -p tt-provider-gemini translate_tool_choice_required 2>&1 | tail -15`
Expected: FAIL — assertion: `mode` is `"AUTO"`, not `"ANY"` (required currently falls into the `Auto(_)` → AUTO arm).

- [ ] **Step 7: Add the `required` arm to Gemini `translate_tool_choice`**

In `crates/providers/gemini/src/translate.rs`, in `translate_tool_choice` (~line 628), add a `"required"` arm between the `"none"` arm and the `Auto(_)` fallback:
```rust
        ToolChoice::Auto(s) if s == "required" => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "ANY".to_string(), // ANY + empty allowlist = must call some provided fn
                allowed_function_names: vec![],
            },
        },
```
Leave the `"none"`→NONE, `Auto(_)`→AUTO, and `Specific`→ANY+allowlist arms unchanged.

- [ ] **Step 8: Run the Gemini test (expect PASS)**

Run: `cargo test -p tt-provider-gemini translate_tool_choice 2>&1 | tail -15`
Expected: PASS — `translate_tool_choice_auto`, `_specific_function`, `_none`, `_required` all green (the first three are snapshot tests and must remain unchanged).

- [ ] **Step 9: Full gates on both crates (fmt is the recurring CI miss)**

Run: `cargo test -p tt-provider-anthropic -p tt-provider-gemini 2>&1 | tail -15` → all pass.
Run: `cargo fmt --check -p tt-provider-anthropic -p tt-provider-gemini 2>&1 | tail -5` → no diff. If drift, run `cargo fmt -p tt-provider-anthropic -p tt-provider-gemini` then re-check.
Run: `cargo clippy -p tt-provider-anthropic -p tt-provider-gemini --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | head` → none (ignore a benign `failed to auto-clean cache data` line).

- [ ] **Step 10: Commit (stage only the three files)**

```bash
git add crates/providers/anthropic/src/translate.rs crates/providers/gemini/src/translate.rs crates/providers/gemini/tests/translate.rs
git commit -m "fix(providers): honor tool_choice required/none on Anthropic + Gemini

Anthropic: none -> {type:none}, required -> {type:any} (was: both -> auto).
Gemini: required -> mode ANY (was: AUTO). Adds AnthropicToolChoice::None.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-provider-anthropic -p tt-provider-gemini 2>&1 | tail -10
cargo fmt --check -p tt-provider-anthropic -p tt-provider-gemini
cargo clippy -p tt-provider-anthropic -p tt-provider-gemini --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | head
```
All green / no output. **Stage only the three changed files** (the working tree also carries an unrelated stale `docs/reviews/...audit-checklist.md` edit + a `rust_out` junk file — do NOT stage them).

## Notes for the implementer
- Anthropic `AnthropicToolChoice` uses `#[serde(tag = "type", rename_all = "snake_case")]`, so `None` serializes to `{"type":"none"}` and the now-constructed `Any` to `{"type":"any"}` — both verified against current Anthropic Messages API docs.
- Gemini `ANY` with an EMPTY `allowed_function_names` means "must call one of the provided functions" (force tool use). `ANY` with a non-empty allowlist (the `Specific` arm) restricts to named functions — leave that unchanged.
- The new Gemini test deliberately has NO `insta::assert_json_snapshot!` (the sibling tests do) — that avoids needing `cargo insta accept` + a committed `.snap` for this slice; the field assertions fully verify the behavior.
- Compat / OpenAI-native adapters are NOT touched — they forward `tool_choice` verbatim in OpenAI format where `required`/`none` are native.
- Unknown `ToolChoice::Auto(s)` strings still fall to `auto`/`AUTO` (unchanged safe default); `"any"` is not aliased to `required`.
