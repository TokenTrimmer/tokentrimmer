# Provider-adapter correctness batch — Design

**Status:** approved (design)
**Date:** 2026-06-08
**Slice:** Audit-remediation (public repo, `crates/providers/{gemini,anthropic,local}`). Closes four low-severity provider-adapter findings + corrects one mis-marked checklist item.

## Background (verified against current code)
1. **Gemini usage not reconciled** (`gemini/src/translate.rs:784` `translate_usage`): copies `prompt_token_count`/`candidates_token_count`/`total_token_count` straight from `GeminiUsageMetadata` (all `#[serde(default)]` → 0). When Gemini omits `candidatesTokenCount`/`totalTokenCount` on partial responses, the invariant `total == prompt + completion` silently breaks → skews `compute_cost`. The Anthropic adapter recomputes total from components; Gemini doesn't.
2. **Gemini multi-candidate streaming** (`gemini/src/stream.rs:251`): `for candidate in event.candidates { … }` emits every candidate as `index: 0` with a single shared `*first_chunk` flag. If Gemini ever returns >1 candidate they collapse/corrupt onto choice 0. `n>1` is dropped upstream for Gemini (`dropped_params`), so this is latent — but the code structurally can't represent multiple candidates.
3. **Anthropic cache_control byte miscount** (`anthropic/src/translate.rs:291`): `let estimated_tokens = last.text.len() / 4` uses **byte** length. Multibyte (CJK) over-counts, so a sub-1024-token block can trip the `>= 1024` gate, attaching `cache_control` to a block below Anthropic's per-model minimum → Anthropic returns **400**.
4. **LocalProvider drops `dropped_params`** (`local/src/lib.rs:141-191`): the `impl Provider for LocalProvider` overrides id/models/pricing/chat/stream/embeddings but **not** `dropped_params`, so it falls to the trait default (empty `Vec`). The `X-TokenTrimmer-Warnings: param_dropped:*` warnings the inner compat layer would surface are lost for local backends — an observability regression vs groq/together/mistral, which delegate `self.inner.dropped_params(req)` (groq/src/lib.rs:91-93).

`GeminiUsageMetadata` fields: `prompt_token_count`, `candidates_token_count`, `total_token_count`, `cached_content_token_count`. `Provider::dropped_params(&self, req: &ChatCompletionRequest) -> Vec<String>` (provider.rs:39, default empty).

**Already resolved (not a fix — a checklist correction):** the section's "Model name interpolated into Gemini URL path without validation" finding is closed — `translate::validate_model_id(&model)?` runs **before** the URL build at `gemini/src/lib.rs:156` and `stream.rs:72` (shipped in #65). The checklist marks it `🔴 OPEN` in error.

## Decision (user-approved)
Fix the four low bugs in one cohesive provider-correctness batch; flip the mis-marked Gemini-URL item to ✅ #65.

## Architecture

### 1. `gemini/src/translate.rs` — reconcile usage
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

### 2. `gemini/src/stream.rs` — process only the first candidate
Change the loop header (line 251) to enumerate and skip extras:
```rust
    for (idx, candidate) in event.candidates.into_iter().enumerate() {
        if idx > 0 {
            tracing::debug!(
                "gemini stream: ignoring extra candidate #{idx} — this gateway is \
                 single-candidate (n>1 is dropped for Gemini)"
            );
            continue;
        }
        // …existing body UNCHANGED…
    }
```
(The single-candidate path is unchanged; only a hypothetical 2nd+ candidate is skipped + logged instead of corrupting choice 0.)

### 3. `anthropic/src/translate.rs` — char-count heuristic
```rust
    if let Some(last) = system_blocks.last_mut() {
        // char count, not byte len: multibyte (CJK) over-counts bytes and could
        // push a sub-1024-token block over the gate, which Anthropic 400s.
        let estimated_tokens = last.text.chars().count() / 4;
        if estimated_tokens >= 1024 {
            last.cache_control = Some(AnthropicCacheControl {
                ctype: "ephemeral".to_string(),
            });
        }
    }
```
(Scope: fix the multibyte miscount. The "which block to cache" cost-optimization refinement stays last-block, as today.)

### 4. `local/src/lib.rs` — forward `dropped_params`
In `impl Provider for LocalProvider`, add (mirroring groq/together/mistral):
```rust
    fn dropped_params(&self, req: &tt_shared::ChatCompletionRequest) -> Vec<String> {
        self.inner.dropped_params(req)
    }
```
(Confirm the exact import path for `ChatCompletionRequest` matches the file's existing `use` — the chat methods already take `ChatCompletionRequest`, so use the same path, qualified or not, consistently.)

### 5. Checklist correction (`docs/reviews/2026-06-06-audit-checklist.md`)
Flip the two entries for "Model name interpolated into Gemini URL path without validation/encoding" (the `pub-providers-a` body item ~line 252 and its priority-queue twin if present ~line 111) from `🔴 OPEN` to `✅ DONE in #65 (2026-06-08)` — `validate_model_id` is called before both URL builds. Status-suffix change only; leave the Where/Issue/Action detail intact.

## Error handling
- Gemini usage reconciliation only fills gaps (never reduces a provided value); a fully-populated `usageMetadata` is unchanged.
- The Gemini stream skip emits a `debug!` (not an error) — extra candidates are silently dropped at the wire-format level (they can't be represented), which is the documented single-candidate assumption.
- Anthropic: the char-count change only narrows when `cache_control` is attached (fewer false-positives) — never a new failure.
- LocalProvider: `dropped_params` delegation can't fail.

## Testing
- **`gemini/src/translate.rs`** (unit): `translate_usage` — `{prompt:10, candidates:0, total:25}` → completion 15, total 25; `{prompt:10, candidates:5, total:0}` → total 15; `{prompt:10, candidates:5, total:15}` → unchanged; all-zero → all zero.
- **`gemini/src/stream.rs`** (unit, mirror an existing stream test): feed an event JSON with **two** candidates → the handler yields outcomes for only the **first** (assert the emitted chunk count / content matches candidate 0, not a merge). A single-candidate event behaves as before.
- **`anthropic/src/translate.rs`** (unit): a system message of ~1100 **multibyte** chars whose `byte_len/4 ≥ 1024` but `chars/4 < 1024` (e.g. ~1100 CJK chars) → `system_blocks.last()` has **no** `cache_control`; an ASCII system message of ≥ 4096 chars → `cache_control` IS attached.
- **`local/src/lib.rs`** (unit): a `LocalProvider` whose inner reports a dropped param for a given request → `LocalProvider::dropped_params(req)` returns that param (not empty). (If constructing an inner with a non-empty `dropped_params` is awkward, at minimum assert the method delegates — e.g. equals `self.inner.dropped_params(req)` for a request — mirroring whatever the sibling adapters test, or keep it covered by the delegation being a one-liner + the gateway warnings integration tests.)

Gates (public repo, scoped per ADR-012): `cargo test -p tt-provider-gemini -p tt-provider-anthropic -p tt-provider-local`; **`cargo fmt --check`** on those crates; `cargo clippy` on those crates `--all-targets -- -D warnings` clean. No public-signature change (all additive/internal) — no workspace ripple.

## Out of scope
- The medium DX "two near-duplicate OpenAI-compatible bases" consolidation (a refactor — separate slice).
- The local-SSRF write-time credential validation (cloud-deferred `rv-ssrf-write-validate`; the public `allow_local` skip is the load-bearing gap, tracked for the cloud repo).
- Anthropic per-model minimum-cacheable-size (Haiku=2048) awareness — would require threading the model into this heuristic; the char-count fix already removes the multibyte false-positive. Noted.
- `fee_multiplier` on LocalProvider (harmless — local pricing is zero).
