//! REPL command surface for `tt chat`: line parsing (`Command::parse`), the
//! `/help` text, and the OSC52 clipboard helpers used by `/copy`.

use crate::ui;

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

pub(super) fn print_help() {
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
pub(super) const OSC52_MAX_BYTES: usize = 55 * 1024;

/// Wrap a bare OSC52 sequence for tmux/screen passthrough so `/copy` works
/// inside a multiplexer. Env is read at the call site so this stays pure.
#[must_use]
pub(super) fn wrap_osc52_for_mux(seq: String, in_tmux: bool, term: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn command_parse_ergonomics() {
        assert!(matches!(Command::parse("/editor"), Command::Editor));
        assert!(matches!(Command::parse("/e"), Command::Editor));
        assert!(matches!(Command::parse("/retry"), Command::Retry));
        assert!(matches!(Command::parse("/r"), Command::Retry));
        assert!(matches!(Command::parse("/copy"), Command::Copy));
        assert!(matches!(Command::parse("/y"), Command::Copy));
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
    fn osc52_wraps_base64() {
        let s = osc52_copy("hi");
        assert!(s.starts_with("\x1b]52;c;"), "{s:?}");
        assert!(s.ends_with('\x07'), "{s:?}");
        assert!(s.contains("aGk="), "base64 of 'hi' missing: {s:?}");
    }
}
