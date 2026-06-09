# SDK parity + correctness (batch 7i) — Design

**Status:** approved (2026-06-09)
**Date:** 2026-06-09
**Slice:** Audit-remediation, public repo. Closes the three `pub-sdks` `gap/medium` findings as one slice, and fixes the two latent correctness footguns the §pub-sdks overview flags so the new tests are honest.

## Findings addressed
1. **[gap/medium] Large parity gap — SDKs expose none of the Rust client's cost-control surface** (checklist L431). Add `X-TokenTrimmer-Cost-Limit-Usd` and `X-TokenTrimmer-Cache` convenience to both SDKs (Rust `tt-client` already has `cost_limit()`; the gateway documents both headers as Honored).
2. **[gap/medium] examples/ directory is empty** (L436). Populate with runnable py + ts + rust examples.
3. **[dx/medium] No tests ship with either SDK** (L441). Add Python (respx) + TypeScript (vitest) suites.

Plus the two footguns from the §pub-sdks overview prose (not separate line items): Python's `_MetaStash` thread-local mechanism and TS streaming returning the parsed body instead of the stream. Finding #3 explicitly asks for a concurrency-isolation test "(will currently fail for Python)", so honest tests require fixing these.

## Scope decisions (resolved during brainstorming)
- **Full scope:** fix both footguns so the concurrency + streaming tests genuinely pass (not document-and-defer).
- **Examples verified by compile/typecheck/lint in CI, not run live.** Behavioral coverage lives in the mocked unit tests. No gateway/network coupling.

## Python (`sdk-python/tokentrimmer/client.py`)
- **Parity:** in the `create` wrap, lift `tt_cost_limit` → `X-TokenTrimmer-Cost-Limit-Usd` (stringified float) and `tt_cache` → `X-TokenTrimmer-Cache`, in the same block as the existing `tt_tag`. Validate `tt_cache` against the documented request-override set `{bypass, force-write, read-only, disabled}`; raise `ValueError` on anything else. (Note: these are the *request-override* values from API-reference §6.1, distinct from the *response* cache-status values.)
- **Footgun fix (race-free metadata):** remove `_MetaStash`, `_capture_meta`, and the custom `http_client` event-hook injection entirely. The wrapped `create` instead calls `self.chat.completions.with_raw_response.create(...)`, reads `X-TokenTrimmer-*` off `.headers`, parses into `TokenTrimmerMeta`, and attaches `.tt` to the `.parse()` result. No process/thread-global mutable state → correct under threads and retries. The constructor no longer needs to build an httpx client; a caller-supplied `http_client` still passes through to `super().__init__`.
- **Streaming:** when `stream=True`, pass straight through to the original `create` (return the `Stream`, no `.tt` attach), matching TS. Documented as a known limitation.

## TypeScript (`sdk-typescript/src/index.ts`)
- **Parity:** lift `ttCostLimit` → `X-TokenTrimmer-Cost-Limit-Usd` and `ttCache` → `X-TokenTrimmer-Cache`, same validation set; throw on an invalid `ttCache`.
- **Footgun fix (streaming):** if `body.stream === true`, return `originalCreate(rest, opts)` untouched (a `Stream`) — do **not** call `.withResponse()` and return `data`. Non-streaming keeps the `.withResponse()` → parse-headers → attach-`.tt` path. Replace the blanket `as any` return with: non-streaming → `WithTokenTrimmerMeta<ChatCompletion>`, streaming → the stream type, via an overload or a typed conditional.

## Examples (`examples/`, grouped by language)
- `examples/python/{cost_attribution,streaming,self_hosted}.py`
- `examples/typescript/{cost-attribution,streaming,self-hosted}.ts`
- `examples/rust/` via `crates/client/examples/cost_attribution.rs` (fluent `chat().tag().cost_limit().send()` + `CostInfo`/`savings_pct()`)
- `examples/README.md` — how to run each, expected `.tt` output, key/gateway requirement.
Each example: non-streaming cost-attribution prints `.tt`; streaming reads terminal usage and shows the no-`.tt`-on-stream caveat; self-hosted shows `base_url`/`baseURL` override + `tt_cost_limit`/`tt_cache`.

## Tests
- **Python (respx)** — mock the gateway HTTP layer: header parse incl. non-numeric → `None`; `max_tokens` default injection + explicit override (incl. `max_completion_tokens`); `tt_tag`/`tt_cost_limit`/`tt_cache` → headers; invalid `tt_cache` raises `ValueError`; **concurrency-isolation** (N threads issuing interleaved requests with distinct cost headers each observe their own `.tt`); a `stream=True` call returns a stream and does not raise.
- **TypeScript (vitest + injected stub `fetch`)** — pass a stub `fetch` via `ClientOptions` returning canned bodies + `x-tokentrimmer-*` headers (no msw/nock dependency). Same matrix: parse incl. non-finite → `null`; `max_tokens` default/override; the three lifts + invalid-`ttCache` rejection; a `stream:true` call returns the stream (not parsed `data`).

## CI (`.github/workflows/sdks.yml`)
Path-filtered to `sdk-python/**`, `sdk-typescript/**`, `examples/**`, `crates/client/**`:
- Python: `python -m py_compile` the examples + `pytest` the suite (install `.[test]`).
- TypeScript: `npm ci` + `tsc --noEmit` (src and examples) + `vitest run`.
- Rust: `cargo build --example cost_attribution -p tt-client`.

## Out of scope (noted, not done)
- Streaming `.tt` cost metadata — the gateway emits terminal usage in-band on the SSE stream; surfacing it as `.tt` on a stream is a separate feature. Documented as a known limitation.
- Async (`AsyncOpenAI`) subclass — neither SDK ships one; not adding.
- Tool-calling / embeddings convenience wrappers — the OpenAI SDK already exposes these; only the `X-TokenTrimmer-*` surface is the parity gap.
- Adding `.cache()` to the Rust `tt-client` — optional symmetry, deferred; Rust users can set the header directly. The parity direction here is py/ts → rust.

## Verification
- `pytest` (sdk-python) and `vitest run` (sdk-typescript) green, including the concurrency + streaming cases.
- `tsc --noEmit` clean for src + TS examples; `python -m py_compile` clean for py examples; `cargo build --example cost_attribution -p tt-client` clean.
- Three audit checklist entries flipped to DONE.
