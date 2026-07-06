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
//! - **Prose** → EXTRACTIVE compaction (P1b, LOSSY) via [`prose`](super::prose),
//!   committed ONLY when the [`ContentCompressPass`]'s judge gate trusts the
//!   [`PROSE_CLASS`](super::prose::PROSE_CLASS) — fail-open to verbatim
//!   otherwise. The structural backends above are content-preserving and need no
//!   gate; prose drops information, so it inherits the summarize lever's gate.
//! - **Code** → AST-aware compaction (P1c, LOSSY) via [`code`](super::code):
//!   truncate long function/method bodies while KEEPING imports/signatures/type
//!   declarations, then RE-PARSE-VERIFY ("never serve broken code"). Committed
//!   ONLY when the judge gate trusts the [`CODE_CLASS`](super::code::CODE_CLASS) —
//!   fail-open to verbatim otherwise, exactly like Prose.
//!
//! Every transform commits only on a strict shrink of the block, and the whole
//! pass rides the pipeline's TOKEN-TRUE GATE: a result that tokenizes larger than
//! the input is discarded and the request is dispatched verbatim. The measured
//! reduction is booked into the ISOLATED `content_compress_saved_est_usd`
//! estimate (never the invoice-reconciled headline).

use std::sync::Arc;

use tt_shared::messages::{ContentPart, Message, MessageContent};

use crate::content_compress::{classify, code, prose, ContentKind};
use crate::passes::agentic_budget::summarize_judge::SummaryGate;
use crate::passes::split::{PassContext, StablePrefix, VolatileTail};
use crate::passes::{push_content_text, PassOutcome, RequestPass};

/// A run of at least this many identical consecutive log lines is collapsed to
/// the line plus a repeat-count marker (fewer than this is left in place).
const LOG_MIN_RUN: usize = 3;

/// The per-request join keys threaded to the P1d raw-capture sink. Carries the
/// `org_id` (the ZDR opt-in is instance-level today), the `trace_id` (the join
/// key to the eventual response-side paired quality verdict, Phase 2), and the
/// `model`/`provider_id` (tokenization context the export CLI recomputes per
/// block). Built once in `chat.rs prepare()` from the `RequestContext` + the
/// `PassContext`; the pass clones the `Arc` into each compacted block's record.
///
/// `None` = capture OFF for this request (the default — metrics-only). The pass
/// is a no-op for capture when `None`, so threading is observability-only and
/// never changes the bytes dispatched.
#[derive(Clone)]
pub struct CaptureCtx {
    pub org_id: String,
    pub trace_id: String,
    pub model: String,
    pub provider_id: String,
}

/// The content-aware compression stage (Phase 1). Off by default; enabled by
/// `RouteAction::content_compress`.
///
/// The optional `prose_gate` / `code_gate` enable the LOSSY backends: `None`
/// leaves that kind untouched; `Some(gate)` compresses a block of that kind ONLY
/// when the gate trusts its class (`is_committable(PROSE_CLASS)` /
/// `is_committable(CODE_CLASS)`), fail-open to verbatim otherwise. Both gates are
/// the shared summarize-lever
/// [`SummaryGate`](crate::passes::agentic_budget::summarize_judge::SummaryGate)
/// — the synchronous projection of the ~2% paired judge + the 0.90-floor ratchet
/// — and in production are the SAME gate object (distinct class keys keep the
/// prose and code levers independent).
///
/// The optional `capture` ([`CaptureCtx`]) enables the P1d raw before/after
/// capture: when `Some` AND the instance opted in via `TT_COMPRESS_CAPTURE` +
/// `TT_COMPRESS_CAPTURE_PATH`, each compacted block appends a JSONL record. `None`
/// (default) = capture OFF, zero overhead. Capture is observability — it never
/// changes the bytes the pass dispatches.
#[derive(Clone, Default)]
pub struct ContentCompressPass {
    prose_gate: Option<Arc<dyn SummaryGate>>,
    code_gate: Option<Arc<dyn SummaryGate>>,
    capture: Option<Arc<CaptureCtx>>,
}

impl std::fmt::Debug for ContentCompressPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentCompressPass")
            .field("prose_gate", &self.prose_gate.as_ref().map(|_| "<gate>"))
            .field("code_gate", &self.code_gate.as_ref().map(|_| "<gate>"))
            .field("capture", &self.capture.as_ref().map(|_| "<ctx>"))
            .finish()
    }
}

impl ContentCompressPass {
    /// Construct the pass with BOTH LOSSY backends DISABLED (structural
    /// JSON/CSV/log backends only — the conservative posture; Prose + Code left
    /// verbatim) AND raw capture DISABLED.
    #[must_use]
    pub fn new() -> Self {
        Self {
            prose_gate: None,
            code_gate: None,
            capture: None,
        }
    }

    /// Construct the pass with the P1b prose backend enabled behind `gate` (the
    /// shared summarize-lever gate); the Code backend stays disabled. A Prose
    /// block is compressed only when the gate trusts
    /// [`PROSE_CLASS`](super::prose::PROSE_CLASS); an untrusted / closed /
    /// unavailable gate fails OPEN to the verbatim bytes.
    #[must_use]
    pub fn with_prose_gate(gate: Arc<dyn SummaryGate>) -> Self {
        Self {
            prose_gate: Some(gate),
            code_gate: None,
            capture: None,
        }
    }

    /// Construct the pass with BOTH the P1b prose and the P1c code LOSSY backends
    /// enabled behind the SAME shared summarize-lever `gate`. Prose commits only
    /// when the gate trusts [`PROSE_CLASS`](super::prose::PROSE_CLASS); Code only
    /// when it trusts [`CODE_CLASS`](super::code::CODE_CLASS) — the two classes
    /// (and their 0.90-floor ratchets) are independent, and either failing/closed
    /// fails OPEN to the verbatim bytes for that kind.
    #[must_use]
    pub fn with_gates(gate: Arc<dyn SummaryGate>) -> Self {
        Self {
            prose_gate: Some(gate.clone()),
            code_gate: Some(gate),
            capture: None,
        }
    }

    /// Construct the pass with BOTH lossy backends behind `gate` AND the P1d raw
    /// before/after capture enabled via `capture`. The capture records ONLY when
    /// the instance opted in (`TT_COMPRESS_CAPTURE` + `TT_COMPRESS_CAPTURE_PATH`);
    /// otherwise the [`CaptureCtx`] is carried but [`capture::record_pair`] is a
    /// no-op (zero overhead). Distinct from [`with_gates`](Self::with_gates) so
    /// existing callers are unchanged.
    #[must_use]
    pub fn with_gates_and_capture(gate: Arc<dyn SummaryGate>, capture: Arc<CaptureCtx>) -> Self {
        Self {
            prose_gate: Some(gate.clone()),
            code_gate: Some(gate),
            capture: Some(capture),
        }
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
        compact_in_place(
            tail,
            self.prose_gate.as_deref(),
            self.code_gate.as_deref(),
            // `as_deref` on `Option<Arc<CaptureCtx>>` → `Option<&CaptureCtx>`
            // (Arc derefs to its target); None = capture OFF for this request.
            self.capture.as_deref(),
        );
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

/// Classify + compact every LARGE System / Tool text block in place. `prose_gate`
/// / `code_gate` are threaded to the LOSSY prose (P1b) / code (P1c) backends;
/// `None` disables that kind. `capture` (the P1d [`CaptureCtx`]) is threaded to
/// the per-block capture point; `None` = capture OFF.
fn compact_in_place(
    tail: &mut VolatileTail<'_>,
    prose_gate: Option<&dyn SummaryGate>,
    code_gate: Option<&dyn SummaryGate>,
    capture: Option<&CaptureCtx>,
) {
    for m in tail.messages_mut() {
        match m {
            Message::System { content } | Message::Tool { content, .. } => {
                compact_content(content, prose_gate, code_gate, capture);
            }
            // User + Assistant prose are never touched.
            Message::User { .. } | Message::Assistant { .. } => {}
        }
    }
}

fn compact_content(
    content: &mut MessageContent,
    prose_gate: Option<&dyn SummaryGate>,
    code_gate: Option<&dyn SummaryGate>,
    capture: Option<&CaptureCtx>,
) {
    match content {
        MessageContent::Text(s) => {
            if let Some(compacted) = compact_block(s, prose_gate, code_gate, capture) {
                *s = compacted;
            }
        }
        MessageContent::Parts(parts) => {
            for p in parts {
                if let ContentPart::Text { text } = p {
                    if let Some(compacted) = compact_block(text, prose_gate, code_gate, capture) {
                        *text = compacted;
                    }
                }
            }
        }
    }
}

/// Classify one text block and apply the matching backend. Returns `Some(new)`
/// only on a strict shrink (a real removal); `None` when the block is
/// unclassifiable / a non-backed kind / nothing safely removable — the caller
/// keeps the original. The pipeline's token-true gate is the final arbiter.
///
/// The structural (Json/Csv/Log) backends are content-preserving and commit on
/// the shrink alone. The Prose (P1b) and Code (P1c) backends are LOSSY: each
/// compresses ONLY when its gate is present AND trusts its class
/// (`PROSE_CLASS` / `CODE_CLASS`) — an absent / untrusted / closed gate fails OPEN
/// to the verbatim block (no compression). The Code backend additionally
/// re-parse-verifies its own output (never serve broken code).
///
/// P1d: when `capture` is `Some`, a [`CaptureRecord`] for the compacted pair
/// (`before` = `s`, `after` = `result`, `gate_committed` = true — the block
/// compacted) is appended to the configured sink. [`capture::record_pair`] is a
/// no-op when the instance did not opt in, so the capture is observability-only
/// and never changes the bytes the pass dispatches.
pub(crate) fn compact_block(
    s: &str,
    prose_gate: Option<&dyn SummaryGate>,
    code_gate: Option<&dyn SummaryGate>,
    capture: Option<&CaptureCtx>,
) -> Option<String> {
    let kind = classify(s)?;
    let result = match kind {
        ContentKind::Json => minify_json_whitespace(s)?,
        ContentKind::Csv => strip_trailing_line_ws(s),
        ContentKind::Log => collapse_repeated_log_lines(s),
        ContentKind::Prose => {
            // LOSSY → gated by the shared judge projection. Fail open to verbatim
            // when there is no gate, or the judge does not (yet) trust the class.
            let gate = prose_gate?;
            if !gate.is_committable(prose::PROSE_CLASS) {
                return None;
            }
            prose::compress(s)?.0
        }
        ContentKind::Code => {
            // LOSSY (truncates bodies) → gated by the shared judge projection on
            // the `"code"` class, mirroring Prose. Fail open to verbatim when
            // there is no gate / the class is untrusted. `code::compress` itself
            // re-parse-verifies (returns None → verbatim on any broken transform).
            let gate = code_gate?;
            if !gate.is_committable(code::CODE_CLASS) {
                return None;
            }
            code::compress(s)?.0
        }
        // Diff has no backend in Phase 1 — untouched.
        ContentKind::Diff => return None,
    };
    if result.len() < s.len() {
        // P1d: capture the before/after pair when the route opted into raw
        // capture. record_pair is a no-op unless the instance set
        // TT_COMPRESS_CAPTURE + TT_COMPRESS_CAPTURE_PATH, and a write error is
        // swallowed (capture must never break a request). tokens_removed is 0
        // here; the export CLI recomputes the per-block delta offline.
        if let Some(ctx) = capture {
            let rec = crate::content_compress::CaptureRecord::new(
                kind.as_str(),
                s,
                &result,
                0,
                &ctx.org_id,
                &ctx.trace_id,
                &ctx.model,
                &ctx.provider_id,
            );
            crate::content_compress::capture::record_pair(&rec);
        }
        Some(result)
    } else {
        None
    }
}

/// The [`ContentKind`] the dispatcher has a backend for on this block (Json /
/// Csv / Log structural, Prose extractive, or Code AST), or `None` for an
/// unclassifiable / non-backed kind (Diff). Used by the flywheel to label a
/// compressed request. Gate-independent — it names the kind the dispatcher WOULD
/// compact (Prose / Code only actually compact behind an open judge gate).
#[must_use]
pub fn compactable_kind(s: &str) -> Option<ContentKind> {
    match classify(s)? {
        k @ (ContentKind::Json
        | ContentKind::Csv
        | ContentKind::Log
        | ContentKind::Prose
        | ContentKind::Code) => Some(k),
        ContentKind::Diff => None,
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

    // ── P1b: the LOSSY prose backend, gated by the shared summarize judge ──────

    use crate::passes::agentic_budget::summarize_judge::{
        AlwaysCommitGate, NeverCommitGate, RatchetConfig, RatchetSummaryGate, SummaryGate,
    };
    use std::sync::Arc;

    /// Run the pass with the P1b prose backend enabled behind `gate`, returning
    /// the self-reported token delta (the pipeline's token-true gate measures the
    /// authoritative delta; here we self-report to assert commit vs no-op).
    fn run_gated(req: &mut ChatCompletionRequest, gate: Arc<dyn SummaryGate>) -> u32 {
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(req, &cx);
        split
            .run_pass(|stable, tail| {
                ContentCompressPass::with_prose_gate(gate.clone()).apply(stable, tail, &cx)
            })
            .tokens_removed
    }

    fn system(text: &str) -> Message {
        Message::System {
            content: MessageContent::Text(text.into()),
        }
    }

    /// A large block of DISTINCT prose sentences (no digits / paths / identifiers,
    /// so the must-keep override does not protect everything) — over the prose
    /// backend's size threshold, with recurring topical vocabulary for salience.
    fn prose_blob() -> String {
        [
            "The gateway routes each request to the cheapest capable model.",
            "Routing decisions weigh model capability against the expected cost.",
            "A cheaper model often answers a simple question just as well.",
            "Quality regressions would erode user trust in the routing layer.",
            "The judge samples a small fraction of traffic to catch regressions.",
            "When quality drops the lever pauses itself automatically.",
            "Caching avoids paying twice for an identical repeated request.",
            "Semantic caching matches requests that mean the same thing.",
            "Compression trims redundant scaffolding before dispatch.",
            "Prose blocks were historically left almost entirely untouched.",
            "Extractive selection keeps the most salient sentences of a block.",
            "The remaining sentences are dropped to reach a target ratio.",
            "Every lossy transform stays behind a strict quality gate.",
            "The gate fails open to the verbatim text whenever unsure.",
        ]
        .join(" ")
    }

    #[test]
    fn prose_backend_no_gate_is_verbatim() {
        // `ContentCompressPass::new()` (no prose gate) leaves Prose untouched —
        // the conservative posture (fail-open). This is the default the pipeline
        // builder `content_compress()` uses.
        let blob = prose_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(text_of(&req.messages[0]), blob, "no gate → prose verbatim");
        assert_eq!(removed, 0, "no gate → no prose saving");
    }

    #[test]
    fn prose_backend_open_gate_compresses_and_books_saving() {
        // A judge-trusted `"prose"` class → the extractive backend commits: the
        // block shrinks and books a positive token delta (which the cost path
        // values into the ISOLATED `content_compress_saved_est_usd`).
        let blob = prose_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed = run_gated(&mut req, Arc::new(AlwaysCommitGate));
        assert!(removed > 0, "an open gate compresses prose → saving booked");
        let got = text_of(&req.messages[0]);
        assert!(
            got.len() < blob.len(),
            "the prose block is strictly shorter"
        );
    }

    #[test]
    fn prose_backend_closed_gate_is_verbatim_fail_open() {
        // A closed / unavailable gate → fail OPEN to the verbatim bytes, $0.
        let blob = prose_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed = run_gated(&mut req, Arc::new(NeverCommitGate));
        assert_eq!(
            text_of(&req.messages[0]),
            blob,
            "closed gate → prose verbatim (fail-open)"
        );
        assert_eq!(removed, 0, "closed gate → no prose saving");
    }

    #[test]
    fn prose_backend_rides_090_floor_ratchet() {
        // MIRRORS the summarize lever: the same `RatchetSummaryGate` 0.90 floor
        // (the `route_autopause` floor). A judge history AT/ABOVE 0.90 keeps the
        // `"prose"` class open (compress); a sustained sub-0.90 run ratchets it
        // SHUT (verbatim) — sub-0.90 recall auto-pauses the lever per class.
        let blob = prose_blob();

        // (a) ≥ 0.90 recall history → open → compresses.
        let good = Arc::new(RatchetSummaryGate::new(
            ["prose".to_string()].into_iter().collect(),
            RatchetConfig::default(),
        ));
        for _ in 0..10 {
            good.record_summary_verdict("prose", tt_plan_core::JudgeVerdict::Acceptable);
        }
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        assert!(
            run_gated(&mut req, good) > 0,
            "a >=0.90 judge history keeps prose compressing"
        );

        // (b) sustained sub-0.90 recall → the floor ratchets the class SHUT.
        let bad = Arc::new(RatchetSummaryGate::new(
            ["prose".to_string()].into_iter().collect(),
            RatchetConfig::default(),
        ));
        for _ in 0..5 {
            bad.record_summary_verdict("prose", tt_plan_core::JudgeVerdict::Degraded);
        }
        assert!(
            !bad.is_committable("prose"),
            "a sub-0.90 run must auto-pause the prose lever (0.90 floor)"
        );
        let mut req2 = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed = run_gated(&mut req2, bad);
        assert_eq!(
            text_of(&req2.messages[0]),
            blob,
            "an auto-paused prose class serves verbatim"
        );
        assert_eq!(removed, 0, "auto-paused → no saving");
    }

    #[test]
    fn prose_backend_small_block_untouched_even_with_open_gate() {
        // A prose block below the backend's size threshold is left verbatim even
        // behind an OPEN gate (the extractive backend's own small-block guard).
        let small = "This is short. Only a couple sentences here. Nothing to drop.";
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(small)],
            ..Default::default()
        };
        let removed = run_gated(&mut req, Arc::new(AlwaysCommitGate));
        assert_eq!(text_of(&req.messages[0]), small);
        assert_eq!(removed, 0);
    }

    #[test]
    fn prose_backend_never_touches_user_prose() {
        // Even with an open gate, USER prose is never compressed (prose invariant:
        // the pass only touches System + Tool scaffolding).
        let blob = prose_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![user(&blob)],
            ..Default::default()
        };
        let removed = run_gated(&mut req, Arc::new(AlwaysCommitGate));
        assert_eq!(text_of(&req.messages[0]), blob, "user prose untouched");
        assert_eq!(removed, 0);
    }

    // ── P1c: the LOSSY AST code backend, gated by the shared summarize judge ───

    /// Run the pass with BOTH the prose and code LOSSY backends enabled behind
    /// `gate` (production wires a single shared gate for both), returning the
    /// self-reported token delta.
    fn run_gated_both(req: &mut ChatCompletionRequest, gate: Arc<dyn SummaryGate>) -> u32 {
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(req, &cx);
        split
            .run_pass(|stable, tail| {
                ContentCompressPass::with_gates(gate.clone()).apply(stable, tail, &cx)
            })
            .tokens_removed
    }

    /// A large, clean TypeScript source block: an import + an interface + a class
    /// with a long method + a top-level function with a long body. TypeScript uses
    /// braces, so the live classifier reliably tags it `Code` (unlike brace-free
    /// Python, which the density heuristic tags Prose — see the classifier's own
    /// suite), and the inspect harness parses it. Over `CODE_MIN_CHARS`; the long
    /// bodies exceed the truncation threshold.
    fn code_blob() -> String {
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
    }
}

export function boot(opts: ClientOptions): number {
    const client = new Client();
    const ok = client.connect(opts);
    if (!ok) {
        console.log("connection failed after retries");
        return 1;
    }
    console.log("connection established");
    return 0;
}
"#
        .to_string()
    }

    #[test]
    fn code_blob_classifies_as_code() {
        // Guard the routing assumption: the dispatcher only sends this to the code
        // backend if the live classifier tags it `Code`.
        assert_eq!(classify(&code_blob()), Some(ContentKind::Code));
    }

    #[test]
    fn code_backend_no_gate_is_verbatim() {
        // `ContentCompressPass::new()` (no code gate) leaves Code untouched — the
        // conservative default-off posture (fail-open).
        let blob = code_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(text_of(&req.messages[0]), blob, "no gate → code verbatim");
        assert_eq!(removed, 0, "no gate → no code saving");
    }

    #[test]
    fn code_backend_open_gate_compresses_and_books_saving() {
        // A judge-trusted `"code"` class → the AST backend commits: the block
        // shrinks (bodies truncated) yet keeps imports + signatures, and books a
        // positive token delta (which the cost path values into the ISOLATED
        // `content_compress_saved_est_usd`).
        let blob = code_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed = run_gated_both(&mut req, Arc::new(AlwaysCommitGate));
        assert!(removed > 0, "an open gate compresses code → saving booked");
        let got = text_of(&req.messages[0]);
        assert!(got.len() < blob.len(), "the code block is strictly shorter");
        // Imports, the interface, and signatures survive; a truncated body's
        // contents are gone.
        assert!(got.contains("import { Socket }"), "imports survive: {got}");
        assert!(
            got.contains("interface ClientOptions"),
            "type decls survive: {got}"
        );
        assert!(
            got.contains("connect(opts: ClientOptions): boolean"),
            "signatures survive: {got}"
        );
        assert!(
            got.contains("lines elided"),
            "a truncation marker is present: {got}"
        );
        assert!(
            !got.contains("connection failed after retries"),
            "a truncated body's contents are dropped: {got}"
        );
    }

    #[test]
    fn code_backend_closed_gate_is_verbatim_fail_open() {
        // A closed / unavailable gate → fail OPEN to the verbatim bytes, $0.
        let blob = code_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed = run_gated_both(&mut req, Arc::new(NeverCommitGate));
        assert_eq!(
            text_of(&req.messages[0]),
            blob,
            "closed gate → code verbatim (fail-open)"
        );
        assert_eq!(removed, 0, "closed gate → no code saving");
    }

    #[test]
    fn code_backend_rides_090_floor_ratchet() {
        // MIRRORS the prose lever: the same `RatchetSummaryGate` 0.90 floor, keyed
        // by the `"code"` class. A judge history AT/ABOVE 0.90 keeps the class open
        // (compress); a sustained sub-0.90 run ratchets it SHUT (verbatim).
        let blob = code_blob();

        // (a) >= 0.90 recall history → open → compresses.
        let good = Arc::new(RatchetSummaryGate::new(
            ["code".to_string()].into_iter().collect(),
            RatchetConfig::default(),
        ));
        for _ in 0..10 {
            good.record_summary_verdict("code", tt_plan_core::JudgeVerdict::Acceptable);
        }
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        assert!(
            run_gated_both(&mut req, good) > 0,
            "a >=0.90 judge history keeps code compressing"
        );

        // (b) sustained sub-0.90 recall → the floor ratchets the class SHUT.
        let bad = Arc::new(RatchetSummaryGate::new(
            ["code".to_string()].into_iter().collect(),
            RatchetConfig::default(),
        ));
        for _ in 0..5 {
            bad.record_summary_verdict("code", tt_plan_core::JudgeVerdict::Degraded);
        }
        assert!(
            !bad.is_committable("code"),
            "a sub-0.90 run must auto-pause the code lever (0.90 floor)"
        );
        let mut req2 = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed = run_gated_both(&mut req2, bad);
        assert_eq!(
            text_of(&req2.messages[0]),
            blob,
            "an auto-paused code class serves verbatim"
        );
        assert_eq!(removed, 0, "auto-paused → no saving");
    }

    #[test]
    fn code_backend_rides_pass_pipeline_token_true_gate() {
        use crate::passes::PassPipeline;
        // A code block routed through the REAL pipeline with an open gate: the
        // token-true gate commits the genuine shrink and reports the
        // pipeline-MEASURED delta with no rejection.
        let blob = code_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(&mut req, &cx);
        let out = PassPipeline::content_compress_with_gates(Arc::new(AlwaysCommitGate))
            .run(&mut split, &cx);
        assert!(
            out.tokens_removed > 0,
            "pipeline committed a measured code compaction"
        );
        assert!(
            out.rejected.is_empty(),
            "a genuine shrink is not rejected by the token-true gate"
        );
    }

    #[test]
    fn code_backend_never_touches_user_code() {
        // Even with an open gate, USER content is never compressed (the pass only
        // touches System + Tool scaffolding).
        let blob = code_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![user(&blob)],
            ..Default::default()
        };
        let removed = run_gated_both(&mut req, Arc::new(AlwaysCommitGate));
        assert_eq!(text_of(&req.messages[0]), blob, "user code untouched");
        assert_eq!(removed, 0);
    }

    #[test]
    fn non_code_json_still_takes_structural_path_with_open_gate() {
        // With BOTH lossy gates OPEN, a JSON block is still routed to the JSON
        // STRUCTURAL backend (content-preserving minify), NOT the code backend —
        // the classifier decides routing, not the gate.
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
        let removed = run_gated_both(&mut req, Arc::new(AlwaysCommitGate));
        assert!(removed > 0, "JSON minified via the structural path");
        let got = text_of(&req.messages[0]);
        let after: serde_json::Value = serde_json::from_str(&got).unwrap();
        assert_eq!(
            original, after,
            "JSON values unchanged (structural, not code)"
        );
    }

    // ── P1d: the CaptureCtx is observability-only (never changes the bytes) ──

    /// A helper `CaptureCtx` for the capture-on tests (the env gate is OFF in
    /// the test env, so `record_pair` is a no-op — the test asserts the ctx is
    /// threaded without changing the dispatched bytes).
    fn capture_ctx() -> Arc<CaptureCtx> {
        Arc::new(CaptureCtx {
            org_id: "00000000-0000-0000-0000-0000000000ff".into(),
            trace_id: "00000000-0000-0000-0000-0000000000ee".into(),
            model: "gpt-4o".into(),
            provider_id: "openai".into(),
        })
    }

    #[test]
    fn capture_ctx_threaded_is_byte_identical_to_without_it() {
        // The SAME code blob, an OPEN gate, with vs without the CaptureCtx: the
        // dispatched bytes + tokens_removed MUST be identical — capture is
        // observability-only (it never changes what the pass dispatches).
        let blob = code_blob();
        let mut req_a = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let mut req_b = req_a.clone();
        let gate = Arc::new(AlwaysCommitGate);

        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        // (a) WITHOUT capture (the P1c posture).
        let removed_a = {
            let mut split = SplitRequest::compute(&mut req_a, &cx);
            split
                .run_pass(|stable, tail| {
                    ContentCompressPass::with_gates(gate.clone()).apply(stable, tail, &cx)
                })
                .tokens_removed
        };
        // (b) WITH capture (the P1d builder). Env gate is OFF in tests, so
        // record_pair is a no-op — the bytes + delta are unchanged.
        let removed_b = {
            let mut split = SplitRequest::compute(&mut req_b, &cx);
            split
                .run_pass(|stable, tail| {
                    ContentCompressPass::with_gates_and_capture(gate.clone(), capture_ctx())
                        .apply(stable, tail, &cx)
                })
                .tokens_removed
        };
        assert_eq!(
            text_of(&req_a.messages[0]),
            text_of(&req_b.messages[0]),
            "capture ctx is observability-only — bytes identical with/without it"
        );
        assert_eq!(
            removed_a, removed_b,
            "the token delta is identical with/without capture"
        );
        assert!(removed_b > 0, "the gate is open → code compressed");
    }

    #[test]
    fn capture_ctx_with_capture_off_appends_no_record() {
        // The default test-env posture is capture OFF (capture_enabled() is
        // OnceLock-cached false, so record_pair returns before touching any
        // path). A route with a CaptureCtx threaded + an OPEN gate still
        // compresses (capture is observability, not a control) but writes NO
        // record_pair — the metrics-only plane carries the signal (ZDR).
        assert!(
            !crate::content_compress::capture::capture_enabled(),
            "capture is OFF in the test env"
        );
        let blob = code_blob();
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![system(&blob)],
            ..Default::default()
        };
        let removed =
            run_gated_both_with_capture(&mut req, Arc::new(AlwaysCommitGate), capture_ctx());
        assert!(
            removed > 0,
            "the gate is open → code still compresses (capture is observability)"
        );
    }

    /// Run the pass with BOTH lossy backends AND the capture ctx, returning the
    /// self-reported token delta. Mirrors [`run_gated_both`] + the P1d capture
    /// threading (the env gate is OFF in tests, so record_pair is a no-op).
    fn run_gated_both_with_capture(
        req: &mut ChatCompletionRequest,
        gate: Arc<dyn SummaryGate>,
        capture: Arc<CaptureCtx>,
    ) -> u32 {
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(req, &cx);
        split
            .run_pass(|stable, tail| {
                ContentCompressPass::with_gates_and_capture(gate.clone(), capture)
                    .apply(stable, tail, &cx)
            })
            .tokens_removed
    }
}
