# Honor `X-TokenTrimmer-Cache` request header (F7) — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** F7. Makes the gateway honor the `X-TokenTrimmer-Cache` request header on `/v1/chat/completions` (documented "Planned (not yet honored)").

## Goal

Let a caller override cache behavior per request via `X-TokenTrimmer-Cache: <mode>`, reusing the existing `CacheBehavior` machinery. Four documented modes:

| Header value | `do_lookup` | `do_insert` | Notes |
|---|---|---|---|
| `disabled` | false | false | neither read nor write |
| `read-only` | true | false | read warm cache, never write |
| `bypass` | false | true | skip lookup, still (re)write |
| `force-write` | true | true | write even for normally-ineligible requests (temp>0 / n>1 / seed) |

**force-write** overrides only the *eligibility* gate. The tool-call exclusion is unchanged: tool-call responses are never cached, even under force-write.

## Background (current behavior)

- `CacheBehavior { do_lookup, do_insert, ttl_secs }` (`chat.rs:354`) gates all four cache call sites (streaming + non-streaming L1/L2 reads, and inserts). It is resolved once at `chat.rs:646`: `let mut cache_behavior = CacheBehavior::resolve(&req);`.
- `CacheBehavior::resolve` (`chat.rs:365`): returns `{false,false}` early when `!is_cache_eligible(req)` (temperature>0 / top_p<1 / n>1 / seed); otherwise maps the request-**body** `tt_extras.cache` field (`CacheMode` Normal/Bypass/Refresh/ReadOnly) to the flags.
- Immediately after, `chat.rs:647-650` forces `{false,false}` when a matched privacy route set `disable_cache`.
- The insert sites additionally gate on `!response_has_tool_calls(&response)` (`chat.rs:342`, checked at insert time — `chat.rs:1215` non-stream, `sse.rs` stream).
- The `X-TokenTrimmer-Cache` **header** is read nowhere today. Docs: request-header row `docs/04-gateway-api-reference.md:407` ("Planned"); semantics prose §6.3 (lines 436-440).

## Architecture

All changes in `crates/core/src/routes/chat.rs` plus the docs.

### Header parser (`pub(crate)`)
```rust
/// `X-TokenTrimmer-Cache` → `(do_lookup, do_insert)` per the documented modes.
/// Absent/blank → `None`. Unknown value → `400` (the four values are documented).
fn cache_override_from_header(headers: &HeaderMap) -> ApiResult<Option<(bool, bool)>> {
    let Some(raw) = headers
        .get("x-tokentrimmer-cache")
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };
    let v = raw.trim().to_ascii_lowercase();
    if v.is_empty() {
        return Ok(None);
    }
    let pair = match v.as_str() {
        "disabled" => (false, false),
        "read-only" => (true, false),
        "bypass" => (false, true),
        "force-write" => (true, true),
        other => {
            return Err(ApiError::InvalidRequest(format!(
                "invalid X-TokenTrimmer-Cache value: {other} (expected disabled, read-only, bypass, or force-write)"
            )))
        }
    };
    Ok(Some(pair))
}
```

### Handler wiring (`chat.rs`, between 646 and 647)
```rust
let mut cache_behavior = CacheBehavior::resolve(&req);
// X-TokenTrimmer-Cache overrides the body tt_extras.cache decision (header beats
// body). force-write=(true,true) applied here overrides the eligibility gate that
// resolve() may have set; the tool-call exclusion at insert time is unaffected.
if let Some((lookup, insert)) = cache_override_from_header(&headers)? {
    cache_behavior.do_lookup = lookup;
    cache_behavior.do_insert = insert;
}
if route_disable_cache {
    cache_behavior.do_lookup = false;
    cache_behavior.do_insert = false;
}
```

### Precedence (falls out of the ordering)
1. `resolve(&req)` — eligibility + request-body `tt_extras.cache`.
2. Header override — **header beats body**.
3. Route `disable_cache` — **privacy route wins** over everything (a caller cannot force-cache a privacy-routed request).

`ttl_secs` is left as `resolve()` derived it (the header carries no TTL; only the body `tt_extras.cache.ttl_secs` does). It is irrelevant when `do_insert` is false.

## Testing

Integration (`crates/core/tests/cache_header.rs`, mirroring `l1_cache_hit.rs` — a `CountingProvider` + `with_l1`; no auth key needed). A helper builds a chat request with an optional `X-TokenTrimmer-Cache` header and optional `temperature`.

- **`header_disabled_skips_read_and_write`**: warm the cache (normal request), then a `disabled` request → provider called (not served from cache) and the response header is not `hit-l1`; a follow-up normal request still hits (the `disabled` request wrote nothing — i.e. it didn't refresh).
- **`header_read_only_reads_warm_cache`**: warm cache, then `read-only` → `hit-l1` (provider not called again).
- **`header_read_only_does_not_write`**: cold cache, `read-only` request → miss + provider called; a following normal request still misses (nothing was written).
- **`header_bypass_skips_read_but_writes`**: warm cache, `bypass` request → provider called (lookup skipped) but it refreshes; a following normal request → `hit-l1`.
- **`force_write_caches_ineligible_request`**: two identical `temperature:0.7` (ineligible) requests both with `force-write` → 2nd is `hit-l1`, provider called once (force-write both wrote despite ineligibility and read).
- **`force_write_does_not_cache_tool_calls`**: provider returns a tool-call response; two identical `force-write` requests → provider called twice (tool-call response never cached).
- **`header_beats_body_tt_extras`**: body `tt_extras.cache = {"mode":"disabled"}` + header `read-only`, against a warm cache → `hit-l1` (header read-only wins, performing the lookup the body would have skipped).
- **`invalid_header_value_is_400`**: `X-TokenTrimmer-Cache: nope` → `400`.
- **`privacy_route_disable_cache_beats_force_write`** (routing setup like `disable_cache.rs`): a `disable_cache` route + `force-write` header → provider called twice (route wins; nothing cached).

Unit (`chat.rs` `#[cfg(test)]`): `cache_override_from_header` — each of the four values → expected pair, trim/lowercase, empty/absent → None, unknown → Err.

Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p tt-core`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-core --no-deps`.

## Docs
- Flip `docs/04-gateway-api-reference.md:407` `X-TokenTrimmer-Cache` request-header row → "Honored".
- Fix §6.3 prose: `bypass` description currently says "the same as default but explicit" — that is inaccurate (default also reads); reword to "skip cache lookup, still write/refresh the result." Add a note under `force-write` that tool-call responses are still never cached.

## Out of scope
- Embeddings (`/v1/embeddings` responses are not cached).
- A per-request TTL via the header (TTL stays a body `tt_extras.cache.ttl_secs` concern).
- The other Planned request headers (Route/Fallback/Timeout — F8–F10).
- Negative-cache behavior is governed by the same `do_insert`/`do_lookup` flags and inherits the header override unchanged (no special handling).
