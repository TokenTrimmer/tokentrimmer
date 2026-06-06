# `tt embed` CLI command — Design

**Status:** approved (design)
**Date:** 2026-06-06
**Slice:** Follow-on F4. Surfaces the tt-client `EmbedBuilder` (F3/#44) as a CLI command.
**Depends on:** `tt-client` `EmbedBuilder` (`.model/.input/.dimensions/.encoding_format/.cost_limit/.send`) merged.

## Goal

Let a user embed text from the terminal — `tt embed "hello world"` — and get a
one-line cost-aware summary, with `--json` for the raw vectors. Thin glue over
`tt_client::Client::embeddings()`; the network path is already covered by
tt-client's httpmock tests, so this slice's own tests are pure-unit.

## Command surface (`crates/cli/src/main.rs`)

A new `Command::Embed`, mirroring `Models`/`Advise`:

```
tt embed [INPUT...]
         [--model M] [--dimensions N] [--encoding-format F] [--cost-limit USD]
         [--json] [--tt-api-key K] [--tt-api-base B]
```

```rust
/// Embed text via the gateway and print a cost summary (or --json vectors).
Embed {
    /// Text to embed. One arg → single; many → a batch. Omit to read stdin.
    input: Vec<String>,
    /// Embedding model (default: text-embedding-3-small).
    #[arg(long)]
    model: Option<String>,
    /// Reduce output dimensions (Matryoshka models).
    #[arg(long)]
    dimensions: Option<u32>,
    /// Wire encoding format (e.g. "float" or "base64").
    #[arg(long)]
    encoding_format: Option<String>,
    /// Reject (402) if the estimated cost exceeds this many USD.
    #[arg(long)]
    cost_limit: Option<f64>,
    /// Print the full EmbeddingsResponse JSON to stdout (summary → stderr).
    #[arg(long)]
    json: bool,
    #[arg(long)]
    tt_api_key: Option<String>,
    #[arg(long)]
    tt_api_base: Option<String>,
},
```

Dispatch arm:
```rust
Command::Embed {
    input, model, dimensions, encoding_format, cost_limit, json,
    tt_api_key, tt_api_base,
} => {
    tt_cli::embed::run(
        input, model, dimensions, encoding_format, cost_limit, json,
        tt_api_key, tt_api_base,
    )
    .await?;
}
```

`pub mod embed;` added to `crates/cli/src/lib.rs`.

## `crates/cli/src/embed.rs`

```rust
pub async fn run(
    input: Vec<String>,
    model: Option<String>,
    dimensions: Option<u32>,
    encoding_format: Option<String>,
    cost_limit: Option<f64>,
    json: bool,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()>
```

Steps (mirrors `advise::run`):
1. `ResolvedContext::load(flag_key, flag_base)?` → `ctx.api_key_string().context("no API key — run `tt login` or set TT_API_KEY")?` → `base = ctx.base_url.trim_end_matches('/')` → `tt_client::Client::new(base, key)`.
2. **Assemble input** via the pure helper `assemble_input(args, stdin_text)` (below). `None` → `anyhow::bail!("no input — pass text as an argument or pipe it on stdin")`. Stdin is read with `std::io::read_to_string(std::io::stdin())` only when `args` is empty, and trimmed.
3. Build the request:
   ```rust
   let mut b = client
       .embeddings()
       .model(model.unwrap_or_else(|| DEFAULT_MODEL.to_string()))
       .input(input);
   if let Some(n) = dimensions { b = b.dimensions(n); }
   if let Some(f) = encoding_format { b = b.encoding_format(f); }
   if let Some(c) = cost_limit { b = b.cost_limit(c); }
   let out = b.send().await?;   // tt_client::Error → anyhow (402/missing-key/transport surface)
   ```
   `DEFAULT_MODEL = "text-embedding-3-small"`.
4. **Output**:
   - `json == true`: `println!("{}", serde_json::to_string_pretty(&out.response)?)` → **stdout**; `ui::note(&summary)` → **stderr**.
   - else: `println!("{summary}")` → **stdout**.

   `summary = format_embed_summary(&out, &requested_model)` (see below).

`tt_client::Error` implements `std::error::Error`, so `?` lifts it into `anyhow`
— a `402` cost-limit, missing key, or transport failure all surface as the
process error that `main.rs` already renders.

### Pure helpers (the unit-tested surface)

```rust
/// Build the embeddings input: 1 arg → Single, >1 → Batch; no args → the trimmed
/// stdin text as Single. Returns None when there is nothing to embed.
fn assemble_input(args: &[String], stdin_text: Option<&str>) -> Option<EmbeddingInput>;
```
- `args.len() == 1` → `Single(args[0])`.
- `args.len() > 1` → `Batch(args.to_vec())`.
- `args.is_empty()` → `stdin_text` trimmed; empty/None → `None`, else `Single(trimmed)`.

```rust
/// One-line styled summary, e.g.
/// "text-embedding-3-small · 2 embeddings × 1536 dims · $0.0002 · saved 75%".
fn format_embed_summary(out: &EmbedOutcome, requested_model: &str) -> String;
```
- model = `out.cost.model_used.as_deref().unwrap_or(requested_model)`.
- count = `out.response.data.len()`; "embedding" vs "embeddings" pluralized.
- dims = first row length (`out.response.data.first().map(|d| d.embedding.len())`); omit the `× N dims` segment when there are no rows.
- cost = `out.cost.cost_usd` → `$0.0002` (4-dp), omitted when `None`.
- savings = `saved_usd / baseline_cost_usd * 100` when both known and baseline > 0 → `saved 75%` (rounded, no decimals); omitted otherwise.
- Joined with ` {BULLET} ` and wrapped in `ui::muted()`.

## Testing (`crates/cli`, pure-unit — no network)

In `crates/cli/src/embed.rs` `#[cfg(test)]`:
- `assemble_input_single_arg` → `Single("hi")`.
- `assemble_input_multi_arg_batch` → `Batch(["a","b"])`.
- `assemble_input_stdin_fallback` → no args + `Some(" hi \n")` → `Single("hi")` (trimmed).
- `assemble_input_empty_is_none` → no args + `None`/`Some("  ")` → `None`.
- `format_embed_summary_full` → an `EmbedOutcome` with 2 rows of 3 dims + cost
  `0.0002` + saved `0.0003`/baseline `0.0004` → contains the model, `2 embeddings`,
  `× 3 dims`, `$0.0002`, `saved 75%`.
- `format_embed_summary_minimal` → 1 row, `cost`/`saved` all `None` → contains
  `1 embedding`, no `$`, no `saved`, falls back to `requested_model`.

Constructing an `EmbedOutcome` in tests is direct (both fields `pub`):
`EmbedOutcome { response: EmbeddingsResponse {…}, cost: CostInfo {…} }` using the
re-exported `tt_client::{EmbedOutcome, EmbeddingsResponse, EmbeddingData, CostInfo, Usage}`.

The real send/headers path is already covered by tt-client's httpmock suite — no
duplicate network test here.

## Gates
`cargo fmt`; `cargo clippy --workspace --all-targets -- -D warnings`;
`cargo test -p tt-cli`; `cargo deny check advisories`;
`RUSTDOCFLAGS="-D warnings" cargo doc -p tt-cli --no-deps`.

## Out of scope
- Streaming embeddings.
- Decoding `--encoding-format base64` output (passed through; the raw response
  is what `--json` prints).
- Reading multiple stdin lines as a batch — one positional arg per batch item is
  the batch path; stdin is a single-input convenience.
- Validating `--dimensions` against the model (the gateway/provider is the authority).
```
