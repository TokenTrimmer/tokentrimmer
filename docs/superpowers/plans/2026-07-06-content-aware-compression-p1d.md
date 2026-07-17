# Content-Aware Compression — Phase 1, Slice P1d (flywheel dataset export)

> **For agentic workers:** REQUIRED SUB-SKILL: `superpowers:subagent-driven-development`. Steps use checkbox (`- [ ]`) syntax. TDD per step.

**Goal:** Close the Phase-1→Phase-2 handoff. P1a left the raw before/after capture as a **metrics-only scaffold** (`capture.rs`: `record(kind, tokens_removed)` emits a `tracing` event, persists nothing). P1d makes that sink **real** (opt-in JSONL file) and ships the offline CLI that materials it into a versioned training corpus for Phase 2's learned compressor. ZDR-respecting throughout: capture is OFF by default; the export refuses any pair not marked opted-in.

**Architecture (owner-approved 2026-07-06):** capture writes JSONL to `TT_COMPRESS_CAPTURE_PATH` (only when `TT_COMPRESS_CAPTURE=1` + a path is set); `tt export compress-corpus --input <capture.jsonl> --output <corpus.jsonl>` is a pure offline transformation (mirrors `tt verify-bundle`'s offline purity). No DB table, no network hop, no per-org flag in P1d (the instance env IS the opt-in; a per-org flag is a later cloud concern noted in the spec).

**Tech stack:** Rust (`tt-core` capture, `tt-cli` export), `serde_json` (JSONL), `chrono` (already a workspace dep). No new deps. No migration (P1d adds a sink + a CLI, not a cost field). `cargo test --workspace --no-run` is the field-ripple gate; the existing 0033 columns are untouched.

## Global constraints (verbatim from the spec)
- **Opt-in, OFF by default** — `TT_COMPRESS_CAPTURE` gates capture; `TT_COMPRESS_CAPTURE_PATH` names the sink. Unset/empty either → no capture, zero overhead (the hot path never opens a file).
- **ZDR** — capture records ONLY when the instance opted in; every record carries `capture_opted_in: true` (it cannot exist otherwise). The export refuses any record missing/contradicting this.
- **No new lossy behavior** — P1d does NOT change what gets compressed, only what gets *recorded* when capture is ON. The compression path is byte-identical to P1c with capture OFF.
- **Verdict honesty** — the `gate_committed` field is the ONLY verdict P1d can attach: `true` means the lossy gate trusted the class at commit time (structural backends: always `true`). The richer *paired recall-of-baseline* verdict is a Phase-2 concern (it runs against the *response*, which `content_compress` never sees). The corpus header states this explicitly so Phase 2 doesn't mislabel.
- **Hot-path discipline** — capture is a blocking `std::fs::OpenOptions::append` write, ONLY on the opt-in path. The gateway is async; file append is fast + this path is off by default. No buffering/threading in P1d (a/background-writer is a later optimization if capture sees real use).

## File structure
- Modify `crates/core/src/content_compress/capture.rs` — make `record` real (JSONL append) + add `record_pair` (before/after) + the `CaptureRecord` schema.
- Modify `crates/core/src/content_compress/structural.rs` — thread a `CaptureCtx` through `compact_block` → `compact_content` → `compact_in_place` → `apply`; call `capture::record_pair` at the `Some(result)` point (before + after co-exist).
- Modify `crates/core/src/passes/mod.rs` — `ContentCompressPass` carries the `CaptureCtx`; `content_compress_with_gates` threads it.
- Modify `crates/core/src/routes/chat.rs` — build the `CaptureCtx` from `RequestContext` + `PassContext` + the dominant kind; pass into the pipeline.
- Create `crates/cli/src/compress_corpus.rs` — the export: read capture JSONL → emit versioned corpus JSONL; ZDR refuse.
- Modify `crates/cli/src/main.rs` — add `Command::Export { action: ExportAction::CompressCorpus { input, output } }` (or a top-level `CompressCorpusExport`; pick whichever matches the CLI's command grouping).

---

## SLICE P1d

**Read first:** `crates/core/src/content_compress/capture.rs` (the scaffold to make real); `structural.rs:compact_block` (the capture point — `s`=before, `result`=after); `crates/cli/src/bundle.rs` (the offline-pure-artifact pattern to mirror); `crates/cli/src/main.rs` `Command::VerifyBundle` dispatch + the `Commands` enum. `RequestContext` (`crates/shared/src/context.rs:49` — `trace_id`, `org_id`). `crates/core/Cargo.toml` (chrono + serde_json already deps).

### Task D1: the `CaptureRecord` schema + a real JSONL sink
**Files:** `crates/core/src/content_compress/capture.rs`.
**Produces:** `pub struct CaptureRecord { schema_version: u32, capture_opted_in: bool, kind: String, content_kind_before: String, before: String, after: String, tokens_before: u32, tokens_after: u32, tokens_removed: u32, gate_committed: bool, org_id: String, trace_id: String, model: String, provider_id: String, ts: String }` (`#[derive(Serialize)]`); `pub fn capture_path() -> Option<PathBuf>` (reads `TT_COMPRESS_CAPTURE_PATH` once, cached via `OnceLock`); `pub fn record_pair(rec: &CaptureRecord)` — when `capture_enabled()` AND a path is set, append one JSON line (`serde_json::to_writer` on an `OpenOptions::new().create(true).append(true).open(path)`); else no-op. Keep the existing `record(kind, tokens_removed)` as a thin caller of the new path (back-compat with the P1a call site until D4 rewires it). Schema version `1`.

- [ ] **Step 1: Write failing tests.** (a) `record_pair` with capture OFF (no env) is a no-op (no file created). (b) With `TT_COMPRESS_CAPTURE=1` + `TT_COMPRESS_CAPTURE_PATH` set to a temp file, `record_pair` appends one JSONL line that round-trips via `serde_json::from_str` to the same `CaptureRecord` (assert `capture_opted_in==true`, the before/after string fields, `kind`, etc.). (c) Two `record_pair` calls append two lines. NOTE: the `OnceLock`-cached `capture_enabled()`/`capture_path()` can't be flipped mid-process — use `#[serial]` OR (cleaner) factor the *sink* behind a function that takes the path as a param, so the env-gate and the write are separately testable. Prefer the latter (no test-dependency on `serial_test`).
- [ ] **Step 2:** `cargo test -p tt-core --lib content_compress::capture 2>&1 | tail -5` → FAIL.
- [ ] **Step 3: Implement** the `CaptureRecord` struct + `record_pair` (the sink is `fn write_pair(path: &Path, rec: &CaptureRecord) -> io::Result<()>`, fully injectable for tests) + the env-gated `capture_path()` wrapper. `record_pair` reads `capture_enabled()` + `capture_path()`; if either is missing → return (no-op, no error). On a write error → `tracing::warn!` and continue (capture must NEVER break a request). `serde_json` is already a core dep.
- [ ] **Step 4:** run → PASS. **Step 5:** `cargo fmt -p tt-core` + clippy + commit (`feat(core): P1d flywheel — JSONL capture sink + CaptureRecord schema`).

### Task D2: thread `CaptureCtx` through the pass to the capture point
**Files:** `crates/core/src/content_compress/structural.rs` + `crates/core/src/passes/mod.rs`.
**Produces:** `pub struct CaptureCtx { org_id: String, trace_id: String, model: String, provider_id: String }` (the join keys + the tokenizer context); `ContentCompressPass { capture: Option<Arc<CaptureCtx>>, .. }` (default `None` = today's behavior); `with_gates` gains a `capture: Option<Arc<CaptureCtx>>` param OR a new `with_gates_and_capture(gate, capture)` builder (prefer the latter to keep `with_gates` stable for existing callers — grep-verify no other caller breaks). `compact_block` gains a `capture: Option<&CaptureCtx>` param; at the `Some(result)` return point, if `capture.is_some()` AND `capture_enabled()`, call `capture::record_pair` with `before=s, after=result, gate_committed=true` (the block compacted; for structural kinds gate_committed is true; lossy kinds only reach here when the gate trusted the class — also true). `tokens_before`/`tokens_after` are NOT computed in the pass (the pass self-measures whole-tail delta only); record `0`/`0` as placeholders and let the EXPORT compute token counts offline from the captured before/after text (cleaner: the tokenizer is `tt_tokenize`, available to the CLI too, and computing per-block in the hot path is wasteful when the ratio is what matters).

- [ ] **Step 1: Write failing tests.** (a) A `compact_block` call with capture ON + a `CaptureCtx` + a temp `TT_COMPRESS_CAPTURE_PATH` appends a record with the right `before`/`after`/`kind`/`org_id`/`trace_id`; (b) capture OFF → no record; (c) `compact_block` returns the SAME `Some(result)` whether capture is on or off (capture is observability, not a behavior change). Reuse the existing `AlwaysCommitGate` + `code_blob()`/`prose` fixtures from the structural test module.
- [ ] **Step 2:** tests FAIL. **Step 3: Implement** — thread the param, add `with_gates_and_capture`, wire the `record_pair` call.
- [ ] **Step 4:** PASS. **Step 5:** `cargo test --workspace --no-run` EXIT 0 (the `with_gates` signature is unchanged → no caller ripple; the new builder is additive). fmt + clippy + commit.

### Task D3: wire the `CaptureCtx` from `chat.rs prepare()`
**Files:** `crates/core/src/routes/chat.rs`.
**Produces:** in `prepare()`, when `route_content_compress` is set, build `CaptureCtx { org_id: ctx.org_id.to_string(), trace_id: ctx.trace_id.to_string(), model: pass_cx.model.into(), provider_id: pass_cx.provider_id.into() }` (wrap in `Arc`), pass to `PassPipeline::content_compress_with_gates_and_capture(state.summary_gate.clone(), Some(capture_ctx))`. Remove the now-redundant `capture::record(kind, tokens_removed)` call at line ~3262 (the per-block `record_pair` supersedes it — the kind + tokens are both in the per-block record). Keep the `dominant_compactable_kind` call for the metrics-plane `content_compress_kind` column (unchanged).

- [ ] **Step 1: Write a failing integration-shape test** that asserts: when capture is OFF (the default), a content_compress request is byte-identical with/without the `CaptureCtx` threaded (i.e., threading is a no-op behaviorally). Hard to assert the JSONL side-effect from a unit test without env; assert the no-op + rely on the D2 tests for the sink. 
- [ ] **Step 2-5:** implement, `cargo test -p tt-core --lib` green, fmt + clippy, `cargo test --workspace --no-run` EXIT 0, commit.

### Task D4: the `tt export compress-corpus` CLI (offline-pure)
**Files:** `crates/cli/src/compress_corpus.rs` + `crates/cli/src/main.rs` (`Commands` enum + dispatch).
**Produces:** `pub const CORPUS_SCHEMA_VERSION: u32 = 1;` `pub struct TrainingCorpus { schema_version: u32, tool_version: String, produced_at: String, note: String, pairs: Vec<CorpusPair> }` + `pub struct CorpusPair { kind, before, after, tokens_before, tokens_after, tokens_removed, gate_committed, org_id, trace_id, model, provider_id, ts }` (`#[derive(Serialize)]`); `pub fn run_export(input: &Path, output: &Path) -> Result<()>` reads the capture JSONL line-by-line, for EACH line: `serde_json::from_str` → `CaptureRecord`; **refuse** any record where `capture_opted_in != true` (return an error naming the line + `trace_id` — ZDR); compute `tokens_before`/`tokens_after` via `tt_tokenize::estimate_tokens_for_model(provider_id, model, &before/after)` (the capture recorded 0/0); collect into `CorpusPair`; write `TrainingCorpus` as pretty JSON to `output`. The corpus `note` states the verdict-honesty caveat (gate verdict only; paired quality verdict is Phase-2). Mirrors `bundle.rs`'s schema-version refusal + offline purity (no network/DB).

- [ ] **Step 1: Write failing tests.** (a) A capture file with N well-formed opted-in records → `run_export` produces a corpus with `schema_version==1`, `pairs.len()==N`, each pair's `tokens_before`/`after` match a `tt_tokenize` recomputation, and the corpus JSON round-trips. (b) A capture file containing a record with `capture_opted_in: false` → `run_export` returns `Err` naming the offending `trace_id` and writes NO output (ZDR refuse). (c) A malformed JSONL line → `Err` naming the line. (d) An empty capture file → an empty corpus (not an error).
- [ ] **Step 2:** tests FAIL. **Step 3: Implement** `compress_corpus.rs`. (d) is the empty-file edge: treat as zero pairs, not an error.
- [ ] **Step 4:** PASS. **Step 5:** `cargo fmt -p tt-cli` + clippy + commit.

### Task D5: the `Commands` enum wiring + dispatch
**Files:** `crates/cli/src/main.rs`.
**Produces:** a new `Command::Export { action: ExportAction::CompressCorpus { input, output } }` (or `Command::CompressCorpusExport { input, output }` — match the existing `Commands` grouping style; the repo has `Audit { action: AuditAction }` so a nested `Export { action }` is the idiomatic fit). Wire dispatch → `compress_corpus::run_export(input, output)`. Help text: "Materialize the opt-in content-compression capture into a versioned Phase-2 training corpus (offline)."

- [ ] **Step 1-5:** add the variant (with `--input`/`--output` args), wire dispatch, `cargo build -p tt-cli` + a `--help` smoke check, fmt + clippy, commit.

### Task D6: P1d PR
- [ ] `cargo fmt --all --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace --no-run` EXIT 0; `cargo test -p tt-core --lib content_compress` green (P1a/b/c unregressed); `cargo test -p tt-cli --lib compress_corpus` green. Push `feat/content-compress-p1d`; PR "feat(core,cli): P1d — flywheel dataset export (content-aware compression Phase-1 handoff)"; auto-merge. If workspace-test/cargo-deny CI flakes (disk / RustSec-DB), re-run.

---

## Self-Review
**Spec coverage:** real JSONL sink + `CaptureRecord` (D1) ✓; capture threaded to the before/after point (D2) ✓; wired from `prepare()` (D3) ✓; offline export → versioned training corpus (D4) ✓; CLI command (D5) ✓; ZDR refuse non-opted (D4b) ✓; verdict-honesty caveat in the corpus header (D4) ✓.
**Hot-path safety:** capture is gated by `capture_enabled()` + `capture_path()` (both `OnceLock`-cached, env-off by default); the `record_pair` write is only reached on the opt-in path; a write error warns + continues (never breaks a request). With capture OFF, the `CaptureCtx` is `None` and `compact_block`'s capture call is a `None`-check no-op.
**Field-ripple:** `with_gates` signature UNCHANGED (D2 adds `with_gates_and_capture`, additive); `compact_block` gains a param — update its 2 call sites in `compact_content`/the test module. `cargo test --workspace --no-run` proves no other caller breaks.
**No migration, no new deps, `rand` untouched.** Phase-1 complete after merge: P1a✅ P1b✅ P1c✅ P1d✅.
