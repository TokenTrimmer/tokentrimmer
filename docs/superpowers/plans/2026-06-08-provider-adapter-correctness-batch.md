# Provider-adapter correctness batch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix four low-severity provider-adapter bugs — Gemini usage reconciliation, Gemini single-candidate streaming guard, Anthropic char-count cache heuristic, and LocalProvider `dropped_params` forwarding — and correct one mis-marked checklist item.

**Architecture:** Small, independent edits in three provider crates (gemini ×2, anthropic, local) plus a one-line status flip in the public checklist. All additive/internal — no signature changes.

**Tech Stack:** Rust (`crates/providers/{gemini,anthropic,local}`).

Spec: `docs/superpowers/specs/2026-06-08-provider-adapter-correctness-batch-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`.** No public-signature change → no workspace ripple; scope gates to the three provider crates.

---

### Task 1: Four adapter fixes + checklist correction

**Files:**
- Modify: `crates/providers/gemini/src/translate.rs` (`translate_usage` + test)
- Modify: `crates/providers/gemini/src/stream.rs` (candidate loop + test)
- Modify: `crates/providers/anthropic/src/translate.rs` (cache heuristic + test)
- Modify: `crates/providers/local/src/lib.rs` (`dropped_params`)
- Modify: `docs/reviews/2026-06-06-audit-checklist.md` (status flip)

#### Fix A — Gemini usage reconciliation

- [ ] **Step 1: Write the failing test**

In `crates/providers/gemini/src/translate.rs` `#[cfg(test)] mod tests`, add:
```rust
    #[test]
    fn translate_usage_reconciles_partial_metadata() {
        // candidatesTokenCount missing but total present → derive completion.
        let u = super::translate_usage(GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 0,
            total_token_count: 25,
            cached_content_token_count: 0,
        });
        assert_eq!(u.completion_tokens, 15);
        assert_eq!(u.total_tokens, 25);

        // totalTokenCount missing → total = prompt + completion.
        let u = super::translate_usage(GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 5,
            total_token_count: 0,
            cached_content_token_count: 0,
        });
        assert_eq!(u.total_tokens, 15);

        // Fully populated → unchanged.
        let u = super::translate_usage(GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 5,
            total_token_count: 15,
            cached_content_token_count: 2,
        });
        assert_eq!((u.prompt_tokens, u.completion_tokens, u.total_tokens, u.cached_tokens), (10, 5, 15, 2));
    }
```
(If `GeminiUsageMetadata` isn't in scope in the test module, add `use super::GeminiUsageMetadata;` or qualify it.)

- [ ] **Step 2: Run to confirm it fails**

Run: `cargo test -p tt-provider-gemini translate_usage_reconciles 2>&1 | tail -12`
Expected: FAIL — current `translate_usage` returns completion 0 / total 25 (no reconciliation).

- [ ] **Step 3: Reconcile in `translate_usage`**

Replace `translate_usage` (translate.rs:784):
```rust
pub fn translate_usage(u: GeminiUsageMetadata) -> Usage {
    let prompt = u.prompt_token_count;
    let mut completion = u.candidates_token_count;
    let mut total = u.total_token_count;
    // Gemini can omit candidatesTokenCount/totalTokenCount on partial responses.
    // Reconcile so `total == prompt + completion` (mirrors the Anthropic adapter).
    if completion == 0 && total > prompt {
        completion = total - prompt;
    }
    if total == 0 {
        total = prompt + completion;
    }
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        cached_tokens: u.cached_content_token_count,
        cache_creation_input_tokens: None,
    }
}
```

- [ ] **Step 4: Run to confirm it passes**

Run: `cargo test -p tt-provider-gemini translate_usage 2>&1 | tail -10`
Expected: PASS (new + any existing usage tests).

#### Fix B — Gemini single-candidate streaming guard

- [ ] **Step 5: Write the failing test**

In `crates/providers/gemini/src/stream.rs` `#[cfg(test)] mod tests` (mirroring `process_sse_event_with_finish_reason`), add:
```rust
    #[test]
    fn process_sse_event_ignores_extra_candidates() {
        // Two candidates: only the first (index 0) should be emitted; the second
        // must not collapse onto choice 0.
        let event = b"data: {\"candidates\":[\
            {\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"first\"}]},\"index\":0},\
            {\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"second\"}]},\"index\":1}]}\n\n";
        let mut first = false;
        let outcomes = process_sse_event(event, "id", 0, "gemini-3.1-pro", &mut first);
        // Exactly one content chunk, carrying the FIRST candidate's text.
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            SseOutcome::Chunk(c) => {
                assert_eq!(c.choices[0].delta.content.as_deref(), Some("first"));
            }
            other => panic!("expected one content chunk, got {other:?}"),
        }
    }
```
(`first = false` so the role-only first chunk is skipped — the content chunk is the only outcome. If `SseOutcome` isn't `Debug`, drop the `{other:?}` and `panic!("expected content chunk")`.)

- [ ] **Step 6: Run to confirm it fails**

Run: `cargo test -p tt-provider-gemini process_sse_event_ignores_extra 2>&1 | tail -12`
Expected: FAIL — current loop emits TWO content chunks (one per candidate), so `outcomes.len()` is 2.

- [ ] **Step 7: Guard the candidate loop**

In `crates/providers/gemini/src/stream.rs`, change the loop header (line 251) from `for candidate in event.candidates {` to:
```rust
    for (idx, candidate) in event.candidates.into_iter().enumerate() {
        if idx > 0 {
            tracing::debug!(
                "gemini stream: ignoring extra candidate #{idx} — this gateway is \
                 single-candidate (n>1 is dropped for Gemini)"
            );
            continue;
        }
```
Leave the loop body unchanged (it already binds `candidate`). Ensure the closing brace structure is intact.

- [ ] **Step 8: Run to confirm it passes**

Run: `cargo test -p tt-provider-gemini process_sse_event 2>&1 | tail -12`
Expected: PASS — the new test + all existing `process_sse_event_*` tests green.

#### Fix C — Anthropic char-count cache heuristic

- [ ] **Step 9: Write the failing test**

In `crates/providers/anthropic/src/translate.rs` `#[cfg(test)] mod tests`, add (mirror the existing `translate_request` tests — they build a request via `base_request(...)` and set messages; a system message becomes a `system` block):
```rust
    #[test]
    fn cache_control_uses_char_count_not_bytes() {
        use tt_shared::messages::{Message, MessageContent};
        // ~1400 CJK chars: byte_len/4 = 1050 (≥1024, old heuristic attaches) but
        // chars/4 = 350 (<1024, correct heuristic does NOT) — so a sub-1024-token
        // block must NOT get cache_control (Anthropic would 400 it).
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![Message::System {
            content: MessageContent::Text("中".repeat(1400)),
            name: None,
        }];
        let body = translate_request(req).expect("translate ok");
        let sys = body.system.expect("system blocks present");
        assert!(sys.last().unwrap().cache_control.is_none(), "must not cache a sub-1024-token block");

        // A genuinely long ASCII system prompt (≥4096 chars → ≥1024 est. tokens)
        // still gets cache_control.
        let mut req = base_request("claude-sonnet-4-6");
        req.messages = vec![Message::System {
            content: MessageContent::Text("a".repeat(4096)),
            name: None,
        }];
        let body = translate_request(req).expect("translate ok");
        assert!(body.system.unwrap().last().unwrap().cache_control.is_some());
    }
```
(Check the exact `Message::System` shape against the existing tests / `tt_shared::messages` — adjust the variant fields if `System` differs, e.g. no `name`. Mirror how a sibling test constructs a system message.)

- [ ] **Step 10: Run to confirm it fails**

Run: `cargo test -p tt-provider-anthropic cache_control_uses_char_count 2>&1 | tail -12`
Expected: FAIL — the 1400-CJK block currently gets `cache_control` (byte_len/4 = 1050 ≥ 1024), so the `is_none()` assert fails.

- [ ] **Step 11: Switch to char count**

In `crates/providers/anthropic/src/translate.rs` (~line 291), change:
```rust
        let estimated_tokens = last.text.len() / 4;
```
to:
```rust
        // char count, not byte len: multibyte (CJK) over-counts bytes and could
        // push a sub-1024-token block over the gate, which Anthropic 400s.
        let estimated_tokens = last.text.chars().count() / 4;
```

- [ ] **Step 12: Run to confirm it passes**

Run: `cargo test -p tt-provider-anthropic 2>&1 | tail -12`
Expected: PASS — the new test + all existing translate tests green.

#### Fix D — LocalProvider forwards dropped_params

- [ ] **Step 13: Forward `dropped_params`**

In `crates/providers/local/src/lib.rs`, in `impl Provider for LocalProvider`, add (matching groq/together/mistral, which delegate `self.inner.dropped_params(req)`):
```rust
    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        self.inner.dropped_params(req)
    }
```
(No dedicated test: this is a one-line passthrough identical to the sibling group-B adapters, whose passthroughs are likewise untested — the underlying logic is covered by `crates/providers/compat/src/translate.rs::dropped_params_temperature_only_for_reasoning_models`. Verified by compile + clippy.)

#### Checklist correction + gates + commit

- [ ] **Step 14: Flip the resolved Gemini-URL item in the checklist**

In `docs/reviews/2026-06-06-audit-checklist.md`, find the entries for "Model name interpolated into Gemini URL path without validation/encoding" (the `pub-providers-a` body item ~line 252, and its priority-queue twin ~line 111 if present). Change the `🔴 OPEN` status suffix on each to `✅ DONE in #65 (2026-06-08): validate_model_id runs before the URL build (lib.rs:156, stream.rs:72)` and flip the checkbox `- [ ]` → `- [x]`. Change ONLY the status suffix + checkbox; leave the Where/Issue/Action detail intact.

- [ ] **Step 15: Full gates on the three crates**

Run: `cargo test -p tt-provider-gemini -p tt-provider-anthropic -p tt-provider-local 2>&1 | tail -15` → all pass.
Run: `cargo fmt --check -p tt-provider-gemini -p tt-provider-anthropic -p tt-provider-local 2>&1 | tail -3` → no diff (if drift: `cargo fmt -p tt-provider-gemini -p tt-provider-anthropic -p tt-provider-local`, re-check).
Run: `cargo clippy -p tt-provider-gemini -p tt-provider-anthropic -p tt-provider-local --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean | head` → none.

- [ ] **Step 16: Commit (stage the five files)**

```bash
git add crates/providers/gemini/src/translate.rs crates/providers/gemini/src/stream.rs crates/providers/anthropic/src/translate.rs crates/providers/local/src/lib.rs docs/reviews/2026-06-06-audit-checklist.md
git commit -m "fix(providers): gemini usage reconcile + single-candidate stream, anthropic char-count cache, local dropped_params

- gemini translate_usage: derive completion/total when usageMetadata is partial
  (keeps total == prompt + completion, mirroring Anthropic).
- gemini stream: process only the first candidate (debug-log + skip extras) so a
  hypothetical n>1 can't collapse onto choice 0.
- anthropic auto cache_control: estimate tokens from chars(), not bytes — a
  multibyte block no longer trips the 1024 gate and gets a 400.
- LocalProvider: forward dropped_params to the inner (param_dropped warnings).
- checklist: mark the Gemini-URL-validation finding resolved (#65).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-provider-gemini -p tt-provider-anthropic -p tt-provider-local 2>&1 | tail -10
cargo fmt --check -p tt-provider-gemini -p tt-provider-anthropic -p tt-provider-local
cargo clippy -p tt-provider-gemini -p tt-provider-anthropic -p tt-provider-local --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean
```
All green / empty. **Stage only the five listed files** (the working tree also carries a `rust_out` junk file — do NOT stage it).

## Notes for the implementer
- The gemini usage reconciliation only fills gaps — a fully-populated `usageMetadata` is byte-identical to before.
- The gemini stream guard changes nothing for the normal single-candidate path; only a 2nd+ candidate is skipped + `debug!`-logged.
- The anthropic change only NARROWS when `cache_control` attaches (fewer multibyte false-positives) — never a new attach.
- LocalProvider `dropped_params` is a passthrough mirroring the other group-B adapters; the inner (`tt_provider_openai`/compat) computes the actual drops.
- If the `Message::System` variant fields differ from `{ content, name }`, mirror an existing anthropic translate test's system-message construction — don't guess.
