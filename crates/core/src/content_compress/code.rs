//! P1c — the AST CODE compressor backend (LOSSY, judge-gated, re-parse-verified).
//!
//! TT's headline `compression` pass is "content-lossless trim of *non-prose*" — a
//! big source file (a tool result that dumps a file, a system prompt embedding a
//! module) moves ~0 billed tokens through it. This module closes that gap with a
//! deterministic (NO ML — that is Phase 2) AST-aware compressor: it parses the
//! block with the SAME tree-sitter harness the offline `inspect-core` rules use
//! ([`tt_inspect_core::parse::parse_cached`]), then **keeps** the load-bearing
//! surface — imports/`use`/`include` statements, function/method/class SIGNATURES,
//! type/struct/enum/interface declarations, top-level constants, and doc comments
//! on public items — while **truncating** long function/method BODIES down to a
//! single marker line (`… N lines elided`) that preserves the signature and the
//! surrounding braces/colon block.
//!
//! Only the three languages the inspect harness bundles a grammar for are
//! compressible — Python, TypeScript, JavaScript
//! ([`tt_inspect_core::Language`]); a block in any other language (Rust, Go, …) or
//! a fenced/markdown-embedded snippet that will not parse cleanly is left VERBATIM
//! (the fail-open posture — see [`detect_parse`]).
//!
//! # Never serve broken code (the headroom rule, reimplemented)
//! Body truncation is a structural edit, so it can in principle desync the syntax.
//! We adopt headroom's "never serve broken code" invariant VERBATIM as a hard
//! gate: after producing the candidate text we **re-parse it**, and if the result
//! contains ANY `ERROR` or `MISSING` node (it is syntactically broken) we DISCARD
//! the candidate and return the ORIGINAL — no compression, no saving. A perfectly
//! valid signature-preserving truncation is served; anything else fails open.
//!
//! # Moat-wrap (reused, NOT reinvented)
//! Truncating bodies DROPS code, so — exactly like the P1b prose backend — this
//! backend commits ONLY behind the shared quality gate the lossy `summarize` lever
//! rides ([`crate::passes::agentic_budget::summarize_judge::SummaryGate`]), keyed
//! by [`CODE_CLASS`]. The dispatcher reads the gate for the `"code"` class: a
//! judge-trusted class compresses; an untrusted / closed / unavailable gate
//! **fails OPEN to the verbatim bytes** (no compression, no saving). The
//! production [`RatchetSummaryGate`](crate::passes::agentic_budget::summarize_judge::RatchetSummaryGate)
//! opens `"code"` from an operator allowlist and auto-pauses it below the sticky
//! **0.90 recall floor** (the same `route_autopause` floor the prose lever and the
//! down-route judge use). The compacted result still rides the pipeline's
//! TOKEN-TRUE GATE: a block that tokenizes larger than the input is discarded →
//! verbatim.
//!
//! # Attribution
//! The re-parse "never serve broken code" invariant is TT's Rust reimplementation
//! of the primitive popularized by headroom's code compressor (headroom is
//! Apache-2.0). The AST walk, the keep/truncate policy, and the markers here are
//! original TT code written against the documented behavior — NONE of headroom's
//! source was copied.

use std::sync::Arc;

use tt_inspect_core::parse::parse_cached;
use tt_inspect_core::tree_sitter::{Node, Tree};
use tt_inspect_core::Language;

/// The [`SummaryGate`](crate::passes::agentic_budget::summarize_judge::SummaryGate)
/// class key for CODE compression. An operator opens the lever by adding this to
/// `TT_SUMMARIZE_TRUSTED_CLASSES` (the shared lossy-lever allowlist); the
/// 0.90-floor ratchet then auto-pauses it on sustained sub-0.90 recall. Distinct
/// from [`PROSE_CLASS`](super::prose::PROSE_CLASS) and the summarize lever's
/// tool-name classes, so the levers never collide.
pub const CODE_CLASS: &str = "code";

/// Code blocks below this many (trimmed) bytes are left verbatim — AST truncation
/// is not worth the lossy risk on a short snippet. Well above the classifier's
/// [`MIN_BLOB_CHARS`](tt_shared::content_kind::MIN_BLOB_CHARS) so a merely
/// classifiable block is not automatically compressed.
pub const CODE_MIN_CHARS: usize = 400;

/// A function/method body spanning at most this many source lines is left
/// verbatim — truncating a short body saves little and risks losing meaning for
/// no gain. Only bodies STRICTLY longer than this are elided to a marker.
const BODY_MIN_LINES: usize = 6;

/// Compress a code block AST-aware. Returns `Some((compacted, est_tokens_removed))`
/// on a strict, syntactically-verified shrink, or `None` when the block is too
/// small / not one of the supported languages / has no long body to truncate /
/// the result is not shorter / the result would be syntactically broken (the
/// "never serve broken code" fail-safe — the ORIGINAL is returned).
///
/// The caller (the dispatcher) applies the judge gate BEFORE calling this, and the
/// pipeline's token-true gate measures the AUTHORITATIVE token delta; the returned
/// `est_tokens_removed` is an informational whitespace-token-delta estimate.
#[must_use]
pub fn compress(text: &str) -> Option<(String, usize)> {
    compress_inner(text, default_block_marker, default_brace_marker)
}

/// The compression core, parameterized on the two body markers so a test can
/// inject a marker that produces SYNTACTICALLY BROKEN code and assert the
/// re-parse-verify gate fires (returns the original). Production always calls it
/// via [`compress`] with the real, valid markers.
fn compress_inner(
    text: &str,
    // (indent_columns, elided_line_count) -> a valid Python block-body replacement.
    block_marker: impl Fn(usize, usize) -> String,
    // (elided_line_count) -> a valid JS/TS brace-body replacement (keeps `{`/`}`).
    brace_marker: impl Fn(usize) -> String,
) -> Option<(String, usize)> {
    if text.trim().len() < CODE_MIN_CHARS {
        return None;
    }

    // Detect the language by picking the grammar that parses the input CLEANLY —
    // this both identifies the language AND proves the input is not already broken
    // (we never "compress" a block we cannot fully parse).
    let (lang, tree) = detect_parse(text)?;

    // Collect the byte edits that truncate every OUTERMOST long function body.
    let edits = collect_body_edits(tree.root_node(), lang, &block_marker, &brace_marker);
    if edits.is_empty() {
        return None; // nothing long enough to truncate → leave verbatim
    }

    let candidate = apply_edits(text, &edits);
    // Strict-shrink guard (the pipeline token-true gate is the final arbiter).
    if candidate.len() >= text.len() {
        return None;
    }

    // ── NEVER SERVE BROKEN CODE ────────────────────────────────────────────────
    // Re-parse the candidate; if it has ANY ERROR or MISSING node, DISCARD it and
    // return the ORIGINAL (no compression, no saving).
    if !reparse_is_clean(&candidate, lang) {
        return None;
    }

    let removed = word_count(text).saturating_sub(word_count(&candidate));
    Some((candidate, removed))
}

/// Identify the language + return a CLEAN parse of `text`, or `None` when no
/// bundled grammar parses it without error. The candidate order is a cheap
/// signal-count heuristic (Python-ish vs C-family), but correctness rests on the
/// clean-parse check: a block is only ever compressed in a language it parses
/// error-free, so Rust/Go/… and fenced/markdown snippets fail open here.
fn detect_parse(text: &str) -> Option<(Language, Arc<Tree>)> {
    for lang in candidate_order(text) {
        if let Ok(tree) = parse_cached(text, lang) {
            if !tree_is_broken(tree.root_node()) {
                return Some((lang, tree));
            }
        }
    }
    None
}

/// Order the candidate grammars by a cheap signal count. TypeScript is tried
/// before JavaScript because its grammar is a superset (it also parses plain JS),
/// and Python is placed first only when its signals dominate. Ordering only breaks
/// ties — [`detect_parse`] validates each candidate by a clean parse regardless.
fn candidate_order(text: &str) -> [Language; 3] {
    let py = count_any(
        text,
        &[
            "def ",
            "\nimport ",
            "\nfrom ",
            "elif ",
            "self.",
            "print(",
            "):\n",
        ],
    );
    let cfamily = count_any(
        text,
        &["function ", "=>", "const ", "let ", "export ", "};", ") {"],
    );
    if py > cfamily {
        [Language::Python, Language::Typescript, Language::Javascript]
    } else {
        [Language::Typescript, Language::Javascript, Language::Python]
    }
}

/// Sum of the occurrences of every needle in `hay`.
fn count_any(hay: &str, needles: &[&str]) -> usize {
    needles.iter().map(|n| hay.matches(n).count()).sum()
}

/// A single body-truncation edit: replace `text[start..end]` with `replacement`.
struct Edit {
    start: usize,
    end: usize,
    replacement: String,
}

/// Walk the tree and collect a truncation edit for every OUTERMOST function /
/// method body that spans more than [`BODY_MIN_LINES`] lines. Class / struct /
/// interface / enum bodies are NOT function bodies, so their member SIGNATURES
/// survive — only the method bodies inside them are truncated. Once a body is
/// truncated we do not descend into it (a nested closure inside a truncated body
/// is elided with the body), so the edits never overlap.
fn collect_body_edits(
    root: Node,
    _lang: Language,
    block_marker: &impl Fn(usize, usize) -> String,
    brace_marker: &impl Fn(usize) -> String,
) -> Vec<Edit> {
    let mut edits: Vec<Edit> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_function_node(node.kind()) {
            if let Some(body) = node.child_by_field_name("body") {
                if let Some(edit) = body_edit(body, block_marker, brace_marker) {
                    edits.push(edit);
                    // Do NOT descend into a truncated body (skip nested funcs).
                    continue;
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }

    // Apply in source order; drop any (defensive) overlap so the splice is safe.
    edits.sort_by_key(|e| e.start);
    let mut out: Vec<Edit> = Vec::with_capacity(edits.len());
    for e in edits {
        if out.last().is_none_or(|p| e.start >= p.end) {
            out.push(e);
        }
    }
    out
}

/// `true` for the AST node kinds whose `body` field is a truncatable function /
/// method body (Python `function_definition`; the JS/TS function forms). A class
/// / interface / enum node is deliberately NOT here — its body holds member
/// signatures we keep.
fn is_function_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_definition"              // Python def / async def
        | "function_declaration"           // JS/TS  function f() {}
        | "method_definition"              // JS/TS  class method
        | "function_expression"            // JS/TS  const f = function () {}
        | "arrow_function"                 // JS/TS  const f = () => {}
        | "generator_function_declaration" // JS/TS  function* g() {}
        | "generator_function" // JS/TS  const g = function* () {}
    )
}

/// Build the truncation edit for one function `body` node, or `None` when the body
/// is short (≤ [`BODY_MIN_LINES`]) or is not a block/brace body (e.g. an
/// expression-bodied arrow, which we leave alone).
fn body_edit(
    body: Node,
    block_marker: &impl Fn(usize, usize) -> String,
    brace_marker: &impl Fn(usize) -> String,
) -> Option<Edit> {
    let n_lines = body
        .end_position()
        .row
        .saturating_sub(body.start_position().row)
        + 1;
    if n_lines <= BODY_MIN_LINES {
        return None;
    }
    let replacement = match body.kind() {
        // Python indented `:`-block. The body node begins at the first statement
        // (the indentation whitespace stays in the retained prefix), so the marker
        // is emitted at that column to keep the indent consistent.
        "block" => block_marker(body.start_position().column, n_lines),
        // JS/TS `{ … }` — the marker keeps the braces and replaces the interior.
        "statement_block" => brace_marker(n_lines),
        _ => return None,
    };
    Some(Edit {
        start: body.start_byte(),
        end: body.end_byte(),
        replacement,
    })
}

/// The default Python block-body marker: a valid `...` (Ellipsis) statement plus
/// an elision comment, indented to the body column. Re-parses as a one-statement
/// body, so the signature + the `:`-block survive syntactically intact.
fn default_block_marker(indent: usize, n_lines: usize) -> String {
    let _ = indent; // the retained prefix already carries the body indentation
    format!("...  # \u{2026} {n_lines} lines elided")
}

/// The default JS/TS brace-body marker: keep the braces, replace the interior with
/// a block comment. Re-parses as an empty-bodied function.
fn default_brace_marker(n_lines: usize) -> String {
    format!("{{ /* \u{2026} {n_lines} lines elided */ }}")
}

/// Splice the (sorted, non-overlapping) edits into `text`.
fn apply_edits(text: &str, edits: &[Edit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0usize;
    for e in edits {
        if e.start < pos {
            continue; // defensive: never splice backwards
        }
        out.push_str(&text[pos..e.start]);
        out.push_str(&e.replacement);
        pos = e.end;
    }
    out.push_str(&text[pos..]);
    out
}

/// Parse `candidate` as `lang` and report whether it is syntactically clean (no
/// `ERROR` / `MISSING` node) — the re-parse-verify gate. A parse error (NUL bytes
/// / cancellation) counts as broken → fail open.
fn reparse_is_clean(candidate: &str, lang: Language) -> bool {
    match parse_cached(candidate, lang) {
        Ok(tree) => !tree_is_broken(tree.root_node()),
        Err(_) => false,
    }
}

/// `true` when the tree rooted at `root` contains ANY `ERROR` or `MISSING` node.
/// tree-sitter's aggregate [`Node::has_error`] already flags both, but we also
/// walk explicitly for the `MISSING` case so the "never serve broken code" gate is
/// unambiguous and robust across grammar/aggregate-bit quirks.
fn tree_is_broken(root: Node) -> bool {
    if root.has_error() {
        return true;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            return true;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

/// Whitespace-token count (an informational size proxy; the pipeline token-true
/// gate measures the authoritative tokenizer delta).
fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic Python source block: imports + a top-level constant + a class
    /// with two long methods + a top-level function with a long body. Comfortably
    /// over `CODE_MIN_CHARS`; every body is > `BODY_MIN_LINES` lines. Parses clean.
    fn python_source() -> String {
        r#"import os
import sys
from typing import List, Optional

DEFAULT_TIMEOUT = 30


class Client:
    def connect(self, host: str, port: int) -> bool:
        attempts = 0
        last_error = None
        while attempts < DEFAULT_TIMEOUT:
            attempts += 1
            try:
                self._open(host, port)
                return True
            except OSError as exc:
                last_error = exc
                continue
        return False

    def _open(self, host: str, port: int) -> None:
        self.sock = make_socket()
        self.sock.settimeout(DEFAULT_TIMEOUT)
        self.sock.connect((host, port))
        self.buffer = bytearray()
        self.ready = True
        self.host = host
        self.port = port


def main(argv: List[str]) -> int:
    client = Client()
    connected = client.connect("localhost", 8080)
    if not connected:
        print("connection failed after retries")
        return 1
    print("connection established")
    client._open("localhost", 8080)
    return 0
"#
        .to_string()
    }

    /// A realistic TypeScript source block: an import + an interface + a class with
    /// a long method + a top-level function with a long body.
    fn typescript_source() -> String {
        r#"import { Socket } from "net";

interface ClientOptions {
    host: string;
    port: number;
    timeout: number;
}

export class Client {
    private sock: Socket | null = null;

    public connect(opts: ClientOptions): boolean {
        let attempts = 0;
        let established = false;
        while (attempts < opts.timeout) {
            attempts += 1;
            try {
                this.open(opts);
                established = true;
                break;
            } catch (err) {
                continue;
            }
        }
        return established;
    }

    private open(opts: ClientOptions): void {
        this.sock = new Socket();
        this.sock.setTimeout(opts.timeout);
        this.sock.connect(opts.port, opts.host);
        this.ready = true;
    }
}
"#
        .to_string()
    }

    #[test]
    fn python_keeps_imports_and_signatures_truncates_bodies() {
        let src = python_source();
        let (out, removed) = compress(&src).expect("a large clean python block compresses");
        assert!(out.len() < src.len(), "output is strictly shorter");
        assert!(removed > 0, "an informational token-delta is reported");

        // Imports, the constant, the class header, and every signature survive.
        for needle in [
            "import os",
            "from typing import List, Optional",
            "DEFAULT_TIMEOUT = 30",
            "class Client:",
            "def connect(self, host: str, port: int) -> bool:",
            "def _open(self, host: str, port: int) -> None:",
            "def main(argv: List[str]) -> int:",
        ] {
            assert!(
                out.contains(needle),
                "kept surface {needle:?} must survive:\n{out}"
            );
        }

        // A long body's interior was elided (the marker is present, the body text
        // is gone).
        assert!(
            out.contains("lines elided"),
            "a truncation marker is present:\n{out}"
        );
        assert!(
            !out.contains("connection failed after retries"),
            "a truncated body's contents are dropped:\n{out}"
        );

        // And crucially: the result is syntactically clean Python (the backend
        // only ever returns a re-parse-verified candidate).
        assert!(
            reparse_is_clean(&out, Language::Python),
            "compressed output re-parses clean"
        );
    }

    #[test]
    fn typescript_keeps_imports_and_signatures_truncates_bodies() {
        let src = typescript_source();
        let (out, _) = compress(&src).expect("a large clean typescript block compresses");
        assert!(out.len() < src.len());
        for needle in [
            "import { Socket }",
            "interface ClientOptions",
            "connect(opts: ClientOptions): boolean",
            "private open(opts: ClientOptions): void",
        ] {
            assert!(
                out.contains(needle),
                "kept surface {needle:?} must survive:\n{out}"
            );
        }
        assert!(
            out.contains("lines elided"),
            "a truncation marker is present:\n{out}"
        );
        // Re-parse clean in TS (or its JS superset the detector may have chosen).
        assert!(
            reparse_is_clean(&out, Language::Typescript)
                || reparse_is_clean(&out, Language::Javascript)
        );
    }

    #[test]
    fn reparse_gate_returns_original_on_broken_transform() {
        // The re-parse-verify gate ("never serve broken code") fires: a marker that
        // produces SYNTACTICALLY BROKEN code must be discarded → `None` (the caller
        // keeps the ORIGINAL), even though the same input compresses with the real
        // markers.
        let src = python_source();
        let broken = compress_inner(
            &src,
            |_indent, _n| "def (((".to_string(), // invalid Python body
            |_n| "{ def ((( }".to_string(),      // invalid JS body
        );
        assert_eq!(
            broken, None,
            "a would-break transform must return the original (never serve broken code)"
        );
        // Sanity: the REAL markers DO compress the very same input.
        assert!(
            compress(&src).is_some(),
            "the real markers compress the same block"
        );
    }

    #[test]
    fn reparse_is_clean_detects_broken_and_missing() {
        // Direct proof the gate distinguishes clean from broken source.
        assert!(reparse_is_clean(
            "def f():\n    return 1\n",
            Language::Python
        ));
        assert!(
            !reparse_is_clean("def f(:\n    return 1\n", Language::Python),
            "an ERROR node must read as broken"
        );
    }

    #[test]
    fn non_code_prose_is_not_compressed() {
        // A prose block (what the classifier would tag Prose, NOT Code) does not
        // parse as any supported language → the code backend fails open to None.
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(40);
        assert_eq!(
            compress(&prose),
            None,
            "prose is not routed/handled by the code backend"
        );
    }

    #[test]
    fn small_code_block_is_untouched() {
        // Below CODE_MIN_CHARS → left verbatim even though it is valid code.
        let small = "def f(x):\n    return x + 1\n";
        assert!(small.len() < CODE_MIN_CHARS);
        assert_eq!(compress(small), None, "a small code block is left verbatim");
    }

    #[test]
    fn short_bodies_only_yields_no_truncation() {
        // A long-enough file whose every body is <= BODY_MIN_LINES lines: signatures
        // are already the whole content, nothing to truncate → None (no same-length
        // rewrite). Padded with imports to clear CODE_MIN_CHARS.
        let mut src = String::new();
        for i in 0..30 {
            src.push_str(&format!("import module_number_{i}_with_a_long_name\n"));
        }
        for i in 0..10 {
            src.push_str(&format!("def short_{i}(a, b):\n    return a + b\n"));
        }
        assert!(src.len() > CODE_MIN_CHARS);
        assert_eq!(
            compress(&src),
            None,
            "no body exceeds the threshold → nothing truncated"
        );
    }

    #[test]
    fn unsupported_language_fails_open() {
        // Rust source is classified Code upstream, but no bundled grammar parses it
        // cleanly → the code backend fails open (verbatim).
        let rust = "pub fn add(a: i32, b: i32) -> i32 {\n    let mut acc = a;\n    acc += b;\n    acc += 1;\n    acc -= 1;\n    acc += b;\n    acc\n}\n".repeat(8);
        assert!(rust.len() > CODE_MIN_CHARS);
        assert_eq!(
            compress(&rust),
            None,
            "an unsupported language is left verbatim"
        );
    }
}
