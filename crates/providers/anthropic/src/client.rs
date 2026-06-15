//! HTTP client configuration for the Anthropic adapter.
//!
//! The client has a connection timeout but intentionally does **not** configure
//! retries — that responsibility belongs to the `tt-core` retry/backoff layer.
//! Adapters are retry-unaware by design.

use std::time::Duration;

use reqwest::Client;

/// Configuration supplied to [`build_client`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Per-read idle timeout: resets on each received chunk, so a long
    /// streaming response is not cut off, while a stalled connection still
    /// times out. (Not a total-request cap.) Defaults to 120 s.
    pub timeout: Duration,
    /// TCP connection timeout. Defaults to 10 s.
    pub connect_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// Shared base [`reqwest::ClientBuilder`]: timeouts, no redirects, rustls, gzip.
fn base_builder(cfg: &ClientConfig) -> reqwest::ClientBuilder {
    Client::builder()
        .read_timeout(cfg.timeout)
        .connect_timeout(cfg.connect_timeout)
        // Disable automatic redirects — the provider API should not redirect.
        .redirect(reqwest::redirect::Policy::none())
        // Use rustls (consistent with workspace default features).
        .use_rustls_tls()
        .gzip(true)
}

/// Build a [`reqwest::Client`] with the given configuration, with the
/// connect-time SSRF guard ([`tt_shared::GuardedResolver`]) installed.
///
/// Uses rustls (no native TLS) and enables gzip decompression. The client is
/// intended to be created once per [`crate::AnthropicProvider`] and reused
/// across all requests.
///
/// The DNS resolver filters out private/loopback/link-local/metadata addresses
/// at connect time, so a customer-supplied `base_url` host can never be used to
/// reach an internal address even if it rebinds after the validation-time
/// `validate_provider_url` check (DNS-rebind TOCTOU). Use
/// [`build_unguarded_client`] only for `allow_local` paths.
pub fn build_client(cfg: &ClientConfig) -> Result<Client, reqwest::Error> {
    tt_shared::with_guarded_dns(base_builder(cfg)).build()
}

/// Build a [`reqwest::Client`] **without** the connect-time SSRF guard.
///
/// Intended only for `allow_local` (tests against a local mock server), which
/// the guard would otherwise block.
pub fn build_unguarded_client(cfg: &ClientConfig) -> Result<Client, reqwest::Error> {
    base_builder(cfg).build()
}
