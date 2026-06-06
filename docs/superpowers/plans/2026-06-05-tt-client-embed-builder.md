# tt-client EmbedBuilder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a fluent `EmbedBuilder` to `tt-client` exposing `dimensions`, `encoding_format`, and `cost_limit` on the embeddings call, mirroring `ChatBuilder`.

**Architecture:** New `Client::embeddings() -> EmbedBuilder` with setters + `send()`, in `crates/client/src/embeddings.rs`; the existing `embed(model, input)` becomes a thin delegate; a new `Error::MissingInput` variant in `lib.rs`. `send()` serializes the typed `EmbeddingsRequest` (so unset options are omitted) and injects the cost-limit header.

**Tech Stack:** Rust, reqwest, serde, httpmock (tests).

Spec: `docs/superpowers/specs/2026-06-05-tt-client-embed-builder-design.md`. Branch `tt-client-embed-builder` (off `main`, spec committed).

**Verified anchors (`crates/client`):**
- `Error` enum (`#[non_exhaustive]`) — lib.rs:225-242; `MissingModel` at :227-229.
- `embeddings.rs`: top import `use crate::{parse_cost, Client, CostInfo, EmbeddingInput, EmbeddingsResponse, Error, Result};` (line 6) + `use serde_json::json;` (line 4); `EmbedOutcome` (8-20); `impl Client { embed }` (22-63); `#[cfg(test)] mod tests` (66+) uses `json!` via `use super::*`.
- `EmbeddingsRequest { model, input, dimensions: Option<u32>, encoding_format: Option<String> }` — re-exported at the crate root (`crate::EmbeddingsRequest`); `dimensions`/`encoding_format` carry `#[serde(skip_serializing_if = "Option::is_none")]`.

---

### Task 1: Add `Error::MissingInput`

**Files:**
- Modify: `crates/client/src/lib.rs` (Error enum, ~227-229)

- [ ] **Step 1: Add the variant**

In `crates/client/src/lib.rs`, after the `MissingModel` variant (lib.rs:227-229), add:

```rust
    /// No model was set on the builder — call `.model(...)`.
    #[error("model is required — call `.model(...)`")]
    MissingModel,
    /// No input was set on the embeddings builder — call `.input(...)`.
    #[error("input is required — call `.input(...)`")]
    MissingInput,
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p tt-client`
Expected: builds (a new unused enum variant on a `#[non_exhaustive]` enum is fine — no warning).

- [ ] **Step 3: Commit**

```bash
git add crates/client/src/lib.rs
git commit -m "feat(tt-client): add Error::MissingInput"
```

---

### Task 2: `EmbedBuilder` + `embeddings()` + `embed()` delegate

**Files:**
- Modify: `crates/client/src/embeddings.rs` (imports, `impl Client`, new builder, tests)

- [ ] **Step 1: Update the imports**

In `crates/client/src/embeddings.rs`, replace the top two `use` lines (lines 4 + 6):

```rust
use serde_json::json;

use crate::{parse_cost, Client, CostInfo, EmbeddingInput, EmbeddingsResponse, Error, Result};
```

with (drop `serde_json::json` — non-test code no longer builds JSON by hand — and add `EmbeddingsRequest`):

```rust
use crate::{
    parse_cost, Client, CostInfo, EmbeddingInput, EmbeddingsRequest, EmbeddingsResponse, Error,
    Result,
};
```

- [ ] **Step 2: Replace the `impl Client { embed }` block with the builder**

In `crates/client/src/embeddings.rs`, replace the entire `impl Client { … pub async fn embed … }` block (lines 22-63) with:

```rust
impl Client {
    /// Start building an embeddings request:
    /// `client.embeddings().model(m).input(i).dimensions(256).send()`.
    #[must_use]
    pub fn embeddings(&self) -> EmbedBuilder<'_> {
        EmbedBuilder {
            client: self,
            model: String::new(),
            input: None,
            dimensions: None,
            encoding_format: None,
            cost_limit: None,
        }
    }

    /// Embed `input` with `model` — a convenience for
    /// `embeddings().model(model).input(input).send()`.
    ///
    /// # Errors
    /// See [`EmbedBuilder::send`].
    pub async fn embed(
        &self,
        model: impl Into<String>,
        input: EmbeddingInput,
    ) -> Result<EmbedOutcome> {
        self.embeddings().model(model).input(input).send().await
    }
}

/// Fluent builder for an embeddings request. See [`Client::embeddings`].
pub struct EmbedBuilder<'a> {
    client: &'a Client,
    model: String,
    input: Option<EmbeddingInput>,
    dimensions: Option<u32>,
    encoding_format: Option<String>,
    cost_limit: Option<f64>,
}

impl EmbedBuilder<'_> {
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
    #[must_use]
    pub fn input(mut self, input: EmbeddingInput) -> Self {
        self.input = Some(input);
        self
    }
    /// Reduce the embedding to `n` dimensions (Matryoshka models).
    #[must_use]
    pub fn dimensions(mut self, n: u32) -> Self {
        self.dimensions = Some(n);
        self
    }
    /// Wire encoding format (e.g. `"float"` or `"base64"`).
    #[must_use]
    pub fn encoding_format(mut self, format: impl Into<String>) -> Self {
        self.encoding_format = Some(format.into());
        self
    }
    /// `X-TokenTrimmer-Cost-Limit-Usd` — the gateway rejects with `402` if the
    /// estimated cost exceeds `usd`.
    #[must_use]
    pub fn cost_limit(mut self, usd: f64) -> Self {
        self.cost_limit = Some(usd);
        self
    }

    /// Send the request and return the vectors + cost.
    ///
    /// # Errors
    /// [`Error::MissingModel`] / [`Error::MissingInput`] (pre-flight),
    /// [`Error::Request`] on transport failure, [`Error::Status`] on a non-2xx
    /// response (carrying cost/trace), [`Error::Decode`] on an invalid body.
    pub async fn send(self) -> Result<EmbedOutcome> {
        if self.model.trim().is_empty() {
            return Err(Error::MissingModel);
        }
        let Some(input) = self.input else {
            return Err(Error::MissingInput);
        };
        let req = EmbeddingsRequest {
            model: self.model,
            input,
            dimensions: self.dimensions,
            encoding_format: self.encoding_format,
        };
        let mut http_req = self
            .client
            .http
            .post(format!("{}/v1/embeddings", self.client.base))
            .bearer_auth(&self.client.key)
            .json(&req);
        if let Some(limit) = self.cost_limit {
            http_req = http_req.header("X-TokenTrimmer-Cost-Limit-Usd", format!("{limit}"));
        }
        let resp = http_req.send().await.map_err(Error::Request)?;
        let cost = parse_cost(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
                cost: Box::new(cost),
            });
        }
        let response = resp
            .json::<EmbeddingsResponse>()
            .await
            .map_err(Error::Decode)?;
        Ok(EmbedOutcome { response, cost })
    }
}
```

- [ ] **Step 3: Add the test-module `json!` import + the new tests**

In the `#[cfg(test)] mod tests` block, add `use serde_json::json;` to the imports (after `use httpmock::prelude::*;`), since the top-level import was removed:

```rust
    use super::*;
    use crate::Client;
    use httpmock::prelude::*;
    use serde_json::json;
```

Then add these tests inside `mod tests`:

```rust
    #[tokio::test]
    async fn embeddings_builder_sends_dimensions_and_encoding_format() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_contains("\"dimensions\":256")
                .body_contains("\"encoding_format\":\"float\"");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(embeddings_body());
        });
        let client = Client::new(server.base_url(), "k");
        let out = client
            .embeddings()
            .model("text-embedding-3-small")
            .input(EmbeddingInput::Single("hi".into()))
            .dimensions(256)
            .encoding_format("float")
            .send()
            .await
            .unwrap();
        assert_eq!(out.vectors().count(), 2);
    }

    #[tokio::test]
    async fn embed_builder_sends_cost_limit_header() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .header("x-tokentrimmer-cost-limit-usd", "0.01");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(embeddings_body());
        });
        let client = Client::new(server.base_url(), "k");
        // The mock requires the header, so `.send()` only succeeds if it was sent.
        let out = client
            .embeddings()
            .model("text-embedding-3-small")
            .input(EmbeddingInput::Single("hi".into()))
            .cost_limit(0.01)
            .send()
            .await
            .unwrap();
        assert_eq!(out.vectors().count(), 2);
    }

    #[tokio::test]
    async fn embed_builder_missing_input_errors() {
        // dead base — no network because input is unset.
        let client = Client::new("http://127.0.0.1:1", "k");
        let result = client.embeddings().model("m").send().await;
        assert!(matches!(result, Err(Error::MissingInput)));
    }

    #[tokio::test]
    async fn embed_builder_missing_model_errors() {
        let client = Client::new("http://127.0.0.1:1", "k");
        let result = client
            .embeddings()
            .input(EmbeddingInput::Single("hi".into()))
            .send()
            .await;
        assert!(matches!(result, Err(Error::MissingModel)));
    }
```

- [ ] **Step 4: Run the SDK tests**

Run: `cargo test -p tt-client`
Expected: the four new tests pass alongside `embed_returns_vectors_and_cost` (the convenience delegate) and the full suite. (`embed_builder_sends_cost_limit_header`'s mock requires the header, so it only matches if the builder sends it; the `dimensions`/`encoding_format` test's mock requires both body substrings.)

- [ ] **Step 5: Commit**

```bash
git add crates/client/src/embeddings.rs
git commit -m "feat(tt-client): EmbedBuilder with dimensions/encoding_format/cost_limit"
```

---

### Task 3: Gates + finish the branch

**Files:** none (verification + PR)

- [ ] **Step 1: Format + clippy**

Run: `cargo fmt --all`
Then: `git diff --quiet || git commit -am "style: cargo fmt"`
Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: exit 0. (Confirms the dropped `serde_json::json` top-level import left no unused-import error, and the new builder is lint-clean.)

- [ ] **Step 2: Tests + advisories + docs**

Run: `cargo test -p tt-client`
Expected: all pass.
Run: `cargo deny check advisories`
Expected: ok.
Run: `RUSTDOCFLAGS="-D warnings" cargo doc -p tt-client --no-deps`
Expected: exit 0.

- [ ] **Step 3: Finish the branch**

Use the **superpowers:finishing-a-development-branch** skill: verify tests, push `tt-client-embed-builder`, create the PR (option 2). PR body: the fluent `EmbedBuilder` (dimensions/encoding_format/cost_limit), the `embed()` convenience delegate, and `Error::MissingInput`.

- [ ] **Step 4: Adversarial review + CI**

After the PR is open, run a Workflow-based adversarial review (lenses: request serialization parity with the gateway `EmbeddingsRequest` + cost-limit header; builder/API hygiene + the new error variant). Watch CI; fix confirmed findings before merge. Update roadmap memory (F3 done) when green.

---

## Notes for the implementer

- **Typed request serialization:** building `EmbeddingsRequest` and `.json(&req)` is why unset `dimensions`/`encoding_format` are omitted (their `skip_serializing_if`); don't reintroduce manual `json!` in `send`.
- **`json!` moved to tests:** the top-level `use serde_json::json;` is removed (non-test code no longer needs it); the test module gains its own `use serde_json::json;` for `embeddings_body()`.
- **Same-crate field access:** `EmbedBuilder::send` reaches `self.client.http/base/key` (private `Client` fields) because `embeddings.rs` is a child module of the crate root — same as `ChatBuilder`.
