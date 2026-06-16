# Coding-Agent Context Preloader Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface the N most relevant files for a coding task (symbol/import-graph + lexical ranking, fully local) via a read-only MCP tool `get_repo_context` and a `tt context` CLI, so coding agents skip exploration turns.

**Architecture:** Reuse `tt-inspect-core`'s tree-sitter parser + repo `walk()`. Add per-file symbol/import extraction to inspect-core, then a new focused crate `tt-context` (`crates/context`) that builds a `RepoIndex` (import graph), ranks files for a task, assembles a token-budgeted `ContextPack`, and caches the index in-process. Deliver via an MCP tool + CLI that both call `tt_context::repo_context(...)`.

**Tech Stack:** Rust, tree-sitter (python/typescript/javascript v0.23), serde/serde_json, `tt-tokenize`, `async-trait` (MCP), clap (CLI).

**Spec:** `docs/superpowers/specs/2026-06-16-coding-agent-context-preloader-design.md`

---

## File Structure

| File | Create/Modify | Responsibility |
|---|---|---|
| `crates/inspect-core/src/symbols.rs` | Create | `FileSymbols`/`SymbolDef`/`ImportRef` + `extract_symbols(source, language)` (py/ts/js) |
| `crates/inspect-core/src/lib.rs` | Modify | `pub mod symbols;` + re-exports |
| `crates/context/Cargo.toml` | Create | new `tt-context` crate manifest |
| `crates/context/src/lib.rs` | Create | crate root + the `repo_context(...)` convenience entry |
| `crates/context/src/index.rs` | Create | `RepoIndex`, `FileEntry`, `build()`, import-graph resolution |
| `crates/context/src/rank.rs` | Create | `RankedFile`, `rank(&RepoIndex, task)` |
| `crates/context/src/assemble.rs` | Create | `ContextPack`, `ContextFile`, `assemble(...)` (token budget) |
| `crates/context/src/cache.rs` | Create | `IndexCache` (in-process, mtime/TTL) |
| `Cargo.toml` (workspace) | Modify | add `crates/context` to members + `tt-context` to workspace.dependencies |
| `crates/mcp/src/tools/get_repo_context.rs` | Create | read-only MCP tool |
| `crates/mcp/src/tools/mod.rs` + `Cargo.toml` | Modify | register module; add `tt-context` dep |
| `crates/cli/src/repo_context.rs` | Create | `tt context` command impl (name avoids the existing `context/` module) |
| `crates/cli/src/main.rs` + `Cargo.toml` | Modify | clap `Command::Context` variant + dispatch + register the MCP tool; add `tt-context` dep |
| `docs/tt-cli-commands.md`, `docs/tt-mcp-usage.md` | Modify | document `tt context` + `get_repo_context` |

**Verification convention (every task):** public CI gates `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Run the named per-task commands. Determinism: ranking MUST be deterministic (stable tie-break). No network in `tt-context`.

---

## Task 1: Scaffold the `tt-context` crate

**Files:**
- Create: `crates/context/Cargo.toml`, `crates/context/src/lib.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Add the crate to the workspace** — in the root `Cargo.toml`, add `"crates/context",` to `[workspace] members`, and under `[workspace.dependencies]` (with the other `tt-*` entries) add:
```toml
tt-context = { path = "crates/context", version = "0.1" }
```

- [ ] **Step 2: Create `crates/context/Cargo.toml`**
```toml
[package]
name = "tt-context"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
description = "Coding-agent context preloader: repo symbol/import index + relevance ranking."

[dependencies]
tt-inspect-core.workspace = true
tt-tokenize.workspace = true
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 3: Create `crates/context/src/lib.rs`** (module declarations + a smoke test)
```rust
//! Coding-agent context preloader. Builds a repo symbol/import index, ranks
//! files for a task, and assembles a token-budgeted context pack. Fully local
//! and deterministic — no embeddings, no network.
pub mod assemble;
pub mod cache;
pub mod index;
pub mod rank;

#[cfg(test)]
mod smoke {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}
```
(Create empty `index.rs`/`rank.rs`/`assemble.rs`/`cache.rs` with a `//! TODO next task` doc line so the module decls compile; later tasks fill them. Each file must at least compile.)

- [ ] **Step 4: Verify** — `cargo build -p tt-context` and `cargo test -p tt-context` → PASS (smoke). `cargo fmt -p tt-context`.

- [ ] **Step 5: Commit**
```bash
git add Cargo.toml crates/context/
git commit -m "feat(context): scaffold tt-context crate"
```

---

## Task 2: Symbol + import extraction in inspect-core

**Files:**
- Create: `crates/inspect-core/src/symbols.rs`
- Modify: `crates/inspect-core/src/lib.rs` (add `pub mod symbols;`)
- Test: in `symbols.rs` (`#[cfg(test)] mod tests`)

Reuse the existing traversal pattern from `ast.rs::call_sites` (stack of `tree_sitter::Node`, match `node.kind()`, `child_by_field_name("name")`, `node.utf8_text(source.as_bytes())`, `node.start_position().row + 1`). Parse via `crate::parse::parse_cached(source, language)`.

- [ ] **Step 1: Write the failing tests** (inline fixture sources; one per language)
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Language;

    #[test]
    fn python_symbols() {
        let src = "import os\nfrom a.b import c\n\ndef foo(x):\n    return x\n\nclass Bar:\n    def m(self):\n        pass\n";
        let s = extract_symbols(src, Language::Python);
        assert!(s.functions.iter().any(|f| f.name == "foo"));
        assert!(s.classes.iter().any(|c| c.name == "Bar"));
        assert!(s.imports.iter().any(|i| i.raw.contains("os")));
        assert!(s.imports.iter().any(|i| i.raw.contains("a.b")));
    }

    #[test]
    fn javascript_symbols() {
        let src = "import {x} from './util.js';\nfunction foo(){}\nconst bar = () => {};\nclass Baz {}\n";
        let s = extract_symbols(src, Language::Javascript);
        assert!(s.functions.iter().any(|f| f.name == "foo"));
        assert!(s.functions.iter().any(|f| f.name == "bar")); // arrow const
        assert!(s.classes.iter().any(|c| c.name == "Baz"));
        assert!(s.imports.iter().any(|i| i.raw.contains("./util")));
    }

    #[test]
    fn typescript_symbols() {
        let src = "import type {T} from '../t';\nexport function handle(): void {}\nexport class Svc {}\n";
        let s = extract_symbols(src, Language::Typescript);
        assert!(s.functions.iter().any(|f| f.name == "handle"));
        assert!(s.classes.iter().any(|c| c.name == "Svc"));
        assert!(s.imports.iter().any(|i| i.raw.contains("../t")));
    }

    #[test]
    fn markdown_and_parse_failure_are_empty() {
        let s = extract_symbols("# hi", Language::Markdown);
        assert!(s.functions.is_empty() && s.classes.is_empty() && s.imports.is_empty());
    }
}
```

- [ ] **Step 2: Run → FAIL** — `cargo test -p tt-inspect-core symbols::` (no `extract_symbols`).

- [ ] **Step 3: Implement `symbols.rs`**
```rust
//! Per-file symbol + import extraction (functions, classes, imports) for
//! Python/TS/JS, built on the shared tree-sitter parser. Markdown/parse
//! failures yield an empty `FileSymbols` (never errors).
use serde::{Deserialize, Serialize};
use tree_sitter::Node;

use crate::{parse::parse_cached, Language};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolDef {
    pub name: String,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportRef {
    /// The raw module/path string as written (e.g. "os", "a.b", "./util.js").
    pub raw: String,
    pub line: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSymbols {
    pub functions: Vec<SymbolDef>,
    pub classes: Vec<SymbolDef>,
    pub imports: Vec<ImportRef>,
}

/// Extract symbols + imports. Returns empty for Markdown or on parse failure.
#[must_use]
pub fn extract_symbols(source: &str, language: Language) -> FileSymbols {
    if language == Language::Markdown {
        return FileSymbols::default();
    }
    let Ok(tree) = parse_cached(source, language) else {
        return FileSymbols::default();
    };
    let src = source.as_bytes();
    let mut out = FileSymbols::default();
    let line = |n: &Node| (n.start_position().row + 1) as u32;
    let name_of = |n: &Node| n.child_by_field_name("name").and_then(|x| x.utf8_text(src).ok()).map(str::to_string);

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            // Functions
            "function_definition" | "function_declaration" => {
                if let Some(name) = name_of(&node) { out.functions.push(SymbolDef { name, line: line(&node) }); }
            }
            // Arrow/function consts: `const bar = () => {}` / `const f = function(){}`
            "variable_declarator" => {
                let has_fn = {
                    let mut c = node.walk();
                    node.children(&mut c).any(|ch| matches!(ch.kind(), "arrow_function" | "function" | "function_expression"))
                };
                if has_fn {
                    if let Some(name) = name_of(&node).or_else(|| node.child_by_field_name("name").and_then(|x| x.utf8_text(src).ok()).map(str::to_string)) {
                        out.functions.push(SymbolDef { name, line: line(&node) });
                    }
                }
            }
            // Classes
            "class_definition" | "class_declaration" => {
                if let Some(name) = name_of(&node) { out.classes.push(SymbolDef { name, line: line(&node) }); }
            }
            // Imports (capture the whole statement text as `raw`)
            "import_statement" | "import_from_statement" => {
                if let Ok(txt) = node.utf8_text(src) {
                    out.imports.push(ImportRef { raw: txt.trim().to_string(), line: line(&node) });
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) { stack.push(child); }
    }
    out
}
```
Add `pub mod symbols;` to `crates/inspect-core/src/lib.rs`.

> NOTE for implementer: the exact node kinds above are the common tree-sitter-{python,typescript,javascript} v0.23 kinds, but VERIFY against the real grammars by running the tests — if `variable_declarator`'s name field differs (some grammars expose the name as the first `identifier` child rather than a `name` field), adjust `name_of` for that arm so `javascript_symbols`'s `bar` assertion passes. Keep `raw` import text trimmed; tests only require substring matches.

- [ ] **Step 4: Run → PASS** — `cargo test -p tt-inspect-core symbols::`. Then `cargo clippy -p tt-inspect-core --all-targets -- -D warnings` + `cargo fmt -p tt-inspect-core`.

- [ ] **Step 5: Commit**
```bash
git add crates/inspect-core/src/symbols.rs crates/inspect-core/src/lib.rs
git commit -m "feat(inspect-core): per-file symbol + import extraction (py/ts/js)"
```

---

## Task 3: `RepoIndex` + import graph

**Files:**
- Create/replace: `crates/context/src/index.rs`
- Test: in `index.rs`

- [ ] **Step 1: Write the failing test** (build a tiny temp repo, assert symbols + import edges)
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn builds_index_with_import_edges() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("util.py"), "def helper():\n    return 1\n").unwrap();
        fs::write(root.join("main.py"), "from util import helper\n\ndef run():\n    return helper()\n").unwrap();
        let idx = RepoIndex::build(root);
        // both files present
        assert_eq!(idx.files().len(), 2);
        let main = idx.files().iter().find(|f| f.path.ends_with("main.py")).unwrap();
        assert!(main.symbols.functions.iter().any(|f| f.name == "run"));
        // main imports util -> util.py has main.py as an importer (in-degree 1)
        let util = idx.files().iter().find(|f| f.path.ends_with("util.py")).unwrap();
        assert!(util.importers.iter().any(|p| p.ends_with("main.py")),
            "util.py should record main.py as an importer; importers={:?}", util.importers);
    }

    #[test]
    fn empty_repo_is_empty() {
        let dir = tempdir().unwrap();
        assert!(RepoIndex::build(dir.path()).files().is_empty());
    }
}
```

- [ ] **Step 2: Run → FAIL** — `cargo test -p tt-context index::`.

- [ ] **Step 3: Implement `index.rs`**
```rust
//! The repo index: walk + per-file symbols + a best-effort in-repo import graph.
use std::path::{Path, PathBuf};

use serde::Serialize;
use tt_inspect_core::symbols::{extract_symbols, FileSymbols};
use tt_inspect_core::{walk, Language};

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub language: Language,
    pub symbols: FileSymbols,
    pub loc: u32,
    /// In-repo files that import this file (graph in-degree = importers.len()).
    pub importers: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoIndex {
    root: PathBuf,
    files: Vec<FileEntry>,
}

impl RepoIndex {
    #[must_use]
    pub fn root(&self) -> &Path { &self.root }
    #[must_use]
    pub fn files(&self) -> &[FileEntry] { &self.files }

    /// Walk `root`, extract symbols, and resolve in-repo import edges.
    #[must_use]
    pub fn build(root: &Path) -> RepoIndex {
        let mut files: Vec<FileEntry> = Vec::new();
        for (path, language) in walk(root) {
            let Ok(source) = std::fs::read_to_string(&path) else { continue };
            let loc = source.lines().count() as u32;
            let symbols = extract_symbols(&source, language);
            files.push(FileEntry { path, language, symbols, loc, importers: Vec::new() });
        }
        resolve_import_edges(root, &mut files);
        RepoIndex { root: root.to_path_buf(), files }
    }
}

/// Best-effort: for each file's imports, resolve the raw import string to another
/// indexed file and record the importer. Relative JS/TS imports resolve against
/// the importing file's dir (trying common extensions + /index); Python dotted
/// modules resolve against repo root (a/b -> a/b.py or a/b/__init__.py). Bare
/// package specifiers that don't resolve to an indexed file add no edge.
fn resolve_import_edges(root: &Path, files: &mut [FileEntry]) {
    // index of canonical absolute path -> position
    let positions: std::collections::HashMap<PathBuf, usize> = files
        .iter().enumerate()
        .map(|(i, f)| (f.path.clone(), i))
        .collect();
    // collect (importer_path, target_pos) first to avoid borrow conflicts
    let mut edges: Vec<(PathBuf, usize)> = Vec::new();
    for f in files.iter() {
        for imp in &f.symbols.imports {
            if let Some(target) = resolve_import(root, &f.path, f.language, &imp.raw, &positions) {
                edges.push((f.path.clone(), target));
            }
        }
    }
    for (importer, target_pos) in edges {
        let importers = &mut files[target_pos].importers;
        if !importers.contains(&importer) { importers.push(importer); }
    }
    for f in files.iter_mut() { f.importers.sort(); }
}

fn resolve_import(
    root: &Path,
    importer: &Path,
    language: Language,
    raw: &str,
    positions: &std::collections::HashMap<PathBuf, usize>,
) -> Option<usize> {
    let candidates = candidate_paths(root, importer, language, raw);
    for c in candidates {
        if let Some(&pos) = positions.get(&c) { return Some(pos); }
    }
    None
}

/// Produce candidate resolved paths for an import string (extension list + index files).
fn candidate_paths(root: &Path, importer: &Path, language: Language, raw: &str) -> Vec<PathBuf> {
    // Extract the module/path token from the raw statement (between quotes for
    // JS/TS, or the dotted module for Python). Implementer: parse `raw` to get
    // the specifier; the tests above use simple forms.
    // ... (implement specifier extraction + the resolution rules described in the doc comment) ...
    Vec::new()
}
```

> The `candidate_paths`/specifier-extraction body is the substantive part. Implement it to satisfy the Task-3 test (`from util import helper` → `util.py`). Rules: **Python** — pull the dotted module after `import`/`from`; map dots to path separators; try `<root>/<mod>.py` and `<root>/<mod>/__init__.py`. **JS/TS** — pull the quoted specifier; if it starts with `.`/`..`, join to `importer.parent()` and try extensions `[ts, tsx, js, jsx, mjs, cjs]` and `<spec>/index.<ext>`; bare specifiers (no leading dot) → return no candidate. Canonicalize candidates to match the absolute `path`s stored from `walk` (use the same form `walk` yields — they are `root.join(...)` style; compare via `Path::ends_with` if exact canonicalization is fragile, but prefer building the same path shape `walk` produced). Add focused unit tests for `candidate_paths` (python dotted, js relative-with-extension, bare specifier → none) alongside the integration test.

- [ ] **Step 4: Run → PASS** — `cargo test -p tt-context index::`; clippy + fmt clean.

- [ ] **Step 5: Commit**
```bash
git add crates/context/src/index.rs
git commit -m "feat(context): RepoIndex with per-file symbols + best-effort import graph"
```

---

## Task 4: Relevance ranking

**Files:**
- Create/replace: `crates/context/src/rank.rs`
- Test: in `rank.rs`

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RepoIndex;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn ranks_symbol_match_and_centrality_first_deterministically() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // auth.py defines `authenticate`; imported by two others (central).
        fs::write(root.join("auth.py"), "def authenticate(user):\n    return True\n").unwrap();
        fs::write(root.join("api.py"), "from auth import authenticate\n").unwrap();
        fs::write(root.join("web.py"), "from auth import authenticate\n").unwrap();
        fs::write(root.join("unrelated.py"), "def widget():\n    return 0\n").unwrap();
        let idx = RepoIndex::build(root);

        let ranked = rank(&idx, "add a new authenticate handler");
        assert!(ranked.first().unwrap().path.ends_with("auth.py"),
            "auth.py should rank first (symbol match + centrality); got {:?}", ranked.iter().map(|r| &r.path).collect::<Vec<_>>());
        // deterministic: same input -> same order
        let ranked2 = rank(&idx, "add a new authenticate handler");
        assert_eq!(ranked.iter().map(|r| r.path.clone()).collect::<Vec<_>>(),
                   ranked2.iter().map(|r| r.path.clone()).collect::<Vec<_>>());
    }
}
```

- [ ] **Step 2: Run → FAIL**.

- [ ] **Step 3: Implement `rank.rs`**
```rust
//! Deterministic relevance ranking over a `RepoIndex` for a task description.
use std::path::PathBuf;

use serde::Serialize;
use crate::index::RepoIndex;

#[derive(Debug, Clone, Serialize)]
pub struct RankedFile {
    pub path: PathBuf,
    pub score: f64,
    pub reasons: Vec<String>,
}

const W_SYMBOL: f64 = 3.0;   // task keyword matches a symbol/path token
const W_CENTRAL: f64 = 1.0;  // per importer (in-degree)
const SIZE_PENALTY: f64 = 0.0008; // per LOC

/// Rank all indexed files by relevance to `task`. Stable tie-break on path so
/// the order is deterministic across runs.
#[must_use]
pub fn rank(index: &RepoIndex, task: &str) -> Vec<RankedFile> {
    let keywords = tokenize(&task.to_lowercase());
    let mut ranked: Vec<RankedFile> = index.files().iter().map(|f| {
        let mut score = 0.0;
        let mut reasons = Vec::new();

        // (a) lexical/symbol match
        let names: Vec<String> = f.symbols.functions.iter().chain(f.symbols.classes.iter())
            .map(|s| s.name.to_lowercase())
            .chain(path_tokens(&f.path))
            .collect();
        let hits = keywords.iter().filter(|k| names.iter().any(|n| n.contains(*k))).count();
        if hits > 0 { score += W_SYMBOL * hits as f64; reasons.push(format!("matches {hits} task term(s)")); }

        // (b) import centrality
        let indeg = f.importers.len();
        if indeg > 0 { score += W_CENTRAL * indeg as f64; reasons.push(format!("imported by {indeg} file(s)")); }

        // (c) size penalty (prefer focused files)
        score -= SIZE_PENALTY * f.loc as f64;

        RankedFile { path: f.path.clone(), score, reasons }
    }).collect();

    // (d) deterministic order: score desc, then path asc.
    ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.path.cmp(&b.path)));
    ranked
}

fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(str::to_string)
        .collect()
}

fn path_tokens(p: &std::path::Path) -> Vec<String> {
    p.file_stem().and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .into_iter().collect()
}
```

> Graph-expansion (reason (d) in the spec — pull neighbors of a file whose symbol the task names) is OPTIONAL polish: if a top-scored file has importers/imports, you MAY add a small bonus to its neighbors. Keep it deterministic. The Task-4 test only requires symbol-match + centrality + determinism; add a neighbor-expansion unit test only if you implement it.

- [ ] **Step 4: Run → PASS**; clippy + fmt clean.

- [ ] **Step 5: Commit**
```bash
git add crates/context/src/rank.rs
git commit -m "feat(context): deterministic relevance ranking (lexical + centrality + size)"
```

---

## Task 5: Context assembly with token budget

**Files:**
- Create/replace: `crates/context/src/assemble.rs`
- Test: in `assemble.rs`

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RepoIndex;
    use crate::rank::rank;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn assembles_outlines_and_respects_token_budget() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let big = "x = 1\n".repeat(5000); // large file
        fs::write(root.join("big.py"), &big).unwrap();
        fs::write(root.join("small.py"), "def helper():\n    return 1\n").unwrap();
        let idx = RepoIndex::build(root);
        let ranked = rank(&idx, "helper");

        let pack = assemble(&ranked, &idx, /*max_files*/ 10, /*token_budget*/ 50);
        // every file has an outline entry; at least one path present
        assert!(!pack.files.is_empty());
        // token estimate of inlined content must not exceed the budget
        assert!(pack.token_estimate <= 50, "estimate {} over budget", pack.token_estimate);
        // a small file may be inlined; a huge file must NOT be fully inlined under a 50-token budget
        let big_inlined = pack.files.iter().find(|f| f.path.ends_with("big.py")).and_then(|f| f.content.as_ref());
        assert!(big_inlined.is_none(), "big.py should not be inlined under a 50-token budget");
    }
}
```

- [ ] **Step 2: Run → FAIL**.

- [ ] **Step 3: Implement `assemble.rs`**
```rust
//! Assemble a ranked file list into a token-budgeted context pack: every file
//! gets a path + symbol outline + reasons; top files are inlined until the
//! token budget is reached.
use std::path::PathBuf;

use serde::Serialize;
use crate::index::RepoIndex;
use crate::rank::RankedFile;

#[derive(Debug, Clone, Serialize)]
pub struct ContextFile {
    pub path: PathBuf,
    /// One-line summary: top symbols.
    pub summary: String,
    pub symbols: Vec<String>,
    pub reasons: Vec<String>,
    /// Full content, present only when it fit within the token budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextPack {
    pub files: Vec<ContextFile>,
    pub token_estimate: u32,
    pub note: String,
}

/// Build the pack. `max_files` caps how many ranked files are described;
/// `token_budget` caps the total tokens of INLINED content (outlines are free).
#[must_use]
pub fn assemble(ranked: &[RankedFile], index: &RepoIndex, max_files: usize, token_budget: u32) -> ContextPack {
    let mut files = Vec::new();
    let mut spent: u32 = 0;
    for r in ranked.iter().take(max_files) {
        let entry = index.files().iter().find(|f| f.path == r.path);
        let symbols: Vec<String> = entry.map(|e| {
            e.symbols.functions.iter().chain(e.symbols.classes.iter()).map(|s| s.name.clone()).collect()
        }).unwrap_or_default();
        let summary = if symbols.is_empty() { "(no top-level symbols)".to_string() }
            else { format!("symbols: {}", symbols.join(", ")) };

        // Inline content only if it fits the remaining budget.
        let mut content = None;
        if let Ok(src) = std::fs::read_to_string(&r.path) {
            let cost = tt_tokenize::estimate_tokens("openai", &src);
            if spent + cost <= token_budget {
                spent += cost;
                content = Some(src);
            }
        }
        files.push(ContextFile { path: r.path.clone(), summary, symbols, reasons: r.reasons.clone(), content });
    }
    let note = if files.is_empty() {
        "No matching files found in the repo index.".to_string()
    } else {
        format!("{} files ranked; {} inlined within the {}-token budget.",
            files.len(), files.iter().filter(|f| f.content.is_some()).count(), token_budget)
    };
    ContextPack { files, token_estimate: spent, note }
}
```

- [ ] **Step 4: Run → PASS**; clippy + fmt clean.

- [ ] **Step 5: Commit**
```bash
git add crates/context/src/assemble.rs
git commit -m "feat(context): token-budgeted context pack assembly"
```

---

## Task 6: In-process cache + `repo_context` entry point

**Files:**
- Create/replace: `crates/context/src/cache.rs`
- Modify: `crates/context/src/lib.rs` (add the `repo_context` convenience fn)
- Test: in `cache.rs`

- [ ] **Step 1: Write the failing test**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn cache_reuses_until_mtime_changes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();
        let cache = IndexCache::new();
        let i1 = cache.get_or_build(root);
        let i2 = cache.get_or_build(root);
        assert_eq!(i1.files().len(), i2.files().len()); // served from cache, same content
        // add a file -> mtime advances -> rebuild picks it up
        fs::write(root.join("b.py"), "def b():\n    pass\n").unwrap();
        let i3 = cache.get_or_build(root);
        assert_eq!(i3.files().len(), 2, "new file should be picked up after mtime change");
    }
}
```

- [ ] **Step 2: Run → FAIL**.

- [ ] **Step 3: Implement `cache.rs`** (mtime-keyed cache) and the `repo_context` entry in `lib.rs`
```rust
//! In-process cache for `RepoIndex`, keyed on repo root + a cheap max-mtime
//! fingerprint so edits are picked up without disk persistence.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::index::RepoIndex;
use tt_inspect_core::walk;

#[derive(Default)]
pub struct IndexCache {
    inner: Mutex<HashMap<PathBuf, (u128, Arc<RepoIndex>)>>, // root -> (fingerprint, index)
}

impl IndexCache {
    #[must_use]
    pub fn new() -> Self { Self { inner: Mutex::new(HashMap::new()) } }

    /// Return a cached index for `root`, rebuilding if the max-mtime fingerprint changed.
    #[must_use]
    pub fn get_or_build(&self, root: &Path) -> Arc<RepoIndex> {
        let fp = fingerprint(root);
        let mut guard = self.inner.lock().expect("index cache poisoned");
        if let Some((cached_fp, idx)) = guard.get(root) {
            if *cached_fp == fp { return Arc::clone(idx); }
        }
        let idx = Arc::new(RepoIndex::build(root));
        guard.insert(root.to_path_buf(), (fp, Arc::clone(&idx)));
        idx
    }
}

/// Cheap fingerprint: count of indexed files + max mtime (ns). Detects adds/edits/removes.
fn fingerprint(root: &Path) -> u128 {
    let mut count: u128 = 0;
    let mut max_mtime: u128 = 0;
    for (path, _lang) in walk(root) {
        count += 1;
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if let Ok(dur) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                    max_mtime = max_mtime.max(dur.as_nanos());
                }
            }
        }
    }
    count.wrapping_mul(1_000_000_007).wrapping_add(max_mtime)
}
```
In `lib.rs`, add a process-wide cache + the convenience entry both surfaces call:
```rust
use std::path::Path;
use std::sync::OnceLock;
use crate::assemble::{assemble, ContextPack};
use crate::cache::IndexCache;
use crate::rank::rank;

fn global_cache() -> &'static IndexCache {
    static CACHE: OnceLock<IndexCache> = OnceLock::new();
    CACHE.get_or_init(IndexCache::new)
}

/// Build/reuse the repo index, rank for `task`, and assemble a context pack.
/// The single entry point shared by the MCP tool and the CLI.
#[must_use]
pub fn repo_context(repo_root: &Path, task: &str, max_files: usize, token_budget: u32) -> ContextPack {
    let index = global_cache().get_or_build(repo_root);
    let ranked = rank(&index, task);
    assemble(&ranked, &index, max_files, token_budget)
}
```

- [ ] **Step 4: Run → PASS** — `cargo test -p tt-context`; clippy + fmt clean.

- [ ] **Step 5: Commit**
```bash
git add crates/context/src/cache.rs crates/context/src/lib.rs
git commit -m "feat(context): in-process index cache + repo_context entry point"
```

---

## Task 7: MCP tool `get_repo_context`

**Files:**
- Create: `crates/mcp/src/tools/get_repo_context.rs`
- Modify: `crates/mcp/src/tools/mod.rs` (add `pub mod get_repo_context;`), `crates/mcp/Cargo.toml` (add `tt-context.workspace = true`), `crates/cli/src/main.rs` (register the tool)
- Test: in `get_repo_context.rs`

- [ ] **Step 1: Add the dep** — in `crates/mcp/Cargo.toml` `[dependencies]`: `tt-context.workspace = true`.

- [ ] **Step 2: Write the failing test** (the tool returns the documented shape)
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn returns_ranked_files_for_a_task() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("auth.py"), "def authenticate():\n    pass\n").unwrap();
        let tool = GetRepoContextTool;
        let out = tool.call(json!({
            "repo_path": dir.path().to_string_lossy(),
            "task": "fix authenticate",
            "max_files": 5,
            "token_budget": 1000
        })).await.unwrap();
        assert!(out["files"].is_array());
        assert!(out["files"].as_array().unwrap().iter().any(|f| f["path"].as_str().unwrap().ends_with("auth.py")));
        assert!(out["token_estimate"].is_number());
        // def() advertises the tool
        assert_eq!(tool.def().name, "get_repo_context");
    }
}
```
(Add `tempfile` to `crates/mcp` `[dev-dependencies]` if absent: `tempfile = { workspace = true }`.)

- [ ] **Step 3: Run → FAIL**.

- [ ] **Step 4: Implement the tool**
```rust
//! `get_repo_context` — read-only MCP tool. Given a task, returns the most
//! relevant repo files (symbol/import-graph + lexical ranking) + outlines +
//! budget-bounded inlined content, so a coding agent skips exploration.
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

pub struct GetRepoContextTool;

#[derive(Deserialize)]
struct Input {
    #[serde(default = "default_path")]
    repo_path: String,
    task: String,
    #[serde(default = "default_max_files")]
    max_files: usize,
    #[serde(default = "default_budget")]
    token_budget: u32,
}
fn default_path() -> String { ".".into() }
fn default_max_files() -> usize { 12 }
fn default_budget() -> u32 { 6000 }

#[async_trait]
impl Tool for GetRepoContextTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "get_repo_context",
            description: "Given a coding task, return the most relevant files in \
                the repo (ranked by symbol/import-graph + lexical match) with a \
                symbol outline, why each was chosen, and the top files' content \
                within a token budget — so you can skip exploring the codebase. \
                Read-only, fully local (no network).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string", "description": "Repo root to index (default: current dir)." },
                    "task": { "type": "string", "description": "The coding task in plain English." },
                    "max_files": { "type": "integer", "description": "Max files to describe (default 12)." },
                    "token_budget": { "type": "integer", "description": "Token cap for inlined file content (default 6000)." }
                },
                "required": ["task"]
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let inp: Input = serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let pack = tt_context::repo_context(
            std::path::Path::new(&inp.repo_path), &inp.task, inp.max_files, inp.token_budget,
        );
        serde_json::to_value(&pack).map_err(|e| McpError::Internal(e.to_string()))
    }
}
```
Register in `crates/cli/src/main.rs` beside the other read-only tools (`server.tools.register(Box::new(...))`):
```rust
server.tools.register(Box::new(tt_mcp::tools::get_repo_context::GetRepoContextTool));
```
Add `pub mod get_repo_context;` to `crates/mcp/src/tools/mod.rs`.

- [ ] **Step 5: Run → PASS** — `cargo test -p tt-mcp get_repo_context`; `cargo build -p tt-cli`; clippy + fmt clean for both.

- [ ] **Step 6: Commit**
```bash
git add crates/mcp/ crates/cli/src/main.rs
git commit -m "feat(mcp): get_repo_context read-only tool"
```

---

## Task 8: `tt context` CLI command

**Files:**
- Create: `crates/cli/src/repo_context.rs`
- Modify: `crates/cli/src/main.rs` (add `mod repo_context;`, the clap `Command::Context` variant + dispatch), `crates/cli/Cargo.toml` (add `tt-context.workspace = true`)
- Test: in `repo_context.rs`

- [ ] **Step 1: Add the dep** — `crates/cli/Cargo.toml` `[dependencies]`: `tt-context.workspace = true`.

- [ ] **Step 2: Write the failing test** (the formatter is the pure, testable part)
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats_json_and_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "def helper():\n    pass\n").unwrap();
        let pack = tt_context::repo_context(dir.path(), "helper", 5, 1000);
        let j = render(&pack, "json");
        assert!(serde_json::from_str::<serde_json::Value>(&j).is_ok());
        let md = render(&pack, "md");
        assert!(md.contains("a.py"));
    }
}
```
(Add `tempfile = { workspace = true }` to `crates/cli` `[dev-dependencies]` if absent.)

- [ ] **Step 3: Implement `repo_context.rs`**
```rust
//! `tt context` — print the most relevant repo files for a task (json or md).
use tt_context::assemble::ContextPack;

pub fn run(path: &str, task: &str, format: &str, max_files: usize, token_budget: u32) -> anyhow::Result<()> {
    let pack = tt_context::repo_context(std::path::Path::new(path), task, max_files, token_budget);
    println!("{}", render(&pack, format));
    Ok(())
}

fn render(pack: &ContextPack, format: &str) -> String {
    if format.eq_ignore_ascii_case("json") {
        return serde_json::to_string_pretty(pack).unwrap_or_else(|_| "{}".into());
    }
    let mut out = String::new();
    out.push_str(&format!("# Relevant context ({} files, ~{} inlined tokens)\n\n", pack.files.len(), pack.token_estimate));
    out.push_str(&format!("> {}\n\n", pack.note));
    for f in &pack.files {
        out.push_str(&format!("## {}\n", f.path.display()));
        out.push_str(&format!("- {}\n", f.summary));
        if !f.reasons.is_empty() { out.push_str(&format!("- why: {}\n", f.reasons.join("; "))); }
        if let Some(c) = &f.content {
            out.push_str("\n```\n"); out.push_str(c); out.push_str("\n```\n");
        }
        out.push('\n');
    }
    out
}
```
In `crates/cli/src/main.rs`: add `mod repo_context;`, the clap variant (mirroring the `Inspect` variant style), and the dispatch arm:
```rust
    /// Preload the most relevant repo files for a coding task.
    Context {
        /// Task description in plain English.
        #[arg(long)]
        task: String,
        /// Repo path to index (default: current dir).
        #[arg(default_value = ".")]
        path: String,
        /// Output format: json | md.
        #[arg(long, default_value = "md")]
        format: String,
        /// Max files to describe.
        #[arg(long, default_value_t = 12)]
        max_files: usize,
        /// Token cap for inlined file content.
        #[arg(long, default_value_t = 6000)]
        token_budget: u32,
    },
```
```rust
    Command::Context { task, path, format, max_files, token_budget } => {
        repo_context::run(&path, &task, &format, max_files, token_budget)?;
    }
```

- [ ] **Step 4: Run → PASS** — `cargo test -p tt-cli repo_context`; `cargo build -p tt-cli`; `./target/debug/tt context --task "find auth" --format md` prints a report; clippy + fmt clean.

- [ ] **Step 5: Commit**
```bash
git add crates/cli/
git commit -m "feat(cli): tt context command (repo context preloader)"
```

---

## Task 9: Docs + full verification

**Files:**
- Modify: `docs/tt-cli-commands.md`, `docs/tt-mcp-usage.md`

- [ ] **Step 1: Document** — add `tt context` to `docs/tt-cli-commands.md` (what it does: ranks repo files by symbol/import-graph + lexical match for a task, outputs json/md, fully local) and `get_repo_context` to `docs/tt-mcp-usage.md` (read-only tool; inputs task/repo_path/max_files/token_budget; how a coding agent uses it to skip exploration). Match each file's existing format. Commit: `docs: document tt context + get_repo_context`.

- [ ] **Step 2: Full workspace verification** — run + report verbatim:
  1. `cargo fmt --all -- --check` (clean)
  2. `cargo clippy --workspace --all-targets -- -D warnings` (clean)
  3. `cargo test --workspace` (all pass; note: `cli_spawn_smoke` integration tests time out in sandboxes that can't spawn the built binary — that is a known local-env issue, green in CI; flag if seen, do not treat as a regression unless a NON-spawn test fails)
  4. `cargo test -p tt-context -p tt-inspect-core -p tt-mcp` (the feature's crates — all green)
  Fix any fmt/clippy issue in the new code. Commit any fixups: `chore(context): verification fixups` (only if needed).

---

## Self-review notes (author)

- **Spec coverage:** symbol extraction (T2) · RepoIndex+graph (T3) · ranking (T4) · token-budgeted pack (T5) · in-process cache (T6) · MCP tool (T7) · CLI (T8) · docs (T9). Measurement proxy = `token_estimate` in the pack (T5). Non-goals (embeddings/Go-Java/persistence/proxy-injection) correctly absent.
- **Type consistency:** `extract_symbols`/`FileSymbols`/`SymbolDef`/`ImportRef` (T2) used unchanged in T3; `RepoIndex`/`FileEntry`/`.files()`/`.importers` consistent T3→T4→T5; `RankedFile`/`rank` T4→T5; `ContextPack`/`ContextFile`/`assemble` T5→T6→T7→T8; `repo_context(repo_root, task, max_files, token_budget)` identical in T6/T7/T8; `Tool`/`ToolDef`/`McpError` per the real mcp signatures; `tt_tokenize::estimate_tokens("openai", text)` per the real signature.
- **Implementer must close (flagged inline, not placeholders):** the exact tree-sitter node kinds for the arrow-const case (T2 — tests drive it), the import specifier-extraction + path-resolution body in `candidate_paths` (T3 — the doc comment + tests specify the rules), and matching the clap variant/dispatch style to `main.rs`'s actual structure (T7/T8).
- **Determinism:** `rank` sorts score-desc then path-asc (stable); no RNG/clock in ranking; cache fingerprint is mtime-based and only affects freshness, not output for a fixed tree.
