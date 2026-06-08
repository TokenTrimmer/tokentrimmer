# tt-client header validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Validate the `X-TokenTrimmer-Tag` and `X-TokenTrimmer-Cost-Limit-Usd` header values in tt-client, failing pre-network with a typed error instead of an opaque reqwest error (or a future panic) on an invalid tag / non-finite cost limit.

**Architecture:** One shared `pub(crate) apply_tt_headers` helper validates + attaches both headers; two additive `#[non_exhaustive] Error` variants (`InvalidTag`, `InvalidCostLimit`); the four duplicated injection blocks (lib `send`/`stream`, embeddings, tools `send_round`) call the helper.

**Tech Stack:** Rust (`crates/client` = `tt-client`), reqwest, thiserror, httpmock (dev).

Spec: `docs/superpowers/specs/2026-06-08-tt-client-header-validation-design.md`

> **REPO CAVEATS (public OSS repo):** Scoped cargo only (ADR-012). **Public CI gates `cargo fmt --check`.** Additive (`pub(crate)` helper + two `#[non_exhaustive]` Error variants) — no public-signature break, no workspace ripple; scope gates to `tt-client`.

---

### Task 1: Validate tag + cost_limit headers via a shared helper

**Files:**
- Modify: `crates/client/src/lib.rs` (Error variants, `apply_tt_headers`, send/stream sites, tests)
- Modify: `crates/client/src/embeddings.rs` (send site)
- Modify: `crates/client/src/tools.rs` (`send_round` site)

- [ ] **Step 1: Write the failing unit + pre-flight tests**

In `crates/client/src/lib.rs`, inside the `#[cfg(test)] mod tests` block, add:
```rust
    #[test]
    fn apply_tt_headers_accepts_valid_and_rejects_invalid() {
        let http = reqwest::Client::new();
        let url = "http://127.0.0.1:0/x";

        // Valid tag + finite cost limit → Ok.
        assert!(super::apply_tt_headers(http.get(url), Some("team-a"), Some(0.5)).is_ok());
        // No headers → Ok.
        assert!(super::apply_tt_headers(http.get(url), None, None).is_ok());

        // Tag with a newline is not a valid header value → InvalidTag.
        let e = super::apply_tt_headers(http.get(url), Some("bad\ntag"), None).unwrap_err();
        assert!(matches!(e, Error::InvalidTag(_)), "{e:?}");

        // Non-finite cost limits → InvalidCostLimit.
        let nan = super::apply_tt_headers(http.get(url), None, Some(f64::NAN)).unwrap_err();
        assert!(matches!(nan, Error::InvalidCostLimit(_)), "{nan:?}");
        let inf = super::apply_tt_headers(http.get(url), None, Some(f64::INFINITY)).unwrap_err();
        assert!(matches!(inf, Error::InvalidCostLimit(_)), "{inf:?}");
    }

    #[tokio::test]
    async fn invalid_tag_fails_before_any_request() {
        let server = httpmock::MockServer::start_async().await;
        // A mock that would match ANY chat request; we assert it is NEVER hit.
        let m = server
            .mock_async(|when, then| {
                when.method(httpmock::Method::POST).path("/v1/chat/completions");
                then.status(200).json_body(serde_json::json!({
                    "id":"x","object":"chat.completion","created":0,"model":"gpt-4o-mini",
                    "choices":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}
                }));
            })
            .await;
        let client = Client::new(server.base_url(), "tt_test_k");
        let err = client
            .chat()
            .model("gpt-4o-mini")
            .messages(vec![])
            .tag("bad\ntag")
            .send()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidTag(_)), "{err:?}");
        m.assert_hits_async(0).await; // no request reached the server
    }
```
(Adjust `Client::new(...)` / `.chat().model(...).messages(...)` to the crate's actual constructor + builder method names if they differ — check the existing `cost_limit_402_surfaces_as_status` / `MissingModel` tests in this file and mirror them exactly.)

- [ ] **Step 2: Run to confirm it fails to compile**

Run: `cargo test -p tt-client apply_tt_headers invalid_tag 2>&1 | tail -15`
Expected: FAIL — `apply_tt_headers` not found and `Error::InvalidTag` / `Error::InvalidCostLimit` not defined.

- [ ] **Step 3: Add the two Error variants**

In `crates/client/src/lib.rs`, in the `#[non_exhaustive] pub enum Error` (around line 238), add (e.g. after the `Decode` variant):
```rust
    /// The `tag` is not a valid HTTP header value (control chars, CR/LF, …).
    #[error("invalid tag (not a valid HTTP header value): {0:?}")]
    InvalidTag(String),
    /// The cost limit is not a finite number (NaN / infinity).
    #[error("invalid cost limit (must be a finite number): {0}")]
    InvalidCostLimit(f64),
```

- [ ] **Step 4: Add the `apply_tt_headers` helper**

In `crates/client/src/lib.rs` (e.g. immediately after the `Error` enum or near the other free functions), add:
```rust
/// Attach the optional `X-TokenTrimmer-Tag` + `X-TokenTrimmer-Cost-Limit-Usd`
/// headers, validating both. Rejects a tag that isn't a legal HTTP header value
/// and a non-finite cost limit — surfaced at send time, before any network I/O.
pub(crate) fn apply_tt_headers(
    mut req: reqwest::RequestBuilder,
    tag: Option<&str>,
    cost_limit: Option<f64>,
) -> Result<reqwest::RequestBuilder, Error> {
    if let Some(tag) = tag {
        let value = reqwest::header::HeaderValue::from_str(tag)
            .map_err(|_| Error::InvalidTag(tag.to_string()))?;
        req = req.header("X-TokenTrimmer-Tag", value);
    }
    if let Some(limit) = cost_limit {
        if !limit.is_finite() {
            return Err(Error::InvalidCostLimit(limit));
        }
        req = req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
    }
    Ok(req)
}
```

- [ ] **Step 5: Run the unit test (helper) to confirm it passes**

Run: `cargo test -p tt-client apply_tt_headers 2>&1 | tail -10`
Expected: PASS — `apply_tt_headers_accepts_valid_and_rejects_invalid` green. (`invalid_tag_fails_before_any_request` still FAILS — the send path doesn't call the helper yet.)

- [ ] **Step 6: Wire `ChatBuilder::send` + `::stream` (lib.rs)**

In `crates/client/src/lib.rs`, in `send` (~line 406) replace:
```rust
        if let Some(tag) = &self.tag {
            req = req.header("X-TokenTrimmer-Tag", tag);
        }
        if let Some(limit) = self.cost_limit {
            req = req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
        }
```
with:
```rust
        let req = apply_tt_headers(req, self.tag.as_deref(), self.cost_limit)?;
```
Apply the identical replacement in `stream` (~line 452). (`req` was `let mut req = …`; after this it no longer needs `mut` if nothing else mutates it — drop `mut` if clippy flags `unused_mut`, or keep it if a later `.header(...)`/reassignment remains. Adjust to satisfy `-D warnings`.)

- [ ] **Step 7: Wire `EmbedBuilder::send` (embeddings.rs)**

In `crates/client/src/embeddings.rs` (~line 118) replace:
```rust
        if let Some(limit) = self.cost_limit {
            http_req = http_req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
        }
```
with:
```rust
        let http_req = crate::apply_tt_headers(http_req, None, self.cost_limit)?;
```
(EmbedBuilder has no `tag` → `None`. Drop `mut` on `http_req`'s `let` if clippy flags it.)

- [ ] **Step 8: Wire `send_round` (tools.rs)**

In `crates/client/src/tools.rs` (~line 132) replace:
```rust
    if let Some(t) = tag {
        req = req.header("X-TokenTrimmer-Tag", t);
    }
    if let Some(limit) = cost_limit {
        req = req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
    }
```
with:
```rust
    let req = crate::apply_tt_headers(req, tag, cost_limit)?;
```
(`tag` is `Option<&str>`, `cost_limit` is `Option<f64>` — pass through. Drop `mut` on `req`'s `let` if clippy flags it.)

- [ ] **Step 9: Run the full crate tests**

Run: `cargo test -p tt-client 2>&1 | tail -20`
Expected: PASS — both new tests + all existing tests (incl. the tag/cost_limit happy-path httpmock tests, which use valid values) green.

- [ ] **Step 10: fmt + clippy**

Run: `cargo fmt --check -p tt-client 2>&1 | tail -3` → no diff (if drift: `cargo fmt -p tt-client`, re-check).
Run: `cargo clippy -p tt-client --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean | head` → none (watch for `unused_mut` on the rewritten `let req`/`let http_req` — drop `mut` as needed).

- [ ] **Step 11: Commit (stage only the three files)**

```bash
git add crates/client/src/lib.rs crates/client/src/embeddings.rs crates/client/src/tools.rs
git commit -m "fix(client): validate tag + cost_limit headers, fail typed pre-network

reqwest stashed an invalid-tag conversion error and surfaced it opaquely at
send (and a future HeaderValue::from_str().unwrap() would panic); a non-finite
cost_limit was sent as NaN/inf with no client guard. Add a shared apply_tt_headers
helper that validates both and returns Error::InvalidTag / Error::InvalidCostLimit
before any network I/O; reuse it at all four header-injection sites.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (before finishing the branch)
```bash
cargo test -p tt-client 2>&1 | tail -10
cargo fmt --check -p tt-client
cargo clippy -p tt-client --all-targets -- -D warnings 2>&1 | grep -E "warning:|error:" | grep -v auto-clean
```
All green / empty. **Stage only the three changed files** (the working tree also carries a `rust_out` junk file — do NOT stage it).

## Notes for the implementer
- `HeaderValue::from_str` is the correct validity check for the tag — it rejects exactly the bytes (CR/LF, control, non-visible-ASCII) that would otherwise make reqwest fail at send or a future `.unwrap()` panic.
- `apply_tt_headers` is `pub(crate)`; `lib.rs` calls it bare, `embeddings.rs`/`tools.rs` call `crate::apply_tt_headers`.
- All four sites already return `Result<_, Error>` and use `?`, so `apply_tt_headers(...)?` slots in directly.
- Do NOT change the fluent `tag()`/`cost_limit()` setters (they stay `-> Self`); validation lives at send time, like `MissingModel`.
- Finite-negative cost_limit is intentionally NOT rejected (out of scope).
