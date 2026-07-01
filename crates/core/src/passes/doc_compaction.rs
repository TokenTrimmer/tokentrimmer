//! Document-compaction pass (Document Lane D2) — an opt-in, lossless,
//! text-side compaction of LARGE non-prose documents.
//!
//! It runs **only** when a matched route opted in (`RouteAction::doc_compaction`)
//! and is **off by default**. Like [`compression`](crate::passes::compression)
//! it operates on the [`VolatileTail`] the pipeline hands it (the cache-stable
//! prefix is read-only by type) and touches ONLY non-prose text blocks — System
//! and Tool message content — never user/assistant prose. Every transform is
//! text-only, so it rides the pipeline's token-true gate cleanly, and the
//! measured token delta folds into `baseline_cost_usd` exactly like the
//! `compression` pass (no new attestation term).
//!
//! # What distinguishes it from `compression`
//!
//! `compression` is a whole-tail canonicalizer (per-line trailing-whitespace,
//! blank-line collapse, adjacent-message dedup). This pass instead targets the
//! *inside* of a single LARGE document block (≥ [`MIN_DOC_BYTES`]), where the
//! redundancy that actually moves billed tokens lives: exactly-duplicated
//! multi-line blocks and repeated pure-formatting boilerplate that assembled
//! RAG / tool-result payloads accumulate. Small blocks are left byte-identical.
//!
//! # The three lossless transforms (applied per LARGE text block, in order)
//!
//! 1. **markdown-normalize** — strip trailing whitespace on each line and
//!    collapse runs of 3+ consecutive blank lines to a single blank line.
//!    Trailing spaces/tabs and padding blank-line runs carry no information in
//!    machine-assembled documents; one blank line preserves any paragraph break.
//! 2. **boilerplate-strip** — a line is *boilerplate* when it is a pure
//!    separator (≥ [`MIN_SEPARATOR_LEN`] characters, every non-whitespace
//!    character drawn from the separator set `-=*_~#+.`). When the SAME
//!    separator line occurs at least [`BOILERPLATE_MIN_REPEATS`] times across
//!    the document, the first occurrence is kept and the rest dropped —
//!    removing redundant repeated dividers while preserving the surrounding
//!    text and its blank-line paragraph structure.
//! 3. **exact block dedup** — split the document into paragraphs on blank-line
//!    boundaries; a paragraph of ≥ [`MIN_DEDUP_LINES`] lines that is
//!    byte-identical to an EARLIER kept paragraph is dropped (keeping the
//!    first). Re-sending an identical multi-line block adds no information; the
//!    ≥3-line floor never merges legitimately-repeated short lines.
//!
//! A rewrite is committed only when it strictly shrinks the block; if any step
//! would not remove characters the original is kept. That conservatism — plus
//! being off-by-default and text-only under the token-true gate — is the safety.

use tt_shared::messages::{ContentPart, Message, MessageContent};

use super::split::{PassContext, StablePrefix, VolatileTail};
use super::{push_content_text, PassOutcome, RequestPass};

/// Minimum size (bytes) of a text block for the pass to consider it a
/// "document". Blocks smaller than this are left byte-identical — the redundancy
/// this pass targets (duplicated multi-line blocks, repeated dividers) only
/// appears at document scale, and skipping small blocks keeps chat-shaped
/// traffic untouched.
pub const MIN_DOC_BYTES: usize = 4096;

/// A paragraph must be at least this many lines to be a dedup candidate — the
/// floor that prevents dropping legitimately-repeated short lines.
pub const MIN_DEDUP_LINES: usize = 3;

/// A pure-separator line must occur at least this many times to have its
/// repeats stripped (the first occurrence is always kept).
pub const BOILERPLATE_MIN_REPEATS: usize = 3;

/// Minimum trimmed length for a line to qualify as a pure separator — avoids
/// treating a lone `#`/`-` (which may be meaningful markdown) as boilerplate.
pub const MIN_SEPARATOR_LEN: usize = 3;

/// The lossless document-compaction stage (Document Lane D2).
#[derive(Debug, Clone, Copy, Default)]
pub struct DocCompactionPass;

impl DocCompactionPass {
    /// Construct the pass.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl RequestPass for DocCompactionPass {
    fn name(&self) -> &'static str {
        "doc-compaction"
    }

    fn apply(
        &self,
        _stable: &StablePrefix<'_>,
        tail: &mut VolatileTail<'_>,
        cx: &PassContext<'_>,
    ) -> PassOutcome {
        // Measure the non-prose text we are allowed to touch BEFORE and AFTER
        // the compaction and report the tokenized delta. (Informational: the
        // pipeline's token-true gate measures the whole-tail delta itself and
        // uses that for attribution; this self-report is drift telemetry.)
        let before = touchable_text(tail.messages());
        compact_in_place(tail);
        let after = touchable_text(tail.messages());

        let before_tokens =
            tt_tokenize::estimate_tokens_for_model(cx.provider_id, cx.model, &before);
        let after_tokens = tt_tokenize::estimate_tokens_for_model(cx.provider_id, cx.model, &after);
        PassOutcome {
            tokens_removed: before_tokens.saturating_sub(after_tokens),
            warnings: Vec::new(),
        }
    }
}

/// Concatenate the text of every block the pass is *allowed* to modify (System +
/// Tool message text) — used to measure the token delta. User/Assistant prose is
/// excluded, mirroring the compression pass's non-prose selection.
fn touchable_text(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        if let Message::System { content } | Message::Tool { content, .. } = m {
            push_content_text(&mut out, content);
        }
    }
    out
}

/// Apply the lossless document transforms to the volatile tail in place. Only
/// LARGE (`>= MIN_DOC_BYTES`) System / Tool text blocks are rewritten.
fn compact_in_place(tail: &mut VolatileTail<'_>) {
    for m in tail.messages_mut() {
        match m {
            Message::System { content } | Message::Tool { content, .. } => {
                compact_content(content);
            }
            // User + Assistant prose are never touched (a Markdown hard-break or
            // deliberate spacing in typed prose could be meaningful).
            Message::User { .. } | Message::Assistant { .. } => {}
        }
    }
}

/// Apply the document transforms to a [`MessageContent`] in place, per text
/// block, only when the block is at least [`MIN_DOC_BYTES`] bytes.
fn compact_content(content: &mut MessageContent) {
    match content {
        MessageContent::Text(s) => {
            if let Some(compacted) = compact_block(s) {
                *s = compacted;
            }
        }
        MessageContent::Parts(parts) => {
            for p in parts {
                if let ContentPart::Text { text } = p {
                    if let Some(compacted) = compact_block(text) {
                        *text = compacted;
                    }
                }
            }
        }
    }
}

/// Losslessly compact one LARGE document text block. Returns `Some(new)` only
/// when the result is strictly shorter than the input (a real removal);
/// `None` when the block is below [`MIN_DOC_BYTES`] or nothing could be safely
/// removed (caller keeps the original).
fn compact_block(s: &str) -> Option<String> {
    if s.len() < MIN_DOC_BYTES {
        return None;
    }
    let normalized = markdown_normalize(s);
    let de_boilerplated = strip_repeated_boilerplate(&normalized);
    let result = dedup_repeated_blocks(&de_boilerplated);
    // Only commit a strict shrink (never grow, never rewrite for a wash).
    if result.len() < s.len() {
        Some(result)
    } else {
        None
    }
}

/// Transform 1: strip trailing whitespace on each line and collapse runs of 3+
/// consecutive blank lines to a single blank line.
fn markdown_normalize(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blank_run = 0usize;
    // Buffer trimmed line strings so we can borrow them; collect first.
    let trimmed: Vec<String> = s.split('\n').map(|l| l.trim_end().to_string()).collect();
    for line in &trimmed {
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push(line);
            }
        } else {
            blank_run = 0;
            out.push(line);
        }
    }
    out.join("\n")
}

/// True when a line is a pure separator: at least [`MIN_SEPARATOR_LEN`]
/// non-whitespace characters, every one of which is drawn from the separator
/// set. (Whitespace between separator characters is allowed, e.g. `- - - -`.)
fn is_separator_line(line: &str) -> bool {
    const SEP: &[char] = &['-', '=', '*', '_', '~', '#', '+', '.'];
    let non_ws: usize = line.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws < MIN_SEPARATOR_LEN {
        return false;
    }
    line.chars().all(|c| c.is_whitespace() || SEP.contains(&c))
}

/// Transform 2: when the SAME pure-separator line occurs at least
/// [`BOILERPLATE_MIN_REPEATS`] times across the document, keep the first
/// occurrence and drop the rest. Non-separator lines and separators that repeat
/// fewer times are left exactly in place.
fn strip_repeated_boilerplate(s: &str) -> String {
    use std::collections::HashMap;
    let lines: Vec<&str> = s.split('\n').collect();

    // Count occurrences of each separator line.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in &lines {
        if is_separator_line(line) {
            *counts.entry(*line).or_insert(0) += 1;
        }
    }

    let mut seen: HashMap<&str, bool> = HashMap::new();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    for line in &lines {
        if is_separator_line(line)
            && counts.get(line).copied().unwrap_or(0) >= BOILERPLATE_MIN_REPEATS
        {
            // Keep the first occurrence, drop later repeats.
            if seen.insert(*line, true).is_none() {
                out.push(line);
            }
        } else {
            out.push(line);
        }
    }
    out.join("\n")
}

/// Transform 3: split on blank-line boundaries into paragraphs; drop any
/// paragraph of at least [`MIN_DEDUP_LINES`] lines that is byte-identical to an
/// earlier KEPT paragraph (keeping the first occurrence). Short paragraphs and
/// blank separators between paragraphs are preserved.
fn dedup_repeated_blocks(s: &str) -> String {
    use std::collections::HashSet;

    // Partition into paragraphs (maximal runs of non-blank lines) while
    // remembering the exact blank-line separators so the rebuild preserves
    // spacing between kept paragraphs.
    let lines: Vec<&str> = s.split('\n').collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());

    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].is_empty() {
            out.push(lines[i]);
            i += 1;
            continue;
        }
        // Gather the paragraph (contiguous non-blank lines).
        let start = i;
        while i < lines.len() && !lines[i].is_empty() {
            i += 1;
        }
        let para_lines = &lines[start..i];
        if para_lines.len() >= MIN_DEDUP_LINES {
            let key = para_lines.join("\n");
            if seen.contains(&key) {
                // Duplicate multi-line block: drop it. Also drop ONE trailing
                // blank separator so we don't leave a double blank behind.
                if i < lines.len() && lines[i].is_empty() {
                    i += 1;
                }
                continue;
            }
            seen.insert(key);
        }
        out.extend_from_slice(para_lines);
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::split::SplitRequest;
    use tt_shared::messages::{Message, MessageContent};
    use tt_shared::ChatCompletionRequest;

    /// Apply the pass through an ALL-VOLATILE split (no pricing → no cache
    /// minimum → empty stable prefix).
    fn run(req: &mut ChatCompletionRequest) -> u32 {
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(req, &cx);
        split
            .run_pass(|stable, tail| DocCompactionPass::new().apply(stable, tail, &cx))
            .tokens_removed
    }

    fn tool(text: &str, id: &str) -> Message {
        Message::Tool {
            content: MessageContent::Text(text.into()),
            tool_call_id: id.into(),
        }
    }
    fn sys(text: &str) -> Message {
        Message::System {
            content: MessageContent::Text(text.into()),
        }
    }
    fn user(text: &str) -> Message {
        Message::User {
            content: MessageContent::Text(text.into()),
            name: None,
        }
    }
    fn text_of(m: &Message) -> String {
        match m {
            Message::System { content }
            | Message::User { content, .. }
            | Message::Tool { content, .. } => match content {
                MessageContent::Text(s) => s.clone(),
                MessageContent::Parts(_) => String::new(),
            },
            Message::Assistant { .. } => String::new(),
        }
    }

    /// Pad a block past the MIN_DOC_BYTES threshold with a large unique
    /// prefix so the transform under test is what actually shrinks it.
    fn large(body: &str) -> String {
        let filler: String = (0..200)
            .map(|n| format!("unique context line number {n} with some words\n"))
            .collect();
        // Blank line between the filler and the body so the body's paragraphs
        // are cleanly blank-line-delimited (the shape real RAG/tool payloads
        // carry — the pass only dedups blank-line-delimited blocks).
        format!("{filler}\n{body}")
    }

    #[test]
    fn small_block_is_untouched() {
        // Below MIN_DOC_BYTES → byte-identical, zero savings, even with
        // redundant whitespace/blank runs a large block would get trimmed for.
        let body = "row1   \n\n\n\n\nrow2   ";
        assert!(body.len() < MIN_DOC_BYTES);
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(body, "c1")],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(text_of(&req.messages[0]), body, "small block untouched");
        assert_eq!(removed, 0);
    }

    #[test]
    fn markdown_normalize_collapses_blank_runs_and_trailing_ws() {
        let out = markdown_normalize("a   \n\n\n\n\nb\t\t\nc");
        assert_eq!(out, "a\n\nb\nc");
    }

    #[test]
    fn markdown_normalize_on_large_block_strips_trailing_ws() {
        // Trailing whitespace costs real tokens under tiktoken, so a large
        // block laden with it yields an honest positive delta.
        let body = large("payload aaaa bbbb   \n\n\n\n\ncccc dddd   \neeee   ");
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&body, "c1")],
            ..Default::default()
        };
        let removed = run(&mut req);
        let got = text_of(&req.messages[0]);
        assert!(!got.contains("   \n"), "trailing whitespace removed");
        assert!(!got.contains("\n\n\n"), "3+ blank runs collapsed");
        assert!(removed > 0, "trailing-ws trim removed tokens");
    }

    #[test]
    fn strips_repeated_separator_boilerplate() {
        // A pure-separator divider repeated many times keeps only the first.
        let mut body = String::new();
        for n in 0..12 {
            body.push_str("--------------------\n");
            body.push_str(&format!("section {n} body text goes here\n"));
        }
        let padded = large(&body);
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&padded, "c1")],
            ..Default::default()
        };
        run(&mut req);
        let got = text_of(&req.messages[0]);
        assert_eq!(
            got.matches("--------------------").count(),
            1,
            "only the first repeated separator survives"
        );
        // The section bodies are all preserved.
        for n in 0..12 {
            assert!(got.contains(&format!("section {n} body text goes here")));
        }
    }

    #[test]
    fn single_separator_is_preserved() {
        // Fewer than BOILERPLATE_MIN_REPEATS separators are left in place.
        let out = strip_repeated_boilerplate("intro\n---\nbody\n---\noutro");
        assert_eq!(out, "intro\n---\nbody\n---\noutro");
    }

    #[test]
    fn dedups_exact_repeated_multiline_block() {
        let block = "line one of the chunk\nline two of the chunk\nline three of the chunk";
        let body = format!("{block}\n\nmiddle unique paragraph\n\n{block}\n\ntail");
        let padded = large(&body);
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&padded, "c1")],
            ..Default::default()
        };
        let removed = run(&mut req);
        let got = text_of(&req.messages[0]);
        assert_eq!(
            got.matches("line two of the chunk").count(),
            1,
            "the repeated 3-line block appears exactly once"
        );
        assert!(got.contains("middle unique paragraph"));
        assert!(got.contains("tail"));
        assert!(removed > 0);
    }

    #[test]
    fn keeps_repeated_short_blocks() {
        // A 2-line paragraph (< MIN_DEDUP_LINES) repeated is NOT deduped.
        let block = "yes\nok";
        let out = dedup_repeated_blocks(&format!("{block}\n\nmid\n\n{block}"));
        assert_eq!(out.matches("yes").count(), 2, "short blocks are preserved");
    }

    #[test]
    fn never_modifies_user_prose_even_when_large() {
        // A LARGE user block with redundant whitespace + duplicated blocks a
        // tool block would get compacted for — must be byte-identical.
        let block = "para line a\npara line b\npara line c";
        let prose = large(&format!("{block}   \n\n\n\n{block}"));
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![user(&prose)],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(text_of(&req.messages[0]), prose, "user prose untouched");
        assert_eq!(removed, 0, "no savings claimed from prose");
    }

    #[test]
    fn compacts_large_system_block() {
        // System blocks are in-scope (non-prose scaffolding).
        let block = "boilerplate rule line 1\nboilerplate rule line 2\nboilerplate rule 3";
        let body = format!("{block}\n\nunique middle\n\n{block}");
        let padded = large(&body);
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![sys(&padded)],
            ..Default::default()
        };
        let removed = run(&mut req);
        let got = text_of(&req.messages[0]);
        assert_eq!(got.matches("boilerplate rule line 1").count(), 1);
        assert!(removed > 0);
    }

    #[test]
    fn clean_large_block_is_a_noop_with_zero_savings() {
        // A large block with no trailing ws, no repeated separators, no dup
        // blocks → strict-shrink guard keeps it byte-identical, books zero.
        let body: String = (0..300)
            .map(|n| format!("distinct line {n} carrying real content\n"))
            .collect();
        assert!(body.len() >= MIN_DOC_BYTES);
        let before = body.clone();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&body, "c1")],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(
            text_of(&req.messages[0]),
            before,
            "clean large block is byte-identical"
        );
        assert_eq!(removed, 0);
    }

    #[test]
    fn compacts_large_text_part_in_tool_block() {
        let block = "chunk row alpha\nchunk row beta\nchunk row gamma";
        let body = format!("{block}\n\nunique\n\n{block}");
        let padded = large(&body);
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::Tool {
                content: MessageContent::Parts(vec![ContentPart::Text { text: padded }]),
                tool_call_id: "c1".into(),
            }],
            ..Default::default()
        };
        let removed = run(&mut req);
        let Message::Tool { content, .. } = &req.messages[0] else {
            panic!();
        };
        let MessageContent::Parts(parts) = content else {
            panic!();
        };
        let ContentPart::Text { text } = &parts[0] else {
            panic!();
        };
        assert_eq!(text.matches("chunk row beta").count(), 1);
        assert!(removed > 0);
    }
}
