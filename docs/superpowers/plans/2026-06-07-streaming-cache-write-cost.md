# Streaming cache-write cost Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Price Anthropic `cache_creation_input_tokens` at the cache-write premium on the streaming path (matching non-streaming) and carry the field into the reconstructed cache entry.

**Architecture:** Capture `cache_creation_input_tokens` through `UsageTrackingStream` (4-tuple authoritative + new `PartialUsage.cache_creation_tokens`), then delete the parallel `compute_streaming_cost`/`compute_streaming_baseline` and delegate to the authoritative `chat::compute_cost` via a `PartialUsage → Usage` converter — eliminating the drift that caused the bug.

**Tech Stack:** Rust, `crates/core/src/routes/sse.rs` (+ reuse of `crates/core/src/routes/chat.rs::compute_cost`).

Spec: `docs/superpowers/specs/2026-06-07-streaming-cache-write-cost-design.md`

> **CI note (`ci-verify-all-targets`):** before the final push run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --no-run`. `cargo build` does not compile test targets.

---

### Task 1: Capture `cache_creation_input_tokens` through the streaming pipeline

**Files:**
- Modify: `crates/core/src/routes/sse.rs`

This task adds the field + capture and carries it into the cache entry. It compiles as a standalone increment: `compute_streaming_cost`/`compute_streaming_baseline` still exist and simply ignore the new field.

- [ ] **Step 1: Write the failing capture test**

Append inside the `#[cfg(test)] mod tests` block in `crates/core/src/routes/sse.rs` (before the closing `}` of the module):

```rust
    #[tokio::test]
    async fn usage_tracking_captures_cache_creation_tokens() {
        let chunks = vec![Ok(ChatCompletionChunk {
            id: "x".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: "m".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: Some("stop".into()),
            }],
            usage: Some(tt_shared::Usage {
                prompt_tokens: 100,
                completion_tokens: 10,
                total_tokens: 110,
                cached_tokens: 20,
                cache_creation_input_tokens: Some(30),
            }),
        })];
        let stream = futures::stream::iter(chunks).boxed();
        let mut tracker = UsageTrackingStream::new(stream, 100, 20, "anthropic");
        let _ = tracker.next().await;

        // snapshot carries the cache-creation count.
        let usage = tracker.snapshot();
        assert_eq!(usage.cache_creation_tokens, 30);

        // cache_completion_data carries it into the reconstructed Usage.
        let (_text, _fr, reconstructed) = tracker.cache_completion_data().unwrap();
        assert_eq!(reconstructed.cache_creation_input_tokens, Some(30));
    }
```

- [ ] **Step 2: Run to confirm it fails**

Run: `cargo test -p tt-core --lib routes::sse::tests::usage_tracking_captures_cache_creation_tokens 2>&1 | tail -15`
Expected: FAIL — compile errors: `PartialUsage` has no field `cache_creation_tokens`, and `reconstructed.cache_creation_input_tokens` is `None` (so the assert would fail even once the field exists). This is the expected red.

- [ ] **Step 3: Add the field to `PartialUsage`**

In `crates/core/src/routes/sse.rs`, change the struct (currently `input_tokens`/`output_tokens`/`cached_tokens`):

```rust
#[derive(Debug, Clone, Default)]
pub struct PartialUsage {
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cached_tokens: i32,
    pub cache_creation_tokens: i32,
}
```

- [ ] **Step 4: Widen the authoritative tuple + capture in `poll_next`**

Change the field declaration:
```rust
    /// Authoritative usage from the provider's terminal chunk.
    authoritative: Option<(i32, i32, i32)>,
```
to:
```rust
    /// Authoritative usage from the provider's terminal chunk:
    /// (prompt, completion, cached, cache_creation).
    authoritative: Option<(i32, i32, i32, i32)>,
```

In `poll_next`, change the capture:
```rust
            if let Some(ref usage) = chunk.usage {
                self.authoritative = Some((
                    usage.prompt_tokens as i32,
                    usage.completion_tokens as i32,
                    usage.cached_tokens as i32,
                ));
            }
```
to:
```rust
            if let Some(ref usage) = chunk.usage {
                self.authoritative = Some((
                    usage.prompt_tokens as i32,
                    usage.completion_tokens as i32,
                    usage.cached_tokens as i32,
                    usage.cache_creation_input_tokens.unwrap_or(0) as i32,
                ));
            }
```

- [ ] **Step 5: Update `snapshot()`**

Change `snapshot()`:
```rust
    pub(crate) fn snapshot(&self) -> PartialUsage {
        if let Some((input, output, cached)) = self.authoritative {
            PartialUsage {
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
            }
        } else {
            // Fallback: estimate output tokens from accumulated text via
            // tt_tokenize rather than raw byte length (§2.12).
            let output_tokens =
                tt_tokenize::estimate_tokens(&self.provider_id, &self.output_text) as i32;
            PartialUsage {
                input_tokens: self.input_tokens,
                output_tokens,
                cached_tokens: self.cached_tokens,
            }
        }
    }
```
to:
```rust
    pub(crate) fn snapshot(&self) -> PartialUsage {
        if let Some((input, output, cached, cache_creation)) = self.authoritative {
            PartialUsage {
                input_tokens: input,
                output_tokens: output,
                cached_tokens: cached,
                cache_creation_tokens: cache_creation,
            }
        } else {
            // Fallback: estimate output tokens from accumulated text via
            // tt_tokenize rather than raw byte length (§2.12). No authoritative
            // block → no known cache-creation count.
            let output_tokens =
                tt_tokenize::estimate_tokens(&self.provider_id, &self.output_text) as i32;
            PartialUsage {
                input_tokens: self.input_tokens,
                output_tokens,
                cached_tokens: self.cached_tokens,
                cache_creation_tokens: 0,
            }
        }
    }
```

- [ ] **Step 6: Carry it through `cache_completion_data()`**

Change `cache_completion_data()`:
```rust
    pub(crate) fn cache_completion_data(&self) -> Option<(String, String, Usage)> {
        let (prompt_tokens, completion_tokens, cached_tokens) = self.authoritative?;
        let finish_reason = self.finish_reason.clone().unwrap_or_else(|| "stop".into());
        let text = self.output_text.clone();
        let usage = Usage {
            prompt_tokens: prompt_tokens as u64,
            completion_tokens: completion_tokens as u64,
            total_tokens: (prompt_tokens + completion_tokens) as u64,
            cached_tokens: cached_tokens as u64,
            cache_creation_input_tokens: None,
        };
        Some((text, finish_reason, usage))
    }
```
to:
```rust
    pub(crate) fn cache_completion_data(&self) -> Option<(String, String, Usage)> {
        let (prompt_tokens, completion_tokens, cached_tokens, cache_creation) = self.authoritative?;
        let finish_reason = self.finish_reason.clone().unwrap_or_else(|| "stop".into());
        let text = self.output_text.clone();
        let usage = Usage {
            prompt_tokens: prompt_tokens as u64,
            completion_tokens: completion_tokens as u64,
            total_tokens: (prompt_tokens + completion_tokens) as u64,
            cached_tokens: cached_tokens as u64,
            cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation as u64),
        };
        Some((text, finish_reason, usage))
    }
```

- [ ] **Step 7: Keep the existing fee test compiling**

The existing test `streaming_fee_multiplier_scales_cost_and_baseline` constructs a `PartialUsage { input_tokens, output_tokens, cached_tokens }` literal (the only other literal besides `snapshot`). Add the new field so it still compiles (Task 2 rewrites this test fully):

Change:
```rust
        let usage = PartialUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cached_tokens: 0,
        };
```
to:
```rust
        let usage = PartialUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        };
```

- [ ] **Step 8: Run the capture test + the full sse test module**

Run: `cargo test -p tt-core --lib routes::sse 2>&1 | tail -20`
Expected: PASS — `usage_tracking_captures_cache_creation_tokens` plus all existing sse tests green.

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/routes/sse.rs
git commit -m "feat(sse): capture cache_creation_input_tokens in streaming usage

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Unify streaming cost on `chat::compute_cost`

**Files:**
- Modify: `crates/core/src/routes/sse.rs`

- [ ] **Step 1: Write the failing premium-pricing test**

Append inside the `#[cfg(test)] mod tests` block in `crates/core/src/routes/sse.rs`:

```rust
    #[test]
    fn streaming_prices_cache_write_at_premium() {
        // 100 prompt tokens: 20 cache_read, 30 cache_write, 50 fresh; 10 output.
        let usage = PartialUsage {
            input_tokens: 100,
            output_tokens: 10,
            cached_tokens: 20,
            cache_creation_tokens: 30,
        };
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: Some(0.1),
            cache_write_per_million: Some(1.25), // 1.25x write premium
            effective_at: chrono::DateTime::UNIX_EPOCH,
        };
        // Unified path (fee = 1.0): fresh 50*1.0 + read 20*0.1 + write 30*1.25
        //   + out 10*2.0, all / 1e6.
        let (cost, _baseline) = crate::routes::chat::compute_cost(
            &partial_to_usage(&usage),
            Some(&pricing),
            None,
            1.0,
        );
        let expected = (50.0 * 1.0 + 20.0 * 0.1 + 30.0 * 1.25 + 10.0 * 2.0) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-12, "cost={cost} expected={expected}");

        // It must be MORE than folding cache_write into fresh input (the old bug).
        let folded = (80.0 * 1.0 + 20.0 * 0.1 + 10.0 * 2.0) / 1_000_000.0;
        assert!(cost > folded, "premium not applied: cost={cost} folded={folded}");
    }
```

- [ ] **Step 2: Run to confirm it fails**

Run: `cargo test -p tt-core --lib routes::sse::tests::streaming_prices_cache_write_at_premium 2>&1 | tail -15`
Expected: FAIL — `partial_to_usage` is not defined yet (compile error).

- [ ] **Step 3: Add the `partial_to_usage` converter**

In `crates/core/src/routes/sse.rs`, add this free function near the old `compute_streaming_cost` (which you will delete in Step 4):

```rust
/// Build a `Usage` from accumulated streaming counts so the streaming path can
/// reuse the authoritative non-streaming cost math (`chat::compute_cost`).
fn partial_to_usage(u: &PartialUsage) -> Usage {
    let prompt = u.input_tokens.max(0) as u64;
    let completion = u.output_tokens.max(0) as u64;
    let cache_creation = u.cache_creation_tokens.max(0) as u64;
    Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt + completion,
        cached_tokens: u.cached_tokens.max(0) as u64,
        cache_creation_input_tokens: (cache_creation > 0).then_some(cache_creation),
    }
}
```

- [ ] **Step 4: Delete the parallel cost functions**

Remove the entire `compute_streaming_cost` function and the entire `compute_streaming_baseline` function from `crates/core/src/routes/sse.rs` (the two `fn`s with their doc-comments).

- [ ] **Step 5: Update call site A — `usage_event`**

In `TrackedEventStream::usage_event`, replace:
```rust
        let cost_usd = compute_streaming_cost(&usage, Some(pricing)) * self.fee_multiplier;
        let baseline_cost_usd =
            compute_streaming_baseline(&usage, self.baseline_pricing.as_ref().or(Some(pricing)))
                * self.fee_multiplier;
        let saved_usd = (baseline_cost_usd - cost_usd).max(0.0_f64);
```
with:
```rust
        let (cost_usd, baseline_cost_usd) = crate::routes::chat::compute_cost(
            &partial_to_usage(&usage),
            Some(pricing),
            self.baseline_pricing.as_ref(),
            self.fee_multiplier,
        );
        let saved_usd = (baseline_cost_usd - cost_usd).max(0.0_f64);
```

- [ ] **Step 6: Update call site B — `DropGuard`**

In the `DropGuard` closure, replace:
```rust
                // Apply provider surcharge to both cost and baseline (§2.13).
                let cost_usd = compute_streaming_cost(&usage, pricing.as_ref()) * fee_multiplier;
                let baseline_cost_usd =
                    compute_streaming_baseline(&usage, baseline_pricing.as_ref()) * fee_multiplier;
```
with:
```rust
                // Reuse the authoritative non-streaming cost math (3-bucket
                // input pricing incl. cache-write premium); fee applied inside.
                let (cost_usd, baseline_cost_usd) = crate::routes::chat::compute_cost(
                    &partial_to_usage(&usage),
                    pricing.as_ref(),
                    baseline_pricing.as_ref(),
                    fee_multiplier,
                );
```

- [ ] **Step 7: Rewrite the fee-multiplier test for the unified path**

Replace the entire `streaming_fee_multiplier_scales_cost_and_baseline` test with one that exercises the unified path (it no longer references the deleted helpers):

```rust
    /// §2.13 — the unified streaming cost (via `compute_cost`) applies the
    /// fee multiplier to both cost and baseline. 1 000 input + 500 output at
    /// $1/$2 per M, no cache, ×1.05.
    #[test]
    fn streaming_fee_multiplier_scales_cost_and_baseline() {
        let pricing = ModelPricing {
            input_per_million: 1.0,
            output_per_million: 2.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        };
        let usage = PartialUsage {
            input_tokens: 1_000,
            output_tokens: 500,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        };
        let u = partial_to_usage(&usage);

        // fee = 1.0 → unscaled. cost = 1000/1e6 + 500*2/1e6 = 0.002.
        let (base_cost, base_baseline) =
            crate::routes::chat::compute_cost(&u, Some(&pricing), None, 1.0);
        assert!((base_cost - 0.002_f64).abs() < 1e-9, "base_cost={base_cost}");

        // fee = 1.05 → both scale by 1.05.
        let (scaled_cost, scaled_baseline) =
            crate::routes::chat::compute_cost(&u, Some(&pricing), None, 1.05);
        assert!(
            (scaled_cost - base_cost * 1.05).abs() < 1e-9,
            "scaled_cost={scaled_cost}"
        );
        assert!(
            (scaled_baseline - base_baseline * 1.05).abs() < 1e-9,
            "scaled_baseline={scaled_baseline}"
        );
    }
```

- [ ] **Step 8: Add a no-cache-creation parity regression test**

Append inside the test module:

```rust
    /// Regression: an all-fresh-input stream (no cache read/write) is priced
    /// identically to the simple input×rate + output×rate formula — i.e. the
    /// unify refactor did not change non-Anthropic streaming costs.
    #[test]
    fn streaming_all_fresh_input_parity() {
        let usage = PartialUsage {
            input_tokens: 800,
            output_tokens: 200,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        };
        let pricing = ModelPricing {
            input_per_million: 3.0,
            output_per_million: 6.0,
            cached_input_per_million: None,
            cache_write_per_million: None,
            effective_at: chrono::DateTime::UNIX_EPOCH,
        };
        let (cost, _baseline) =
            crate::routes::chat::compute_cost(&partial_to_usage(&usage), Some(&pricing), None, 1.0);
        let expected = (800.0 * 3.0 + 200.0 * 6.0) / 1_000_000.0;
        assert!((cost - expected).abs() < 1e-12, "cost={cost} expected={expected}");
    }
```

- [ ] **Step 9: Run the full sse test module**

Run: `cargo test -p tt-core --lib routes::sse 2>&1 | tail -25`
Expected: PASS — `streaming_prices_cache_write_at_premium`, `streaming_fee_multiplier_scales_cost_and_baseline`, `streaming_all_fresh_input_parity`, and all existing sse tests green.

- [ ] **Step 10: Gates**

Run: `cargo clippy -p tt-core --all-targets -- -D warnings 2>&1 | grep -v "Permission denied\|auto-clean" | tail -15`
Expected: no warnings (confirm `compute_streaming_cost`/`compute_streaming_baseline` are fully removed — no dead-code warnings).

Run: `cargo fmt -p tt-core -- --check 2>&1 | tail -5`
Expected: clean. If a diff, run `cargo fmt -p tt-core`.

- [ ] **Step 11: Commit**

```bash
git add crates/core/src/routes/sse.rs
git commit -m "feat(sse): unify streaming cost on compute_cost (cache-write premium)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)

Per `ci-verify-all-targets`:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-run
cargo test -p tt-core
```
Expected: all clean/green.

## Notes for the implementer
- `chat::compute_cost` is `pub(crate)` — reachable as `crate::routes::chat::compute_cost` from `sse.rs` (same crate). No visibility change needed.
- `compute_cost` applies `fee_multiplier` internally and returns `(cost, baseline)` already scaled; do NOT multiply again at the call sites.
- `compute_cost` falls back `baseline_pricing → pricing` internally, matching the old `.or(Some(pricing))`.
- Stage only `crates/core/src/routes/sse.rs`; do not whole-workspace `cargo fmt`.
