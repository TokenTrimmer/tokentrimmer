//! The `content_compress` dispatcher + the P1a JSON/CSV/log structural backend.
//!
//! [`ContentCompressPass`] is a [`RequestPass`](crate::passes::RequestPass):
//! enabled ONLY by `RouteAction::content_compress` (off by default), it operates
//! on the [`VolatileTail`] the pipeline hands it (the cache-stable prefix is
//! read-only by type) and touches ONLY non-prose scaffolding — System and Tool
//! message text — never user/assistant prose, mirroring the `compression` /
//! `doc_compaction` passes. For each such text block it classifies the
//! [`ContentKind`](crate::content_compress::ContentKind) and dispatches:
//!
//! - **Json** → whitespace-minify (drop insignificant whitespace OUTSIDE string
//!   literals; validated by a real JSON parse first, key order preserved). A
//!   pretty-printed JSON payload collapses to its compact form with byte-for-byte
//!   identical semantics.
//! - **Csv** → strip trailing whitespace padding on each line (end-of-line
//!   whitespace never carries information in delimited data).
//! - **Log** → strip trailing whitespace, then collapse runs of ≥3 identical
//!   consecutive lines into the line plus a `[... previous line repeated N more
//!   times]` marker. The repeat COUNT is preserved, so the transform is
//!   content-preserving (a model reads the marker as the N copies).
//! - **Code / Prose** → classified but LEFT UNTOUCHED in P1a (their backends land
//!   in P1c / P1b).
//!
//! Every transform commits only on a strict shrink of the block, and the whole
//! pass rides the pipeline's TOKEN-TRUE GATE: a result that tokenizes larger than
//! the input is discarded and the request is dispatched verbatim. The measured
//! reduction is booked into the ISOLATED `content_compress_saved_est_usd`
//! estimate (never the invoice-reconciled headline).

use tt_shared::messages::{ContentPart, Message, MessageContent};

use crate::content_compress::{classify, ContentKind};
use crate::passes::split::{PassContext, StablePrefix, VolatileTail};
use crate::passes::{push_content_text, PassOutcome, RequestPass};

/// A run of at least this many identical consecutive log lines is collapsed to
/// the line plus a repeat-count marker (fewer than this is left in place).
const LOG_MIN_RUN: usize = 3;

/// The content-aware compression stage (Phase 1). Off by default; enabled by
/// `RouteAction::content_compress`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContentCompressPass;

impl ContentCompressPass {
    /// Construct the pass.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl RequestPass for ContentCompressPass {
    fn name(&self) -> &'static str {
        "content-compress"
    }

    fn apply(
        &self,
        _stable: &StablePrefix<'_>,
        tail: &mut VolatileTail<'_>,
        cx: &PassContext<'_>,
    ) -> PassOutcome {
        // Self-report only (drift telemetry): the pipeline's token-true gate
        // measures the whole-tail delta itself and uses THAT for attribution.
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

/// The text of every block the pass is *allowed* to modify (System + Tool) —
/// used only to self-measure the delta. User/Assistant prose is excluded.
fn touchable_text(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        if let Message::System { content } | Message::Tool { content, .. } = m {
            push_content_text(&mut out, content);
        }
    }
    out
}

/// Classify + structurally compact every LARGE System / Tool text block in place.
fn compact_in_place(tail: &mut VolatileTail<'_>) {
    for m in tail.messages_mut() {
        match m {
            Message::System { content } | Message::Tool { content, .. } => {
                compact_content(content);
            }
            // User + Assistant prose are never touched.
            Message::User { .. } | Message::Assistant { .. } => {}
        }
    }
}

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

/// Classify one text block and apply the matching structural backend. Returns
/// `Some(new)` only on a strict shrink (a real removal); `None` when the block
/// is unclassifiable / a non-P1a kind / nothing safely removable — the caller
/// keeps the original. The pipeline's token-true gate is the final arbiter.
pub(crate) fn compact_block(s: &str) -> Option<String> {
    let result = match classify(s)? {
        ContentKind::Json => minify_json_whitespace(s)?,
        ContentKind::Csv => strip_trailing_line_ws(s),
        ContentKind::Log => collapse_repeated_log_lines(s),
        // Code / Prose backends land in P1c / P1b — untouched here.
        ContentKind::Code | ContentKind::Diff | ContentKind::Prose => return None,
    };
    if result.len() < s.len() {
        Some(result)
    } else {
        None
    }
}

/// The [`ContentKind`] the P1a dispatcher would COMPACT for this block (Json /
/// Csv / Log), or `None` for an unclassifiable / non-compacting kind. Used by
/// the flywheel to label a compressed request.
#[must_use]
pub fn compactable_kind(s: &str) -> Option<ContentKind> {
    match classify(s)? {
        k @ (ContentKind::Json | ContentKind::Csv | ContentKind::Log) => Some(k),
        ContentKind::Code | ContentKind::Diff | ContentKind::Prose => None,
    }
}

/// The dominant compactable kind across a request's System + Tool blocks (the
/// kind of the largest such block the P1a backend would touch) — the label the
/// flywheel records on a compressed request. `None` when no block is compactable.
#[must_use]
pub fn dominant_compactable_kind(msgs: &[Message]) -> Option<ContentKind> {
    let mut best: Option<(usize, ContentKind)> = None;
    let mut consider = |text: &str| {
        if let Some(kind) = compactable_kind(text) {
            if best.is_none_or(|(len, _)| text.len() > len) {
                best = Some((text.len(), kind));
            }
        }
    };
    for m in msgs {
        if let Message::System { content } | Message::Tool { content, .. } = m {
            match content {
                MessageContent::Text(s) => consider(s),
                MessageContent::Parts(parts) => {
                    for p in parts {
                        if let ContentPart::Text { text } = p {
                            consider(text);
                        }
                    }
                }
            }
        }
    }
    best.map(|(_, kind)| kind)
}

/// Drop JSON whitespace OUTSIDE string literals. Returns `None` unless the block
/// parses as valid JSON (so whitespace between tokens is provably insignificant)
/// AND the minified form is strictly shorter. Key/element order is preserved (a
/// char walk, NOT a `serde_json::Value` re-serialization).
fn minify_json_whitespace(s: &str) -> Option<String> {
    // Validate: only a real JSON document has provably-insignificant inter-token
    // whitespace. `IgnoredAny` parses+discards without building a value tree.
    serde_json::from_str::<serde::de::IgnoredAny>(s).ok()?;

    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c.is_ascii_whitespace() {
            // Insignificant inter-token whitespace — drop.
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Strip trailing whitespace on each line (end-of-line padding). Preserves the
/// line count and every non-whitespace character — losslessly.
fn strip_trailing_line_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for line in s.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(line.trim_end());
    }
    out
}

/// Strip trailing whitespace, then collapse runs of ≥ [`LOG_MIN_RUN`] identical
/// consecutive lines into the line plus a `[... previous line repeated N more
/// times]` marker. The repeat count is preserved (content-preserving); shorter
/// runs are left exactly in place.
fn collapse_repeated_log_lines(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').map(str::trim_end).collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0usize;
    while i < lines.len() {
        let cur = lines[i];
        let mut j = i + 1;
        while j < lines.len() && lines[j] == cur {
            j += 1;
        }
        let run = j - i;
        out.push(cur.to_string());
        if run >= LOG_MIN_RUN {
            out.push(format!(
                "[... previous line repeated {} more times]",
                run - 1
            ));
        } else {
            for _ in 1..run {
                out.push(cur.to_string());
            }
        }
        i = j;
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::split::SplitRequest;
    use tt_shared::ChatCompletionRequest;

    /// Run the pass through an ALL-VOLATILE split (no pricing → empty stable
    /// prefix), returning the self-reported token delta.
    fn run(req: &mut ChatCompletionRequest) -> u32 {
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(req, &cx);
        split
            .run_pass(|stable, tail| ContentCompressPass::new().apply(stable, tail, &cx))
            .tokens_removed
    }

    fn tool(text: &str) -> Message {
        Message::Tool {
            content: MessageContent::Text(text.into()),
            tool_call_id: "c1".into(),
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

    #[test]
    fn minifies_pretty_json_losslessly() {
        // A pretty-printed JSON tool result → compact form, same values.
        let mut obj = String::from("{\n");
        for i in 0..40 {
            obj.push_str(&format!("  \"key_{i}\": \"value {i}\",\n"));
        }
        obj.push_str("  \"last\": true\n}");
        let original: serde_json::Value = serde_json::from_str(&obj).unwrap();

        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&obj)],
            ..Default::default()
        };
        let removed = run(&mut req);
        let got = text_of(&req.messages[0]);
        assert!(removed > 0, "minifying pretty JSON removed tokens");
        assert!(
            !got.contains("\n  \""),
            "insignificant whitespace collapsed"
        );
        // Semantics preserved byte-for-byte on parse.
        let after: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(original, after, "JSON values are unchanged");
    }

    #[test]
    fn json_string_internal_whitespace_is_preserved() {
        // Whitespace INSIDE a string value must survive minification.
        let body = format!(
            "{{\n  \"note\": \"keep    these   spaces\",\n{}}}",
            "  \"pad\": 1,\n".repeat(20)
        );
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&body)],
            ..Default::default()
        };
        run(&mut req);
        let got = text_of(&req.messages[0]);
        assert!(
            got.contains("keep    these   spaces"),
            "in-string whitespace preserved: {got}"
        );
    }

    #[test]
    fn collapses_repeated_log_lines_preserving_count() {
        let mut body = String::from("2026-07-03 10:00:00 INFO starting up service now\n");
        for _ in 0..30 {
            body.push_str("2026-07-03 10:00:01 WARN retrying connection to upstream\n");
        }
        body.push_str("2026-07-03 10:00:02 INFO done with the retries finally\n");
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&body)],
            ..Default::default()
        };
        let removed = run(&mut req);
        let got = text_of(&req.messages[0]);
        assert!(removed > 0, "collapsing repeated log lines removed tokens");
        assert_eq!(
            got.matches("WARN retrying connection to upstream").count(),
            1,
            "the repeated line survives exactly once"
        );
        assert!(
            got.contains("repeated 29 more times"),
            "the repeat count is preserved: {got}"
        );
        assert!(got.contains("done with the retries finally"));
    }

    #[test]
    fn trims_csv_trailing_padding() {
        // Messy space/tab trailing padding (the shape real spreadsheet exports
        // carry) — irregular trailing whitespace costs real tokens, so trimming
        // it books an honest positive delta. (Uniform trailing runs tiktoken
        // merges for free → the token-true gate correctly books 0 for those.)
        let mut body = String::from("id,name,value,ts \t \n");
        for i in 0..40 {
            body.push_str(&format!("{i},row{i},{},2026-01-01 \t \t \n", i * 7));
        }
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&body)],
            ..Default::default()
        };
        let removed = run(&mut req);
        let got = text_of(&req.messages[0]);
        assert!(removed > 0, "trimming CSV padding removed tokens");
        assert!(!got.contains(" \t"), "trailing padding removed");
        // Every row's data survives.
        for i in 0..40 {
            assert!(got.contains(&format!("{i},row{i},")));
        }
    }

    #[test]
    fn leaves_code_and_prose_untouched_in_p1a() {
        let code = "fn a() {\n  let x = 1;\n}\n".repeat(30);
        let prose = "The quick brown fox jumps over the lazy dog. ".repeat(40);
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![
                tool(&code),
                Message::System {
                    content: MessageContent::Text(prose.clone()),
                },
            ],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(
            text_of(&req.messages[0]),
            code,
            "code left untouched in P1a"
        );
        assert_eq!(
            text_of(&req.messages[1]),
            prose,
            "prose left untouched in P1a"
        );
        assert_eq!(removed, 0, "no savings from code/prose in P1a");
    }

    #[test]
    fn never_touches_user_prose_json() {
        // Even a JSON-shaped USER block is never modified (prose invariant).
        let body = format!("{{\n{}}}", "  \"k\": 1,\n".repeat(30));
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![user(&body)],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(text_of(&req.messages[0]), body, "user block untouched");
        assert_eq!(removed, 0);
    }

    #[test]
    fn small_or_unclassifiable_block_is_a_noop() {
        let body = "ok"; // below MIN_BLOB_CHARS → classify None → untouched
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(body)],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(text_of(&req.messages[0]), body);
        assert_eq!(removed, 0);
    }

    #[test]
    fn rides_pass_pipeline_token_true_gate() {
        use crate::passes::PassPipeline;
        // A pretty-printed JSON tool block routed through the REAL pipeline: the
        // token-true gate commits the compaction (a genuine shrink) and reports
        // the pipeline-MEASURED delta with no rejection.
        let mut obj = String::from("{\n");
        for i in 0..40 {
            obj.push_str(&format!("  \"key_{i}\": \"value {i}\",\n"));
        }
        obj.push_str("  \"last\": true\n}");
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(&obj)],
            ..Default::default()
        };
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(&mut req, &cx);
        let out = PassPipeline::content_compress().run(&mut split, &cx);
        assert!(
            out.tokens_removed > 0,
            "pipeline committed a measured JSON compaction"
        );
        assert!(
            out.rejected.is_empty(),
            "a genuine shrink is not rejected by the token-true gate"
        );
    }

    #[test]
    fn dominant_kind_labels_largest_compactable_block() {
        let big_json = format!("{{\n{}}}", "  \"k\": 1,\n".repeat(50));
        let small_log = "2026-07-03 10:00:00 INFO x\n".repeat(4);
        let msgs = vec![
            tool(&small_log),
            Message::System {
                content: MessageContent::Text(big_json),
            },
        ];
        assert_eq!(dominant_compactable_kind(&msgs), Some(ContentKind::Json));
    }
}
