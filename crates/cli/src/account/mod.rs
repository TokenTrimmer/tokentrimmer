//! `tt login` / `tt logout` / `tt whoami` command bodies. The credential store
//! and resolver live in `crate::context`.

use anyhow::Context as _;

use crate::context::{self, store};
use crate::ui;

/// Pure decision: resolve the token text from the `--token` arg and (when the
/// arg is `-`) the stdin contents. Errors on a missing / blank token.
pub fn decide_token(arg: Option<String>, stdin: Option<String>) -> anyhow::Result<String> {
    match arg.as_deref() {
        None => anyhow::bail!("no token provided"),
        Some("-") => {
            let s = stdin.unwrap_or_default();
            let t = s.trim();
            if t.is_empty() {
                anyhow::bail!("no token on stdin");
            }
            Ok(t.to_string())
        }
        Some(other) => {
            let t = other.trim();
            if t.is_empty() {
                anyhow::bail!("empty --token");
            }
            Ok(t.to_string())
        }
    }
}

/// The OS-specific command to open `url` in the default browser. `None` for an
/// unrecognized OS (the caller then just prints the URL).
#[must_use]
pub fn browser_command_for(os: &str, url: &str) -> Option<(&'static str, Vec<String>)> {
    match os {
        "macos" => Some(("open", vec![url.to_string()])),
        "linux" => Some(("xdg-open", vec![url.to_string()])),
        // The empty title arg keeps `start` from treating a quoted URL as a title.
        "windows" => Some((
            "cmd",
            vec!["/C".into(), "start".into(), String::new(), url.to_string()],
        )),
        _ => None,
    }
}

/// `tt login --token <KEY>` (browser login lands in V2). `--token -` reads the
/// key from stdin (keeps it out of shell history). Optionally persists base URL.
pub fn login_with_token(token: Option<String>, base_url: Option<String>) -> anyhow::Result<()> {
    if token.is_none() {
        anyhow::bail!(
            "browser login arrives in V2 — use `tt login --token <KEY>` for now \
             (get a key at app.tokentrimmer.com)"
        );
    }
    let stdin = if token.as_deref() == Some("-") {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
            .context("read token from stdin")?;
        Some(s)
    } else {
        None
    };
    let raw = decide_token(token, stdin)?;
    let validated = tt_mcp::auth::validate_api_key(Some(raw))
        .map_err(|e| anyhow::anyhow!("invalid key: {e}"))?;

    let dir = store::config_dir();
    store::save_credentials(&dir, &validated)?;
    if let Some(b) = base_url.filter(|s| !s.trim().is_empty()) {
        store::save_config(&dir, b.trim())?;
    }
    let base = store::load_config(&dir)?.unwrap_or_else(|| context::DEFAULT_BASE_URL.to_string());
    ui::success(&format!(
        "Logged in. Stored {} in {} (base: {}).",
        context::mask_key(&validated),
        dir.join("credentials.toml").display(),
        base,
    ));
    Ok(())
}

/// `tt whoami` — local only (no network in V0). Exit 1 when no key is configured.
pub fn whoami() -> anyhow::Result<()> {
    let ctx = context::ResolvedContext::load(None, None)?;
    match &ctx.api_key {
        Some(k) => {
            ui::heading("Logged in");
            println!(
                "  {} {} (source: {})",
                ui::muted().apply_to("key:   "),
                context::mask_key(k.expose()),
                ctx.key_source
            );
            println!(
                "  {} {} (source: {})",
                ui::muted().apply_to("base:  "),
                ctx.base_url,
                ctx.base_source
            );
            println!(
                "  {} {}",
                ui::muted().apply_to("config:"),
                store::config_dir().display()
            );
            Ok(())
        }
        None => {
            ui::warn("Not logged in. Run `tt login --token <KEY>` or set TT_API_KEY.");
            eprintln!(
                "  {} {} (source: {})",
                ui::muted().apply_to("base:"),
                ctx.base_url,
                ctx.base_source
            );
            std::process::exit(1);
        }
    }
}

/// `tt logout` — remove the local key only (does NOT revoke server-side).
pub fn logout() -> anyhow::Result<()> {
    let dir = store::config_dir();
    if store::delete_credentials(&dir)? {
        ui::success(&format!(
            "Logged out — removed {}.",
            dir.join("credentials.toml").display()
        ));
        ui::info(
            "Note: this only clears the local key; it does not revoke it server-side. \
             Revoke in the dashboard if it may be compromised.",
        );
    } else {
        ui::info("Not logged in (nothing to remove).");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_from_arg() {
        assert_eq!(
            decide_token(Some("tt_live_x".into()), None).unwrap(),
            "tt_live_x"
        );
    }

    #[test]
    fn token_trimmed_from_stdin() {
        let t = decide_token(Some("-".into()), Some("tt_test_y\n".into())).unwrap();
        assert_eq!(t, "tt_test_y");
    }

    #[test]
    fn empty_token_is_rejected() {
        assert!(decide_token(Some("   ".into()), None).is_err());
        assert!(decide_token(Some("-".into()), Some("\n".into())).is_err());
        assert!(decide_token(Some("-".into()), None).is_err());
    }

    #[test]
    fn browser_command_per_os() {
        assert_eq!(
            browser_command_for("macos", "http://x"),
            Some(("open", vec!["http://x".to_string()]))
        );
        assert_eq!(
            browser_command_for("linux", "http://x"),
            Some(("xdg-open", vec!["http://x".to_string()]))
        );
        let (prog, args) = browser_command_for("windows", "http://x").unwrap();
        assert_eq!(prog, "cmd");
        assert_eq!(
            args,
            vec![
                "/C".to_string(),
                "start".to_string(),
                String::new(),
                "http://x".to_string()
            ]
        );
        assert!(browser_command_for("plan9", "http://x").is_none());
    }
}
