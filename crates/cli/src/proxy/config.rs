//! Runtime config for tt proxy. Mostly comes from CLI args; a few values
//! (default session log path) resolve from ~/.tokentrimmer/.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// All endpoints → hosted TokenTrimmer Gateway with the TokenTrimmer key
    /// injected (full features). The gateway exposes an Anthropic-native
    /// `/v1/messages` ingress that multiplexes through the same routing/cache/
    /// failover pipeline as `/v1/chat/completions`, so Anthropic-wire clients
    /// (Claude Code, Cursor) get the same optimization as OpenAI-wire clients.
    Gateway,
    /// Forward to upstream provider directly (logging only).
    Bypass,
    /// Same routing as Gateway — all endpoints (/v1/chat/completions,
    /// /v1/messages, /v1/models) → gateway — but the client's own credentials
    /// pass through (no TokenTrimmer key injection).
    Hybrid,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.to_lowercase().as_str() {
            "gateway" => Some(Mode::Gateway),
            "bypass" => Some(Mode::Bypass),
            "hybrid" => Some(Mode::Hybrid),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub mode: Mode,
    pub tt_api_key: Option<String>,
    pub gateway_base_url: String,
    pub upstream_anthropic: String,
    pub upstream_openai: String,
    pub session_log_dir: PathBuf,
    pub no_tui: bool,
    pub no_preview: bool,
}

impl Config {
    pub fn build(
        port: u16,
        bind: IpAddr,
        mode: Mode,
        tt_api_key: Option<String>,
        no_tui: bool,
        no_preview: bool,
        session_log_dir: Option<PathBuf>,
    ) -> Self {
        let default_log_dir = dirs::home_dir()
            .map(|h| h.join(".tokentrimmer").join("sessions"))
            .unwrap_or_else(|| PathBuf::from("./sessions"));
        Self {
            bind: SocketAddr::new(bind, port),
            mode,
            tt_api_key,
            gateway_base_url: "https://api.tokentrimmer.com".into(),
            upstream_anthropic: "https://api.anthropic.com".into(),
            upstream_openai: "https://api.openai.com".into(),
            session_log_dir: session_log_dir.unwrap_or(default_log_dir),
            no_tui,
            no_preview,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes() {
        assert_eq!(Mode::parse("gateway"), Some(Mode::Gateway));
        assert_eq!(Mode::parse("bypass"), Some(Mode::Bypass));
        assert_eq!(Mode::parse("HYBRID"), Some(Mode::Hybrid));
        assert_eq!(Mode::parse("nope"), None);
    }

    #[test]
    fn build_sets_defaults() {
        let cfg = Config::build(
            31415,
            "127.0.0.1".parse().unwrap(),
            Mode::Gateway,
            Some("k".into()),
            false,
            false,
            None,
        );
        assert_eq!(cfg.bind.port(), 31415);
        assert!(cfg.gateway_base_url.contains("tokentrimmer"));
    }
}
