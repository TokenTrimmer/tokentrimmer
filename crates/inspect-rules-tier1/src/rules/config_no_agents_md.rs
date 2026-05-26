//! Rule: `config-no-agents-md`
//!
//! Whole-scan check: fires once when the scanned project uses LLM imports but
//! has no `AGENTS.md`, `CLAUDE.md`, or `.cursorrules` file anywhere in the
//! scan tree.
//!
//! Implementation note: the `Rule` trait is stateless (called once per file
//! in whatever order the walker hands them out), so we use a pre-scan on
//! first `check()` invocation. We walk up from the first file's path to find
//! a likely project root (parent of the nearest `Cargo.toml` / `package.json`,
//! falling back to CWD), then `walkdir` that root looking for any agents
//! file. This avoids the previous order-dependent false positive where the
//! rule fired on an LLM import before the walker had reached `AGENTS.md`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use tt_inspect_core::{Finding, Language, Rule, Severity};
use walkdir::WalkDir;

/// Fires once per scan when the repo has LLM imports but no agent config file.
pub struct ConfigNoAgentsMdRule {
    /// `true` once the pre-scan has run.
    prescan_done: AtomicBool,
    /// Set by the pre-scan: did the project root contain any agents file?
    has_agents_file: AtomicBool,
    /// Set to `true` after the finding has been emitted (idempotent).
    fired: AtomicBool,
}

impl ConfigNoAgentsMdRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self {
            prescan_done: AtomicBool::new(false),
            has_agents_file: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        }
    }

    /// Walk up from `hint_path` to find the project root. Returns the nearest
    /// ancestor containing `Cargo.toml`, `package.json`, `pyproject.toml`,
    /// `pnpm-workspace.yaml`, or a `.git` directory. Falls back to CWD.
    fn detect_project_root(hint_path: &str) -> PathBuf {
        let p = Path::new(hint_path);
        let start = if p.is_file() {
            p.parent().unwrap_or(p)
        } else {
            p
        };
        let markers = [
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "pnpm-workspace.yaml",
            ".git",
        ];
        for ancestor in start.ancestors() {
            for marker in &markers {
                if ancestor.join(marker).exists() {
                    return ancestor.to_path_buf();
                }
            }
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    /// Walk the project root looking for any agents-style file. We cap depth
    /// at 4 so a million-file vendored tree doesn't stall the scan; agents
    /// files should always live near the root anyway.
    fn project_has_agents_file(root: &Path) -> bool {
        WalkDir::new(root)
            .max_depth(4)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                // Skip vendored/build dirs to keep this cheap.
                let name = e.file_name().to_string_lossy();
                e.depth() == 0
                    || !matches!(
                        name.as_ref(),
                        ".git"
                            | "target"
                            | "node_modules"
                            | ".venv"
                            | "venv"
                            | "dist"
                            | "build"
                            | ".next"
                            | "vendor"
                            | ".pnpm-store"
                    )
            })
            .filter_map(|res| res.ok())
            .any(|entry| {
                let lower = entry.file_name().to_string_lossy().to_lowercase();
                lower == "agents.md" || lower == "claude.md" || lower == ".cursorrules"
            })
    }

    /// Ensure the one-time agents-file pre-scan has run. Safe to call from
    /// any thread; only the first caller does the walkdir.
    fn ensure_prescan(&self, hint_path: &str) {
        if self
            .prescan_done
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let root = Self::detect_project_root(hint_path);
        if Self::project_has_agents_file(&root) {
            self.has_agents_file.store(true, Ordering::SeqCst);
        }
    }
}

impl Default for ConfigNoAgentsMdRule {
    fn default() -> Self {
        Self::new()
    }
}

/// LLM import signals.
const LLM_IMPORT_PATTERNS: &[&str] = &[
    "import openai",
    "from openai",
    "from \"openai\"",
    "from 'openai'",
    "import anthropic",
    "from anthropic",
    "from \"anthropic\"",
    "from 'anthropic'",
    "@anthropic-ai/sdk",
    "import langchain",
    "from langchain",
    "crewai",
    "langgraph",
    "autogen",
];

/// Return `true` when the file path indicates a test fixture.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for ConfigNoAgentsMdRule {
    fn id(&self) -> &'static str {
        "config-no-agents-md"
    }

    fn severity(&self) -> Severity {
        Severity::Medium
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[
            Language::Python,
            Language::Typescript,
            Language::Javascript,
            Language::Markdown,
        ]
    }

    fn check(&self, source: &str, _language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        // One-time project-root pre-scan: looks for AGENTS.md / CLAUDE.md /
        // .cursorrules anywhere in the project root. Order-independent —
        // unlike the previous incremental observation which would fire
        // before the walker had reached the agents file.
        self.ensure_prescan(path);

        // If pre-scan found an agents file, this project is already configured.
        if self.has_agents_file.load(Ordering::Relaxed) {
            return vec![];
        }

        // Check for LLM imports in this file.
        let has_import = LLM_IMPORT_PATTERNS.iter().any(|p| source.contains(p));
        if !has_import {
            return vec![];
        }

        // First LLM-importing file we see in an agents-fileless project — fire once.
        if self.fired.swap(true, Ordering::Relaxed) {
            return vec![];
        }

        vec![Finding {
            rule_id: self.id().to_string(),
            severity: self.severity(),
            file: path.to_string(),
            line: 1,
            message: "This project uses LLM libraries but has no AGENTS.md, CLAUDE.md, \
                       or .cursorrules file. Without a project-context file, AI tools work \
                       blind and users must provide verbose prompts on every session."
                .to_string(),
            confidence: 0.9,
            fix_hint: Some(
                "Create an AGENTS.md (or CLAUDE.md) at the repo root describing the project, \
                 build commands, and coding conventions. Run `tt inspect --suggest-agents-md` \
                 for a generated starter."
                    .to_string(),
            ),
        }]
    }
}
