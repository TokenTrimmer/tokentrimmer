//! Redaction pass — a conservative, **safety** transform that strips PII and
//! secrets from the OUTBOUND request before it is dispatched upstream.
//!
//! This is a guardrail, not a savings feature. It runs **only** when a matched
//! route opted in (`RouteAction::redact`) and is **off by default**. When it
//! fires it replaces each matched span with the fixed placeholder
//! `[REDACTED]` so the secret/PII never reaches the upstream provider.
//!
//! # Honest framing
//!
//! Redaction is a SAFETY transform, NOT a savings feature. Any token delta this
//! pass reports is internal accounting only — there is no user-facing "savings"
//! claim for redaction, and the gateway never brands a redaction token delta as
//! a saving. The primary signal that redaction occurred is the
//! `x-tokentrimmer-warnings` header (e.g. `redacted:system`, `redacted:body`,
//! `redacted:tool_result`). This pass redacts the *upstream request* — it does
//! NOT redact the gateway's own logs.
//!
//! # What it touches
//!
//! - **User messages** (`role: "user"`) — the field class `body`. User prose is
//!   the most common PII vector (emails, SSNs pasted into a prompt).
//! - **System messages** (`role: "system"`) — the field class `system`. System
//!   blocks are assembled by tooling and CAN leak agent-config secrets (an API
//!   key baked into an AGENTS.md-style instruction block).
//! - **Tool messages** (`role: "tool"`) — the field class `tool_result`. Tool
//!   output / RAG documents routinely echo secrets and PII scraped from
//!   upstream systems.
//!
//! # What it NEVER touches
//!
//! - **Assistant content / `tool_calls`** — the model's own output in the
//!   transcript. Redacting model output is out of scope (this is a *request*
//!   guardrail, not response redaction).
//! - **Image / audio parts** — bytes are never inspected.
//! - **`tools` definitions, every non-message field** — untouched.
//!
//! # Conservatism
//!
//! The matcher set is deliberately broad: we prefer over-redacting to leaking.
//! The secret patterns are ported verbatim from the Tier-1 inspect rule
//! `config-agents-md-contains-secrets`, plus an email matcher and a US-SSN
//! matcher. Each pattern replaces its full match with `[REDACTED]`. When a block
//! contains no match it is left **byte-identical**.

use std::sync::OnceLock;

use regex::Regex;

use tt_shared::messages::{Message, MessageContent};
use tt_shared::ChatCompletionRequest;

use super::{PassOutcome, RequestPass};

/// The placeholder substituted for every matched secret / PII span. Fixed so the
/// redacted value is never derivable from the output and so the redaction is
/// visually obvious to anyone reading the dispatched request.
pub const REDACTION_PLACEHOLDER: &str = "[REDACTED]";

/// Secret / PII pattern descriptors: (human label, regex pattern).
///
/// The first block is ported verbatim from the Tier-1 inspect rule
/// `config-agents-md-contains-secrets` (`SECRET_PATTERNS`). The trailing two
/// (email, US SSN) are added for the request-guardrail use case. Be
/// conservative: prefer over-redacting to leaking.
static SECRET_PATTERNS: &[(&str, &str)] = &[
    ("Anthropic API key", r"sk-ant-[A-Za-z0-9_-]{32,}"),
    // OpenAI scoped keys (modern): sk-proj-…, sk-svcacct-…, sk-admin-…
    (
        "OpenAI API key (scoped)",
        r"sk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{20,}",
    ),
    // Legacy bare OpenAI key: sk- + 32+ base62 chars.
    ("OpenAI API key (legacy)", r"sk-[A-Za-z0-9]{32,}"),
    ("Stripe live secret key", r"sk_live_[A-Za-z0-9]{20,}"),
    ("AWS Access Key ID", r"AKIA[0-9A-Z]{16}"),
    ("Google API key", r"AIza[0-9A-Za-z_-]{35}"),
    ("GitHub personal access token", r"ghp_[A-Za-z0-9]{36,}"),
    (
        "Generic API key / secret / token",
        r#"(?i)(api[_\-]?key|secret|token)["'\s]*[:=]\s*["'][A-Za-z0-9!@#$%^&*_\-]{20,}["']"#,
    ),
    // --- PII matchers (request-guardrail additions) ---
    // Email address. Conservative RFC-ish local-part; matches the overwhelming
    // majority of real addresses without trying to be exhaustive.
    (
        "Email address",
        r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
    ),
    // US Social Security Number — NNN-NN-NNNN (with optional spaces as the
    // separator). The word boundaries keep it from matching inside a longer
    // digit run (e.g. a 16-digit card number).
    ("US SSN", r"\b\d{3}[-\s]\d{2}[-\s]\d{4}\b"),
];

/// Compiled redaction regexes, initialised once.
static COMPILED: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();

fn compiled_patterns() -> &'static Vec<(&'static str, Regex)> {
    COMPILED.get_or_init(|| {
        SECRET_PATTERNS
            .iter()
            .map(|(label, pattern)| {
                (
                    *label,
                    Regex::new(pattern).expect("invalid redaction regex"),
                )
            })
            .collect()
    })
}

/// The field class a redaction fired in — used to name the
/// `x-tokentrimmer-warnings` entry (`redacted:<class>`). The secret VALUE is
/// never carried; only the class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RedactedField {
    /// A `role: "system"` block.
    System,
    /// A `role: "user"` block (the request body / prose).
    Body,
    /// A `role: "tool"` block (a tool-result / RAG payload).
    ToolResult,
}

impl RedactedField {
    /// The stable token used in the warnings header (`redacted:<class>`).
    #[must_use]
    pub fn warning_token(self) -> &'static str {
        match self {
            RedactedField::System => "redacted:system",
            RedactedField::Body => "redacted:body",
            RedactedField::ToolResult => "redacted:tool_result",
        }
    }
}

/// The conservative request-redaction guardrail pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct RedactionPass;

impl RedactionPass {
    /// Construct the pass.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Redact `req` in place, returning the SORTED, DEDUPLICATED set of field
    /// classes in which at least one redaction fired. An empty result means the
    /// request was left byte-for-byte unchanged.
    ///
    /// This is the entry point the gateway uses to build the warnings-header
    /// entries; the [`RequestPass`] impl wraps it for the pipeline.
    #[must_use]
    pub fn redact(&self, req: &mut ChatCompletionRequest) -> Vec<RedactedField> {
        let mut fired: Vec<RedactedField> = Vec::new();
        for m in &mut req.messages {
            match m {
                Message::System { content } => {
                    if redact_content(content) {
                        fired.push(RedactedField::System);
                    }
                }
                Message::User { content, .. } => {
                    if redact_content(content) {
                        fired.push(RedactedField::Body);
                    }
                }
                Message::Tool { content, .. } => {
                    if redact_content(content) {
                        fired.push(RedactedField::ToolResult);
                    }
                }
                // The assistant transcript (model output / tool_calls) is the
                // model's own text — out of scope for a REQUEST guardrail.
                Message::Assistant { .. } => {}
            }
        }
        fired.sort_unstable();
        fired.dedup();
        fired
    }
}

impl RequestPass for RedactionPass {
    fn name(&self) -> &'static str {
        "redaction-1"
    }

    fn apply(&self, req: &mut ChatCompletionRequest, provider_id: &str) -> PassOutcome {
        // Measure the redactable text before and after so the pipeline can sum a
        // token delta for internal accounting. NOTE: redaction is a SAFETY
        // transform — this delta is NOT a saving and must never be branded as
        // one. Replacing a long secret with the short placeholder can shrink the
        // prompt, but that is incidental.
        let before = redactable_text(req);
        let fired = self.redact(req);
        if fired.is_empty() {
            return PassOutcome::NONE;
        }
        let after = redactable_text(req);
        let before_tokens = tt_tokenize::estimate_tokens(provider_id, &before);
        let after_tokens = tt_tokenize::estimate_tokens(provider_id, &after);
        PassOutcome {
            tokens_removed: before_tokens.saturating_sub(after_tokens),
        }
    }
}

/// Concatenate the text of every block the pass is allowed to scan — used only
/// to measure the (internal-accounting) token delta. Mirrors exactly which
/// blocks [`RedactionPass::redact`] touches: System + User + Tool message text.
fn redactable_text(req: &ChatCompletionRequest) -> String {
    let mut out = String::new();
    for m in &req.messages {
        match m {
            Message::System { content }
            | Message::User { content, .. }
            | Message::Tool { content, .. } => {
                push_content_text(&mut out, content);
            }
            Message::Assistant { .. } => {}
        }
    }
    out
}

fn push_content_text(out: &mut String, content: &MessageContent) {
    match content {
        MessageContent::Text(s) => {
            out.push_str(s);
            out.push('\n');
        }
        MessageContent::Parts(parts) => {
            for p in parts {
                if let tt_shared::messages::ContentPart::Text { text } = p {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }
    }
}

/// Redact a [`MessageContent`] in place. Returns `true` when at least one
/// pattern fired (the content was changed); `false` leaves it byte-identical.
fn redact_content(content: &mut MessageContent) -> bool {
    let mut changed = false;
    match content {
        MessageContent::Text(s) => {
            if let Some(redacted) = redact_str(s) {
                *s = redacted;
                changed = true;
            }
        }
        MessageContent::Parts(parts) => {
            for p in parts {
                if let tt_shared::messages::ContentPart::Text { text } = p {
                    if let Some(redacted) = redact_str(text) {
                        *text = redacted;
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

/// Replace every matched secret / PII span in `s` with [`REDACTION_PLACEHOLDER`].
///
/// Returns `Some(new)` when at least one pattern matched; `None` when the input
/// is left byte-identical (no match) so the caller can avoid a needless clone.
///
/// Patterns are applied in order; each pass over `s` substitutes all
/// non-overlapping matches of one pattern. Applying the patterns sequentially is
/// deliberately conservative: a later pattern can still match text the earlier
/// ones did not, and the fixed placeholder contains no characters any matcher
/// fires on, so re-scanning it is a no-op.
fn redact_str(s: &str) -> Option<String> {
    let mut current = std::borrow::Cow::Borrowed(s);
    let mut any = false;
    for (_label, re) in compiled_patterns() {
        if re.is_match(&current) {
            any = true;
            current = std::borrow::Cow::Owned(
                re.replace_all(&current, REDACTION_PLACEHOLDER).into_owned(),
            );
        }
    }
    if any {
        Some(current.into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::{ContentPart, ToolCall, ToolCallFunction};

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
    fn req(messages: Vec<Message>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-4o".into(),
            messages,
            ..Default::default()
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

    // --- per-matcher isolation ---

    #[test]
    fn email_matcher_redacts_only_the_email() {
        let out = redact_str("Contact me at jane.doe+test@example.co.uk for details").unwrap();
        assert_eq!(out, "Contact me at [REDACTED] for details");
    }

    #[test]
    fn ssn_matcher_redacts_dashed_and_spaced() {
        assert_eq!(
            redact_str("SSN 123-45-6789 on file").unwrap(),
            "SSN [REDACTED] on file"
        );
        assert_eq!(redact_str("ssn 123 45 6789").unwrap(), "ssn [REDACTED]");
    }

    #[test]
    fn ssn_matcher_does_not_fire_inside_a_long_digit_run() {
        // A 16-digit card-like run must not be sliced into a phantom SSN.
        let raw = "card 1234567890123456 end";
        assert!(
            redact_str(raw).is_none(),
            "no SSN inside a contiguous 16-digit run"
        );
    }

    #[test]
    fn anthropic_api_key_matcher_isolates() {
        let key = format!("sk-ant-{}", "a".repeat(40));
        let out = redact_str(&format!("key={key} done")).unwrap();
        assert_eq!(out, "key=[REDACTED] done");
    }

    #[test]
    fn openai_scoped_key_matcher_isolates() {
        let out = redact_str("OPENAI_API_KEY=sk-proj-abcdefghijklmno0123456789_-XY tail").unwrap();
        assert_eq!(out, "OPENAI_API_KEY=[REDACTED] tail");
    }

    #[test]
    fn aws_access_key_matcher_isolates() {
        let out = redact_str("AKIAIOSFODNN7EXAMPLE is the id").unwrap();
        assert_eq!(out, "[REDACTED] is the id");
    }

    #[test]
    fn github_pat_matcher_isolates() {
        let key = format!("ghp_{}", "a".repeat(36));
        let out = redact_str(&format!("token {key}")).unwrap();
        assert_eq!(out, "token [REDACTED]");
    }

    // --- non-matching text is byte-identical ---

    #[test]
    fn clean_text_is_left_byte_identical() {
        // None of the matchers fire: redact_str returns None (no clone).
        assert!(redact_str("just a normal sentence with no secrets").is_none());
        assert!(redact_str("the number is 42 and the year 2026").is_none());
        assert!(redact_str("").is_none());
    }

    // --- multiple patterns each fire ---

    #[test]
    fn multiple_distinct_patterns_each_fire_in_one_block() {
        let raw =
            "email a@b.com ssn 111-22-3333 key sk-ant-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let out = redact_str(raw).unwrap();
        // Each distinct secret/PII span is replaced; no plaintext survives.
        assert!(!out.contains("a@b.com"));
        assert!(!out.contains("111-22-3333"));
        assert!(!out.contains("sk-ant-"));
        assert_eq!(out.matches("[REDACTED]").count(), 3);
    }

    #[test]
    fn multiple_occurrences_of_same_pattern_all_redacted() {
        let out = redact_str("a@x.com and b@y.org").unwrap();
        assert_eq!(out, "[REDACTED] and [REDACTED]");
    }

    // --- field-class attribution via redact() ---

    #[test]
    fn redact_reports_each_field_class_that_fired() {
        let mut r = req(vec![
            sys("config token sk-ant-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            user("my email is me@example.com"),
            tool("scraped 123-45-6789 from upstream", "c1"),
        ]);
        let fired = RedactionPass::new().redact(&mut r);
        assert_eq!(
            fired,
            vec![
                RedactedField::System,
                RedactedField::Body,
                RedactedField::ToolResult
            ]
        );
        // Plaintext is gone from every block.
        assert!(text_of(&r.messages[0]).contains("[REDACTED]"));
        assert_eq!(text_of(&r.messages[1]), "my email is [REDACTED]");
        assert_eq!(text_of(&r.messages[2]), "scraped [REDACTED] from upstream");
    }

    #[test]
    fn redact_dedups_repeated_field_class() {
        // Two user messages both with PII collapse to a single `Body` entry.
        let mut r = req(vec![user("a@b.com"), user("c@d.com")]);
        let fired = RedactionPass::new().redact(&mut r);
        assert_eq!(fired, vec![RedactedField::Body]);
    }

    #[test]
    fn clean_request_is_a_noop() {
        let mut r = req(vec![
            sys("you are helpful"),
            user("what is the capital of france"),
            tool("paris", "c1"),
        ]);
        let before = serde_json::to_string(&r).unwrap();
        let fired = RedactionPass::new().redact(&mut r);
        assert!(fired.is_empty());
        assert_eq!(serde_json::to_string(&r).unwrap(), before);
    }

    #[test]
    fn assistant_output_is_never_redacted() {
        // The model's own transcript text (and tool_calls) is out of scope.
        let mut r = req(vec![Message::Assistant {
            content: Some(MessageContent::Text(
                "here is the key sk-ant-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            )),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "f".into(),
                    arguments: "{\"email\":\"x@y.com\"}".into(),
                },
            }],
            name: None,
        }]);
        let before = serde_json::to_string(&r).unwrap();
        let fired = RedactionPass::new().redact(&mut r);
        assert!(
            fired.is_empty(),
            "assistant content is request-out-of-scope"
        );
        assert_eq!(serde_json::to_string(&r).unwrap(), before);
    }

    #[test]
    fn redacts_text_parts_in_user_block() {
        let mut r = req(vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::Text {
                text: "ping me at p@q.com".into(),
            }]),
            name: None,
        }]);
        let fired = RedactionPass::new().redact(&mut r);
        assert_eq!(fired, vec![RedactedField::Body]);
        let Message::User { content, .. } = &r.messages[0] else {
            panic!();
        };
        let MessageContent::Parts(parts) = content else {
            panic!();
        };
        let ContentPart::Text { text } = &parts[0] else {
            panic!();
        };
        assert_eq!(text, "ping me at [REDACTED]");
    }

    // --- RequestPass impl / pipeline behavior ---

    #[test]
    fn apply_is_noop_outcome_on_clean_request() {
        let mut r = req(vec![user("nothing sensitive here")]);
        let before = serde_json::to_string(&r).unwrap();
        let outcome = RedactionPass::new().apply(&mut r, "openai");
        assert!(outcome.is_noop());
        assert_eq!(serde_json::to_string(&r).unwrap(), before);
    }

    #[test]
    fn apply_redacts_and_returns_an_outcome_when_fired() {
        // A long secret replaced by the short placeholder shrinks the block, so
        // the (internal-accounting) token delta is reported. This is NOT a
        // user-facing saving — see the module docs.
        let key = "a".repeat(60);
        let mut r = req(vec![user(&format!("key sk-ant-{key}"))]);
        let outcome = RedactionPass::new().apply(&mut r, "openai");
        // It fired (content changed); the token delta is accounting-only.
        let Message::User { content, .. } = &r.messages[0] else {
            panic!();
        };
        let MessageContent::Text(s) = content else {
            panic!();
        };
        assert_eq!(s, "key [REDACTED]");
        // The placeholder is shorter than the secret, so tokens_removed >= 0;
        // we only assert it does not underflow / panic.
        let _ = outcome.tokens_removed;
    }

    #[test]
    fn warning_tokens_are_stable() {
        assert_eq!(RedactedField::System.warning_token(), "redacted:system");
        assert_eq!(RedactedField::Body.warning_token(), "redacted:body");
        assert_eq!(
            RedactedField::ToolResult.warning_token(),
            "redacted:tool_result"
        );
    }
}
