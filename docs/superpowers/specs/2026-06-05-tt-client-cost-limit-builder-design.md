# tt-client `.cost_limit()` Builder Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** Follow-on F2. Adds a `.cost_limit()` builder option to the `tt-client` SDK.
**Depends on:** the gateway `X-TokenTrimmer-Cost-Limit-Usd` support (#41) merged.

## Goal

Let SDK callers cap a request's estimated cost: `client.chat().model(…).cost_limit(0.05)…`
sends `X-TokenTrimmer-Cost-Limit-Usd`, and the gateway returns `402
cost_limit_exceeded` when the estimate exceeds it. Mirrors the existing `.tag()`
builder option exactly.

## Architecture

All in `crates/client`. The `cost_limit` is a `ChatBuilder` option threaded onto
every terminal request path the SDK has, identical to how `tag` is already
handled.

### Builder option (`lib.rs`)
- `ChatBuilder` gains `cost_limit: Option<f64>`.
- `Client::chat()` initialises it to `None`.
- New setter (next to `tag`):
  ```rust
  /// `X-TokenTrimmer-Cost-Limit-Usd` — reject (402) if the gateway's estimated
  /// request cost exceeds `usd`.
  #[must_use]
  pub fn cost_limit(mut self, usd: f64) -> Self {
      self.cost_limit = Some(usd);
      self
  }
  ```

### Header injection (three paths)
The SDK builds a request in three places, each of which already injects
`X-TokenTrimmer-Tag`. Add the cost-limit header right after, when set:
```rust
if let Some(limit) = self.cost_limit {
    req = req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
}
```
- `ChatBuilder::send` (lib.rs) — uses `self.cost_limit`.
- `ChatBuilder::stream` (lib.rs) — uses `self.cost_limit`.
- `send_round` (tools.rs, the `run_tools` loop) — gains a `cost_limit: Option<f64>`
  parameter; `run_tools` destructures `cost_limit` from the builder and passes it
  to both the per-round and the forced-final `send_round` calls. Inside
  `send_round`, inject the header next to the existing `tag` injection. (`send_round`
  already carries `#[allow(clippy::too_many_arguments)]`.)

### Value formatting
The header value is `format!("{limit}")` — Rust's `f64` `Display` (e.g. `0.05`,
`0.001`, `0.000000001`); the gateway's `parse::<f64>` accepts it (incl. scientific
notation for extreme values). No client-side validation: the gateway already treats
non-positive limits as "no limit" (`cost_limit_from_header` filters `> 0.0`).

## Testing (`crates/client`, httpmock)

- **`send_sends_cost_limit_header`**: `.cost_limit(0.05).send()` → the mock matches
  on the request header `X-TokenTrimmer-Cost-Limit-Usd: 0.05`; asserts a successful
  round-trip. (httpmock `header("x-tokentrimmer-cost-limit-usd", "0.05")` matcher.)
  The header being **optional** is already covered by the existing `send` tests,
  which don't set `.cost_limit()` and pass against mocks that don't require it — no
  dedicated absence test needed.
- **`stream_sends_cost_limit_header`**: `.cost_limit(…).stream()` → the mock SSE
  endpoint matches the header; iterate one event to confirm the stream still works.
- **`run_tools_sends_cost_limit_header`**: `.cost_limit(…).run_tools(&exec)` with a
  mock that requires the header and returns an immediate text answer → the loop's
  request carries the header.
- **`cost_limit_402_surfaces_as_status`**: a mock returning `402` → `send()` yields
  `Error::Status { status: 402, .. }` (confirms the round-trip; no new error type
  needed).
- Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo test -p tt-client`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D
  warnings" cargo doc -p tt-client --no-deps`.

## Out of scope

- `embed()` cost-limit — `embed` is a plain method, not a builder; it folds into
  F3's embed enhancements (`dimensions`/`encoding_format`).
- Any new `Error` variant — a 402 is already `Error::Status`.
- Client-side validation / clamping of the limit value.
