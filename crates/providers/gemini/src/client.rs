//! HTTP client configuration for the Gemini adapter.
//!
//! The client has a connection timeout but intentionally does **not** configure
//! retries — that responsibility belongs to the `tt-core` retry/backoff layer.
//! Adapters are retry-unaware by design.

use std::time::Duration;

use reqwest::Client;

/// Configuration supplied to [`build_client`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Total request timeout (connect + read). Defaults to 120 s.
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

/// Build a [`reqwest::Client`] with the given configuration.
///
/// Uses rustls (no native TLS) and enables gzip decompression. The client is
/// intended to be created once per [`crate::GeminiProvider`] and reused
/// across all requests.
pub fn build_client(cfg: &ClientConfig) -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(cfg.timeout)
        .connect_timeout(cfg.connect_timeout)
        // Disable automatic redirects — the provider API should not redirect.
        .redirect(reqwest::redirect::Policy::none())
        // Use rustls (consistent with workspace default features).
        .use_rustls_tls()
        .gzip(true)
        .build()
}
