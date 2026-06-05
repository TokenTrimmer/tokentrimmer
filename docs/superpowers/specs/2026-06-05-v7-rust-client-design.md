# V7 — `tt-client` Rust SDK Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V7 (final roadmap area — "AI client"). A reusable Rust SDK over the gateway.
**Depends on:** `tt-shared` (request/response/message types) — merged.

## Goal

A small, typed Rust crate (`tt-client`) that any Rust app embeds to call the TokenTrimmer gateway and get **typed cost/savings** back. The gateway is OpenAI-compatible, so the SDK's added value over "just set base_url" is: typed `CostInfo` parsed from the `x-tokentrimmer-*` headers, ergonomic request building, and the `X-TokenTrimmer-Tag`/`-Route` extensions — first-class, not raw header strings. Non-streaming this slice.

## Architecture

New workspace crate `crates/client` (package `tt-client`). Library only (no binary). Reuses `tt-shared` for `Message`/`ChatCompletionResponse`/`Usage`; builds the request body itself (so users aren't blocked by `ChatCompletionRequest`'s lack of `Default`).

### `crates/client/src/lib.rs`
- **`pub struct Client { http: reqwest::Client, base: String, key: String }`** — `Client::new(base: impl Into<String>, key: impl Into<String>)` (builds a `reqwest::Client`), `Client::with_http_client(http, base, key)`.
- **Message helpers** (ergonomics, tested): `pub fn user(content)`, `system(content)`, `assistant(content)` → `tt_shared::messages::Message` (re-export `Message` too).
- **`Client::chat(&self) -> ChatBuilder`** — fluent builder: `.model(impl Into<String>)`, `.messages(Vec<Message>)` / `.message(Message)`, `.max_tokens(u32)`, `.temperature(f32)`, `.tag(impl Into<String>)`, `.route(impl Into<String>)`, then `.send().await`.
- **`build_body(model, messages, max_tokens, temperature) -> serde_json::Value`** (pure, tested): the `{model, messages, stream:false, max_tokens?, temperature?}` body.
- **`ChatBuilder::send(self) -> Result<ChatOutcome, Error>`**: `POST {base}/v1/chat/completions`, bearer auth, set `X-TokenTrimmer-Tag`/`-Route` when present, json body; on non-2xx → `Error::Status{status, body}`; else parse `ChatCompletionResponse` (`Error::Decode`) + `parse_cost(headers)`.
- **`pub struct CostInfo`** — all `Option` (a header may be absent): `cost_usd: Option<f64>`, `saved_usd: Option<f64>`, `baseline_cost_usd: Option<f64>`, `model_used: Option<String>`, `provider: Option<String>`, `trace_id: Option<String>`, `cache: Option<String>`.
- **`parse_cost(headers: &reqwest::header::HeaderMap) -> CostInfo`** (pure, tested): reads `x-tokentrimmer-cost-usd`/`-saved-usd`/`-baseline-cost-usd` (f64), `-model-used`/`-provider`/`-trace-id`/`-cache` (string). Missing → `None`; un-parseable → `None`.
- **`pub struct ChatOutcome { pub response: ChatCompletionResponse, pub cost: CostInfo }`** with `pub fn text(&self) -> Option<&str>` (first choice's text content) and `pub fn savings_pct(&self) -> Option<f64>`.
- **`#[derive(thiserror::Error)] pub enum Error { Request(reqwest::Error), Status { status: u16, body: String }, Decode(reqwest::Error) }`** + `pub type Result<T> = std::result::Result<T, Error>`.
- `lib.rs` re-exports: `Client`, `ChatBuilder`, `ChatOutcome`, `CostInfo`, `Error`, `Message`, the message helpers.

### Workspace
- Add `"crates/client"` to the root `Cargo.toml` `members`.
- `crates/client/Cargo.toml`: `tt-shared.workspace = true`, `reqwest.workspace = true`, `serde_json.workspace = true`, `thiserror.workspace = true`; dev-deps `tokio` + `httpmock` (workspace); `[lints] workspace = true`.

## Usage (doc example, tested via httpmock)
```rust
let client = tt_client::Client::new("https://api.tokentrimmer.com", "tt_live_…");
let out = client.chat()
    .model("gpt-4o-mini")
    .messages(vec![tt_client::user("Summarize TokenTrimmer in one line.")])
    .tag("feature=demo")
    .send().await?;
println!("{}", out.text().unwrap_or(""));
println!("cost ${:?} saved ${:?}", out.cost.cost_usd, out.cost.saved_usd);
```

## Testing
- **`parse_cost`**: a `HeaderMap` with the cost headers → the right `Some` values; missing headers → `None`; a non-numeric cost header → `None`.
- **`build_body`**: includes `model`/`messages`/`stream:false`; `max_tokens`/`temperature` present only when set.
- **message helpers**: `user("hi")` → `Message::User{content:Text("hi")}` etc.
- **`ChatOutcome::{text, savings_pct}`**: from a synthetic response/cost.
- **Integration (httpmock)**: a mock `/v1/chat/completions` returning a `ChatCompletionResponse` JSON + cost headers (+ assert the request carried `X-TokenTrimmer-Tag`) → `send()` yields the parsed `ChatOutcome` with `text()` + `cost.cost_usd`; a 500 → `Error::Status`.
- `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt`; `cargo deny`; `cargo test -p tt-client`.

## Out of Scope (later)
- **Streaming** (`chat_stream` surfacing the terminal `tokentrimmer.usage` SSE event) — a follow-up V7 slice.
- **Embeddings** / other endpoints, and **tool-calling** convenience.
- **CLI adoption** (refactoring `tt chat`/`tt advise` onto `tt-client`) — a separate cleanup.
- Publishing to crates.io; the TS/Python SDKs (the docs' wrappers) — separate cross-ecosystem work.
