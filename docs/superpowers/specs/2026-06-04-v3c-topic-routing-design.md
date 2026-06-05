# Design: V3c — Topic/keyword routing (`prompt_contains_any_of`)

_Date: 2026-06-04 · Status: approved design, pre-implementation · Repo: `public` (gateway `tt-routing`, `tt-shared`, `tt-plan-core`, `tt` CLI)_

> Third slice of **V3 — Routing overhaul**. Routes by **what the prompt is about**:
> a new condition matches when the request's input text contains any of a set of
> keywords (case-insensitive substring). Composes with the rest of V3 — e.g. route
> legally-sensitive prompts to a local model and skip the cache:
> `tt route add --when-prompt-contains confidential --to ollama/llama3 --disable-cache`.

## Problem

Routing today matches on `model_in`, token counts, `tag_equals` (an explicit
header), and input modality (V3a-1). There's no way to route on **content** — "send
anything mentioning `attorney-client` / `diagnosis` / `salary` to model X (or to
local, uncached)." The explicit `X-TokenTrimmer-Tag` only works when the caller
remembers to set it; content-based routing detects the topic from the prompt itself.

## Current state (verified 2026-06-04)

- `tt_routing::RouteConditions` (`crates/routing/src/lib.rs`): `model_in`,
  `input_tokens_lt/gt`, `tag_equals`, `has_images`, `has_audio` (all
  `#[serde(default)]`; derives `Default`). `matches()` AND-es them. The crate-doc
  notes to extend the `tt_plan_core::types::RouteConditions` mirror in lockstep.
- `tt_shared::capability_check` (`crates/shared/src/capability_check.rs`): has
  `request_has_images`/`request_has_audio`, `content_of`, and the private
  `extract_text(&MessageContent) -> String`; `message_text_for_estimation` extracts
  **all** message text. `Message::{User{content},System{content},Assistant{content:Option},Tool{content}}`.
- `tt_plan_core::types::RouteConditions` mirrors the fields; `matches_conditions`
  (`routing.rs`) replays against `RequestLog`, which has `body: Option<String>`
  (the raw prompt, populated only when body-logging is opted in).
- `tt route add` (`crates/cli/src/route/mod.rs`): `build_new_route` maps flags to
  the `when`/`then` JSON; `--when-tag` / `--when-has-images` already exist.

## Goals / non-goals

**Goals:** a `prompt_contains_any_of: Vec<String>` condition; the gateway matcher
scans the request's **user + system** text and matches if it contains **any**
keyword (case-insensitive substring); plan-core mirror (matches `RequestLog.body`
when present, else conservative no-match); `tt route add --when-prompt-contains`
(repeatable).

**Non-goals:** word-boundary / regex / **semantic** matching (substring MVP — the
user chose this); scanning assistant/tool output (user+system input only);
dashboard exposure (cloud follow-up); per-org keyword lists.

## Design

### 1. Input-text helper (`tt_shared::capability_check`)

```rust
/// Concatenated text of the **user + system** messages — the caller-controlled
/// input. Used for content/topic routing.
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
(Reuses the existing private `extract_text`; assistant/tool turns are excluded so a
model's own output can't spuriously trigger a topic route.)

### 2. Condition + matcher (`tt_routing`)

Add to `RouteConditions`:
```rust
/// Match if the request's user+system text contains ANY of these keywords
/// (case-insensitive substring). Empty = ignore.
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub prompt_contains_any_of: Vec<String>,
```
Matcher arm in `matches()` (after the modality arms):
```rust
if !c.prompt_contains_any_of.is_empty() {
    let text = tt_shared::capability_check::request_input_text(req).to_lowercase();
    if !c.prompt_contains_any_of.iter().any(|kw| text.contains(&kw.to_lowercase())) {
        return false;
    }
}
```

### 3. Plan-core mirror (`tt_plan_core`)

Add the same field (lockstep). `matches_conditions` scans `RequestLog.body` when
present, else conservative no-match (body logging is opt-in — same honesty stance
as the modality limitation):
```rust
if !c.prompt_contains_any_of.is_empty() {
    let Some(body) = &req.body else { return false };
    let text = body.to_lowercase();
    if !c.prompt_contains_any_of.iter().any(|kw| text.contains(&kw.to_lowercase())) {
        return false;
    }
}
```

### 4. CLI (`tt route`)

`tt route add --when-prompt-contains <kw>` (repeatable → `Vec<String>`); maps to
`when.prompt_contains_any_of`. Omitted when empty.

## Data flow

A request whose user/system text contains e.g. `confidential` →
`request_input_text` lowercased → matcher's contains-any → the route fires
(rewrite + any `disable_cache`/local target) exactly like the other conditions.

## Error handling

No validation needed — keywords are free-form strings; an empty list is ignored.
Existing same-provider/capability validation on the action is unchanged.

## Testing (TDD; scoped `cargo test -p <crate>`)

- `tt-shared`: `request_input_text` (user+system concatenated; assistant/tool
  excluded; `Parts` text extracted; empty for no text).
- `tt-routing`: matcher matrix — keyword present (case-insensitive) → match; absent
  → no match; multiple keywords (any); empty list ignored; AND-ed with `model_in`.
- `tt-plan-core`: field parses; matches against `RequestLog.body` when present;
  no-match when `body` is `None`.
- `tt-cli`: `--when-prompt-contains` (repeated) → `when.prompt_contains_any_of`;
  omitted when empty.

## Success criteria

- A route with `prompt_contains_any_of: ["confidential"]` fires for a request whose
  prompt contains "Confidential" (case-insensitive) and not otherwise; AND-s with
  other conditions; empty list is a no-op.
- `tt route add --when-prompt-contains confidential --when-prompt-contains salary …`
  creates it. Plan-core stays in lockstep; existing routing/plan tests unchanged.

## Out of scope (restated)

Word-boundary / regex / semantic matching; assistant/tool-text scanning; dashboard
exposure (cloud follow-up); per-org keyword config; capturing prompt text in
`request_logs` for non-opted-in orgs (Plan projection limitation noted).
