# Coding-Agent Context Preloader

**Status:** approved design (2026-06-16) · **Repo:** public OSS core · **Origin:** competitive review of GrapeRoot (Codex-CLI-Compact) — `GTM-11`-class "coding-agent context layer".

## Problem

Coding agents (Claude Code, Codex, Cursor) burn tokens and turns *exploring* a repo blindly before they can act. TokenTrimmer's gateway attacks cost at/below the API call (routing, caching, shaping) but **structurally cannot reduce an agent's exploration turns** — that is an agent-side decision the proxy never sees. The competing tool GrapeRoot shows real savings (self-reported ~30–45%) from a client-side "load the right context up front" lever. TokenTrimmer already owns most of the pieces (tree-sitter parsing, a repo walker, an MCP server) but has no symbol/import index and no context-preload surface.

## Goal

Surface the *N most relevant files* for a coding task up front, so the agent skips exploration. Fully **local, free, deterministic** (no embeddings, no network, no code leaves the machine), delivered as an **MCP tool the agent pulls** plus a thin CLI, built on existing assets.

## Non-goals (v1)

- Semantic embedding ranking (deferred v2 "hybrid": graph/lexical shortlist → embedding re-rank).
- Languages beyond Python/TS/JS (Go/Java deferred — ties to `COST-8(U)`/`PROD-11` reach).
- On-disk index persistence (v1 caches in-process only).
- Proxy-side auto-injection — **architecturally blocked**: `tt proxy` cannot rewrite request bodies and Anthropic `/v1/messages` bypasses the gateway. The agent-pull MCP surface is the v1 delivery.
- Wrapping each agent's launcher CLI (GrapeRoot's `dgc`/`dg`): MCP is the standard surface; no per-agent shims.
- Full A/B "turns-saved" attestation (v2; v1 reports a measurable token proxy).

## Reused existing assets

- `crates/inspect-core`: `walk()` (parallel repo walk, already skips `node_modules`/`target`/hidden/>1MB), `parse_cached()` + tree-sitter grammars for py/ts/js/md, `call_sites()`. **Rule-driven today — no symbol/import index** (the gap this fills).
- `crates/mcp`: `Tool` trait + `ToolDef` + registry; stdio + HTTP transports; read/write tool gating. Coding agents consume via `tools/list` + `tools/call`.
- `crates/tokenize`: token estimation for the context budget.
- `crates/cli`: clap `Command` enum for the new subcommand.

## Design

### 1. Symbol extraction — `crates/inspect-core/src/symbols.rs` (new module)
Reuse `parse_cached` + the existing grammars to extract, per file, AST-based symbols for **Python/TS/JS** (Markdown skipped — no symbols):
```
FileSymbols { path, functions: Vec<SymbolDef>, classes: Vec<SymbolDef>, imports: Vec<ImportRef> }
SymbolDef  { name, line }
ImportRef  { raw, resolved_hint }   // raw module/path string + a best-effort resolution hint
```
New per-language tree-sitter queries for function-def, class-def, and import nodes. Public API: `extract_symbols(language, source) -> FileSymbols`. Unsupported language / parse failure → empty `FileSymbols` (never errors out the walk).

### 2. Index + ranking — new focused crate `crates/context`
A small isolated lib so MCP and CLI share one engine:
- **`RepoIndex`** — `build(repo_root) -> RepoIndex`: walk via `inspect_core::walk`, extract `FileSymbols` per file, and resolve `imports` into an **in-repo import graph** (edges only for imports resolvable to a file in the repo; external/unresolved imports add no edge). Stores `FileEntry { path, symbols, imports, loc, importers: Vec<PathBuf> }`.
- **`rank(&RepoIndex, task: &str) -> Vec<RankedFile>`** — deterministic score:
  - (a) **lexical/symbol match**: task keywords ∩ {file path, function/class names} (case-insensitive token overlap).
  - (b) **import centrality**: in-degree / PageRank-lite over the import graph (load-bearing files rank up).
  - (c) **size penalty**: prefer focused modules (down-weight very large files).
  - (d) **graph expansion**: if the task names a symbol/file present in the index, include its direct graph neighbors.
  `RankedFile { path, score, reasons: Vec<String> }`. Stable tie-break (by path) for determinism.
- **`assemble(ranked, &RepoIndex, token_budget) -> ContextPack`** — returns per file: path + a **symbol outline** + `reasons`; inlines full `content` only for the top-ranked files until `token_budget` (via `tt-tokenize`) is reached. `ContextPack { files: Vec<ContextFile>, token_estimate, graph_note }`. Token-efficient: a *map* + the few highest-value files, never a repo dump.
- **`IndexCache`** — in-process cache keyed on canonical `repo_root`; invalidated by a cheap max-mtime scan or short TTL so edits are picked up. The one-shot CLI builds fresh; the long-lived MCP server reuses across calls.

### 3. Delivery
- **MCP tool** `get_repo_context` (read-only) — `crates/mcp/src/tools/get_repo_context.rs`. Input `{ repo_path?: String (default cwd), task: String, max_files?: usize, token_budget?: u32 }`; output the `ContextPack` as JSON `{ files:[{path, summary, symbols, reasons, content?}], token_estimate, graph_note }`. Registered alongside `inspect_diff`; uses `IndexCache`.
- **CLI** `tt context` — `crates/cli/src/<repo_context module>.rs` + a clap `Command::Context { path, task, format, max_files, token_budget }` variant. `tt context --task "…" [path] [--format json|md] [--max-files N] [--token-budget T]`. `json` for piping into any agent's prompt; `md` for humans / debugging the index. (Impl module named to avoid colliding with the internal `ResolvedContext`; command name `tt context`.)

### 4. Measurement
v1 emits a measurable proxy: `token_estimate` of surfaced context + file counts ("surfaced 6 files / ~4k tokens"). Used with the gateway, downstream call savings are already measured/attested by the existing pipeline. Full A/B turns-saved attestation is v2 — but the numbers ride the existing methodology surface (the differentiator vs a self-reported local dashboard).

## Components (isolation)

| Unit | Location | Responsibility | Depends on |
|---|---|---|---|
| symbol extraction | `inspect-core/src/symbols.rs` | per-file functions/classes/imports (py/ts/js) | tree-sitter (existing) |
| index + graph + rank + assemble + cache | new `crates/context` | `RepoIndex`, import graph, `rank`, `ContextPack`, `IndexCache` | inspect-core (walk+symbols), tt-tokenize |
| MCP tool | `crates/mcp/src/tools/get_repo_context.rs` | agent-facing read-only tool | `crates/context` |
| CLI | `crates/cli/src/<repo_context>.rs` + clap | `tt context` | `crates/context` |

## Error handling / edge cases

- Unsupported/unparseable files: skipped (walk filters; symbol extraction returns empty).
- Empty repo / no matches: empty `ContextPack` + a `graph_note` explaining nothing matched.
- Huge repos: bounded by the walk's filters (`node_modules`/`target`/>1MB) and the token budget; ranking is O(files) + O(edges).
- Import resolution: best-effort (relative + obvious package-path forms resolved to repo files; unresolved imports simply add no graph edge — never an error).
- Read-only + no network: the tool/CLI never write or call out; safe by construction.

## Testing

- **Symbol extraction** per language (py/ts/js fixtures): functions/classes/imports extracted correctly; unsupported/empty handled.
- **Import graph**: edges resolved within a fixture repo; centrality computed; external imports add no edge.
- **Ranking**: a task naming a symbol surfaces its defining file + direct neighbors; size penalty applies; **deterministic** (stable order across runs).
- **Assembly/budget**: `ContextPack` respects `token_budget`; outlines always present, content inlined only within budget.
- **MCP tool**: returns the documented JSON shape; read-only (no writes/network); honors `max_files`/`token_budget`.
- **CLI**: `tt context --task` emits valid json and readable md.

## Rollout

1. v1 (this spec): symbol extraction + `crates/context` + MCP tool + `tt context` CLI, py/ts/js, graph/lexical, in-process cache.
2. v2 candidates: embedding re-rank (hybrid) reusing `crates/retrieval`; Go/Java grammars (with `COST-8(U)`); on-disk persisted index; turns-saved attestation; (much later, if architecture allows) proxy/gateway Anthropic ingress for auto-injection.
