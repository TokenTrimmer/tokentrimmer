# tt-client header validation — Design

**Status:** approved (design)
**Date:** 2026-06-08
**Slice:** Audit-remediation (public repo, `crates/client`). Closes the finding *"tag() and cost_limit header values can fail/panic on invalid bytes"* (bug/medium, `pub-sdks`).

## Background (verified against current code)
The `tt-client` SDK injects two request headers from caller-supplied values, unvalidated, at **four** sites:
- `crates/client/src/lib.rs` `ChatBuilder::send` (~407) and `::stream` (~453): `req.header("X-TokenTrimmer-Tag", tag)` + `req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"))`.
- `crates/client/src/embeddings.rs` `EmbedBuilder::send` (~118): cost_limit only (`EmbedBuilder` has no `tag` field).
- `crates/client/src/tools.rs` `send_round` (~132, a free fn taking `tag: Option<&str>, cost_limit: Option<f64>`): both.

Problems:
- **tag:** `reqwest::RequestBuilder::header` stashes a conversion error and surfaces it as an opaque `Error::Request` only at `.send()`. A tag with CR/LF/control/non-visible-ASCII bytes (e.g. built from user/document data) yields a confusing error far from the cause, and any future `HeaderValue::from_str(tag).unwrap()` would panic.
- **cost_limit:** `format!("{limit}")` on a non-finite `f64` produces `"NaN"`/`"inf"`, sent blindly; the gateway rejects it, with no client-side guard on what is a **cost cap**.

Relevant types: `Error` is `#[non_exhaustive]` (lib.rs:237) with `MissingModel`/`MissingInput`/`Request`/`Status`/`Decode` — new variants are non-breaking. The `tag(self, impl Into<String>) -> Self` / `cost_limit(self, f64) -> Self` setters are fluent (can't return `Result`). `ChatBuilder.tag: Option<String>`, `cost_limit: Option<f64>`.

## Decision (user-approved)
Validate at **send/build time** (where `MissingModel` already fails, pre-network) via one shared helper. **Reject** an invalid tag with a typed error (don't silently sanitize — a mangled cost-attribution tag is worse than a clear error). **Error** on a non-finite cost_limit (don't skip — silently dropping a malformed *cost cap* leaves the caller falsely protected).

## Architecture

### 1. Shared helper — `crates/client/src/lib.rs`
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
`HeaderValue::from_str` is precisely the "is this a valid header value" check (rejects CR/LF, control bytes, non-visible-ASCII), so it's the right validator for `tag`.

### 2. `Error` enum — add two variants (`crates/client/src/lib.rs`, the `#[non_exhaustive] pub enum Error`)
```rust
    /// The `tag` is not a valid HTTP header value (control chars, CR/LF, …).
    #[error("invalid tag (not a valid HTTP header value): {0:?}")]
    InvalidTag(String),
    /// The cost limit is not a finite number (NaN / infinity).
    #[error("invalid cost limit (must be a finite number): {0}")]
    InvalidCostLimit(f64),
```

### 3. Replace the duplicated injection block at all four sites
- `lib.rs` `send` + `stream`: after building `req`/the request builder with `.json(&body)`, replace the two `if let Some(...)` blocks with:
  ```rust
  let req = apply_tt_headers(req, self.tag.as_deref(), self.cost_limit)?;
  ```
- `embeddings.rs` `send`: replace its `if let Some(limit)` block with:
  ```rust
  let http_req = crate::apply_tt_headers(http_req, None, self.cost_limit)?;
  ```
  (No tag field → `None`.)
- `tools.rs` `send_round`: replace its two `if let Some(...)` blocks with:
  ```rust
  let req = crate::apply_tt_headers(req, tag, cost_limit)?;
  ```
  (`tag` is already `Option<&str>`, `cost_limit` is `Option<f64>` — pass through.)

All four sites already return `Result<_, Error>` and use `?`, so the propagation is uniform. The fluent `tag()`/`cost_limit()` setters are unchanged.

## Error handling
- `Error::InvalidTag`/`Error::InvalidCostLimit` are returned pre-network from `send`/`stream`/`run_tools` (via `send_round`), exactly like `MissingModel`. In `run_tools` the validation fires on the first `send_round` (same tag/limit each round), so a bad value fails fast with no partial rounds.
- A *finite negative* `cost_limit` is left as-is (formats cleanly; the gateway decides) — out of scope; the finding is non-finite values.

## Testing (`crates/client` — mirror the existing httpmock + unit tests)
- **Unit tests on `apply_tt_headers`** (no network — a bare `reqwest::Client::new().get(url)` builder is fine to attach headers to):
  - valid tag + finite cost_limit → `Ok` (build succeeds).
  - tag containing `"\n"` (or a control char) → `Err(Error::InvalidTag(_))`.
  - `cost_limit = f64::NAN` → `Err(Error::InvalidCostLimit(_))`; `f64::INFINITY` → same.
  - `None`/`None` → `Ok` (no headers, no error).
- **httpmock pre-flight test:** a `ChatBuilder` with `.model(...).tag("bad\ntag")` → `.send()` returns `Err(Error::InvalidTag)` and the mock server received **no** request (mirrors the existing `MissingModel` no-network test). The existing tag/cost_limit happy-path httpmock tests (which use valid values) stay green.

Gates (public repo, scoped per ADR-012): `cargo test -p tt-client`; **`cargo fmt --check -p tt-client`** (public CI gates fmt); `cargo clippy -p tt-client --all-targets -- -D warnings` clean. Additive (`pub(crate)` helper + two `#[non_exhaustive]` Error variants) — no public-signature break, no workspace ripple; scope to `tt-client`.

## Out of scope
- Finite-negative cost_limit handling (the gateway rejects; not the reported bug).
- Client-side timeout / retry / User-Agent (the sibling `pub-sdks` opportunity finding — separate slice).
- Any change to the fluent `tag()`/`cost_limit()` setter signatures or the wire header names.
- The TypeScript/Python SDKs (separate `pub-sdks` parity finding).
