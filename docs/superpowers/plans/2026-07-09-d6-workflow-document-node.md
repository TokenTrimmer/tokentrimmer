# D6 — Workflow Document node + hash-keyed distillation reuse cache plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax. **STATUS: design decisions RESOLVED 7-09 (owner-approved) → ready to execute.**
>
> **Resolved design decisions (owner-approved 7-09):**
> 1. **Cache store → NEW cloud table + migration.** `flow_doc_distill_cache` keyed `(org_id, content_hash)` → `(text, engine, pages, distilled_at)` + a TTL. Clean separation from the retrieval schema; needs a cloud migration + a cloud-side store fn + a re-pin. The retrieval store's hash-key is the design precedent (not the storage target).
> 2. **Fail posture → ERROR `NodeOutput` (fail-loud).** A cache-miss with the sidecar unreachable/erroring → the Document node emits an error `NodeOutput` (no distilled text); the engine's error handling decides if the workflow aborts. A node that silently emits raw bytes misleads the downstream Model node.
> 3. **Cost booking → on `NodeOutput` (reuse the D4c-v2 seam).** The Document node books its OWN isolated `doc_vision_saved_est_usd` on its `NodeOutput` via the D4c-v2 seam's `document_projection::project` (raw_image_tokens vs distilled_text_tokens → the served-model input rate, Gemini guard). The cache-hit (reused) case books $0 (no distillation — only the retrieval cost, ~$0). Add a `doc_vision_saved_est_usd` field to `NodeOutput` (isolated, mirroring the gateway `CostBreakdown` field) — NEVER folded into `cost_usd`/`baseline_cost_usd`/`saved_usd`.

**Goal:** A workflow `Document` node that runs the document-lane distillation as a workflow step + a per-org content-hash distillation reuse cache, so the same PDF distilled once is reused across runs. Reuses the shipped `document_lane::seam` module (`DistillHarness`, `distill_request_parts`) from D4c v1 (#306) + the `content_hash` blake3 precedent.

**Why:** Workflows that ingest documents (an intake pipeline, an agent that reads an attached PDF) re-distill the same document on every run. A hash-keyed reuse cache (per-org, content-addressed) makes the second+ run a free lookup. The `Document` node surfaces the seam as a first-class workflow primitive (no need for an LLM-shaped detour).

**Architecture:**
1. `NodeKind::Document { source, cache_key: Option<String> }` — a new workflow node kind. `source` is either an inline base64 doc (a `DocumentSource::Base64`) OR a `{{template}}` token resolving to bytes the trigger carried. The node:
   - hashes the doc's bytes (blake3) → the content-addressed key;
   - checks the per-org distillation cache (a cloud-side store keyed by `org_id + content_hash`);
   - on a hit → emits the cached distilled text as `NodeOutput` (zero cost, zero sidecar call);
   - on a miss → calls `document_lane::seam::distill_part` (the SAME extraction the gateway uses), stores the result in the cache, emits it as `NodeOutput`.
2. The cache: a cloud-side table (`flow_doc_distill_cache` or reuse the retrieval store's hash-key pattern — verify which). Keyed `(org_id, content_hash)` → `(text, engine, pages, distilled_at)`. Per-org (one org's distillation never leaks to another). A TTL or LRU (the cache is a cache, not a permanent store — fail-open if the cache is unreachable).
3. Engine executor: a new `NodeKind::Document` arm in `engine.rs`'s `match &node.kind` dispatch (line ~558), calling a `spawn_document_node` helper that owns the cache-lookup → distill → cache-store loop. Fail-open: a sidecar error / cache error → the node surfaces an error `NodeOutput` (the workflow continues; the Document node is advisory, not blocking — OR fail-soft to the raw bytes; design decision).
4. Validation: the `Document` node validates `source` is well-formed (a base64 doc OR a template token resolving to bytes) + the `cache_key` (if set) is a non-empty string. The node does NOT do outbound network (no SSRF surface — it distills inline bytes; remote URLs are the seam's deferred future, NOT this node's).

**Constraints:**
- Reuse the shipped `document_lane::seam` module (`DistillHarness`, `distill_part`) — do NOT reimplement extraction. The node is a cache wrapper around the seam, not a new extractor.
- Per-org isolation on the cache (one org's distillation never served to another — the org_id is part of the key).
- Fail-open: cache-unreachable / sidecar-error → the workflow continues (the Document node emits a best-effort output or an error; decide: fail-soft to raw bytes vs an error `NodeOutput`).
- The cache is opt-in or always-on? — default-off (an org must configure the sidecar for distillation to even fire; the cache is a perf optimization on top). Document the posture.
- D5 (V4 `doc_micros` attestation) is a SEPARATE OPEN item — this node does NOT require it. The node's cost booking (if any) uses the D4c-v2 seam (the seam already books `doc_vision_saved_est_usd` when it fires in the gateway path — verify whether the workflow-engine path threads the same booking or whether the node emits its own `NodeOutput.cost_usd`).
- Mutex: a cache stampede on the same doc (concurrent first runs) — dedup via a single-flight or accept the redundant distill (the cache is eventually-consistent; a double-distill is a perf cost, not a correctness bug). v1: accept the stampede (single-flight is D4c's precedent — the L2 cache uses single-flight; mirror if cheap).

**Read first:**
- `crates/core/src/document_lane/seam.rs` (the `DistillHarness` + `distill_part` the node reuses)
- `crates/core/src/workflow/types.rs:50-138` (the `Node`/`NodeKind` enum + existing variants), `:199-218` (`NodeOutput`), `:223` (`content_hash` blake3 precedent)
- `crates/core/src/workflow/engine.rs:558` (the `match &node.kind` dispatch — where the new arm goes)
- `crates/core/src/workflow/validate.rs` (the validation surface + how each node kind validates)
- The retrieval store's hash-key pattern (search `content_hash` / `hash_key` in `crates/retrieval`) — the cache design precedent
- `tt_shared::messages::DocumentSource` (the `Base64`/`Url` shape the node's `source` mirrors)

**Open design questions to resolve before executing:**
- Cache store: a new cloud table (migration) vs reusing the retrieval store's hash-key pattern. (Precedent: the L2 cache + retrieval both hash-key; verify the cleanest reuse.)
- Fail posture: cache-miss + sidecar-unreachable → error `NodeOutput` (workflow aborts that node) vs fail-soft to raw bytes. (Prefer error — a workflow Document node that silently emits raw bytes misleads the downstream Model node.)
- The node's cost surface: does the distillation sidecar call have a cost to book on the `NodeOutput.cost_usd`? (The sidecar is self-hosted; likely $0 — but the gateway-side booking is the D4c-v2 `doc_vision_saved_est_usd` if the seam fires on the routed request. The workflow-engine path may NOT route — the node distills for a DOWNSTREAM Model node. Clarify the booking.)
- Default-on vs opt-in cache.
- Single-flight on the cache stampede (v1: skip — accept the redundant distill).

---

## SLICE 1 — `NodeKind::Document` variant + validation (one PR: `feat/d6-document-node`)

### Task 1.1: the `Document` node variant + `DocumentSource` reuse
**Files:** `crates/core/src/workflow/types.rs`.
**Produces:** `NodeKind::Document { source: tt_shared::messages::DocumentSource, cache_key: Option<String> }`. The `source` uses the SHARED `DocumentSource` (Base64/Url) so it round-trips the same wire shape as a chat `ContentPart::Document`. Re-export `NodeKind` (already public). Document the node's contract (distills inline bytes; remote URLs are a future slice, same as the seam's v1).
- [ ] TDD: a `Document` node deserializes from JSON + round-trips; the `content_hash` is stable for an unchanged `Document` node + differs when the source bytes change. Run → fail → implement → pass → commit.

### Task 1.2: validation
**Files:** `crates/core/src/workflow/validate.rs`.
**Produces:** a `Document` node validates: `source` is a `Base64` w/ non-empty `media_type` + `data` (a `Url` source is rejected — the seam's v1 doesn't fetch, same posture); `cache_key` if `Some` is a non-empty string ≤ N chars. No outbound network — no SSRF surface. Mirror the `Http` node's validation discipline.
- [ ] TDD: a valid `Document` node passes; a `Url` source is rejected; an empty base64/media_type is rejected. Commit.

## SLICE 2 — the engine executor + the cache (one PR: `feat/d6-document-node-executor`, depends on Slice 1 + the cache store decision)

### Task 2.1: the content-hash distillation cache store
**Files:** TBD per the cache-store decision (a new cloud migration + a store fn, OR reuse the retrieval store's hash-key).
**Produces:** `get_distill_cache(org_id, content_hash) -> Option<CachedDistill>` + `upsert_distill_cache(org_id, content_hash, text, engine, pages)`. Per-org keyed. Fail-open (a DB error → `None`, the node distills fresh). TTL/LRU bound.
- [ ] TDD: a round-trip; org isolation (org A's cached distill is NOT visible to org B); a TTL expiry. Commit.

### Task 2.2: the `NodeKind::Document` executor arm
**Files:** `crates/core/src/workflow/engine.rs` (the dispatch at ~558).
**Produces:** `spawn_document_node` — hash the source bytes (blake3) → cache lookup (Slice 2.1) → on a hit, emit the cached text as `NodeOutput` (`cost_usd: 0.0`, no sidecar call) → on a miss, call `document_lane::seam::distill_part` (the seam's per-part extraction; reuse the `DistillHarness::from_env()`), store the result in the cache, emit it as `NodeOutput`. Fail-open: cache error → distill fresh; sidecar error → an error `NodeOutput` (the workflow's downstream Model node sees no distilled text — TBD: does the workflow abort or continue? prefer an error `NodeOutput` w/ a clear note + let the engine's error handling decide).
- [ ] TDD: a workflow with a Document → Model chain: the first run distills (sidecar called once) + the second run (same doc) hits the cache (sidecar NOT called) + the Model node receives the distilled text both times. A different doc (different hash) → cache miss. Fail-open: cache-unreachable → distills fresh. Commit.

## SLICE 3 — Verify + docs

- [ ] `cargo test -p tt-core --lib workflow` (the workflow suite is large; verify the new node's tests pass + no regressions).
- [ ] `cargo check --workspace` + `clippy --workspace --all-targets -D warnings` + `cargo test --workspace --no-run` (field-ripple).
- [ ] A docs/superpowers spec for the Document node + the cache (mirror the D4 spec format).
- [ ] Commit trailer + push + PR. Merge on green (public CI free; cloud cache store is a cloud-side migration — the cloud half, if any, needs the cloud repo + a re-pin).

## Post-merge
- [ ] Update `[[project-review-2026-07-01-campaign]]` memory: D6 DONE (the D-lane's last remaining code item). Remaining OPEN: D5 (V4 `doc_micros` attestation) only. The + #2/#3/#4/#7 infra-blocked / contrarian-deferred items stay as verified-non-actionable.
