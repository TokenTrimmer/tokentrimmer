//! Compression pass #1 — a conservative, content-lossless trim of *non-prose*
//! request blocks.
//!
//! This is the pass that makes the product's name true: it removes input tokens
//! a request never needed, without altering meaning. It runs **only** when a
//! matched route opted in (`RouteAction::compress`) and is **off by default**.
//!
//! # What it touches (non-prose only, VOLATILE TAIL only)
//!
//! The pass operates on the [`VolatileTail`] the pipeline hands it — it cannot
//! reach the cache-stable prefix by type (see the [`crate::passes`] module
//! docs). Within the tail it touches:
//!
//! - **System messages** — instructions/scaffolding assembled by tooling, not
//!   typed prose. Frameworks routinely concatenate templates with stray blank
//!   lines and trailing whitespace. (A cache-qualified system prefix sits in
//!   the STABLE region and is therefore out of reach — trimming it would bust
//!   the provider prompt cache for a fraction of the cache discount.)
//! - **Tool messages** (`role: "tool"`) — tool-result / RAG-document payloads.
//!   Verbose machine output: logs, JSON dumps, scraped documents.
//! - **Assistant `tool_calls` arguments** — a stringified-JSON blob the model
//!   emitted; re-serializing it canonically removes insignificant JSON
//!   whitespace without changing the decoded value.
//!
//! # What it NEVER touches
//!
//! - **The cache-stable prefix** — structurally unreachable (type-level).
//! - **User messages** — the actual instruction / prose. Left byte-identical.
//! - **Assistant text content** — model prose in the transcript. Left
//!   byte-identical.
//! - **Image / audio parts** — bytes are never inspected.
//! - **`tools` definitions, `tool_choice`, every non-message field** —
//!   structurally unreachable through the tail handle.
//!
//! # Why each trim is provably safe
//!
//! Applied to a non-prose text block, each trim preserves the block's semantic
//! content:
//!
//! 1. **Strip trailing whitespace on each line.** Trailing spaces/tabs before a
//!    newline carry no information in tool output, logs, or assembled
//!    instructions. (We do NOT do this to user/assistant prose, where a
//!    Markdown hard-break — two trailing spaces — could be meaningful.)
//! 2. **Collapse 3+ consecutive blank lines to a single blank line.** A run of
//!    empty lines is visual padding; one blank line preserves any intended
//!    paragraph break. We keep one rather than zero so we never *join* two
//!    separated sections.
//! 3. **Strip leading/trailing blank lines of the whole block.** Surrounding
//!    empty lines are pure padding.
//! 4. **Canonicalize `tool_calls` arguments JSON.** When the arguments string
//!    parses as JSON, re-serialize compactly: `{ "a" : 1 }` → `{"a":1}`. The
//!    decoded value is identical, so the tool sees the same arguments.
//!
//! De-duplication of exactly-repeated context/tool-result blocks: when two
//! **adjacent** messages of the same non-prose role carry byte-identical
//! content, the second is dropped — re-sending the identical block immediately
//! adds no information. We require adjacency + identical role so we never change
//! conversational structure (e.g. two different tool results that happen to
//! match are not adjacent-duplicates of the *same* logical block unless they sit
//! back-to-back with the same role).
//!
//! When a trim would not strictly shrink a block, the original is kept — the
//! pass only ever commits a change that removes characters, and only via the
//! transforms above. If at any point we are unsure a trim is lossless, we do
//! nothing. That conservatism — plus being off-by-default — is the safety; a
//! judge gate (Wave-B2) is therefore optional for this pass and would attach in
//! [`crate::passes::PassPipeline::run`] for any future, more aggressive stage.

use tt_shared::messages::{Message, MessageContent};

use super::split::{PassContext, StablePrefix, VolatileTail};
use super::{push_content_text, PassOutcome, RequestPass};

/// The conservative, content-lossless compression stage (compression pass #1).
#[derive(Debug, Clone, Copy, Default)]
pub struct CompressionPass;

impl CompressionPass {
    /// Construct the pass.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl RequestPass for CompressionPass {
    fn name(&self) -> &'static str {
        "compression-1"
    }

    fn apply(
        &self,
        _stable: &StablePrefix<'_>,
        tail: &mut VolatileTail<'_>,
        cx: &PassContext<'_>,
    ) -> PassOutcome {
        // Measure the non-prose text we are allowed to touch BEFORE the trim,
        // then again AFTER, and report the tokenized delta. (Informational:
        // the pipeline's token-true gate measures the whole-tail delta itself
        // and uses that for attribution; this self-report is drift telemetry.)
        let before = touchable_text(tail.messages());
        compress_in_place(tail);
        let after = touchable_text(tail.messages());

        let before_tokens = tt_tokenize::estimate_tokens(cx.provider_id, &before);
        let after_tokens = tt_tokenize::estimate_tokens(cx.provider_id, &after);
        PassOutcome {
            tokens_removed: before_tokens.saturating_sub(after_tokens),
            warnings: Vec::new(),
        }
    }
}

/// Concatenate the text of every block the pass is *allowed* to modify — used
/// to measure the token delta. Mirrors exactly which blocks
/// [`compress_in_place`] rewrites: System + Tool message text, and Assistant
/// `tool_calls` arguments. User/Assistant prose is excluded.
fn touchable_text(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        match m {
            Message::System { content } | Message::Tool { content, .. } => {
                push_content_text(&mut out, content);
            }
            Message::Assistant { tool_calls, .. } => {
                for tc in tool_calls {
                    out.push_str(&tc.function.arguments);
                    out.push('\n');
                }
            }
            // User prose + Assistant prose are never touched.
            Message::User { .. } => {}
        }
    }
    out
}

/// Apply the conservative trims to the volatile tail in place.
fn compress_in_place(tail: &mut VolatileTail<'_>) {
    // 1. Per-message trims on non-prose roles.
    for m in tail.messages_mut() {
        match m {
            Message::System { content } | Message::Tool { content, .. } => {
                compress_content(content);
            }
            Message::Assistant { tool_calls, .. } => {
                for tc in tool_calls {
                    if let Some(canon) = canonicalize_json(&tc.function.arguments) {
                        // Only commit when it strictly shrinks (never grow).
                        if canon.len() < tc.function.arguments.len() {
                            tc.function.arguments = canon;
                        }
                    }
                }
            }
            Message::User { .. } => {}
        }
    }

    // 2. De-duplicate exactly-repeated ADJACENT non-prose blocks (same role +
    //    byte-identical content). Walk back-to-front so index removal is stable.
    //    Tail-relative indices: the stable prefix is not part of this walk, so
    //    a tail block can never be deduplicated against a stable one (which
    //    would mutate nothing but still requires the boundary to be respected).
    let mut i = tail.len();
    while i >= 2 {
        let cur = i - 1;
        let prev = i - 2;
        if adjacent_nonprose_duplicate(&tail.messages()[prev], &tail.messages()[cur]) {
            tail.remove(cur);
        }
        i -= 1;
    }
}

/// Apply the whitespace trims to a [`MessageContent`] in place.
fn compress_content(content: &mut MessageContent) {
    match content {
        MessageContent::Text(s) => {
            if let Some(trimmed) = trim_block(s) {
                *s = trimmed;
            }
        }
        MessageContent::Parts(parts) => {
            for p in parts {
                if let tt_shared::messages::ContentPart::Text { text } = p {
                    if let Some(trimmed) = trim_block(text) {
                        *text = trimmed;
                    }
                }
            }
        }
    }
}

/// Conservative, lossless whitespace trim of a non-prose text block.
///
/// Returns `Some(new)` only when the result is strictly shorter than the input;
/// `None` when nothing could be safely removed (caller keeps the original).
///
/// Transforms (all lossless on non-prose):
/// - strip trailing whitespace on each line,
/// - collapse 3+ consecutive blank lines to one blank line,
/// - strip leading/trailing blank lines of the whole block.
///
/// The final newline style is preserved per line: we rebuild with `\n` joins
/// but only ever DELETE characters relative to the input on a per-line basis,
/// so a block that used `\n` is unchanged in separator style. (Blocks mixing
/// `\r\n` keep their `\r`-stripping handled by trailing-whitespace removal,
/// which is still lossless for machine output.)
fn trim_block(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    // Split on '\n' (keeping the structure); strip trailing whitespace per line.
    let lines: Vec<&str> = s.split('\n').collect();
    let mut trimmed: Vec<String> = lines.iter().map(|l| l.trim_end().to_string()).collect();

    // Collapse runs of 3+ blank lines to a single blank line.
    let mut collapsed: Vec<String> = Vec::with_capacity(trimmed.len());
    let mut blank_run = 0usize;
    for line in trimmed.drain(..) {
        if line.is_empty() {
            blank_run += 1;
            // Keep at most one blank line per run.
            if blank_run <= 1 {
                collapsed.push(line);
            }
        } else {
            blank_run = 0;
            collapsed.push(line);
        }
    }

    // Strip leading/trailing blank lines.
    while collapsed.first().is_some_and(|l| l.is_empty()) {
        collapsed.remove(0);
    }
    while collapsed.last().is_some_and(|l| l.is_empty()) {
        collapsed.pop();
    }

    let result = collapsed.join("\n");
    // Only commit a strict shrink.
    if result.len() < s.len() {
        Some(result)
    } else {
        None
    }
}

/// Canonicalize a stringified-JSON `tool_calls` arguments blob by re-serializing
/// it compactly. Returns `None` when the string is not valid JSON (left as-is —
/// never risk corrupting a non-JSON argument string).
fn canonicalize_json(s: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(s).ok()?;
    serde_json::to_string(&value).ok()
}

/// True when two adjacent messages are the SAME non-prose role with
/// byte-identical content (a re-sent context/tool-result block).
fn adjacent_nonprose_duplicate(a: &Message, b: &Message) -> bool {
    match (a, b) {
        (Message::System { content: ca }, Message::System { content: cb }) => content_eq(ca, cb),
        (
            Message::Tool {
                content: ca,
                tool_call_id: ia,
            },
            Message::Tool {
                content: cb,
                tool_call_id: ib,
            },
        ) => {
            // Same tool_call_id AND identical content: the identical result was
            // appended twice for the same call. Distinct ids are distinct
            // results even when their text coincides — never merge those.
            ia == ib && content_eq(ca, cb)
        }
        _ => false,
    }
}

fn content_eq(a: &MessageContent, b: &MessageContent) -> bool {
    match (a, b) {
        (MessageContent::Text(sa), MessageContent::Text(sb)) => sa == sb,
        _ => {
            // For Parts (or mismatched shapes) compare the serialized form —
            // conservative: only equal when structurally identical.
            serde_json::to_string(a).ok() == serde_json::to_string(b).ok()
                && serde_json::to_string(a).is_ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::split::SplitRequest;
    use crate::passes::PassPipeline;
    use chrono::Utc;
    use tt_shared::messages::{ContentPart, ToolCall, ToolCallFunction};
    use tt_shared::pricing::ModelPricing;
    use tt_shared::ChatCompletionRequest;

    /// Apply the pass through an ALL-VOLATILE split (no pricing → no cache
    /// minimum → empty stable prefix), so every assertion below exercises the
    /// exact pre-split behavior.
    fn run(req: &mut ChatCompletionRequest) -> u32 {
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(req, &cx);
        split
            .run_pass(|stable, tail| CompressionPass::new().apply(stable, tail, &cx))
            .tokens_removed
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
    fn tool(text: &str, id: &str) -> Message {
        Message::Tool {
            content: MessageContent::Text(text.into()),
            tool_call_id: id.into(),
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
            Message::Assistant { content, .. } => match content {
                Some(MessageContent::Text(s)) => s.clone(),
                _ => String::new(),
            },
        }
    }

    #[test]
    fn collapses_blank_line_runs_in_tool_block() {
        // Pure blank-line collapse changes the CONTENT (fewer bytes) but is a
        // lossless structural trim. We assert the content change here; the
        // token-count delta is asserted separately on a trailing-whitespace case
        // (tiktoken encodes a run of newlines as the same token count whether
        // there are 5 or 2, so blank-line collapse alone yields 0 token delta —
        // and the pass honestly reports that, never overclaiming).
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool("row1\n\n\n\n\nrow2", "c1")],
            ..Default::default()
        };
        run(&mut req);
        assert_eq!(text_of(&req.messages[0]), "row1\n\nrow2");
    }

    #[test]
    fn trailing_whitespace_trim_reduces_estimated_tokens() {
        // Trailing whitespace genuinely costs tokens under tiktoken, so stripping
        // it yields a real, honest token delta.
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool("aaaa bbbb cccc   \n\n\n\n\ndddd eeee", "c1")],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(text_of(&req.messages[0]), "aaaa bbbb cccc\n\ndddd eeee");
        assert!(removed > 0, "trailing-ws trim should remove tokens");
    }

    #[test]
    fn strips_trailing_whitespace_in_system_block() {
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![sys("line one    \nline two\t\t\n")],
            ..Default::default()
        };
        run(&mut req);
        assert_eq!(text_of(&req.messages[0]), "line one\nline two");
    }

    #[test]
    fn never_modifies_user_prose() {
        // User prose with the SAME redundant whitespace a tool block would get
        // trimmed for — must be left byte-identical.
        let prose = "Hello   \n\n\n\nworld.   ";
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![user(prose)],
            ..Default::default()
        };
        let removed = run(&mut req);
        assert_eq!(text_of(&req.messages[0]), prose, "user prose untouched");
        assert_eq!(removed, 0, "no savings claimed from prose");
    }

    #[test]
    fn dedupes_adjacent_identical_tool_results_same_call() {
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![
                user("q"),
                tool("RESULT-BLOCK", "call_x"),
                tool("RESULT-BLOCK", "call_x"),
            ],
            ..Default::default()
        };
        let removed = run(&mut req);
        // The second identical tool block for the same call is dropped.
        assert_eq!(req.messages.len(), 2);
        assert_eq!(text_of(&req.messages[1]), "RESULT-BLOCK");
        assert!(removed > 0);
    }

    #[test]
    fn keeps_distinct_tool_calls_even_if_text_matches() {
        // Same text but DIFFERENT tool_call_id → distinct results, never merged.
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool("SAME", "call_a"), tool("SAME", "call_b")],
            ..Default::default()
        };
        run(&mut req);
        assert_eq!(req.messages.len(), 2, "distinct ids are not duplicates");
    }

    #[test]
    fn canonicalizes_tool_call_arguments_json_losslessly() {
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: "f".into(),
                        arguments: "{  \"a\" :  1,  \"b\"  : \"x\"  }".into(),
                    },
                }],
                name: None,
            }],
            ..Default::default()
        };
        let removed = run(&mut req);
        let Message::Assistant { tool_calls, .. } = &req.messages[0] else {
            panic!("expected assistant");
        };
        let args = &tool_calls[0].function.arguments;
        // Decoded value is identical (lossless), but the string is compact.
        let orig: serde_json::Value =
            serde_json::from_str("{  \"a\" :  1,  \"b\"  : \"x\"  }").unwrap();
        let now: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(orig, now, "arguments value preserved exactly");
        assert!(args.len() < "{  \"a\" :  1,  \"b\"  : \"x\"  }".len());
        assert!(removed > 0);
    }

    #[test]
    fn non_json_tool_arguments_left_untouched() {
        let raw = "not json   with   spaces";
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::Assistant {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: "f".into(),
                        arguments: raw.into(),
                    },
                }],
                name: None,
            }],
            ..Default::default()
        };
        run(&mut req);
        let Message::Assistant { tool_calls, .. } = &req.messages[0] else {
            panic!();
        };
        assert_eq!(tool_calls[0].function.arguments, raw);
    }

    #[test]
    fn clean_request_is_a_noop_with_zero_savings() {
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![sys("clean system"), user("clean user"), tool("clean", "c1")],
            ..Default::default()
        };
        let before = serde_json::to_string(&req).unwrap();
        let removed = run(&mut req);
        assert_eq!(removed, 0);
        assert_eq!(serde_json::to_string(&req).unwrap(), before);
    }

    #[test]
    fn preserves_meaningful_content_modulo_safe_trims() {
        // A tool block with a code-like body: internal indentation is preserved
        // (we only strip trailing whitespace + collapse blank-line runs).
        let body = "def f(x):\n    return x + 1   \n\n\n\nprint('ok')";
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![tool(body, "c1")],
            ..Default::default()
        };
        run(&mut req);
        // Leading indentation preserved; only trailing ws + blank run trimmed.
        assert_eq!(
            text_of(&req.messages[0]),
            "def f(x):\n    return x + 1\n\nprint('ok')"
        );
    }

    /// THE type-invariant behavioral pin: on a cache-qualified multi-turn
    /// request the entire message list is stable, so the pipeline leaves every
    /// message byte-identical — dirty system whitespace, settled tool blocks,
    /// duplicates and all — and books exactly zero. (Before the cache-span
    /// invariant, the pass would have trimmed the system/tool whitespace here
    /// and booked a "saving" that busts the provider prompt cache.)
    #[test]
    fn compression_cannot_touch_stable_prefix() {
        let pricing = ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
            cached_input_per_million: Some(1.0),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: Some(8),
            effective_at: Utc::now(),
        };
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&pricing),
        };
        // Multi-turn (assistant present) + comfortably over the 8-token min →
        // MultiTurnHistory: everything is stable.
        let mut req = ChatCompletionRequest {
            model: "gpt-x".into(),
            messages: vec![
                sys("You are concise.   \n\n\n\n\nFollow the rules.   "),
                user("Please summarize."),
                Message::Assistant {
                    content: Some(MessageContent::Text("Working on it.".into())),
                    tool_calls: vec![],
                    name: None,
                },
                tool("RESULT   \n\n\n\n\nDATA", "call_x"),
                tool("RESULT   \n\n\n\n\nDATA", "call_x"),
            ],
            ..Default::default()
        };
        let before = serde_json::to_string(&req).unwrap();

        let out = {
            let mut split = SplitRequest::compute(&mut req, &cx);
            PassPipeline::conservative_compression().run(&mut split, &cx)
        };

        assert_eq!(out.tokens_removed, 0, "stable prefix books zero");
        assert!(
            out.rejected.is_empty(),
            "a structural no-op is not a rejection"
        );
        assert_eq!(
            serde_json::to_string(&req).unwrap(),
            before,
            "every message must be byte-identical on the cache-qualified path"
        );
    }

    #[test]
    fn trims_text_parts_in_non_prose_block() {
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages: vec![Message::Tool {
                content: MessageContent::Parts(vec![ContentPart::Text {
                    text: "a   \n\n\n\nb".into(),
                }]),
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
        assert_eq!(text, "a\n\nb");
        assert!(removed > 0);
    }
}
