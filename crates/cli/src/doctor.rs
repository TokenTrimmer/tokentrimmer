//! `tt doctor` — self-diagnosis. Runs the probes a support ticket would, and
//! prints the exact fix per failure:
//!   1. base-URL reachability — DNS/parse + `GET /health` (liveness).
//!   2. live-key validity — authenticated `GET /v1/capabilities`.
//!   3. gateway-vs-CLI version — the /health `version` + `git_sha` vs this CLI.
//!   4. MCP config health — whether an installed client's config points at the
//!      gateway.
//!
//! Non-zero exit if any check fails, so it drops into CI / a "is my setup
//! right?" flow. Every §1.2-class failure (DNS NXDOMAIN, wrong base URL, dead
//! key, stale CLI vs gateway) becomes a self-diagnosis instead of a ticket.
//!
//! All network is best-effort + short-timeout: doctor is a *diagnostic*, never
//! a blocker. Unconfigured (no key) → the key-validity check is skipped with a
//! clear "run `tt login`" hint, not a failure.

use anyhow::Context;

use crate::mcp_install::{config_path_for, McpClient};
use crate::ui;
use crate::{capabilities, context};

const HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// The entry point for `tt doctor`.
///
/// # Errors
/// Returns an error (→ non-zero exit) when any check fails; the final message
/// summarizes the count. A missing key is reported but not a failure (the
/// authenticated round-trip is simply skipped with a hint).
pub async fn run() -> anyhow::Result<()> {
    let mut failures = 0usize;

    // Resolve the base URL the same way every other command does (flag > env >
    // file > built-in default) so doctor reports what `tt chat` would hit.
    let ctx = context::ResolvedContext::load(None, None);
    let (base, base_source) = match ctx.as_ref() {
        Ok(c) => (c.base_url.clone(), c.base_source),
        Err(_) => {
            // No stored config at all → fall back to the default.
            (
                context::DEFAULT_BASE_URL.to_string(),
                context::BaseSource::Default,
            )
        }
    };

    ui::heading("TokenTrimmer doctor");
    println!(
        "  {} {} (source: {})",
        ui::muted().apply_to("base:  "),
        base,
        base_source
    );

    // ── 1. base-URL parse + DNS ─────────────────────────────────────────────
    let health_url = format!("{}/health", base.trim_end_matches('/'));
    let dns_ok = match reqwest::Url::parse(&health_url) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("(no host)");
            match resolve_host(host) {
                Ok(ips) => {
                    if ips.is_empty() {
                        ui::error(&format!("DNS: {host} does not resolve (NXDOMAIN)"));
                        ui::note(
                            "  fix: the base URL's hostname has no DNS A/AAAA record. Point it at",
                        );
                        ui::note("       a live gateway, or `tt login --base-url <live-url>`.");
                        false
                    } else {
                        ui::ok(&format!("DNS: {host} → {}", ips.join(", ")));
                        true
                    }
                }
                Err(e) => {
                    ui::error(&format!("DNS: could not resolve {host}: {e}"));
                    false
                }
            }
        }
        Err(e) => {
            ui::error(&format!("base URL unparseable: {e}"));
            ui::note("  fix: `tt login --base-url https://<host>` with a valid https URL.");
            false
        }
    };
    if !dns_ok {
        failures += 1;
    }

    // ── 2 + 3. GET /health (liveness + gateway version) ────────────────────
    if dns_ok {
        match probe_health(&health_url).await {
            Ok((status, version, git_sha)) => {
                ui::ok(&format!(
                    "gateway: /health → {status} (version {version}, sha {git_sha})"
                ));
                let cli_ver = env!("CARGO_PKG_VERSION");
                if version != cli_ver {
                    ui::note(&format!(
                        "  note: CLI v{cli_ver} vs gateway v{version} — a mismatch is usually fine"
                    ));
                }
            }
            Err(e) => {
                ui::error(&format!("gateway: /health unreachable: {e}"));
                ui::note("  fix: is the gateway running + reachable at the base URL? If the host");
                ui::note(
                    "       resolves but /health 404s, you may be pointing at a non-gateway host.",
                );
                failures += 1;
            }
        }
    }

    // ── 4. live-key validity (one authenticated round-trip) ────────────────
    match ctx.as_ref().ok().and_then(|c| c.api_key.as_ref()) {
        Some(k) if k.expose().starts_with("tt_test_") => {
            ui::note(
                "key:     sandbox token format accepted locally; tt_test_* tokens have no \
                 server-side identity to verify",
            );
        }
        Some(k) => match capabilities::fetch_capabilities(&base, k.expose()).await {
            Ok(_) => {
                ui::ok("key:     GET /v1/capabilities → authenticated OK");
            }
            Err(e) => {
                ui::error(&format!(
                    "key:     GET /v1/capabilities verification failed: {e}"
                ));
                ui::note(
                    "  fix: confirm the gateway supports /v1/capabilities; otherwise re-issue",
                );
                ui::note("       the key and run `tt login --token <new-key>`.");
                failures += 1;
            }
        },
        None => {
            ui::note("key:     none configured (run `tt login` to add one — skipped)");
        }
    }

    // ── 5. MCP config health ────────────────────────────────────────────────
    check_mcp_config();

    println!();
    if failures == 0 {
        ui::success("All checks passed.");
        Ok(())
    } else {
        ui::error(&format!(
            "{failures} check(s) failed — see the fix hints above."
        ));
        anyhow::bail!("doctor found {failures} problem(s)")
    }
}

/// Resolve a host to its IPv4/AAAA records (best-effort; uses the std resolver
/// via `to_socket_addrs`).
fn resolve_host(host: &str) -> anyhow::Result<Vec<String>> {
    use std::net::ToSocketAddrs;
    // to_socket_addrs needs a host:port; use a dummy port (we only want IPs).
    let addrs = (host, 443u16)
        .to_socket_addrs()
        .context("DNS lookup failed")?;
    Ok(addrs.map(|a| a.ip().to_string()).collect())
}

#[derive(serde::Deserialize)]
struct HealthBody {
    status: String,
    version: String,
    git_sha: String,
}

async fn probe_health(url: &str) -> anyhow::Result<(String, String, String)> {
    let body: HealthBody = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .context("build HTTP client")?
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .json()
        .await
        .context("decode /health JSON")?;
    Ok((body.status, body.version, body.git_sha))
}

/// Check whether any installed MCP client config points at the gateway
/// (best-effort; not a failure if none installed).
fn check_mcp_config() {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
    let os = std::env::consts::OS;
    let clients = [
        McpClient::ClaudeCode,
        McpClient::Cursor,
        McpClient::ClaudeDesktop,
    ];
    let mut any = false;
    for c in clients {
        let home_ref: &std::path::Path =
            home.as_deref().unwrap_or_else(|| std::path::Path::new(""));
        if let Some(path) = config_path_for(c, os, home_ref, appdata.as_deref()) {
            any = true;
            if path.exists() {
                let configured = if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                        val.get("mcpServers")
                            .and_then(|s| s.get("tokentrimmer"))
                            .is_some()
                    } else {
                        false
                    }
                } else {
                    false
                };

                if configured {
                    ui::ok(&format!(
                        "mcp:     {} configured with tokentrimmer ({})",
                        c.display_name(),
                        path.display()
                    ));
                } else {
                    ui::note(&format!(
                        "mcp:     {} config found at {} but tokentrimmer server missing — run `tt mcp install --client {}`",
                        c.display_name(),
                        path.display(),
                        match c {
                            McpClient::ClaudeCode => "claude-code",
                            McpClient::Cursor => "cursor",
                            McpClient::ClaudeDesktop => "claude-desktop",
                        }
                    ));
                }
            } else {
                ui::note(&format!(
                    "mcp:     {} config not found at {} — run `tt mcp install --client {}`",
                    c.display_name(),
                    path.display(),
                    match c {
                        McpClient::ClaudeCode => "claude-code",
                        McpClient::Cursor => "cursor",
                        McpClient::ClaudeDesktop => "claude-desktop",
                    }
                ));
            }
        }
    }
    if !any {
        ui::note("mcp:     no MCP client config path resolvable (HOME unset?) — skipped");
    }
}
