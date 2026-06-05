//! `tt chat` — interactive chat REPL routed through the TokenTrimmer gateway,
//! surfacing per-turn cost + savings from the gateway's streaming usage event.

use serde::Deserialize;

use tt_shared::messages::{Message, MessageContent};

use crate::ui;

const DEFAULT_CHAT_MODEL: &str = "gpt-4o-mini";

/// Cost/usage payload from the gateway's terminal `tokentrimmer.usage` SSE event.
#[derive(Debug, Clone, Deserialize)]
pub struct UsageInfo {
    pub cost_usd: f64,
    pub baseline_cost_usd: f64,
    pub saved_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
}

/// One parsed SSE frame from the gateway stream.
#[derive(Debug)]
pub enum StreamEvent {
    Delta(String),
    Usage(UsageInfo),
    Done,
    Ignore,
}

/// Parse a single SSE frame (the text between `\n\n` separators).
#[must_use]
pub fn parse_sse_frame(frame: &str) -> StreamEvent {
    let mut event_name: Option<&str> = None;
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event_name = Some(v.trim());
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    let data = data.trim();
    if data.is_empty() {
        return StreamEvent::Ignore;
    }
    if data == "[DONE]" {
        return StreamEvent::Done;
    }
    if event_name == Some("tokentrimmer.usage") {
        return serde_json::from_str::<UsageInfo>(data)
            .map(StreamEvent::Usage)
            .unwrap_or(StreamEvent::Ignore);
    }
    let v: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return StreamEvent::Ignore,
    };
    match v["choices"][0]["delta"]["content"].as_str() {
        Some(c) if !c.is_empty() => StreamEvent::Delta(c.to_string()),
        _ => StreamEvent::Ignore,
    }
}

/// A REPL line, parsed.
#[derive(Debug)]
pub enum Command {
    Help,
    Clear,
    Exit,
    Model(Option<String>),
    System(Option<String>),
    Unknown(String),
    Chat(String),
}

impl Command {
    #[must_use]
    pub fn parse(line: &str) -> Command {
        let t = line.trim();
        let Some(rest) = t.strip_prefix('/') else {
            return Command::Chat(t.to_string());
        };
        let mut it = rest.splitn(2, char::is_whitespace);
        let cmd = it.next().unwrap_or("");
        let arg = it
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        match cmd {
            "help" | "h" | "?" => Command::Help,
            "clear" => Command::Clear,
            "exit" | "quit" | "q" => Command::Exit,
            "model" => Command::Model(arg),
            "system" => Command::System(arg),
            other => Command::Unknown(other.to_string()),
        }
    }
}

/// Muted per-turn footer. `saved …%` only when there is a positive saving.
#[must_use]
pub fn format_turn_footer(
    model: &str,
    in_tok: u64,
    out_tok: u64,
    cost_usd: f64,
    saved_usd: f64,
    baseline_usd: f64,
) -> String {
    let mut s = format!(
        "{} {} · {} tok · ${:.4}",
        ui::BULLET,
        model,
        in_tok + out_tok,
        cost_usd
    );
    if baseline_usd > 0.0 && saved_usd > 0.0 {
        let pct = (saved_usd / baseline_usd * 100.0).round();
        s.push_str(&format!(" · saved {pct:.0}%"));
    }
    ui::muted().apply_to(s).to_string()
}

/// In-memory conversation state.
pub struct Conversation {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
}

impl Conversation {
    #[must_use]
    pub fn new(model: String, system: Option<String>) -> Self {
        Self {
            model,
            system,
            messages: Vec::new(),
        }
    }
    pub fn push_user(&mut self, text: String) {
        self.messages.push(Message::User {
            content: MessageContent::Text(text),
            name: None,
        });
    }
    pub fn push_assistant(&mut self, text: String) {
        self.messages.push(Message::Assistant {
            content: Some(MessageContent::Text(text)),
            tool_calls: Vec::new(),
            name: None,
        });
    }
    pub fn clear(&mut self) {
        self.messages.clear();
    }
    /// The full message list to send: system message (if any) prepended.
    #[must_use]
    pub fn wire_messages(&self) -> Vec<Message> {
        let mut v = Vec::new();
        if let Some(s) = &self.system {
            v.push(Message::System {
                content: MessageContent::Text(s.clone()),
            });
        }
        v.extend(self.messages.iter().cloned());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_delta() {
        let f = r#"data: {"choices":[{"index":0,"delta":{"content":"Hi"}}]}"#;
        assert!(matches!(parse_sse_frame(f), StreamEvent::Delta(t) if t == "Hi"));
    }

    #[test]
    fn parse_usage_event() {
        let f = "event: tokentrimmer.usage\ndata: {\"cost_usd\":0.0001,\"baseline_cost_usd\":0.0004,\"saved_usd\":0.0003,\"input_tokens\":10,\"output_tokens\":20,\"cached_tokens\":0}";
        match parse_sse_frame(f) {
            StreamEvent::Usage(u) => {
                assert_eq!(u.input_tokens, 10);
                assert!((u.saved_usd - 0.0003).abs() < 1e-9);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_done_and_ignore() {
        assert!(matches!(parse_sse_frame("data: [DONE]"), StreamEvent::Done));
        assert!(matches!(
            parse_sse_frame(r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#),
            StreamEvent::Ignore
        ));
        assert!(matches!(parse_sse_frame(""), StreamEvent::Ignore));
    }

    #[test]
    fn command_parse() {
        assert!(matches!(Command::parse("/help"), Command::Help));
        assert!(matches!(Command::parse("/clear"), Command::Clear));
        assert!(matches!(Command::parse("/exit"), Command::Exit));
        assert!(matches!(Command::parse("/model gpt-4o"), Command::Model(Some(m)) if m == "gpt-4o"));
        assert!(matches!(Command::parse("/model"), Command::Model(None)));
        assert!(matches!(Command::parse("/nope"), Command::Unknown(c) if c == "nope"));
        assert!(matches!(Command::parse("hello there"), Command::Chat(t) if t == "hello there"));
    }

    #[test]
    fn footer_formats_with_savings() {
        console::set_colors_enabled(false);
        let s = format_turn_footer("gpt-4o-mini", 10, 20, 0.0001, 0.0003, 0.0004);
        assert_eq!(s, "· gpt-4o-mini · 30 tok · $0.0001 · saved 75%");
        let s2 = format_turn_footer("gpt-4o", 5, 5, 0.001, 0.0, 0.0);
        assert_eq!(s2, "· gpt-4o · 10 tok · $0.0010");
    }

    #[test]
    fn conversation_clear_and_system() {
        let mut c = Conversation::new("m".into(), Some("be brief".into()));
        c.push_user("hi".into());
        c.push_assistant("yo".into());
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.wire_messages().len(), 3); // system prepended
        c.clear();
        assert!(c.messages.is_empty());
    }
}
