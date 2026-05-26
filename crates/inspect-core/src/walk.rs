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
/// for the scan root itself at depth 0).
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

/// Walk `root` recursively and yield every file eligible for inspection,
/// paired with the [`Language`] inferred from its extension.
///
/// Files are skipped when:
/// - they are inside a directory listed in [`SKIP_DIRS`],
/// - they are inside any hidden directory (name starts with `.`) below depth 0,
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
                // Skip hidden directories and any directory in the skip list.
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
            let lang = match ext {
                "py" => Language::Python,
                "ts" | "tsx" => Language::Typescript,
                "js" | "jsx" | "mjs" | "cjs" => Language::Javascript,
                "md" => Language::Markdown,
                _ => return None,
            };
            Some((path, lang))
        })
}
