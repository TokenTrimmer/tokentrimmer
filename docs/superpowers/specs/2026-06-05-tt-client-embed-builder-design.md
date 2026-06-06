# tt-client `EmbedBuilder` Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** Follow-on F3. Brings `tt-client` embeddings to parity with the gateway's `EmbeddingsRequest`.
**Depends on:** `tt-client` `embed()` (#40), `.cost_limit()` (F2/#43) merged.

## Goal

Expose the embeddings options the SDK currently hardcodes off — `dimensions`,
`encoding_format` — plus the `X-TokenTrimmer-Cost-Limit-Usd` header (deferred from
F2), via a fluent `EmbedBuilder` mirroring the existing `ChatBuilder`. The simple
`embed(model, input)` convenience stays.

## Architecture

All in `crates/client/src/embeddings.rs` (+ one new `Error` variant in `lib.rs`).

### `EmbedBuilder` (mirrors `ChatBuilder`)
- `Client::embeddings(&self) -> EmbedBuilder<'_>` — entry point, parallel to `chat()`.
- ```rust
  pub struct EmbedBuilder<'a> {
      client: &'a Client,
      model: String,
      input: Option<EmbeddingInput>,
      dimensions: Option<u32>,
      encoding_format: Option<String>,
      cost_limit: Option<f64>,
  }
  ```
- `#[must_use]` setters (consume + return `self`, matching the chat builder style):
  - `model(impl Into<String>)`, `input(EmbeddingInput)`, `dimensions(u32)`,
    `encoding_format(impl Into<String>)`, `cost_limit(f64)`.
- `pub async fn send(self) -> Result<EmbedOutcome>`:
  1. `self.model.trim().is_empty()` → `Error::MissingModel`.
  2. `self.input` is `None` → `Error::MissingInput` (new variant).
  3. Build the **typed** request and serialize it:
     ```rust
     let req = EmbeddingsRequest {
         model: self.model,
         input,                 // unwrapped from the Some checked above
         dimensions: self.dimensions,
         encoding_format: self.encoding_format,
     };
     let mut http_req = self.client.http
         .post(format!("{}/v1/embeddings", self.client.base))
         .bearer_auth(&self.client.key)
         .json(&req);
     if let Some(limit) = self.cost_limit {
         http_req = http_req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
     }
     ```
     `EmbeddingsRequest`'s `#[serde(skip_serializing_if = "Option::is_none")]` on
     `dimensions`/`encoding_format` omits them when unset — no manual JSON.
  4. `parse_cost(headers)`; non-2xx → `Error::Status { status, body, cost: Box::new(cost) }`;
     success → decode `EmbeddingsResponse` (`Error::Decode`) → `EmbedOutcome { response, cost }`.

### `embed(model, input)` convenience
Re-implement as a thin delegate so existing callers/tests are unaffected:
```rust
pub async fn embed(&self, model: impl Into<String>, input: EmbeddingInput) -> Result<EmbedOutcome> {
    self.embeddings().model(model).input(input).send().await
}
```

### `Error::MissingInput`
Add to the `#[non_exhaustive] enum Error` (lib.rs), parallel to `MissingModel`:
```rust
/// No input was set on the embeddings builder — call `.input(...)`.
#[error("input is required — call `.input(...)`")]
MissingInput,
```
Non-breaking (the enum is already `#[non_exhaustive]`).

## Testing (`crates/client`, httpmock)

- **`embeddings_builder_sends_dimensions_and_encoding_format`**:
  `client.embeddings().model("text-embedding-3-small").input(Single("hi")).dimensions(256).encoding_format("float").send()`
  → a mock matching `body_contains("\"dimensions\":256")` and `body_contains("\"encoding_format\":\"float\"")`
  returns a vectors body → assert `out.vectors()` rows.
- **`embed_builder_sends_cost_limit_header`**: `.cost_limit(0.01)` → mock requiring
  `header("x-tokentrimmer-cost-limit-usd", "0.01")` → asserts success.
  (Options being **optional** is already covered by the existing
  `embed_returns_vectors_and_cost`, which sends none — no omission test needed.)
- **`embed_builder_missing_input_errors`**: `.model("m").send()` (no input) →
  `Err(Error::MissingInput)`, no network (dead base url).
- **`embed_builder_missing_model_errors`**: `.input(Single("hi")).send()` (no model)
  → `Err(Error::MissingModel)`, no network.
- **`embed_returns_vectors_and_cost`** (existing, convenience path) — stays green.
- Gates: `cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`;
  `cargo test -p tt-client`; `cargo deny check advisories`; `RUSTDOCFLAGS="-D
  warnings" cargo doc -p tt-client --no-deps`.

## Out of scope

- Streaming embeddings.
- Validating `dimensions` against the model (the gateway/provider is the authority).
- Giving the `embed(model, input)` convenience its own option params — callers who
  need options use the builder.
