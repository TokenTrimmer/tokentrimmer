//! Runtime config for tt proxy. Mostly comes from CLI args; a few values
//! (default session log path) resolve from ~/.tokentrimmer/.
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use clap::{builder::PossibleValue, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Gateway,
    Bypass,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardTarget {
    Gateway,
    Provider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialPolicy {
    TokenTrimmer,
    ClientProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeContract {
    pub name: &'static str,
    pub target: ForwardTarget,
    pub credential_policy: CredentialPolicy,
    pub requires_loopback_gateway: bool,
    pub help: &'static str,
}

const MODES: &[Mode] = &[Mode::Gateway, Mode::Bypass, Mode::Hybrid];

impl Mode {
    pub const fn contract(self) -> ModeContract {
        match self {
            Self::Gateway => ModeContract {
                name: "gateway",
                target: ForwardTarget::Gateway,
                credential_policy: CredentialPolicy::TokenTrimmer,
                requires_loopback_gateway: false,
                help: "hosted TokenTrimmer gateway; strips client provider credentials and injects the configured TokenTrimmer key",
            },
            Self::Bypass => ModeContract {
                name: "bypass",
                target: ForwardTarget::Provider,
                credential_policy: CredentialPolicy::ClientProvider,
                requires_loopback_gateway: false,
                help: "provider API directly; preserves the client's provider credential; logging only, without gateway routing, cache, or budget controls",
            },
            Self::Hybrid => ModeContract {
                name: "hybrid",
                target: ForwardTarget::Gateway,
                credential_policy: CredentialPolicy::ClientProvider,
                requires_loopback_gateway: true,
                help: "loopback self-hosted TokenTrimmer gateway; preserves the client's provider credential for BYOK and rejects remote targets",
            },
        }
    }
}

impl ValueEnum for Mode {
    fn value_variants<'a>() -> &'a [Self] {
        MODES
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        let contract = self.contract();
        Some(PossibleValue::new(contract.name).help(contract.help))
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.contract().name)
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

    /// Enforce the proxy's identity boundary before opening its listener.
    ///
    /// Hybrid mode deliberately carries the client's provider credential to a
    /// self-hosted BYOK gateway. Restricting that target to loopback prevents a
    /// configuration mistake from sending anonymous provider credentials to the
    /// hosted multi-tenant gateway (or any other remote host).
    pub fn validate_identity_boundary(&self) -> Result<(), String> {
        if !self.mode.contract().requires_loopback_gateway {
            return Ok(());
        }

        const MESSAGE: &str = "hybrid mode preserves the client's provider credential and \
            therefore requires a loopback --tt-api-base (for example \
            http://127.0.0.1:8080); use gateway mode with a tt_live_* key for the hosted service";

        let url = reqwest::Url::parse(&self.gateway_base_url).map_err(|_| MESSAGE.to_string())?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(MESSAGE.to_string());
        }
        let host = url.host_str().ok_or_else(|| MESSAGE.to_string())?;
        // `Url::host_str` retains brackets around IPv6 literals.
        let address_literal = host
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(host);
        let is_loopback = host.eq_ignore_ascii_case("localhost")
            || address_literal
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if !is_loopback {
            return Err(MESSAGE.to_string());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_values_and_runtime_contract_share_one_source() {
        for mode in MODES {
            let contract = mode.contract();
            assert_eq!(Mode::from_str(contract.name, true), Ok(*mode));
            let possible = mode.to_possible_value().expect("mode must be printable");
            assert_eq!(possible.get_name(), contract.name);
            assert_eq!(
                possible.get_help().map(|help| help.to_string()),
                Some(contract.help.to_string())
            );
        }
        assert_eq!(Mode::from_str("HYBRID", true), Ok(Mode::Hybrid));
        assert!(Mode::from_str("nope", true).is_err());

        assert_eq!(
            Mode::Gateway.contract().credential_policy,
            CredentialPolicy::TokenTrimmer
        );
        assert_eq!(Mode::Bypass.contract().target, ForwardTarget::Provider);
        assert!(Mode::Hybrid.contract().requires_loopback_gateway);
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

    #[test]
    fn hybrid_accepts_only_loopback_gateway_urls() {
        let mut cfg = Config::build(
            31415,
            "127.0.0.1".parse().unwrap(),
            Mode::Hybrid,
            None,
            false,
            false,
            None,
        );

        for url in [
            "http://127.0.0.1:8080",
            "https://localhost:8443",
            "http://[::1]:8080",
        ] {
            cfg.gateway_base_url = url.into();
            assert!(
                cfg.validate_identity_boundary().is_ok(),
                "{url} must be accepted"
            );
        }

        for url in [
            "https://api.tokentrimmer.com",
            "http://192.168.1.10:8080",
            "ftp://127.0.0.1",
            "not-a-url",
        ] {
            cfg.gateway_base_url = url.into();
            let error = cfg
                .validate_identity_boundary()
                .expect_err("remote or malformed hybrid target must fail closed");
            assert!(error.contains("requires a loopback --tt-api-base"));
        }
    }
}
