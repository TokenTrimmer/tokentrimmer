//! The repo index: walk + per-file symbols + a best-effort in-repo import graph.
use std::path::{Path, PathBuf};

use serde::Serialize;
use tt_inspect_core::symbols::{extract_symbols, FileSymbols};
use tt_inspect_core::walk::walk;
use tt_inspect_core::Language;

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub language: Language,
    pub symbols: FileSymbols,
    pub loc: u32,
    pub importers: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoIndex {
    root: PathBuf,
    files: Vec<FileEntry>,
}

impl RepoIndex {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Returns the indexed files in filesystem walk order (OS-dependent).
    /// Note: `FileEntry::importers` lists are sorted for determinism, but
    /// callers must not rely on positional stability of this slice across
    /// platforms or runs.
    #[must_use]
    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    #[must_use]
    pub fn build(root: &Path) -> RepoIndex {
        let mut files: Vec<FileEntry> = Vec::new();
        for (path, language) in walk(root) {
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let loc = source.lines().count() as u32;
            let symbols = extract_symbols(&source, language);
            files.push(FileEntry {
                path,
                language,
                symbols,
                loc,
                importers: Vec::new(),
            });
        }
        resolve_import_edges(root, &mut files);
        RepoIndex {
            root: root.to_path_buf(),
            files,
        }
    }
}

fn resolve_import_edges(root: &Path, files: &mut [FileEntry]) {
    let positions: std::collections::HashMap<PathBuf, usize> = files
        .iter()
        .enumerate()
        .map(|(i, f)| (f.path.clone(), i))
        .collect();
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
        if !importers.contains(&importer) {
            importers.push(importer);
        }
    }
    for f in files.iter_mut() {
        f.importers.sort();
    }
}

fn resolve_import(
    root: &Path,
    importer: &Path,
    language: Language,
    raw: &str,
    positions: &std::collections::HashMap<PathBuf, usize>,
) -> Option<usize> {
    for c in candidate_paths(root, importer, language, raw) {
        if let Some(&pos) = positions.get(&c) {
            return Some(pos);
        }
    }
    None
}

/// Normalize a path by resolving `.` and `..` components without touching the
/// filesystem.  We cannot use `Path::canonicalize` (requires the path to
/// exist), so we do it manually by accumulating the components.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components: Vec<std::path::Component> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {
                // `.` — skip
            }
            std::path::Component::ParentDir => {
                // `..` — pop the last normal component if there is one;
                // otherwise keep the `..` (e.g. we're above root, which
                // shouldn't happen in practice for in-repo imports).
                if matches!(components.last(), Some(std::path::Component::Normal(_))) {
                    components.pop();
                } else {
                    components.push(component);
                }
            }
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Extract the quoted specifier from a JS/TS import statement.
/// Returns the text between the first matching pair of `'...'` or `"..."`.
fn extract_js_specifier(raw: &str) -> Option<&str> {
    // Find the first quote character.
    let quote_pos = raw.find(['\'', '"'])?;
    let quote_char = raw.as_bytes()[quote_pos] as char;
    let after_open = &raw[quote_pos + 1..];
    let close_pos = after_open.find(quote_char)?;
    Some(&after_open[..close_pos])
}

/// Extract the dotted module name from a Python import statement.
/// - `from a.b.c import x` → `a.b.c`
/// - `import a.b.c` → `a.b.c`
/// - `import a.b.c as x` → `a.b.c`
///
/// Returns `None` if we cannot parse the statement.
fn extract_python_module(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("from ") {
        // `from <module> import ...`
        // Module name ends at the first whitespace.
        let module = rest.split_whitespace().next()?;
        // Strip a leading `.` for relative imports — we treat them as bare
        // (no in-repo edge for relative Python imports that start with `.`).
        if module.starts_with('.') {
            return None;
        }
        Some(module)
    } else if let Some(rest) = trimmed.strip_prefix("import ") {
        // `import a.b.c` or `import a.b.c as x`
        // Take only the first token (the module), stop at whitespace.
        let module = rest.split_whitespace().next()?;
        if module.starts_with('.') {
            return None;
        }
        Some(module)
    } else {
        None
    }
}

/// Candidate resolved paths for an import statement.
///
/// - **Python**: `from a.b import c` / `import a.b` → candidates
///   `<root>/a/b.py` and `<root>/a/b/__init__.py`.
/// - **JS/TS**: relative specifiers (start with `.`) → resolve against the
///   importer's directory and try all recognised JS/TS extensions + index
///   variants.  Bare specifiers (no leading `.`) → empty (external dep).
/// - **Markdown**: no imports → empty.
fn candidate_paths(root: &Path, importer: &Path, language: Language, raw: &str) -> Vec<PathBuf> {
    match language {
        Language::Python => {
            let Some(module) = extract_python_module(raw) else {
                return Vec::new();
            };
            // Map dots to path separators: `a.b.c` → `a/b/c`
            let rel_path = module.replace('.', std::path::MAIN_SEPARATOR_STR);
            vec![
                root.join(format!("{rel_path}.py")),
                root.join(&rel_path).join("__init__.py"),
            ]
        }
        Language::Javascript | Language::Typescript => {
            let Some(spec) = extract_js_specifier(raw) else {
                return Vec::new();
            };
            // Bare specifiers (external packages) → no edge.
            if !spec.starts_with('.') {
                return Vec::new();
            }
            // Resolve the specifier relative to the importer's directory.
            let importer_dir = match importer.parent() {
                Some(d) => d.to_path_buf(),
                None => root.to_path_buf(),
            };
            let raw_joined = importer_dir.join(spec);
            let base = normalize_path(&raw_joined);

            // If the specifier already has a recognised extension, just return
            // the single candidate (normalized).
            let has_src_ext = base
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e, "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs"));
            if has_src_ext {
                return vec![base];
            }

            // Otherwise generate extension-appended and index variants.
            const EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];
            let mut candidates = Vec::new();
            for &ext in EXTS {
                candidates.push(base.with_extension(ext));
                candidates.push(base.join(format!("index.{ext}")));
            }
            candidates
        }
        Language::Markdown => Vec::new(),
    }
}

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
        fs::write(
            root.join("main.py"),
            "from util import helper\n\ndef run():\n    return helper()\n",
        )
        .unwrap();
        let idx = RepoIndex::build(root);
        assert_eq!(idx.files().len(), 2);
        let main = idx
            .files()
            .iter()
            .find(|f| f.path.ends_with("main.py"))
            .unwrap();
        assert!(main.symbols.functions.iter().any(|f| f.name == "run"));
        let util = idx
            .files()
            .iter()
            .find(|f| f.path.ends_with("util.py"))
            .unwrap();
        assert!(
            util.importers.iter().any(|p| p.ends_with("main.py")),
            "util.py should record main.py as an importer; importers={:?}",
            util.importers
        );
    }

    #[test]
    fn empty_repo_is_empty() {
        let dir = tempdir().unwrap();
        assert!(RepoIndex::build(dir.path()).files().is_empty());
    }

    // ---------------------------------------------------------------------------
    // Focused candidate_paths unit tests
    // ---------------------------------------------------------------------------

    /// Python dotted module: `from util import x` → candidate `<root>/util.py`.
    #[test]
    fn candidate_paths_python_dotted() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let importer = root.join("main.py");

        let candidates = candidate_paths(root, &importer, Language::Python, "from util import x");
        assert!(
            candidates.contains(&root.join("util.py")),
            "expected util.py candidate; got {candidates:?}"
        );
        assert!(
            candidates.contains(&root.join("util/__init__.py")),
            "expected util/__init__.py candidate; got {candidates:?}"
        );
    }

    /// Python deeper module: `from a.b import c` → candidates `<root>/a/b.py`
    /// and `<root>/a/b/__init__.py`.
    #[test]
    fn candidate_paths_python_nested_module() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let importer = root.join("main.py");

        let candidates = candidate_paths(root, &importer, Language::Python, "from a.b import c");
        assert!(
            candidates.contains(&root.join("a/b.py")),
            "expected a/b.py; got {candidates:?}"
        );
        assert!(
            candidates.contains(&root.join("a/b/__init__.py")),
            "expected a/b/__init__.py; got {candidates:?}"
        );
    }

    /// `__init__.py` candidate uses proper `Path::join` segments so that on
    /// Windows (where `MAIN_SEPARATOR` is `\`) we get `root\a\b\__init__.py`
    /// rather than the mixed-separator string `root\a\b/__init__.py` which
    /// Windows treats as a single filename component.
    #[test]
    fn candidate_paths_python_init_py_uses_path_join() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let importer = root.join("main.py");

        let candidates = candidate_paths(root, &importer, Language::Python, "from a.b import c");

        // Build the expected path the proper way — this is separator-correct on
        // all platforms (Windows: root\a\b\__init__.py, Unix: root/a/b/__init__.py).
        let expected = root.join("a").join("b").join("__init__.py");
        assert!(
            candidates.contains(&expected),
            "expected __init__.py candidate built via Path::join ({expected:?}); got {candidates:?}"
        );
    }

    /// JS relative import with extension: `import './u.js'` from `a/b.js`
    /// should resolve to `<root>/a/u.js`.
    #[test]
    fn candidate_paths_js_relative_with_extension() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let importer = root.join("a/b.js");

        let candidates = candidate_paths(root, &importer, Language::Javascript, "import './u.js'");
        assert!(
            candidates.contains(&root.join("a/u.js")),
            "expected a/u.js; got {candidates:?}"
        );
    }

    /// JS relative import WITHOUT extension: `import './util'` from `a/b.js`
    /// should generate extension candidates under `a/`.
    #[test]
    fn candidate_paths_js_relative_no_extension() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let importer = root.join("a/b.js");

        let candidates = candidate_paths(root, &importer, Language::Javascript, "import './util'");
        assert!(
            candidates.contains(&root.join("a/util.js")),
            "expected a/util.js; got {candidates:?}"
        );
        assert!(
            candidates.contains(&root.join("a/util.ts")),
            "expected a/util.ts; got {candidates:?}"
        );
    }

    /// Bare JS specifier (`import 'react'`) → no candidates (external dep).
    #[test]
    fn candidate_paths_js_bare_specifier_empty() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let importer = root.join("index.js");

        let candidates = candidate_paths(root, &importer, Language::Javascript, "import 'react'");
        assert!(
            candidates.is_empty(),
            "bare specifier should yield no candidates; got {candidates:?}"
        );
    }
}
