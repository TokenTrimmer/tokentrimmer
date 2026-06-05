//! Context-window management for `tt chat`: estimate token usage, warn as the
//! conversation fills a per-model budget, and trim the oldest turns before the
//! limit. The gateway remains the authoritative gate; this is advisory.

use tt_shared::messages::{Message, MessageContent};

use crate::chat::Conversation;
use crate::ui;

/// Fallback budget when the model is unknown.
pub const DEFAULT_CONTEXT_BUDGET: u32 = 128_000;
const WARN_FRAC: f64 = 0.75;
const TRIM_FRAC: f64 = 0.95;
const TRIM_TARGET_FRAC: f64 = 0.70;
/// cl100k is a high-quality general estimator; the chat doesn't reliably know
/// the routed provider, so we estimate with the OpenAI tokenizer for all models.
const ESTIMATE_PROVIDER: &str = "openai";

/// Best-effort context window (input tokens) for a model id, by prefix. These
/// are approximate defaults — overridable via `--max-context` / `/context <n>`;
/// exact live windows are the live-catalog's (V4) job.
#[must_use]
pub fn model_window(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    let s = |p: &str| m.starts_with(p);
    if s("gpt-5") {
        256_000
    } else if s("gpt-4o") || s("gpt-4.1") || s("gpt-4-turbo") {
        128_000
    } else if s("o1") || s("o3") || s("o4") {
        200_000
    } else if s("claude") {
        200_000
    } else if s("gemini") {
        1_000_000
    } else {
        DEFAULT_CONTEXT_BUDGET
    }
}

/// All message text (system prompt + each `Text` content), for estimation.
fn conversation_text(conv: &Conversation) -> String {
    let mut out = String::new();
    if let Some(sys) = &conv.system {
        out.push_str(sys);
        out.push('\n');
    }
    for m in &conv.messages {
        let text = match m {
            Message::System {
                content: MessageContent::Text(t),
            } => Some(t),
            Message::User {
                content: MessageContent::Text(t),
                ..
            } => Some(t),
            Message::Assistant {
                content: Some(MessageContent::Text(t)),
                ..
            } => Some(t),
            Message::Tool {
                content: MessageContent::Text(t),
                ..
            } => Some(t),
            _ => None,
        };
        if let Some(t) = text {
            out.push_str(t);
            out.push('\n');
        }
    }
    out
}

/// Estimated tokens for the whole conversation.
#[must_use]
pub fn estimate_conversation_tokens(conv: &Conversation, provider: &str) -> u32 {
    tt_tokenize::estimate_tokens(provider, &conversation_text(conv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_table_by_prefix() {
        assert_eq!(model_window("gpt-4o-mini"), 128_000);
        assert_eq!(model_window("GPT-4o"), 128_000); // case-insensitive
        assert_eq!(model_window("claude-3-5-sonnet"), 200_000);
        assert_eq!(model_window("o1-preview"), 200_000);
        assert_eq!(model_window("gemini-2.0-pro"), 1_000_000);
        assert_eq!(model_window("some-unknown-model"), DEFAULT_CONTEXT_BUDGET);
    }

    #[test]
    fn estimate_grows_with_messages() {
        let mut c = Conversation::new("gpt-4o-mini".into(), None);
        let empty = estimate_conversation_tokens(&c, ESTIMATE_PROVIDER);
        c.push_user("the quick brown fox jumps over the lazy dog".into());
        let one = estimate_conversation_tokens(&c, ESTIMATE_PROVIDER);
        c.push_assistant("a reply with several more words in it".into());
        let two = estimate_conversation_tokens(&c, ESTIMATE_PROVIDER);
        assert!(one > empty, "{one} > {empty}");
        assert!(two > one, "{two} > {one}");
    }
}
