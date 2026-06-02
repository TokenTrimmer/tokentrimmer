//! Layered configuration loader.
//!
//! Minimal env-var loader for the Gateway. Adds layered file (yaml/toml) and
//! CLI-flag merging in later iterations when those callers exist.
//!
//! # Reading
//!
//! ```no_run
//! use tt_config::Config;
//! let cfg = Config::from_env().expect("env vars valid");
//! ```
//!
//! # Conventions
//!
//! * Required vars return [`ConfigError::Missing`] when absent.
//! * Optional vars return `None` when absent and `Some(_)` when present.
//! * Parse failures (e.g. `PORT="abc"`) return [`ConfigError::Parse`].

use std::num::ParseIntError;

use thiserror::Error;

/// Top-level errors from config loading.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A required env var was not set.
    #[error("required environment variable not set: {0}")]
    Missing(&'static str),
    /// A value was set but could not be parsed into the expected type.
    #[error("invalid value for {var}: {reason}")]
    Parse {
        /// Name of the offending env var.
        var: &'static str,
        /// Human-readable reason.
        reason: String,
    },
}

impl ConfigError {
    fn parse_int(var: &'static str, e: ParseIntError) -> Self {
        Self::Parse {
            var,
            reason: e.to_string(),
        }
    }
}

/// Gateway runtime configuration.
///
/// All fields are populated by [`Config::from_env`]. Fields are intentionally
/// public so handlers can read them directly; adding a setter would just be
/// noise.
#[derive(Debug, Clone)]
pub struct Config {
    /// TCP port the gateway binds. Defaults to 8080.
    pub port: u16,
    /// Postgres connection string. `None` disables persistence at boot.
    pub database_url: Option<String>,
    /// Redis URL for the L1 exact-match cache. `None` disables L1.
    pub redis_url: Option<String>,
    /// Sentry DSN for error reporting. `None` disables Sentry init.
    pub sentry_dsn: Option<String>,
    /// Hex-encoded XChaCha20-Poly1305 root key for provider credentials at
    /// rest. Read by the auth/cred layer when it lands; surfaced here so the
    /// gateway fails fast at boot if production has it misconfigured.
    pub master_key: Option<String>,
}

impl Config {
    /// Load from env. Defaults `port` to 8080 when `PORT` is unset.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`] when `PORT` is set to a non-integer.
    pub fn from_env() -> Result<Self, ConfigError> {
        let port = match std::env::var("PORT") {
            Ok(v) => v
                .parse::<u16>()
                .map_err(|e| ConfigError::parse_int("PORT", e))?,
            Err(_) => 8080,
        };

        Ok(Self {
            port,
            database_url: opt("DATABASE_URL"),
            redis_url: opt("REDIS_URL"),
            sentry_dsn: opt("SENTRY_DSN"),
            master_key: opt("TT_MASTER_KEY"),
        })
    }
}

fn opt(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes the tests below that mutate the process-global `PORT` env
    /// var. Rust runs a crate's tests in one process in parallel, so without
    /// this they race — one clears `PORT` while the other sets it to a bad
    /// value, making `defaults_when_only_required_missing` see the bad value.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// `from_env` is allowed to be called in tests that don't set anything;
    /// must return defaults rather than error.
    #[test]
    fn defaults_when_only_required_missing() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // PORT may already be set by the test runner; clear it inside this
        // test only.
        let prev_port = std::env::var("PORT").ok();
        std::env::remove_var("PORT");

        let cfg = Config::from_env().expect("defaults must load");
        assert_eq!(cfg.port, 8080);

        if let Some(v) = prev_port {
            std::env::set_var("PORT", v);
        }
    }

    #[test]
    fn invalid_port_returns_parse_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("PORT", "not-a-number");
        let err = Config::from_env().expect_err("invalid port must fail");
        match err {
            ConfigError::Parse { var, .. } => assert_eq!(var, "PORT"),
            other => panic!("expected Parse, got {other:?}"),
        }
        std::env::remove_var("PORT");
    }
}
