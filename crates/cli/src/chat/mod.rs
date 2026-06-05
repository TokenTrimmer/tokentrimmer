//! `tt chat` — interactive chat REPL routed through the TokenTrimmer gateway,
//! surfacing per-turn cost + savings from the gateway's streaming usage event.

use anyhow::Context as _;
use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::json;

use tt_shared::messages::{Message, MessageContent};

use crate::context::ResolvedContext;
use crate::ui;

pub mod budget;
pub mod session;
pub mod tools;

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
    Save(Option<String>),
    Resume(String),
    Sessions,
    Cost,
    Editor,
    Retry,
    Copy,
    Tools(Option<bool>),
    Context(Option<u32>),
    Trim,
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
            "save" => Command::Save(arg),
            "resume" | "load" => match arg {
                Some(n) => Command::Resume(n),
                None => Command::Unknown("resume (usage: /resume <name>)".to_string()),
            },
            "sessions" => Command::Sessions,
            "cost" => Command::Cost,
            "editor" | "e" => Command::Editor,
            "retry" | "r" => Command::Retry,
            "copy" | "y" => Command::Copy,
            "tools" => Command::Tools(match arg.as_deref() {
                Some("on") | Some("true") | Some("enable") => Some(true),
                Some("off") | Some("false") | Some("disable") => Some(false),
                _ => None,
            }),
            "context" => Command::Context(arg.as_deref().and_then(|a| a.parse().ok())),
            "trim" => Command::Trim,
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

/// Build the OSC52 terminal escape that copies `text` to the system clipboard.
/// Works locally and over SSH — no platform clipboard dependency. Best-effort:
/// terminals that don't support OSC52 simply ignore it.
#[must_use]
pub fn osc52_copy(text: &str) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{b64}\x07")
}

/// xterm discards OSC52 payloads beyond ~74 994 base64 chars; cap the raw reply
/// conservatively (base64 inflates ~4/3×) so `/copy` never silently truncates.
const OSC52_MAX_BYTES: usize = 55 * 1024;

/// Wrap a bare OSC52 sequence for tmux/screen passthrough so `/copy` works
/// inside a multiplexer. Env is read at the call site so this stays pure.
#[must_use]
fn wrap_osc52_for_mux(seq: String, in_tmux: bool, term: &str) -> String {
    if in_tmux {
        // tmux DCS passthrough: ESC P tmux ; <seq, each ESC doubled> ESC \
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else if term.starts_with("screen") {
        // screen DCS chunk: ESC P <seq> ESC \
        format!("\x1bP{seq}\x1b\\")
    } else {
        seq
    }
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

/// Running cost/savings totals for the current chat session.
#[derive(Default)]
pub struct Ledger {
    pub turns: u32,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub baseline_usd: f64,
}

impl Ledger {
    pub fn add(&mut self, u: &UsageInfo) {
        self.turns += 1;
        self.cost_usd += u.cost_usd;
        self.saved_usd += u.saved_usd;
        self.baseline_usd += u.baseline_cost_usd;
    }
    #[must_use]
    pub fn summary(&self) -> String {
        let pct = if self.baseline_usd > 0.0 {
            (self.saved_usd / self.baseline_usd * 100.0).round()
        } else {
            0.0
        };
        format!(
            "session: {} turn(s) · ${:.4} spent · saved ${:.4} ({pct:.0}%)",
            self.turns, self.cost_usd, self.saved_usd
        )
    }
}

/// What `/retry` should do, computed by [`prepare_retry`].
#[derive(Debug)]
enum RetryPlan {
    /// Re-run the last turn. `restore` holds the assistant reply that was
    /// removed (push it back to undo a failed retry), or `None` when the
    /// trailing turn was an unanswered user message.
    Ready { restore: Option<Message> },
    /// No user turn in history — nothing to retry.
    Nothing,
}

/// Prepare the conversation to re-run the last turn. A trailing assistant reply
/// is popped and returned in `restore` so that a *failed* retry can be made a
/// no-op on history (no dangling user turn — two consecutive user messages
/// break the Anthropic/Gemini APIs — and no lost answer).
fn prepare_retry(conv: &mut Conversation) -> RetryPlan {
    match conv.messages.last() {
        Some(Message::Assistant { .. }) => RetryPlan::Ready {
            restore: conv.messages.pop(),
        },
        Some(Message::User { .. }) => RetryPlan::Ready { restore: None },
        _ => RetryPlan::Nothing,
    }
}

/// The text of the most recent assistant reply, if any.
#[must_use]
fn last_assistant_text(conv: &Conversation) -> Option<String> {
    conv.messages.iter().rev().find_map(|m| match m {
        Message::Assistant {
            content: Some(MessageContent::Text(t)),
            ..
        } => Some(t.clone()),
        _ => None,
    })
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
) -> anyhow::Result<(String, Option<UsageInfo>)> {
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
    if let Some(u) = &usage {
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
    Ok((reply, usage))
}

/// Stream the current conversation: print live, push the assistant reply, and
/// update the ledger. Returns true on success. The caller decides whether to
/// drop the pending user turn on failure.
async fn do_turn(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    conv: &mut Conversation,
    ledger: &mut Ledger,
) -> bool {
    match stream_turn(http, base, key, conv).await {
        Ok((reply, usage)) => {
            conv.push_assistant(reply);
            if let Some(u) = usage {
                ledger.add(&u);
            }
            true
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            false
        }
    }
}

/// Route a turn to the tool-calling loop (tools on) or the streamed path (off).
async fn dispatch_turn(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    conv: &mut Conversation,
    ledger: &mut Ledger,
    reg: &tt_mcp::tools::Registry,
    tools_enabled: bool,
) -> bool {
    if tools_enabled {
        tools::run_tool_turn(http, base, key, conv, reg, ledger).await
    } else {
        do_turn(http, base, key, conv, ledger).await
    }
}

/// Open `$VISUAL`/`$EDITOR` (fallback `vi`) on a temp file and return the
/// composed text, or `None` when left empty / the editor exits non-zero.
fn compose_in_editor() -> anyhow::Result<Option<String>> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    // O_EXCL + 0600 + a randomized name (no symlink/predictable-path attack),
    // auto-removed on drop so every return path cleans up.
    let file = tempfile::Builder::new()
        .prefix("tt-chat-")
        .suffix(".md")
        .tempfile()
        .context("create temp file for editor")?;
    let path = file.path().to_path_buf();
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launch editor `{editor}`"))?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if !status.success() {
        return Ok(None);
    }
    let text = text.trim().to_string();
    Ok(if text.is_empty() { None } else { Some(text) })
}

fn print_help() {
    ui::heading("commands");
    for (c, d) in [
        ("/help", "show this help"),
        ("/clear", "reset the conversation"),
        ("/model [m]", "show or switch the requested model"),
        ("/system [s]", "show or set the system prompt"),
        ("/editor", "compose a multi-line message in $VISUAL/$EDITOR"),
        ("/retry", "re-run the last turn"),
        ("/copy", "copy the last reply to the clipboard"),
        (
            "/tools [on|off]",
            "toggle tool-calling (find_route_for, preview_cost, inspect_diff)",
        ),
        ("/context [n]", "show or set the token budget"),
        ("/trim", "drop oldest turns to fit the budget"),
        ("/save [name]", "save this conversation"),
        ("/resume <name>", "load a saved conversation"),
        ("/sessions", "list saved conversations"),
        ("/cost", "session cost + savings so far"),
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
    resume: Option<String>,
    tools: bool,
    max_context: Option<u32>,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login --token <KEY>` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();

    let mut conv = match resume {
        Some(n) => {
            session::load(&session::sessions_dir(), &n).with_context(|| format!("resume `{n}`"))?
        }
        None => Conversation::new(
            model.unwrap_or_else(|| DEFAULT_CHAT_MODEL.to_string()),
            system,
        ),
    };
    let mut ledger = Ledger::default();
    let registry = tools::build_registry();
    let mut tools_enabled = tools;
    // Best-effort: real per-model windows from the gateway catalog. On any
    // failure (offline / old gateway / pre-auth) fall back to the prefix table.
    let catalog_windows = match crate::catalog::fetch_catalog(&http, &base, &key).await {
        Ok(models) => crate::catalog::windows_map(&models),
        Err(_) => std::collections::HashMap::new(),
    };
    let mut ctx = budget::ContextState::new(max_context, catalog_windows);
    ui::heading(&format!(
        "tt chat · {} via TokenTrimmer{}   (/help)",
        conv.model,
        if tools_enabled { " · tools on" } else { "" }
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
                        let snapshot = conv.messages.clone();
                        conv.push_user(t);
                        ctx.manage(&mut conv);
                        if !dispatch_turn(
                            &http,
                            &base,
                            &key,
                            &mut conv,
                            &mut ledger,
                            &registry,
                            tools_enabled,
                        )
                        .await
                        {
                            // failed turn → no-op on history: drop the user turn
                            // AND undo any trim manage() did before sending.
                            conv.messages = snapshot;
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
                    Command::Save(name) => {
                        let n = name.unwrap_or_else(|| session::auto_name(&conv));
                        match session::save(&session::sessions_dir(), &n, &conv) {
                            Ok(p) => ui::success(&format!("saved session → {}", p.display())),
                            Err(e) => ui::error(&format!("{e:#}")),
                        }
                    }
                    Command::Resume(name) => match session::load(&session::sessions_dir(), &name) {
                        Ok(c) => {
                            conv = c;
                            ui::info(&format!("(resumed · {} messages)", conv.messages.len()));
                        }
                        Err(e) => ui::error(&format!("{e:#}")),
                    },
                    Command::Sessions => {
                        let metas = session::list(&session::sessions_dir()).unwrap_or_default();
                        if metas.is_empty() {
                            ui::info("no saved sessions");
                        } else {
                            let mut t =
                                ui::table(&["NAME", "MODEL", "TURNS"], console::colors_enabled());
                            for m in metas {
                                t.add_row(vec![m.name, m.model, m.turns.to_string()]);
                            }
                            println!("{t}");
                        }
                    }
                    Command::Cost => ui::info(&ledger.summary()),
                    Command::Context(set) => {
                        if let Some(0) = set {
                            ctx.override_budget = None;
                            ui::info("context budget cleared → using the per-model window");
                        } else if let Some(n) = set {
                            ctx.override_budget = Some(n);
                            ui::info(&format!("context budget → {n} tokens"));
                        } else {
                            let budget = ctx.budget(&conv.model);
                            let est = ctx.estimate(&conv);
                            let pct = (f64::from(est) / f64::from(budget) * 100.0) as u32;
                            ui::info(&format!(
                                "context: ~{est} / {budget} tokens ({pct}%) [{}]",
                                conv.model
                            ));
                        }
                    }
                    Command::Trim => {
                        let dropped = budget::manual_trim(&mut conv, &ctx);
                        ui::info(&format!("trimmed {dropped} old message(s)"));
                    }
                    Command::Tools(set) => {
                        tools_enabled = set.unwrap_or(!tools_enabled);
                        if tools_enabled {
                            ui::info("tools: on (find_route_for, preview_cost, inspect_diff)");
                        } else {
                            ui::info("tools: off");
                        }
                    }
                    Command::Editor => match compose_in_editor() {
                        Ok(Some(t)) => {
                            let snapshot = conv.messages.clone();
                            conv.push_user(t);
                            ctx.manage(&mut conv);
                            if !dispatch_turn(
                                &http,
                                &base,
                                &key,
                                &mut conv,
                                &mut ledger,
                                &registry,
                                tools_enabled,
                            )
                            .await
                            {
                                conv.messages = snapshot; // no-op on history
                            }
                        }
                        Ok(None) => ui::info("(editor: nothing sent)"),
                        Err(e) => ui::error(&format!("{e:#}")),
                    },
                    Command::Retry => match prepare_retry(&mut conv) {
                        RetryPlan::Ready { restore } => {
                            if !dispatch_turn(
                                &http,
                                &base,
                                &key,
                                &mut conv,
                                &mut ledger,
                                &registry,
                                tools_enabled,
                            )
                            .await
                            {
                                // Failed retry → no-op on history: restore the
                                // prior reply so we never leave a dangling user
                                // turn or lose a good answer.
                                if let Some(a) = restore {
                                    conv.messages.push(a);
                                }
                            }
                        }
                        RetryPlan::Nothing => ui::warn("nothing to retry"),
                    },
                    Command::Copy => {
                        match last_assistant_text(&conv).filter(|t| !t.trim().is_empty()) {
                            None => ui::warn("nothing to copy"),
                            Some(_) if !console::user_attended() => {
                                ui::warn("/copy needs an interactive terminal")
                            }
                            Some(text) if text.len() > OSC52_MAX_BYTES => ui::warn(&format!(
                                "reply too large to copy ({} KB; OSC52 cap ~{} KB)",
                                text.len() / 1024,
                                OSC52_MAX_BYTES / 1024
                            )),
                            Some(text) => {
                                let seq = wrap_osc52_for_mux(
                                    osc52_copy(&text),
                                    std::env::var_os("TMUX").is_some(),
                                    &std::env::var("TERM").unwrap_or_default(),
                                );
                                print!("{seq}");
                                use std::io::Write as _;
                                let _ = std::io::stdout().flush();
                                ui::info("(sent to clipboard — needs an OSC52-capable terminal)");
                            }
                        }
                    }
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
    fn ledger_accumulates() {
        console::set_colors_enabled(false);
        let u = UsageInfo {
            cost_usd: 0.001,
            baseline_cost_usd: 0.004,
            saved_usd: 0.003,
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
        };
        let mut l = Ledger::default();
        l.add(&u);
        l.add(&u);
        assert_eq!(l.turns, 2);
        let s = l.summary();
        assert!(s.contains("2 turn"), "{s}");
        assert!(s.contains("75%"), "{s}");
    }

    #[test]
    fn command_parse_ergonomics() {
        assert!(matches!(Command::parse("/editor"), Command::Editor));
        assert!(matches!(Command::parse("/e"), Command::Editor));
        assert!(matches!(Command::parse("/retry"), Command::Retry));
        assert!(matches!(Command::parse("/r"), Command::Retry));
        assert!(matches!(Command::parse("/copy"), Command::Copy));
        assert!(matches!(Command::parse("/y"), Command::Copy));
    }

    #[test]
    fn prepare_retry_pops_and_classifies() {
        let mut c = Conversation::new("m".into(), None);
        assert!(matches!(prepare_retry(&mut c), RetryPlan::Nothing));
        c.push_user("hi".into());
        c.push_assistant("yo".into());
        // ends with assistant → pop it, hand it back for restore-on-failure
        assert!(matches!(
            prepare_retry(&mut c),
            RetryPlan::Ready { restore: Some(_) }
        ));
        assert_eq!(c.messages.len(), 1); // assistant popped, user remains
                                         // now ends with a user (unanswered) → ready, nothing to restore
        assert!(matches!(
            prepare_retry(&mut c),
            RetryPlan::Ready { restore: None }
        ));
        assert_eq!(c.messages.len(), 1);
    }

    #[test]
    fn osc52_mux_wrapping() {
        let bare = osc52_copy("x");
        // outside a multiplexer → unchanged
        assert_eq!(
            wrap_osc52_for_mux(bare.clone(), false, "xterm-256color"),
            bare
        );
        // tmux → DCS passthrough with each inner ESC doubled
        let t = wrap_osc52_for_mux(bare.clone(), true, "screen");
        assert!(t.starts_with("\x1bPtmux;"), "{t:?}");
        assert!(t.ends_with("\x1b\\"), "{t:?}");
        assert!(
            t.contains("\x1b\x1b]52"),
            "inner ESC must be doubled: {t:?}"
        );
        // screen (no tmux) → DCS chunk
        let s = wrap_osc52_for_mux(bare, false, "screen.xterm");
        assert!(s.starts_with("\x1bP") && s.ends_with("\x1b\\"), "{s:?}");
    }

    #[test]
    fn last_assistant_text_finds_latest() {
        let mut c = Conversation::new("m".into(), None);
        assert!(last_assistant_text(&c).is_none());
        c.push_user("hi".into());
        c.push_assistant("first".into());
        c.push_user("more".into());
        c.push_assistant("second".into());
        assert_eq!(last_assistant_text(&c).as_deref(), Some("second"));
    }

    #[test]
    fn osc52_wraps_base64() {
        let s = osc52_copy("hi");
        assert!(s.starts_with("\x1b]52;c;"), "{s:?}");
        assert!(s.ends_with('\x07'), "{s:?}");
        assert!(s.contains("aGk="), "base64 of 'hi' missing: {s:?}");
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
