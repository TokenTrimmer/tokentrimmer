# D3 — SDK `user_with_document` helpers (client-side distillation) plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** The multi-language mirror of `tt docprep` (#308): a `user_with_document` helper in each SDK (Rust `tt-client`, Python, TypeScript) that reads a document file, **distills it to text client-side** (PDF text layers, lossless), and returns a user message carrying the distilled text — so the request arrives pre-distilled (no gateway sidecar round-trip) and routes to a text model. Mirrors the `tt docprep` CLI v1 scope (PDF text layers; images return `unsupported`).

**Owner-approved approach:** full client-side distill in all 3 languages (no `tt` CLI dependency, no per-request network).

**Honest v1 scope (matches `crates/cli/src/docprep.rs`):**
- PDF text layers → lossless text extraction (pure-Rust / pure-Python / pure-JS — no native deps).
- Images (`image/*`) → an `unsupported`/note result (OCR is out of scope for the SDK v1, matching the CLI's off-by-default OCR feature gate). A future slice can add per-language OCR or shell out to the sidecar.
- Remote URLs → not fetched (the gateway seam's v1 also defers this).

**Constraints:**
- `tokentrimmer-client` (Rust) is **published to crates.io** → CANNOT depend on `publish = false` `doc-sidecar`. So the Rust helper gets its OWN feature-gated `pdf-extract` optional dep (mirrors `doc-sidecar`'s `ocr` feature-gating philosophy + keeps the default client lean). The `doc_distill` feature default-off = zero added deps for non-distilling callers.
- The Python SDK is a thin `openai.OpenAI` wrapper (messages are plain dicts) → the helper returns a dict `{"role":"user","content":"<distilled text>"}` (or a content-parts list when the caller wants the doc part attached). No base-class break.
- The TS SDK builds messages inline → the helper returns a `ChatCompletionMessageParam`-shaped object.
- Each SDK's extract must produce parity text with `doc_sidecar::extract` for PDFs (the same PDF text layer). The fidelity is documented `lossless` (a text layer) so the gateway gate substitutes unconditionally.
- Fail-soft: a read error / unsupported type / empty extraction returns a result the caller can act on (never throws on "no text" — mirror the sidecar's fail-soft `ExtractResponse`).
- Public CI hard-gates `cargo fmt --all --check` + clippy. Python: `pytest`. TS: `vitest`. Commit trailer required.

**The "gateway manifest-recompute attest" piece** (backlog item #302's last bullet) is DEFERRED — it's a separate concern (the gateway recomputes the distillation manifest server-side to attest the client-side claim), not part of the SDK helpers. This plan covers the SDK helpers only.

---

## SLICE 1 — Rust `tt-client` `user_with_document` (one PR: `feat/d3-rust-user-with-document`)

**Read first:** `crates/client/src/lib.rs:34-41` (the `user()` helper pattern), `crates/doc-sidecar/src/lib.rs:37-93` (`ExtractResponse`/`Span` shape + the fail-soft `empty()`), `crates/cli/src/docprep.rs` (the v1 scope + `media_type_for`), `crates/client/Cargo.toml`.

### Task 1.1: `doc_distill` feature + `pdf-extract` optional dep
**Files:** `crates/client/Cargo.toml`.
**Produces:** `features = { "doc_distill" = ["dep:pdf-extract"] }` (default-off). `pdf-extract = { version = "0.12", optional = true }` (same version + MIT license as `doc-sidecar`). `base64 = "0.22"` is NOT needed (the helper reads file bytes directly, no base64).
- [ ] Commit.

### Task 1.2: `user_with_document` + `DistilledDocument` (feature-gated)
**Files:** `crates/client/src/lib.rs` (or a new `crates/client/src/document.rs` module — prefer the module to keep `lib.rs` lean).
**Produces:**
- `pub struct DistilledDocument { pub text: String, pub pages: u32, pub engine: String, pub note: Option<String> }` (mirrors `doc_sidecar::ExtractResponse`, sans spans — the SDK caller doesn't need per-page fidelity).
- `pub fn distill_document(path: &Path) -> Result<DistilledDocument, DocumentError>` + an overloaded-by-bytes `distill_document_bytes(media_type: &str, bytes: &[u8])`. Uses `media_type_for` (port from `docprep.rs` — DRY it by making it `pub(crate)` in doc-sidecar OR re-implementing the small match; prefer re-implementing the tiny match to avoid the `publish=false` dep).
- When the `doc_distill` feature is OFF: both fns return `Err(DocumentError::FeatureDisabled)` — the helper exists (API-stable) but can't distill. The caller can still attach the raw bytes via a `user_with_document_raw` helper (below).
- `pub fn user_with_document(path: &Path) -> Result<Message, DocumentError>`: distills the doc → returns `Message::User { content: MessageContent::Text(text), name: None }` (the distilled text — the request arrives pre-distilled, no document part).
- `pub fn user_with_document_raw(path: &Path) -> Result<Message, DocumentError>`: reads + base64-inlines the file as a `ContentPart::Document` (no distillation — for callers who want the gateway seam to do it). Available regardless of the feature (just file read + base64 — needs `base64` as a non-optional dep on tt-client; check if already present, else add). **Verify base64 dep posture** — if adding it to the default client is unwanted, gate `user_with_document_raw` behind `doc_distill` too + only document the distilled path as the default.
- [ ] TDD: a PDF with a text layer (build the fixture in-test via `lopdf` as a dev-dep, mirroring `doc-sidecar`'s test pattern) → distills to the text + `engine="pdf-extract"` + `pages=N`; an image → `engine="unsupported"` + note + empty text → `Err` (the caller attaches raw or skips); a missing file → `Err(DocumentError::Read)`; feature-off build → `Err(FeatureDisabled)`. Run → fail → implement → pass → commit.

### Task 1.3: README/docs + an example
**Files:** `crates/client/README.md` (or the lib docs).
**Produces:** A short `## Document distillation` section: opt in via the `doc_distill` feature, `user_with_document(path)` returns a pre-distilled user message. Honest scope note (PDF only v1; images unsupported).
- [ ] Commit.

---

## SLICE 2 — Python `user_with_document` (one PR: `feat/d3-python-user-with-document`)

**Read first:** `sdk-python/tokentrimmer/__init__.py` (exports), `sdk-python/tokentrimmer/client.py` (the `TokenTrimmer` class + how messages flow), `sdk-python/pyproject.toml` (deps + the `test` extra pattern).

### Task 2.1: `document.py` module + `pypdf` dep
**Files:** `sdk-python/tokentrimmer/document.py` (new), `sdk-python/pyproject.toml`.
**Produces:** `pypdf` as a dep (the SDK is thin + `openai` already pulls deps — `pypdf` is pure-Python, light; add to the base deps OR an optional `[project.optional-dependencies] doc-distill` extra. **Prefer the base deps** since the user chose full client-side distill — `pypdf` is tiny + pure-Python). A `media_type_for(path)` port.
- `distill_document(path) -> DistilledDocument` (dataclass mirroring the Rust shape: text, pages, engine, note). Uses `pypdf.PdfReader` to pull the text layer (page-by-page, join). Images / unsupported → `engine="unsupported"` + note + empty text.
- `user_with_document(path) -> dict`: returns `{"role":"user","content":<distilled text>}` (a plain OpenAI-shaped message — the thin-SDK pattern; no new types in the message path).
- [ ] TDD: a fixture PDF (build one in-test via `pypdf`'s writer, or commit a tiny text-layer PDF fixture) → distills to the text; an image → unsupported note; a missing file → raises a `DocumentError`. `pytest`. Commit.

### Task 2.2: export + README
**Files:** `sdk-python/tokentrimmer/__init__.py`, `sdk-python/README.md`.
**Produces:** `from .document import user_with_document, distill_document, DistilledDocument` in `__init__.py`. Readme section.
- [ ] Commit.

---

## SLICE 3 — TypeScript `user_with_document` (one PR: `feat/d3-typescript-user-with-document`)

**Read first:** `sdk-typescript/src/index.ts` (the module shape + exports), `sdk-typescript/package.json`, `sdk-typescript/src/vercel.ts` (a sibling feature module pattern).

### Task 3.1: `document.ts` module + `pdf-parse` dep
**Files:** `sdk-typescript/src/document.ts` (new), `sdk-typescript/package.json`.
**Produces:** `pdf-parse` as a dep (`pdf-parse` v1 is pure-JS; verify license MIT + no native bindings — the RELEASING note). A `mediaTypeFor(path)` port.
- `distillDocument(path): Promise<DistilledDocument>` (an interface mirroring the Rust shape). Uses `pdf-parse` to pull `text` + `numpages`. Images / unsupported → `engine: "unsupported"` + note + empty text. Note `pdf-parse` is callback/Promise-based; wrap it.
- `userWithDocument(path): Promise<ChatCompletionMessageParam>`: returns `{ role: "user", content: <distilled text> }`.
- [ ] TDD: a fixture PDF (commit a tiny text-layer PDF or generate one) → distills to the text; an image → unsupported note; a missing file → rejects. `vitest` (the TS SDK's test runner — verify in package.json). Commit.

### Task 3.2: export + README
**Files:** `sdk-typescript/src/index.ts`, `sdk-typescript/README.md`.
**Produces:** `export * from "./document.js"` (or named exports) in `index.ts`. Readme section. Run `pnpm build` (tsc) + typecheck + vitest. Commit.

---

## Slice 4 — Verify

- [ ] Rust: `cargo test -p tokentrimmer-client` (with + without `--features doc_distill`); `cargo fmt --check` + `clippy -p tokentrimmer-client --all-targets`.
- [ ] Python: `pytest` in `sdk-python/`; `mypy` if the SDK gates on it (check pyproject).
- [ ] TS: `pnpm build` + `pnpm typecheck` + `vitest` (or the package.json `test` script) in `sdk-typescript/`.
- [ ] Parity spot-check: the same text-layer PDF distilled via the Rust CLI (`tt docprep`), the Rust client, Python, + TS all produce the SAME extracted text (the text layer is deterministic). Document any divergence.
- [ ] Commit trailer + push + 3 PRs (one per SDK; or one PR if the reviewer prefers — the slices are language-independent). Merge on green (public CI free; Python/TS SDK tests run in their own CI if configured).

## Post-merge
- [ ] Update `[[project-review-2026-07-01-campaign]]` memory: D3 SDK helpers DONE (mark the D3 OPEN item resolved; D3 now has both the CLI `tt docprep` + the SDK helpers). Remaining D-lane OPEN: D5 (V4 doc_micros attestation) + D6 (workflow Document node + hash-keyed reuse cache). The "gateway manifest-recompute attest" piece stays a documented follow-up.
