//! `tt chat` — interactive chat REPL routed through the TokenTrimmer gateway,
//! surfacing per-turn cost + savings from the gateway's streaming usage event.

use anyhow::Context as _;
use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::json;

use tt_shared::messages::{Message, MessageContent};

use crate::context::ResolvedContext;
use crate::ui;

pub mod session;

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

/// In-memory conversation state (also the on-disk session format).
#[derive(serde::Serialize, serde::Deserialize)]
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

/// Drain complete SSE frames (separated by a blank line) from the byte buffer,
/// each decoded as a trimmed `String`. Incomplete trailing bytes stay in `buf`,
/// so a multi-byte UTF-8 char (or a frame) split across network chunks is never
/// decoded mid-sequence.
fn drain_frames(buf: &mut Vec<u8>) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(idx) = buf.windows(2).position(|w| w == b"\n\n") {
        let frame: Vec<u8> = buf.drain(..idx + 2).collect();
        out.push(String::from_utf8_lossy(&frame).trim_end().to_string());
    }
    out
}

/// Stream one turn. Prints the assistant text live and the cost footer, and
/// returns the full reply for history. Returns `Err` (turn failed) on a non-2xx
/// gateway response so the caller can drop the unanswered user message.
async fn stream_turn(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    conv: &Conversation,
) -> anyhow::Result<String> {
    let body = json!({
        "model": conv.model,
        "messages": conv.wire_messages(),
        "stream": true,
    });
    let resp = http
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .context("request to gateway failed")?;

    let served_model = resp
        .headers()
        .get("x-tokentrimmer-model-used")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&conv.model)
        .to_string();

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("gateway returned {status}: {}", text.trim());
    }

    let mut stream = resp.bytes_stream();
    let mut spinner = Some(ui::spinner("…"));
    let mut buf: Vec<u8> = Vec::new();
    let mut reply = String::new();
    let mut usage: Option<UsageInfo> = None;

    'outer: while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.context("stream error")?);
        for frame in drain_frames(&mut buf) {
            match parse_sse_frame(&frame) {
                StreamEvent::Delta(t) => {
                    spinner.take(); // clear the spinner on the first token
                    print!("{t}");
                    use std::io::Write as _;
                    let _ = std::io::stdout().flush();
                    reply.push_str(&t);
                }
                StreamEvent::Usage(u) => usage = Some(u),
                StreamEvent::Done => break 'outer,
                StreamEvent::Ignore => {}
            }
        }
    }
    drop(spinner);
    println!();
    if let Some(u) = usage {
        println!(
            "{}",
            format_turn_footer(
                &served_model,
                u.input_tokens,
                u.output_tokens,
                u.cost_usd,
                u.saved_usd,
                u.baseline_cost_usd
            )
        );
    }
    Ok(reply)
}

fn print_help() {
    ui::heading("commands");
    for (c, d) in [
        ("/help", "show this help"),
        ("/clear", "reset the conversation"),
        ("/model [m]", "show or switch the requested model"),
        ("/system [s]", "show or set the system prompt"),
        ("/exit", "quit (or Ctrl-D)"),
    ] {
        println!(
            "  {}  {}",
            ui::accent().apply_to(c),
            ui::muted().apply_to(d)
        );
    }
}

/// Entry point for `tt chat`.
pub async fn run(
    model: Option<String>,
    system: Option<String>,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login --token <KEY>` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();

    let mut conv = Conversation::new(
        model.unwrap_or_else(|| DEFAULT_CHAT_MODEL.to_string()),
        system,
    );
    ui::heading(&format!(
        "tt chat · {} via TokenTrimmer   (/help)",
        conv.model
    ));

    let mut rl = rustyline::DefaultEditor::new().context("init readline")?;
    let prompt = ui::accent().apply_to("› ").to_string();
    loop {
        match rl.readline(&prompt) {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                match Command::parse(&line) {
                    Command::Chat(t) if t.is_empty() => {}
                    Command::Chat(t) => {
                        conv.push_user(t);
                        match stream_turn(&http, &base, &key, &conv).await {
                            Ok(reply) => conv.push_assistant(reply),
                            Err(e) => {
                                ui::error(&format!("{e:#}"));
                                conv.messages.pop(); // drop the unanswered user turn
                            }
                        }
                    }
                    Command::Help => print_help(),
                    Command::Clear => {
                        conv.clear();
                        ui::info("(conversation cleared)");
                    }
                    Command::Model(Some(m)) => {
                        conv.model = m;
                        ui::info(&format!("model → {}", conv.model));
                    }
                    Command::Model(None) => ui::info(&format!("model: {}", conv.model)),
                    Command::System(Some(s)) => {
                        conv.system = Some(s);
                        ui::info("(system prompt set)");
                    }
                    Command::System(None) => match &conv.system {
                        Some(s) => ui::info(&format!("system: {s}")),
                        None => ui::info("(no system prompt)"),
                    },
                    Command::Exit => break,
                    Command::Unknown(c) => {
                        ui::warn(&format!("unknown command /{c} — /help for commands"))
                    }
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => continue, // Ctrl-C: cancel line
            Err(rustyline::error::ReadlineError::Eof) => break,            // Ctrl-D: quit
            Err(e) => {
                ui::error(&format!("input error: {e}"));
                break;
            }
        }
    }
    Ok(())
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
        assert!(
            matches!(Command::parse("/model gpt-4o"), Command::Model(Some(m)) if m == "gpt-4o")
        );
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
    fn drain_frames_handles_chunk_split_multibyte() {
        // "café" (é = 2 bytes) + frame terminator, split across two chunks so
        // the second chunk starts mid-é and mid-terminator.
        let full = "data: {\"choices\":[{\"delta\":{\"content\":\"café\"}}]}\n\n".as_bytes();
        let (a, b) = full.split_at(full.len() - 3);
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(a);
        assert!(drain_frames(&mut buf).is_empty(), "no complete frame yet");
        buf.extend_from_slice(b);
        let frames = drain_frames(&mut buf);
        assert_eq!(frames.len(), 1);
        assert!(matches!(parse_sse_frame(&frames[0]), StreamEvent::Delta(t) if t == "café"));
        assert!(buf.is_empty());
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
