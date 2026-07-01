//! `tt mcp install` — autowire an MCP client's config file to launch
//! TokenTrimmer's MCP server.
//!
//! Registering the `tokentrimmer` MCP server in a client (Claude Desktop,
//! Claude Code, Cursor) is otherwise a manual JSON paste. This module locates
//! the client's config JSON per-OS, injects (or overwrites just) an
//! `mcpServers.tokentrimmer` entry carrying the resolved API key via the
//! server's own `TT_API_KEY`/`TT_API_BASE` env resolution, backs up the prior
//! file, and pretty-prints the merged result.
//!
//! The pure pieces — path resolution, entry construction, and JSON merge — are
//! split out (and unit-tested) so the disk-touching orchestration stays thin.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde_json::{json, Map, Value};

use crate::context;
use crate::ui;

/// A supported MCP client target (a single, concrete client — `all` is expanded
/// by the caller into the full set before reaching this module).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpClient {
    /// Anthropic's Claude Desktop app.
    ClaudeDesktop,
    /// The Claude Code CLI (`~/.claude.json` user scope).
    ClaudeCode,
    /// Cursor's global MCP config (`~/.cursor/mcp.json`).
    Cursor,
}

impl McpClient {
    /// Human-facing name for summaries.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            McpClient::ClaudeDesktop => "Claude Desktop",
            McpClient::ClaudeCode => "Claude Code",
            McpClient::Cursor => "Cursor",
        }
    }
}

/// Resolve the config-file path for `client` given an OS string, home dir, and
/// (Windows) `%APPDATA%`. Pure/testable — the real caller passes
/// `std::env::consts::OS`, `dirs::home_dir()`, and `$APPDATA`.
///
/// Returns `None` only when the platform needs a value that wasn't supplied
/// (Windows Claude Desktop without `%APPDATA%`).
#[must_use]
pub fn config_path_for(
    client: McpClient,
    os: &str,
    home: &Path,
    appdata: Option<&Path>,
) -> Option<PathBuf> {
    match client {
        // Claude Desktop stores its config in the per-OS app-support dir.
        McpClient::ClaudeDesktop => match os {
            "macos" => {
                Some(home.join("Library/Application Support/Claude/claude_desktop_config.json"))
            }
            "windows" => appdata.map(|a| a.join("Claude").join("claude_desktop_config.json")),
            // Linux + any other unix: XDG-style ~/.config.
            _ => Some(home.join(".config/Claude/claude_desktop_config.json")),
        },
        // Claude Code's user-scope MCP servers live in ~/.claude.json.
        McpClient::ClaudeCode => Some(home.join(".claude.json")),
        // Cursor's documented global MCP config.
        McpClient::Cursor => Some(home.join(".cursor/mcp.json")),
    }
}

/// Build the `tokentrimmer` server entry. The key rides in `env.TT_API_KEY`
/// (kept out of the process arg list) and is read back by `tt mcp`'s own
/// context resolver; `base` is written as `TT_API_BASE` only when non-default.
#[must_use]
pub fn build_server_entry(command: &str, key: &str, base: Option<&str>) -> Value {
    let mut env = Map::new();
    env.insert("TT_API_KEY".to_string(), Value::String(key.to_string()));
    if let Some(b) = base {
        env.insert("TT_API_BASE".to_string(), Value::String(b.to_string()));
    }
    json!({
        "command": command,
        "args": ["mcp"],
        "env": Value::Object(env),
    })
}

/// Parse a client's existing config text. Empty/whitespace becomes an empty
/// object; malformed JSON is a hard, clearly-labelled error (so we never
/// clobber a file we can't understand).
pub fn parse_existing(contents: &str) -> anyhow::Result<Value> {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(trimmed).context("existing config is not valid JSON")
}

/// Merge `entry` into `existing` as `mcpServers.tokentrimmer`, preserving every
/// other server and unrelated root key. Overwrites only the `tokentrimmer`
/// slot. Errors if the root — or an existing `mcpServers` — isn't an object.
pub fn merge_config(existing: Value, entry: Value) -> anyhow::Result<Value> {
    let mut root = match existing {
        Value::Null => Map::new(),
        Value::Object(m) => m,
        _ => anyhow::bail!("config root must be a JSON object"),
    };
    let servers = root
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let servers_map = servers
        .as_object_mut()
        .context("`mcpServers` in the existing config is not a JSON object")?;
    servers_map.insert("tokentrimmer".to_string(), entry);
    Ok(Value::Object(root))
}

/// Sibling backup path: `<name>.<UTC-timestamp>.bak` (never clobbers a prior
/// backup, unlike a fixed `.bak`).
fn backup_path(path: &Path) -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.json");
    path.with_file_name(format!("{name}.{ts}.bak"))
}

/// Outcome of installing into one client's config (for the printed summary).
#[derive(Debug)]
struct InstallOutcome {
    path: PathBuf,
    backup: Option<PathBuf>,
    created: bool,
    merged: Value,
}

/// Read → parse → merge → (unless `dry_run`) back up + write one client's
/// config. Creates parent dirs as a safety net. Pretty-prints with serde_json's
/// default 2-space indent plus a trailing newline.
fn install_one(path: &Path, entry: &Value, dry_run: bool) -> anyhow::Result<InstallOutcome> {
    let existed = path.exists();

    let existing = if existed {
        let contents =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        parse_existing(&contents).with_context(|| format!("parse {}", path.display()))?
    } else {
        Value::Object(Map::new())
    };

    let merged = merge_config(existing, entry.clone())?;

    let mut backup = None;
    if !dry_run {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
        }
        if existed {
            let bak = backup_path(path);
            std::fs::copy(path, &bak)
                .with_context(|| format!("back up {} to {}", path.display(), bak.display()))?;
            backup = Some(bak);
        }
        let mut rendered = serde_json::to_string_pretty(&merged)?;
        rendered.push('\n');
        std::fs::write(path, rendered.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
    }

    Ok(InstallOutcome {
        path: path.to_path_buf(),
        backup,
        created: !existed,
        merged,
    })
}

/// Return a copy of `merged` with the `tokentrimmer` key masked, for on-screen
/// (dry-run) display when `--reveal` was not passed.
fn merged_for_display(merged: &Value, masked_key: &str) -> Value {
    let mut v = merged.clone();
    if let Some(k) = v.pointer_mut("/mcpServers/tokentrimmer/env/TT_API_KEY") {
        *k = Value::String(masked_key.to_string());
    }
    v
}

/// `tt mcp install` entrypoint. Resolves the key/base (same resolver as the
/// other account commands), builds the entry once, then patches each client's
/// config. Skips (never crashes) when a client isn't installed; for multiple
/// clients it continues past skips/errors and reports a non-zero exit only if a
/// real error occurred.
pub fn run_install(
    clients: &[McpClient],
    dry_run: bool,
    reveal: bool,
    config_path_override: Option<PathBuf>,
    tt_api_key: Option<String>,
    tt_api_base: Option<String>,
) -> anyhow::Result<()> {
    if config_path_override.is_some() && clients.len() != 1 {
        anyhow::bail!("--config-path targets a single file; use it with exactly one --client");
    }

    let ctx = context::ResolvedContext::load(tt_api_key, tt_api_base)?;
    let Some(secret) = ctx.api_key.as_ref() else {
        ui::warn("No API key found. Run `tt login` first (or pass --tt-api-key).");
        anyhow::bail!("no API key configured");
    };
    let key = secret.expose();

    // Only pin TT_API_BASE when it isn't the built-in default, so configs track
    // the shipped default over time instead of freezing an old URL.
    let base = if ctx.base_url.trim_end_matches('/') == context::DEFAULT_BASE_URL {
        None
    } else {
        Some(ctx.base_url.as_str())
    };

    // The absolute path to *this* `tt` binary, so the client launches the same
    // one the user just ran `install` with.
    let command = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "tt".to_string());

    let entry = build_server_entry(&command, key, base);
    let key_display = if reveal {
        key.to_string()
    } else {
        context::mask_key(key)
    };

    let os = std::env::consts::OS;
    let home = dirs::home_dir();
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);

    ui::heading("Installing the TokenTrimmer MCP server");
    println!("  {} {key_display}", ui::muted().apply_to("key:    "));
    println!("  {} {command}", ui::muted().apply_to("command:"));
    if let Some(b) = base {
        println!("  {} {b}", ui::muted().apply_to("base:   "));
    }
    if dry_run {
        ui::info("dry run — no files will be written");
    }

    let mut error_count = 0usize;
    for &client in clients {
        let resolved = match config_path_override {
            Some(ref p) => Some(p.clone()),
            None => home
                .as_deref()
                .and_then(|h| config_path_for(client, os, h, appdata.as_deref())),
        };
        let Some(path) = resolved else {
            ui::warn(&format!(
                "{}: could not resolve a config path on this platform; skipping.",
                client.display_name()
            ));
            continue;
        };

        // "Not installed" heuristic: for auto-detected clients, if neither the
        // config file nor its parent dir exists, the client almost certainly
        // isn't set up. Skip with a clear message instead of materialising a
        // config for an app that isn't there. (An explicit --config-path
        // opts out of this and always writes.)
        if config_path_override.is_none() {
            let parent_exists = path.parent().is_some_and(Path::exists);
            if !parent_exists && !path.exists() {
                let where_ = path
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                ui::warn(&format!(
                    "{}: not detected (no config dir at {where_}); skipping. \
                     Install it first, or pass --config-path.",
                    client.display_name()
                ));
                continue;
            }
        }

        match install_one(&path, &entry, dry_run) {
            Ok(outcome) => {
                if dry_run {
                    ui::info(&format!(
                        "{}: would update {}",
                        client.display_name(),
                        outcome.path.display()
                    ));
                    let shown = if reveal {
                        outcome.merged
                    } else {
                        merged_for_display(&outcome.merged, &key_display)
                    };
                    println!("{}", serde_json::to_string_pretty(&shown)?);
                } else {
                    ui::success(&format!(
                        "{}: registered `tokentrimmer` → {}",
                        client.display_name(),
                        outcome.path.display()
                    ));
                    if let Some(bak) = &outcome.backup {
                        ui::note(&format!("backup: {}", bak.display()));
                    } else if outcome.created {
                        ui::note("created a new config file (no prior file to back up)");
                    }
                }
            }
            Err(e) => {
                error_count += 1;
                ui::error(&format!("{}: {e:#}", client.display_name()));
            }
        }
    }

    if error_count > 0 {
        anyhow::bail!("{error_count} client(s) failed to configure");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Value {
        build_server_entry("/usr/local/bin/tt", "tt_live_secret_key_123456", None)
    }

    #[test]
    fn build_entry_shape() {
        let e = build_server_entry("/bin/tt", "tt_live_k", None);
        assert_eq!(e["command"], "/bin/tt");
        assert_eq!(e["args"], json!(["mcp"]));
        assert_eq!(e["env"]["TT_API_KEY"], "tt_live_k");
        assert!(e["env"].get("TT_API_BASE").is_none());

        let e2 = build_server_entry("/bin/tt", "tt_live_k", Some("https://gw.example/v1"));
        assert_eq!(e2["env"]["TT_API_BASE"], "https://gw.example/v1");
    }

    #[test]
    fn merge_preserves_existing_servers_and_unrelated_keys() {
        let existing = json!({
            "theme": "dark",
            "mcpServers": {
                "other": { "command": "othercmd", "args": ["x"] }
            }
        });
        let merged = merge_config(existing, entry()).unwrap();

        // Unrelated root key preserved.
        assert_eq!(merged["theme"], "dark");
        // Pre-existing server preserved untouched.
        assert_eq!(merged["mcpServers"]["other"]["command"], "othercmd");
        // Our entry injected.
        assert_eq!(
            merged["mcpServers"]["tokentrimmer"]["command"],
            "/usr/local/bin/tt"
        );
    }

    #[test]
    fn merge_overwrites_only_tokentrimmer() {
        let existing = json!({
            "mcpServers": {
                "tokentrimmer": { "command": "OLD", "args": ["stale"] },
                "keepme": { "command": "keep" }
            }
        });
        let merged = merge_config(existing, entry()).unwrap();
        // tokentrimmer replaced wholesale.
        assert_eq!(
            merged["mcpServers"]["tokentrimmer"]["command"],
            "/usr/local/bin/tt"
        );
        assert_eq!(merged["mcpServers"]["tokentrimmer"]["args"], json!(["mcp"]));
        // Sibling server untouched.
        assert_eq!(merged["mcpServers"]["keepme"]["command"], "keep");
    }

    #[test]
    fn merge_into_empty_creates_servers_map() {
        let merged = merge_config(Value::Object(Map::new()), entry()).unwrap();
        assert!(merged["mcpServers"]["tokentrimmer"].is_object());
    }

    #[test]
    fn merge_rejects_non_object_root() {
        let err = merge_config(json!([1, 2, 3]), entry()).unwrap_err();
        assert!(err.to_string().contains("root must be a JSON object"));
    }

    #[test]
    fn merge_rejects_non_object_mcpservers() {
        let existing = json!({ "mcpServers": "oops" });
        let err = merge_config(existing, entry()).unwrap_err();
        assert!(err.to_string().contains("mcpServers"));
    }

    #[test]
    fn parse_empty_is_object() {
        assert_eq!(parse_existing("").unwrap(), json!({}));
        assert_eq!(parse_existing("   \n").unwrap(), json!({}));
    }

    #[test]
    fn parse_malformed_is_handled_as_error_not_panic() {
        let err = parse_existing("{ not valid json ").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn config_paths_per_os() {
        let home = Path::new("/home/u");
        let appdata = Path::new("C:\\Users\\u\\AppData\\Roaming");

        assert_eq!(
            config_path_for(McpClient::ClaudeDesktop, "macos", home, None).unwrap(),
            home.join("Library/Application Support/Claude/claude_desktop_config.json")
        );
        assert_eq!(
            config_path_for(McpClient::ClaudeDesktop, "linux", home, None).unwrap(),
            home.join(".config/Claude/claude_desktop_config.json")
        );
        assert_eq!(
            config_path_for(McpClient::ClaudeDesktop, "windows", home, Some(appdata)).unwrap(),
            appdata.join("Claude").join("claude_desktop_config.json")
        );
        // Windows Claude Desktop with no %APPDATA% can't be resolved.
        assert!(config_path_for(McpClient::ClaudeDesktop, "windows", home, None).is_none());

        assert_eq!(
            config_path_for(McpClient::ClaudeCode, "linux", home, None).unwrap(),
            home.join(".claude.json")
        );
        assert_eq!(
            config_path_for(McpClient::Cursor, "linux", home, None).unwrap(),
            home.join(".cursor/mcp.json")
        );
    }

    #[test]
    fn install_one_merges_preserving_others_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude_desktop_config.json");
        std::fs::write(
            &path,
            r#"{
  "mcpServers": {
    "existing": { "command": "keep", "args": [] }
  }
}
"#,
        )
        .unwrap();

        let outcome = install_one(&path, &entry(), false).unwrap();
        assert!(!outcome.created);
        let bak = outcome.backup.expect("a backup should be made");
        assert!(bak.exists(), "backup file must exist on disk");

        // The written file must be valid JSON preserving the other server.
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["existing"]["command"], "keep");
        assert_eq!(
            written["mcpServers"]["tokentrimmer"]["command"],
            "/usr/local/bin/tt"
        );
        // Backup preserves the ORIGINAL (no tokentrimmer key yet).
        let backed_up: Value =
            serde_json::from_str(&std::fs::read_to_string(&bak).unwrap()).unwrap();
        assert!(backed_up["mcpServers"].get("tokentrimmer").is_none());
    }

    #[test]
    fn install_one_creates_new_file_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("mcp.json");
        let outcome = install_one(&path, &entry(), false).unwrap();
        assert!(outcome.created);
        assert!(outcome.backup.is_none());
        assert!(path.exists());
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(written["mcpServers"]["tokentrimmer"].is_object());
    }

    #[test]
    fn install_one_dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let outcome = install_one(&path, &entry(), true).unwrap();
        assert!(!path.exists(), "dry run must not create the file");
        assert!(outcome.merged["mcpServers"]["tokentrimmer"].is_object());
    }

    #[test]
    fn dry_run_display_masks_key() {
        let merged = merge_config(Value::Object(Map::new()), entry()).unwrap();
        let shown = merged_for_display(&merged, "tt_live_secr…");
        assert_eq!(
            shown["mcpServers"]["tokentrimmer"]["env"]["TT_API_KEY"],
            "tt_live_secr…"
        );
        // The real merged value is untouched.
        assert_eq!(
            merged["mcpServers"]["tokentrimmer"]["env"]["TT_API_KEY"],
            "tt_live_secret_key_123456"
        );
    }

    #[test]
    fn malformed_existing_file_errors_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let err = install_one(&path, &entry(), false).unwrap_err();
        assert!(format!("{err:#}").contains("not valid JSON"));
        // Original left intact (not clobbered).
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json"
        );
    }
}
