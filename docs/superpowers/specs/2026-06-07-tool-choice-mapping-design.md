# tool_choice mapping fix (Anthropic + Gemini) — Design

**Status:** approved (design)
**Date:** 2026-06-07
**Slice:** Audit-remediation Wave 4 (public repo, `crates/providers/{anthropic,gemini}`). Closes the finding *"tool_choice 'required' silently downgraded to auto on Anthropic and Gemini; 'none' ignored on Anthropic"* (`pub-providers-a`).

## Background (verified against current code + Anthropic docs)
Canonical tool choice (`crates/shared/src/messages.rs:226`):
```rust
#[serde(untagged)]
pub enum ToolChoice {
    Auto(String),                                   // OpenAI string: "auto" | "none" | "required"
    Specific { r#type: String, function: ToolChoiceFunction },
}
```

**Anthropic** (`crates/providers/anthropic/src/translate.rs`):
- `AnthropicToolChoice` (line 143, `#[serde(tag = "type", rename_all = "snake_case")]`) has `Auto`, `Any`, `Tool { name }`. `Any` (→ `{"type":"any"}`) is **defined but never constructed** — dead code.
- `translate_tool_choice` (line 403): `Auto(s) if s == "none"` → `Auto`; `Auto(_)` (incl. `"required"`) → `Auto`; `Specific` → `Tool`. So both `none` and `required` collapse to `auto`.
- Verified against current Anthropic Messages API docs: `tool_choice` supports `{"type":"auto"}`, `{"type":"any"}` (force a tool), `{"type":"tool","name":…}`, and `{"type":"none"}` ("the model will not be allowed to use tools"). So a `None` variant serializing `{"type":"none"}` is the correct, supported mapping for `"none"`.

**Gemini** (`crates/providers/gemini/src/translate.rs`):
- `GeminiFunctionCallingConfig { mode: String, allowed_function_names: Vec<String> }`; modes `"AUTO"`/`"ANY"`/`"NONE"`.
- `translate_tool_choice` (line 628): `Auto(s) if s == "none"` → `NONE` ✓; `Auto(_)` (incl. `"required"`) → `AUTO` ✗; `Specific` → `ANY` + allowlist ✓. So `required` wrongly maps to `AUTO`.

**Compat** (`crates/providers/compat/src/translate.rs:114`) assigns `tool_choice: req.tool_choice` — passes the canonical value through verbatim in OpenAI-native format, where `required`/`none` are already correct. **Out of scope** (verified).

## Decision (user-approved)
Match all three OpenAI strings explicitly in both adapters: `"none"` → suppress tools, `"required"` → force a tool, `"auto"` (and any unrecognized string) → auto.

## Architecture

### Anthropic — `crates/providers/anthropic/src/translate.rs`
1. Add a `None` variant to `AnthropicToolChoice` (serializes to `{"type":"none"}` via the existing serde tag):
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
2. Rewrite `translate_tool_choice`:
```rust
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

### Gemini — `crates/providers/gemini/src/translate.rs`
Add a `"required"` arm before the `Auto(_)` fallback:
```rust
fn translate_tool_choice(choice: ToolChoice) -> GeminiToolConfig {
    match choice {
        ToolChoice::Auto(s) if s == "none" => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "NONE".to_string(),
                allowed_function_names: vec![],
            },
        },
        ToolChoice::Auto(s) if s == "required" => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "ANY".to_string(),          // ANY + empty allowlist = must call some provided fn
                allowed_function_names: vec![],
            },
        },
        ToolChoice::Auto(_) => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "AUTO".to_string(),
                allowed_function_names: vec![],
            },
        },
        ToolChoice::Specific { function, .. } => GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: "ANY".to_string(),
                allowed_function_names: vec![function.name],
            },
        },
    }
}
```

## Error handling
- Unrecognized `ToolChoice::Auto(s)` strings fall through to `auto`/`AUTO` — the current safe default, unchanged.
- `"any"` is NOT treated as an alias for `"required"` (canonical semantics are OpenAI `auto`/`none`/`required`). Noted, not added (YAGNI).
- No new failure modes: both functions are total over `ToolChoice` and infallible (return the provider type directly).

## Testing
Add tests for the currently-untested paths (the existing `auto` + `specific` tests stay green). Match each adapter's existing location + style:
- **Anthropic** — in-file `#[cfg(test)] mod tests` (next to `tool_choice_auto_string_translates` / `tool_choice_specific_translates` at translate.rs:659–680). Add:
  - `tool_choice "required"` → `AnthropicToolChoice::Any` (and, since it's `Serialize`, optionally assert it serializes to `{"type":"any"}`).
  - `tool_choice "none"` → `AnthropicToolChoice::None` (optionally serializes to `{"type":"none"}`).
  (Follow the existing tests' pattern: build a request with `tool_choice = Some(ToolChoice::Auto("required"/"none"))`, run `translate_request`, match `body.tool_choice`.)
- **Gemini** — integration tests in `crates/providers/gemini/tests/translate.rs` (alongside `translate_tool_choice_auto` :357, `_specific_function` :374, `_none` :399). `_none` ALREADY exists and is correct — leave it. Add only:
  - `translate_tool_choice_required` → `mode == "ANY"` with empty `allowed_function_names` (mirror how `_auto`/`_none` reach the config — via `translate_request` and inspecting `tool_config`).

Gates (public repo, scoped per ADR-012): `cargo test -p tt-provider-anthropic -p tt-provider-gemini`; **`cargo fmt --check -p tt-provider-anthropic -p tt-provider-gemini`** (public CI gates fmt — the recurring miss); `cargo clippy -p tt-provider-anthropic -p tt-provider-gemini --all-targets -- -D warnings` clean (`AnthropicToolChoice` is a `pub enum`, so `Any` was not dead-code-linted; this change just starts actually constructing it).

## Out of scope
- Compat / OpenAI-native adapters (`tool_choice` passes through verbatim, already correct).
- Adding `"any"` as a `required` alias.
- Any change to the canonical `ToolChoice` type or to how callers/cache/capability layers read it.
- The streaming or response-side tool handling (this is request-translation only).
