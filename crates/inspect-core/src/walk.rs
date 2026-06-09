//! Directory walker that yields source files eligible for inspection.
//!
//! [`walk`] returns an iterator of `(PathBuf, Language)` pairs. It
//! automatically skips hidden directories, common build/tool artefact
//! directories, and files over 1 MB.

use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::Language;

/// Maximum file size we will attempt to parse. Guards against vendored
/// binaries and generated files that happen to have a source extension.
const MAX_FILE_SIZE: u64 = 1_000_000;

/// Directories that are always skipped, regardless of depth. In addition,
/// any directory whose name begins with `.` (hidden) is also skipped (except
/// for the scan root itself at depth 0), unless the name appears in
/// [`ALLOWED_HIDDEN_DIRS`].
const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    ".next",
    ".astro",
    ".solid",
    "vendor",
    ".pnpm-store",
    // .git is hidden so it is caught by the leading-dot rule below, but
    // listing it explicitly makes the intent obvious.
    ".git",
];

/// Hidden directories that are explicitly allowed to be descended into.
///
/// These directories commonly contain agent configuration files (e.g.
/// `.cursor/rules/*.md`, `.github/workflows/*.yml`, `.claude/CLAUDE.md`)
/// that must be reachable by security rules such as
/// `config-agents-md-contains-secrets`. All other hidden directories are
/// still pruned by the leading-dot rule.
///
/// This is an exact-name allowlist — no prefix matching.
const ALLOWED_HIDDEN_DIRS: &[&str] = &[".cursor", ".github", ".claude"];

/// Walk `root` recursively and yield every file eligible for inspection,
/// paired with the [`Language`] inferred from its extension.
///
/// Files are skipped when:
/// - they are inside a directory listed in [`SKIP_DIRS`],
/// - they are inside any hidden directory (name starts with `.`) below depth 0,
///   unless the directory name is listed in [`ALLOWED_HIDDEN_DIRS`],
/// - they are larger than 1 MB, or
/// - their extension is not one of `py`, `ts`, `tsx`, `js`, `jsx`, `mjs`,
///   `cjs`, or `md`.
///
/// Markdown files (`*.md`) are classified as [`Language::Markdown`] and are
/// walked so that rules like `config-agents-md-contains-secrets` can check
/// agent configuration files.
pub fn walk(root: &Path) -> impl Iterator<Item = (PathBuf, Language)> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Always allow the root entry (depth 0) through.
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                // Explicitly allowed hidden directories are descended into
                // before the blanket leading-dot prune runs.
                if ALLOWED_HIDDEN_DIRS.iter().any(|s| *s == name.as_ref()) {
                    return true;
                }
                // Skip all other hidden directories and any directory in the
                // skip list.
                if name.starts_with('.') {
                    return false;
                }
                if SKIP_DIRS.iter().any(|s| *s == name.as_ref()) {
                    return false;
                }
            }
            true
        })
        .filter_map(|res| res.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            // Enforce size limit.
            let size = e.metadata().ok()?.len();
            if size > MAX_FILE_SIZE {
                return None;
            }
            let path = e.path().to_path_buf();
            let ext = path.extension()?.to_str()?;
            // Single source of truth for the extension→language mapping, shared
            // with the inspect_diff single-file path (see Language::from_extension).
            let lang = Language::from_extension(ext)?;
            Some((path, lang))
        })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Build a tree like:
    ///
    /// ```text
    /// <tmp>/
    ///   src/a.py
    ///   .cursor/rules/x.md
    ///   .github/workflows/y.yml
    ///   .claude/z.md
    ///   .git/config          ← must be pruned
    ///   node_modules/p.js    ← must be pruned
    /// ```
    ///
    /// The walk must yield `src/a.py`, `.cursor/rules/x.md`,
    /// `.claude/z.md` (all have recognised extensions), and skip
    /// `.git/config` and `node_modules/p.js`.
    ///
    /// `.github/workflows/y.yml` has a `.yml` extension which is not in the
    /// recognised list, so it is not yielded — but the important thing is that
    /// the `.github` directory itself is **descended into** (no prune panic),
    /// which we verify by also placing a `.github/README.md` that IS yielded.
    #[test]
    fn walk_allowlist_and_prune() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Normal source file.
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.py"), "x = 1\n").unwrap();

        // Allowed hidden dirs — these must be descended.
        fs::create_dir_all(root.join(".cursor/rules")).unwrap();
        fs::write(root.join(".cursor/rules/x.md"), "# rules\n").unwrap();

        fs::create_dir_all(root.join(".github/workflows")).unwrap();
        fs::write(root.join(".github/workflows/y.yml"), "on: push\n").unwrap();
        fs::write(root.join(".github/README.md"), "# ci\n").unwrap();

        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(root.join(".claude/z.md"), "# claude\n").unwrap();

        // Pruned dirs — files inside must NOT be yielded.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "[core]\n").unwrap();

        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/p.js"), "module.exports={}\n").unwrap();

        let paths: Vec<PathBuf> = walk(root).map(|(p, _)| p).collect();

        // Helper: check presence / absence by suffix.
        let has = |suffix: &str| paths.iter().any(|p| p.ends_with(suffix));

        assert!(has("src/a.py"), "src/a.py must be yielded");
        assert!(
            has(".cursor/rules/x.md"),
            ".cursor/rules/x.md must be yielded"
        );
        assert!(
            has(".github/README.md"),
            ".github/README.md must be yielded"
        );
        assert!(has(".claude/z.md"), ".claude/z.md must be yielded");

        assert!(!has(".git/config"), ".git/config must be pruned");
        assert!(
            !has("node_modules/p.js"),
            "node_modules/p.js must be pruned"
        );
        // .yml is not a recognised extension — confirm not yielded.
        assert!(
            !has(".github/workflows/y.yml"),
            ".yml files must not be yielded (not a recognised extension)"
        );
    }
}
