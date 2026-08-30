//! On-disk chat sessions under `~/.tokentrimmer/sessions/`.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::chat::Conversation;
use crate::context::store;

/// Sessions directory (`~/.tokentrimmer/sessions/`).
#[must_use]
pub fn sessions_dir() -> PathBuf {
    store::config_dir().join("sessions")
}

/// Sanitize a user-supplied session name to a safe file stem. Drops `.` and any
/// non-`[a-z0-9_-]` char (→ `-`), so the result can never contain a path
/// separator or `..` — `/resume ../../x` stays inside the sessions dir.
#[must_use]
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "chat".to_string()
    } else {
        s
    }
}

/// Derive a session name from the first user message (≤5 words), else `"chat"`.
#[must_use]
pub fn auto_name(conv: &Conversation) -> String {
    use tt_shared::messages::{Message, MessageContent};
    let first = conv.messages.iter().find_map(|m| match m {
        Message::User {
            content: MessageContent::Text(t),
            ..
        } => Some(t.as_str()),
        _ => None,
    });
    match first {
        Some(t) => {
            let words: Vec<&str> = t.split_whitespace().take(5).collect();
            sanitize_name(&words.join("-"))
        }
        None => "chat".to_string(),
    }
}

fn path_for(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.json", sanitize_name(name)))
}

/// Save `conv` as `<dir>/<sanitized name>.json`. Returns the path written.
pub fn save(dir: &Path, name: &str, conv: &Conversation) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = path_for(dir, name);
    let json = serde_json::to_string_pretty(conv).context("serialize session")?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Load a session by name.
pub fn load(dir: &Path, name: &str) -> anyhow::Result<Conversation> {
    let path = path_for(dir, name);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("no session `{}` ({})", sanitize_name(name), path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

/// One saved session's metadata for `/sessions`.
pub struct SessionMeta {
    pub name: String,
    pub model: String,
    pub turns: usize,
    pub modified: std::time::SystemTime,
}

/// List saved sessions, newest first. Missing dir → empty.
pub fn list(dir: &Path) -> anyhow::Result<Vec<SessionMeta>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(conv) = serde_json::from_str::<Conversation>(&raw) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        out.push(SessionMeta {
            name: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string(),
            model: conv.model,
            turns: conv.messages.len() / 2,
            modified,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Conversation;

    #[test]
    fn sanitize_blocks_traversal_and_seps() {
        let s = sanitize_name("../../etc/passwd");
        assert!(!s.contains('/') && !s.contains('.'), "got {s:?}");
        assert_eq!(sanitize_name("My Chat!"), "my-chat");
        assert_eq!(sanitize_name(""), "chat");
        assert_eq!(sanitize_name("rust-help"), "rust-help");
    }

    #[test]
    fn auto_name_from_first_message() {
        let mut c = Conversation::new("m".into(), None);
        c.push_user("Explain the borrow checker please".into());
        assert_eq!(auto_name(&c), "explain-the-borrow-checker-please");
        assert_eq!(auto_name(&Conversation::new("m".into(), None)), "chat");
    }

    /// The frozen summary round-trips byte-identically through save/load (a
    /// re-encoded block would bust the provider-cached prefix on resume), and
    /// old-format session files (no `summary` field) load as `None`.
    #[test]
    fn session_roundtrips_frozen_summary_bytes() {
        let dir = std::env::temp_dir().join(format!("tt-sess-sum-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let block = "[Conversation summary — earlier turns were compacted by tt chat]\n\
                     the user explored routing; café naïve ünïcode bytes must survive";
        let mut c = Conversation::new("gpt-4o".into(), Some("be brief".into()));
        c.summary = Some(crate::chat::compact::FrozenSummary::replace_at_compaction(
            block.into(),
            7,
            "gpt-4o-mini".into(),
        ));
        c.push_user("hi".into());
        c.push_assistant("yo".into());
        save(&dir, "sum", &c).unwrap();
        let loaded = load(&dir, "sum").unwrap();
        let sum = loaded.summary.expect("summary survives the round-trip");
        assert_eq!(sum.wire_block(), block, "bytes must be identical");
        assert_eq!(sum.folded_turns(), 7);
        // old-format JSON (pre-compaction CLI) → summary == None
        let legacy = r#"{"model":"gpt-4o","system":null,"messages":[]}"#;
        let old: Conversation = serde_json::from_str(legacy).unwrap();
        assert!(old.summary.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_load_roundtrip_and_path_stays_in_dir() {
        let dir = std::env::temp_dir().join(format!("tt-sess-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let mut c = Conversation::new("gpt-4o".into(), Some("be brief".into()));
        c.push_user("hi".into());
        c.push_assistant("yo".into());
        let p = save(&dir, "../../escape", &c).unwrap();
        assert_eq!(p.parent().unwrap(), dir, "path must stay in sessions dir");
        let loaded = load(&dir, "../../escape").unwrap();
        assert_eq!(loaded.model, "gpt-4o");
        assert_eq!(loaded.messages.len(), 2);
        let metas = list(&dir).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].turns, 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
