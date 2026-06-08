# Gateway/core low-bugs sweep (batch 3) — Design

**Status:** approved (design + scope + judgment calls, 2026-06-08)
**Date:** 2026-06-08
**Slice:** Audit-remediation, public repo. Closes the gateway/core/inspect/auth low-severity findings, split into two cohesive PRs. All 10 candidate findings re-verified STILL-PRESENT against current code before scoping.

## Scope decision (user-approved)
Ship **3a now** (5 clean validation/robustness fixes); in **3b** fix the two small/clear items (`force_cache_layer` removal, nil-org cache bypass) and formally **document** the three architectural items (#7 streaming-spend, #8 negative-cache DoS, #10 issue/revoke audit txn) as accept-with-mitigation / defer-to-own-slice in the checklist rather than rushing them.

Judgment calls (user-approved): `classify_task` → reorder Reasoning-before-Code; `force_cache_layer` → **remove** the dead field (serde stays tolerant of the old JSON key).

---

## PR 3a — Input-validation & robustness (`crates/{retrieval,mcp,inspect-rules-tier1}`)

### 3a.1 `min_similarity` NaN / out-of-range (`retrieval/src/tags.rs:55-58`)
Current: parsed `f32` stored unchecked; `NaN`, `1.5`, `-3.0` all flow through to the similarity floor.
Fix: after parse, drop the value unless it is finite and in `[0.0, 1.0]`.
```rust
let min_similarity = sim_re
    .captures(attrs)
    .and_then(|c| c.get(1))
    .and_then(|s| s.as_str().parse::<f32>().ok())
    // Ignore NaN / out-of-range floors; fall back to the default downstream.
    .filter(|v| v.is_finite() && (0.0..=1.0).contains(v));
```
(`Option<f32>` shape unchanged → `unwrap_or(DEFAULT_MIN_SIMILARITY)` in `substitute.rs` still applies for the dropped case. No signature change.)

### 3a.2 Secret-detection OpenAI regex (`inspect-rules-tier1/src/rules/config_agents_md_contains_secrets.rs:40`)
Current: `("OpenAI API key", r"sk-[A-Za-z0-9]{20,}")` — over-matches any `sk-`+20-alnum and **misses** modern `sk-proj-` / `sk-svcacct-` keys (they contain `-`/`_` after the prefix, breaking the alnum-only class right after `sk-`).
Fix: replace the single entry with patterns that match the real key shapes and a tightened legacy form. Anthropic's `sk-ant-` entry already precedes it; keep OpenAI matching distinct.
```rust
("Anthropic API key", r"sk-ant-[A-Za-z0-9_-]{32,}"),
// OpenAI project/service-account keys (modern): sk-proj-…, sk-svcacct-…, sk-admin-…
("OpenAI API key (scoped)", r"sk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{20,}"),
// Legacy bare OpenAI key: sk- + 32+ base62 chars (real legacy keys are 48).
("OpenAI API key (legacy)", r"sk-[A-Za-z0-9]{32,}"),
("Stripe live secret key", r"sk_live_[A-Za-z0-9]{20,}"),
```
Rationale: raising the legacy floor from `{20,}` to `{32,}` cuts the broad false-positive class (random `sk-`+20 strings) while still catching real legacy keys (48 chars) and the existing should-detect fixture (39 chars — an exact `{48}` was too strict and broke it). Scoped keys get their own anchored prefixes. (`sk-ant-` stays first so Anthropic keys aren't mislabeled — but the scoped/legacy OpenAI patterns won't match `sk-ant-…` anyway: `ant` isn't in the scoped alternation, and the unanchored legacy `sk-[A-Za-z0-9]{32,}` can't start a match on `sk-ant-` since the `-` after `ant` breaks the base62 run.)

### 3a.3 `inspect_diff` temp-file extension + swallowed errors (`mcp/src/tools/inspect_diff.rs:38-55`)
Current: caller-controlled `file_path` extension flows raw into the temp-file suffix (`format!(".{ext}")`); engine scan result returned with no error surfacing.
Fix: sanitize the extension to a short alphanumeric token before using it as a suffix.
```rust
let raw_ext = std::path::Path::new(&inp.file_path)
    .extension()
    .and_then(|x| x.to_str())
    .unwrap_or("");
// The extension is caller-controlled and only steers language detection — keep
// it to a short alphanumeric token so it can't inject path/suffix surprises.
let ext: String = raw_ext
    .chars()
    .filter(|c| c.is_ascii_alphanumeric())
    .take(16)
    .collect();
let suffix = if ext.is_empty() { String::new() } else { format!(".{ext}") };
```
(`engine.scan(tmp.path())` returns `Vec<Finding>` directly — there is no `Result` to surface here; the temp-file create/write already `map_err` to `McpError::Internal`. The finding's "ignores read errors" refers to the swallowed-on-missing-extension `unwrap_or("")`, which the sanitizer subsumes. No behavior regression: an empty/garbage extension now yields a no-suffix temp file rather than `.` + junk.)

### 3a.4 `classify_task` Reasoning-before-Code (`mcp/src/tools/find_route_for.rs:55-76`)
Current: the `Code` `else if` block precedes `Reasoning`, so "analyze this code", "compare these diffs", "reason about the compile error" classify as `Code`.
Fix: swap the two blocks so `Reasoning` is checked first. Keyword lists unchanged; only the order changes. Reasoning intent ("analyze/compare/reason/evaluate/step by step") now wins over incidental code nouns; a prompt with only code keywords ("refactor this function") still falls through to `Code`.

### 3a.5 `CleanupStream` Drop tokio-spawn leak (`mcp/src/transport/sse.rs:259-269`)
Current: `Drop` calls `tokio::spawn` to `sessions.lock().await.remove(...)` — leaks if the runtime is shutting down or absent.
Fix: best-effort synchronous removal via `try_lock`, falling back to a guarded spawn only when a runtime is present. `SessionMap = Arc<Mutex<HashMap<…>>>` (tokio `Mutex`), so:
```rust
impl Drop for CleanupStream {
    fn drop(&mut self) {
        let session_id = self.session_id;
        // Fast path: if the lock is free, remove synchronously — no task, no
        // leak on runtime shutdown.
        if let Ok(mut guard) = self.sessions.try_lock() {
            guard.remove(&session_id);
            tracing::debug!(session_id = %session_id, "SSE session cleaned up (sync)");
            return;
        }
        // Contended: only spawn if a runtime is actually live, else the task
        // would silently leak. The session map self-heals (stale senders error
        // on next POST), so dropping cleanup here is safe.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let sessions = self.sessions.clone();
            handle.spawn(async move {
                sessions.lock().await.remove(&session_id);
                tracing::debug!(session_id = %session_id, "SSE session cleaned up (async)");
            });
        } else {
            tracing::debug!(session_id = %session_id, "SSE session cleanup skipped — no runtime");
        }
    }
}
```

**3a gates:** `cargo test -p tt-retrieval -p tt-mcp -p tt-inspect-rules-tier1`; `cargo fmt --check` on changed files; `cargo clippy -p tt-retrieval -p tt-mcp -p tt-inspect-rules-tier1 --all-targets -- -D warnings`. No public-signature change → no workspace ripple.

---

## PR 3b — `force_cache_layer` removal + nil-org cache bypass (`crates/{routing,plan-core,core}`)

### 3b.1 Remove dead `force_cache_layer` field
The field is defined on `RouteAction` in **both** `routing/src/lib.rs:108` and `plan-core/src/types.rs:174`, is never read at runtime in either crate (confirmed: not extracted in `chat.rs` dispatch, not in `RouteMatch`), and exists only for lossless plan↔routing JSON round-trip. User decision: remove.

Steps:
- Delete the field + its doc comment + `#[serde(default, skip_serializing_if = "Option::is_none")]` from both structs.
- Delete every `force_cache_layer: None,` initializer (routing: lib.rs:277, cache.rs:143/242, validate.rs:52, store.rs:357/392/427; plan-core: apply.rs:298, routing.rs:113) and the test-only `Some(...)` initializers.
- Update/delete the round-trip & default tests that assert the field's presence (routing/lib.rs:~574-631, ~664-675; plan-core/types.rs:~458-513, ~535-545). Replace the cross-crate wire test's JSON to drop the `force_cache_layer` key, and **add** a test asserting old JSON *containing* `"force_cache_layer":"l2"` still deserializes successfully (serde ignores the unknown key — back-compat guarantee).

Serde tolerance: neither struct uses `#[serde(deny_unknown_fields)]` (verify), so deserializing legacy JSON with the key is a no-op drop. This is the documented back-compat behavior.

**Ripple:** removing a `pub` field is an API change — `cargo clippy --workspace --all-targets` + `cargo test --workspace --no-run` are mandatory before push (per the enum-variant-ripple lesson).

### 3b.2 nil-org cache — ACCEPTED, not fixed (revised 2026-06-08)
Original plan was to skip L1/L2 for `org_id.is_nil()`. A prototype confirmed this works but **disables legitimate single-tenant dev-mode caching** and breaks ~6 unauthenticated cache-hit test harnesses (`cache_header`, `l1_cache_hit`, `l2_cache_hit`, `negative_cache`, `single_flight_coalesce`, `streaming_cache_write`). Since nil-org is only reachable by unauthenticated requests and production requires auth (routing already enforces it), the shared namespace is only exposed in an unsupported unauth-multi-tenant deploy with effectively one logical tenant. User decision (2026-06-08): **accept + document** rather than rework well-tested cache infra for a low/theoretical exposure. No code change; checklist entry annotated ACCEPTED.

### 3b.3 Checklist documentation (no code) — both repos' `docs/reviews/2026-06-06-audit-checklist.md`
- #7 streaming-spend-on-body-drop → annotate **DEFERRED**: a correct fix is reserve-at-admission + reconcile-on-completion (own slice); the current body-drop recording is the documented best-effort. Leave checkbox open with the deferral note.
- #8 negative-auth-cache DoS → annotate **ACCEPTED (mitigated)**: 100k cap + 50k sweep threshold + 10s negative TTL + argon2 dominating per-attempt CPU make a unique-token flood impractical; true mitigation is edge/IP rate-limiting (infra, out of repo scope).
- #10 issue/revoke audit txn → annotate **DEFERRED**: already fails-loud (returns `Err` + logs "AUDIT GAP"); a true fix needs a shared `KeyStore`+`AuditWriter` transaction (trait-architecture change, own slice).

**3b gates:** `cargo test -p tt-routing -p tt-plan-core -p tt-core`; `cargo fmt --check` on changed files; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --no-run`.

---

## Out of scope
- Reserve-then-reconcile streaming spend accounting (#7) — own slice.
- Shared KeyStore+AuditWriter transaction (#10) — own slice.
- IP/edge rate-limiting for auth (#8) — infra, not this repo.
- Any change to `disable_cache` / `max_cost_usd` semantics.

## Testing summary
- 3a: unit tests per fix (NaN/range rejection; regex matches real OpenAI shapes + no longer matches a bare `sk-`+20 junk string; sanitized extension; "analyze this code" → Reasoning; CleanupStream sync-removal path).
- 3b: round-trip-without-field + legacy-JSON-tolerant deserialize tests; nil-org cache bypass test.
