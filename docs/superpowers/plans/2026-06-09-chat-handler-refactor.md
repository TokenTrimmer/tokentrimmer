# chat.rs Handler Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Behavior-preserving cleanup of the `POST /v1/chat/completions` handler in `crates/core/src/routes/chat.rs` — use the shared token-estimation helper on the streaming path, refresh the stale module docstring, and extract the three early-returning cache-lookup branches into named `async` helpers.

**Architecture:** Pure refactor. No behavior change. The existing `tt-core` test suite (~166 tests incl. `l1_cache_hit`, `negative_cache`, `disable_cache`, `cache_header`, `route_rewrite`, `single_flight_coalesce`) is the safety net — every task ends by running it and confirming it stays green. No new tests.

**Tech Stack:** Rust, axum, `tt-shared`, `tt-tokenize`.

**Spec:** `docs/superpowers/specs/2026-06-09-chat-handler-refactor-batch7l-design.md`

**Branch:** `batch7l-chat-handler-refactor` (already created; spec already committed on it).

**Conventions:** stage ONLY `crates/core/src/routes/chat.rs` per task (plus the checklist file in Task 4). NEVER `git add -A` — the worktree has an untracked `rust_out` and `sdk-typescript/package-lock.json` that must never be staged. End every commit message with:
```
Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
```

**Global verification command (used by every task):**
```bash
cargo test -p tt-core 2>&1 | tail -5
```
Expected: all tests pass (0 failed). The whole point of this refactor is that this never changes.

---

## Task 1: Refresh the stale module docstring

**Files:** Modify `crates/core/src/routes/chat.rs:1-16`

- [ ] **Step 1: Replace the docstring**

Replace the current module docstring (lines 1–16, the `//!` block that begins `//! \`POST /v1/chat/completions\`` and ends with the `//!   - Telemetry / audit row write (W7 telemetry pipeline).` line) with:

```rust
//! `POST /v1/chat/completions` — OpenAI-compatible chat completion.
//!
//! Request pipeline (see the numbered steps in `handler`):
//!   1. Resolve the provider from `request.model`; 404 on an unknown model.
//!   2. Authenticate (bearer key → `ApiKeyContext`), resolve the org's upstream
//!      credentials, apply the routing engine (may rewrite `req.model`), honor
//!      an explicit provider pin, and compute the per-request cache behavior.
//!   3. Non-streaming: try the negative cache, then L1 exact-match, then the L2
//!      semantic cache; on a miss, single-flight-coalesce and dispatch to the
//!      provider (with cross-provider failover), then best-effort insert into
//!      L1 + L2 and write a `request_logs` row.
//!      Streaming: dispatch directly (failover only on initial establishment).
//!   4. Stamp the `X-TokenTrimmer-*` response headers (cost, cache state,
//!      provider, model, route-matched, warnings).
//!
//! `tt_test_*` keys short-circuit to a deterministic sandbox response (step 2a).
```

- [ ] **Step 2: Verify it still compiles + tests pass**

Run: `cargo test -p tt-core 2>&1 | tail -5`
Expected: all pass (a docstring change cannot affect behavior; this confirms no accidental edit outside the comment).

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "$(cat <<'EOF'
docs(core): refresh stale chat.rs module docstring (batch 7l)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Streaming token-estimate uses the shared helper

**Files:** Modify `crates/core/src/routes/chat.rs` (the `let estimated_input_tokens = { … };` block, currently ~lines 793–836, inside the `if req.stream` branch)

**Context:** The streaming branch hand-rolls a per-message text concatenation then calls `tt_tokenize::estimate_tokens`. `tt_shared::message_text_for_estimation(&req)` produces byte-identical text (same `User`/`System`/`Assistant`/`Tool` handling, same `Text` + `Parts`-text-only extraction, same `join("")`), and is what routing / `/v1/preview` / the other call sites already use. Swapping is behavior-preserving and removes the divergence hazard.

- [ ] **Step 1: Replace the inline concat**

Find the block that starts with `let estimated_input_tokens = {` and ends with the matching `};` (the closure maps over `req.messages` building `combined_text`, then calls `tt_tokenize::estimate_tokens(provider_id_for_est, &combined_text) as i32`). Keep the explanatory comment that precedes it (the `// (tiktoken for openai/anthropic …)` lines). Replace the entire `let estimated_input_tokens = { … };` block with:

```rust
        let estimated_input_tokens = tt_tokenize::estimate_tokens(
            provider.id(),
            &tt_shared::message_text_for_estimation(&req),
        ) as i32;
```

(If `provider` is not in scope at this exact point under that name — verify by reading the surrounding lines; the inline version used `provider.id()` via `let provider_id_for_est = provider.id();`, so `provider` is in scope. Use `provider.id()` directly.)

- [ ] **Step 2: Verify**

Run: `cargo test -p tt-core 2>&1 | tail -5`
Expected: all pass. (The streaming establishment tests + cap-check tests exercise this estimate; they must stay green.)

Also confirm no now-unused imports remain (e.g. if `Message`/`MessageContent`/`ContentPart` were used ONLY by the removed block — they are used elsewhere in the file, so they should remain; `cargo test` will fail to compile on a genuinely unused import under the workspace `-D warnings`, surfacing any issue).

- [ ] **Step 3: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "$(cat <<'EOF'
refactor(core): streaming input estimate uses shared message_text_for_estimation (batch 7l)

Replaces a hand-rolled per-message concat with the same helper routing and
/v1/preview use, removing the documented divergence hazard. Behavior-preserving
(the helper produces identical text).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Extract the three cache-lookup branches into helpers

**Files:** Modify `crates/core/src/routes/chat.rs` — add three free `async fn` helpers (place them immediately ABOVE `pub async fn handler`, after the existing free helpers like `cache_override_from_header`), and replace the inline branches (currently the negative-cache block ~1001–1046, the L1 positive block ~1048–1082, and the L2 block ~1084–1128) with calls.

**Context / contract:**
- These three lookups currently `return` early from `handler` on a hit. Each helper returns an `Option` where **`None` = fall through to the next step**.
- Move each block **verbatim**: do NOT alter any `metrics::counter!`, `spawn_request_log`, `request_log_for_l1_hit`/`request_log_for_l2_hit`, `bump_hit_count`, `build_hit_l1_response`/`build_hit_l2_response`, `with_route_matched`, or tracing call. Only rename the captured locals to the parameters.
- The `cache_behavior.do_lookup` and `l2_allowed` gates and the `state.l1.as_ref()`/`state.l2.as_ref()` unwraps stay at the CALL SITE (so each helper takes an already-unwrapped `&L1Config` / `&L2Config`).
- Types (verified): `L1Config`/`L2Config` are `crate::state::{L1Config, L2Config}` (already in scope in this module). `RequestLogWriter` arc is `Option<&std::sync::Arc<dyn RequestLogWriter>>` (the exact type `spawn_request_log` already accepts). `matched_route_id: Option<Uuid>`, `route_matched_name` passed as `Option<&str>` via `.as_deref()`.

- [ ] **Step 1: Add the negative-cache helper**

Add above `pub async fn handler`:

```rust
/// Negative-cache lookup (step 3a-neg). If a prior identical request received a
/// deterministic 4xx that was stored under `neg:{l1_key}`, serve the cached
/// error immediately. `None` falls through to the positive lookups. Best-effort:
/// any cache/deserialize error is logged and treated as a miss.
async fn try_negative_cache_hit(
    l1: &L1Config,
    l1_key: &str,
    route_matched_name: Option<&str>,
) -> Option<Response> {
    let neg_key = negative_l1_key(l1_key);
    match l1.cache.get(&neg_key).await {
        Ok(Some(bytes)) => match serde_json::from_slice::<NegativeCacheEntry>(&bytes) {
            Ok(neg) => {
                tracing::debug!(
                    key = %neg_key,
                    status = neg.status,
                    "negative cache hit — short-circuiting provider call"
                );
                let err_body = serde_json::json!({
                    "error": {
                        "message": neg.message,
                        "type": "invalid_request_error",
                        "code": "cached_client_error",
                        "param": null
                    }
                });
                let status = axum::http::StatusCode::from_u16(neg.status)
                    .unwrap_or(axum::http::StatusCode::BAD_REQUEST);
                let mut resp = (status, Json(err_body)).into_response();
                if let Ok(v) = "neg-hit".parse() {
                    resp.headers_mut().insert("x-tokentrimmer-cache", v);
                }
                Some(with_route_matched(resp, route_matched_name))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    key = %neg_key,
                    "negative cache entry deserialization failed — ignoring"
                );
                None
            }
        },
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(error = %e, "negative cache lookup error — ignoring");
            None
        }
    }
}
```

- [ ] **Step 2: Add the L1 positive-lookup helper**

```rust
/// L1 exact-match lookup (step 3a). `None` falls through to L2. Best-effort: a
/// cache or deserialize error logs and is treated as a miss.
#[allow(clippy::too_many_arguments)]
async fn try_l1_hit(
    l1: &L1Config,
    l1_key: &str,
    ctx: &RequestContext,
    request_log_writer: Option<&std::sync::Arc<dyn RequestLogWriter>>,
    trace_id: Uuid,
    request_started: Instant,
    matched_route_id: Option<Uuid>,
    route_matched_name: Option<&str>,
) -> Option<Response> {
    match l1.cache.get(l1_key).await {
        Ok(Some(bytes)) => match L1Entry::from_bytes(&bytes) {
            Ok(entry) => {
                metrics::counter!("cache_lookups_total", "tier" => "l1", "result" => "hit")
                    .increment(1);
                spawn_request_log(
                    request_log_writer,
                    request_log_for_l1_hit(&entry, ctx, trace_id, request_started, matched_route_id),
                );
                Some(with_route_matched(
                    build_hit_l1_response(entry, trace_id),
                    route_matched_name,
                ))
            }
            Err(e) => {
                tracing::warn!(error = %e, key = %l1_key, "l1 cache entry failed to deserialize");
                None
            }
        },
        Ok(None) => {
            metrics::counter!("cache_lookups_total", "tier" => "l1", "result" => "miss")
                .increment(1);
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "l1 lookup failed");
            None
        }
    }
}
```

- [ ] **Step 3: Add the L2 semantic-lookup helper**

Read the live L2 block (currently ~1086–1128) and move it verbatim into this body. The signature returns `Option<ApiResult<Response>>` so the existing `build_hit_l2_response(...)?` error propagation is preserved. Skeleton (fill the lookup body from the live code — embed query text via `l2_context_text(req)`, embed, `l2.cache.lookup(ctx.org_id, &query_vec, l2.threshold, &req.model, l2.embedder.model())`, `bump_hit_count`, `spawn_request_log(request_log_for_l2_hit(...))`):

```rust
/// L2 semantic-cache lookup (step 3b). `None` falls through to dispatch.
/// `Some(Err(_))` preserves the original `build_hit_l2_response(...)?` error
/// propagation (a hit whose body fails to deserialize). Best-effort on the
/// embed/lookup side: those errors are treated as a miss.
#[allow(clippy::too_many_arguments)]
async fn try_l2_hit(
    l2: &L2Config,
    ctx: &RequestContext,
    req: &ChatCompletionRequest,
    request_log_writer: Option<&std::sync::Arc<dyn RequestLogWriter>>,
    trace_id: Uuid,
    request_started: Instant,
    matched_route_id: Option<Uuid>,
    route_matched_name: Option<&str>,
) -> Option<ApiResult<Response>> {
    let query_text = l2_context_text(req)?;
    let query_vec = match l2.embedder.embed(&query_text).await {
        Ok(v) => v,
        Err(_) => return None,
    };
    match l2
        .cache
        .lookup(ctx.org_id, &query_vec, l2.threshold, &req.model, l2.embedder.model())
        .await
    {
        Ok(Some((entry, similarity))) => {
            metrics::counter!("cache_lookups_total", "tier" => "l2", "result" => "hit")
                .increment(1);
            let _ = l2.cache.bump_hit_count(entry.id).await;
            spawn_request_log(
                request_log_writer,
                request_log_for_l2_hit(&entry, ctx, trace_id, request_started, matched_route_id),
            );
            Some(
                build_hit_l2_response(entry, similarity, trace_id)
                    .map(|resp| with_route_matched(resp, route_matched_name)),
            )
        }
        Ok(None) => {
            metrics::counter!("cache_lookups_total", "tier" => "l2", "result" => "miss")
                .increment(1);
            None
        }
        Err(_) => None,
    }
}
```

Before finalizing, **diff this against the live L2 block (~1086–1128)** and confirm every metrics label, the `lookup` argument order, the `bump_hit_count`, the `request_log_for_l2_hit` args, and the `build_hit_l2_response` args match exactly. If `l2_context_text` returns something other than `Option<String>`, adapt the early-return (`?`) accordingly by reading its signature.

- [ ] **Step 4: Replace the inline branches with calls**

Replace the inline negative-cache block (`if cache_behavior.do_lookup { if let (Some(l1), Some(key)) = … { … } }`, ~1001–1046), the inline L1 positive block (~1048–1082), and the inline L2 block (~1084–1128) with this single call sequence (placed where the negative-cache block was; the `let l1_key = …;` binding at ~986–989 stays put above it):

```rust
        // 3a/3a-neg. Negative cache, then L1 exact-match. Gated on cache
        // eligibility + tt_extras.cache mode; best-effort (errors fall through).
        if cache_behavior.do_lookup {
            if let (Some(l1), Some(key)) = (state.l1.as_ref(), l1_key.as_ref()) {
                if let Some(resp) = try_negative_cache_hit(l1, key, route_matched_name.as_deref()).await {
                    return Ok(resp);
                }
                if let Some(resp) = try_l1_hit(
                    l1,
                    key,
                    &ctx,
                    state.request_log_writer.as_ref(),
                    trace_id,
                    request_started,
                    matched_route_id,
                    route_matched_name.as_deref(),
                )
                .await
                {
                    return Ok(resp);
                }
            }
        }

        // 3b. L2 semantic cache. Gated additionally on l2_allowed.
        if cache_behavior.do_lookup && l2_allowed {
            if let Some(l2) = state.l2.as_ref() {
                if let Some(result) = try_l2_hit(
                    l2,
                    &ctx,
                    &req,
                    state.request_log_writer.as_ref(),
                    trace_id,
                    request_started,
                    matched_route_id,
                    route_matched_name.as_deref(),
                )
                .await
                {
                    return result;
                }
            }
        }
```

Note: this merges the two former `if cache_behavior.do_lookup` blocks (negative + positive L1) under one gate+unwrap — behavior-identical (same gate, same order: negative then positive). Leave everything from `// 3b.5. Single-flight coalescing` onward unchanged.

- [ ] **Step 5: Verify (the critical gate)**

Run: `cargo test -p tt-core 2>&1 | tail -8`
Expected: ALL pass, with no change in counts. Pay specific attention to: `l1_cache_hit`, `negative_cache`, `disable_cache`, `cache_header`, `route_rewrite`, `single_flight_coalesce`. If any fail, the move was not verbatim — diff the helper bodies against the original blocks (git: `git show HEAD~3:crates/core/src/routes/chat.rs` for the pre-refactor version) until behavior matches. Do NOT change a test to make it pass.

Then:
```bash
cargo clippy -p tt-core --all-targets 2>&1 | tail -3   # clean
cargo fmt --all -- --check                              # clean (run `cargo fmt -p tt-core` if not)
```

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/routes/chat.rs
git commit -m "$(cat <<'EOF'
refactor(core): extract chat.rs cache-lookup branches into helpers (batch 7l)

Move the negative-cache, L1 exact-match, and L2 semantic lookup branches out of
the ~1060-line handler into try_negative_cache_hit / try_l1_hit / try_l2_hit
(async, returning Option, None = fall through). Verbatim move — same metrics,
request-log spawns, bump_hit_count, response builders, and route-matched
wrapping. Behavior-preserving; the full tt-core suite passes unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Flip the checklist entry

**Files:** Modify `docs/reviews/2026-06-06-audit-checklist.md` (the L150 entry)

- [ ] **Step 1: Flip it**

Change the entry beginning `- [ ] ⚪ **[dx/low] chat.rs handler is a 1500-line monolith with significant duplicated input-token estimation** — **🔴 OPEN**` to `- [x] … — **✅ DONE (PR #104): …**`, keeping the `- Where:` / `- Issue:` / `- Action:` sub-bullets. DONE text:

`✅ DONE (PR #104): streaming input-token estimate now uses the shared tt_shared::message_text_for_estimation (removes the divergence hazard); stale "Week N" module docstring refreshed to the actual pipeline; the negative-cache / L1 / L2 lookup branches extracted into try_negative_cache_hit / try_l1_hit / try_l2_hit helpers. Behavior-preserving — full tt-core suite passes unchanged (conservative scope: dispatch/cost/insert stay inline; no handler-context struct).`

- [ ] **Step 2: Commit**

```bash
git add docs/reviews/2026-06-06-audit-checklist.md
git commit -m "$(cat <<'EOF'
docs(reviews): flip chat.rs monolith audit entry to done (batch 7l)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes (for the executor)

- **The contract is "the suite stays green."** This refactor introduces no new behavior and no new tests. If `cargo test -p tt-core` count or pass/fail changes, something was moved non-verbatim — fix the move, never the test.
- **`l1_key` lifetime:** it's computed once (~986) and reused by the 3e L1 insert later in the handler. Keep that binding where it is; only the lookups move.
- **`l2_context_text` / `build_hit_l2_response` signatures:** read them live before finalizing Task 3 Step 3 — the skeleton assumes `l2_context_text(req) -> Option<String>` and `build_hit_l2_response(entry, similarity, trace_id) -> ApiResult<Response>`. Adapt if they differ.
- **Import check:** after Task 2, `Message`/`MessageContent`/`ContentPart` remain used elsewhere in the file, so no import removal is expected; the `-D warnings` build will flag any genuinely-orphaned import.
