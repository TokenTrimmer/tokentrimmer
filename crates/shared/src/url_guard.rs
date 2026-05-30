//! SSRF and header-injection guard for customer-supplied provider URLs.
//!
//! ## URL guard
//!
//! [`validate_provider_url`] checks a customer-controlled `base_url` before any
//! HTTP request is dispatched. It enforces:
//!
//! - Scheme must be `https` (or `http` when `allow_local` is true).
//! - The host must not be a private/loopback/link-local/ULA IP address, the
//!   cloud-metadata addresses (169.254.169.254, 100.100.100.200), or the
//!   hostname `localhost`, `*.local`, or `metadata.google.internal`.
//! - When `allow_local` is `true` (self-hosted / `local` provider), all of the
//!   above checks are bypassed — only basic URL parsing is performed.
//! - A best-effort DNS resolution step rejects the URL if **any** resolved
//!   address falls in a private range.
//!
//! ## DNS rebind caveat
//!
//! The DNS resolution step is defense-in-depth only. It is subject to TOCTOU
//! races: a malicious DNS server can return a safe address for the validation
//! call and a private address for the actual HTTP connection (classic
//! DNS-rebinding attack). The connect-time enforcement (binding to a local
//! policy agent or using a kernel eBPF hook) is out of scope here. Operators
//! concerned about DNS rebinding should additionally run the gateway behind a
//! network policy that blocks outbound connections to RFC-1918 ranges.
//!
//! ## Header filter
//!
//! [`filter_extra_headers`] strips any header whose name could override the
//! adapter-set auth (`authorization`, `x-api-key`, `anthropic-version`,
//! `content-type`) or the routing (`host`) headers, or inject hop-by-hop
//! headers that must not be forwarded to upstream HTTP/1.1 servers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use thiserror::Error;
use tracing::warn;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq, Eq)]
pub enum UrlGuardError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("insecure scheme '{0}': only https is allowed for hosted providers")]
    InsecureScheme(String),

    #[error("blocked host '{0}': private/loopback/link-local addresses are not allowed")]
    BlockedHost(String),

    #[error("blocked hostname '{0}': internal/metadata hostnames are not allowed")]
    BlockedHostname(String),

    #[error("resolved address for '{host}' is in a blocked range: {addr}")]
    BlockedResolvedAddress { host: String, addr: IpAddr },
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Validate a customer-supplied provider base URL.
///
/// # Arguments
///
/// * `raw` — The raw URL string from customer credentials.
/// * `allow_local` — When `true`, skip scheme and private-range checks
///   (for the `local` provider that targets Ollama/vLLM/LM Studio).
///
/// # Errors
///
/// Returns [`UrlGuardError`] if the URL is malformed, uses an insecure scheme
/// (when `allow_local` is false), or points to a private/internal host.
pub fn validate_provider_url(raw: &str, allow_local: bool) -> Result<(), UrlGuardError> {
    // Parse the URL.
    let parsed = url::Url::parse(raw)
        .map_err(|e| UrlGuardError::InvalidUrl(format!("{raw}: {e}")))?;

    // Extract scheme.
    let scheme = parsed.scheme();

    if allow_local {
        // Local provider: accept http or https, no range checks.
        if scheme != "http" && scheme != "https" {
            return Err(UrlGuardError::InsecureScheme(scheme.to_string()));
        }
        return Ok(());
    }

    // Hosted providers: require https.
    if scheme != "https" {
        return Err(UrlGuardError::InsecureScheme(scheme.to_string()));
    }

    // Extract the host string.
    let host_str = parsed
        .host_str()
        .ok_or_else(|| UrlGuardError::InvalidUrl(format!("{raw}: no host")))?;

    // Check hostname denylist first.
    check_hostname_denylist(host_str)?;

    // If the host is a literal IP, check the range.
    // The `url` crate returns IPv6 addresses without brackets in host_str(),
    // but strip brackets defensively in case that behaviour changes.
    let bare_host = if host_str.starts_with('[') && host_str.ends_with(']') {
        &host_str[1..host_str.len() - 1]
    } else {
        host_str
    };

    if let Ok(ip) = bare_host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(UrlGuardError::BlockedHost(host_str.to_string()));
        }
        // Literal IP is safe — skip DNS resolution.
        return Ok(());
    }

    // Best-effort DNS resolution to catch SSRF via hostname.
    // TOCTOU/DNS-rebind note: see module-level doc comment.
    let port = parsed.port_or_known_default().unwrap_or(443);
    let lookup_target = format!("{host_str}:{port}");
    match lookup_target.to_socket_addrs() {
        Ok(addrs) => {
            for sa in addrs {
                let ip = sa.ip();
                if is_blocked_ip(ip) {
                    warn!(
                        host = %host_str,
                        addr = %ip,
                        "validate_provider_url: resolved address is in a blocked range"
                    );
                    return Err(UrlGuardError::BlockedResolvedAddress {
                        host: host_str.to_string(),
                        addr: ip,
                    });
                }
            }
        }
        Err(e) => {
            // DNS failure is not treated as a security block — the upstream
            // request will fail with a network error, which is fine.
            warn!(
                host = %host_str,
                error = %e,
                "validate_provider_url: DNS resolution failed (allowed to proceed)"
            );
        }
    }

    Ok(())
}

/// Return `true` if the IP address is in a blocked range.
///
/// Blocked ranges:
/// - IPv4 loopback 127.0.0.0/8
/// - IPv4 link-local 169.254.0.0/16
/// - IPv4 private 10.0.0.0/8
/// - IPv4 private 172.16.0.0/12
/// - IPv4 private 192.168.0.0/16
/// - IPv4 shared-address 100.64.0.0/10
/// - IPv4 cloud-metadata 169.254.169.254 and 100.100.100.200 (covered by above)
/// - IPv6 loopback ::1
/// - IPv6 link-local fe80::/10
/// - IPv6 ULA fc00::/7
/// - IPv6 mapped / compatible IPv4 in blocked ranges
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();

    // 127.0.0.0/8 — loopback
    if o[0] == 127 {
        return true;
    }
    // 10.0.0.0/8 — private
    if o[0] == 10 {
        return true;
    }
    // 172.16.0.0/12 — private
    if o[0] == 172 && (o[1] & 0xf0) == 16 {
        return true;
    }
    // 192.168.0.0/16 — private
    if o[0] == 192 && o[1] == 168 {
        return true;
    }
    // 169.254.0.0/16 — link-local (includes AWS/GCP metadata 169.254.169.254)
    if o[0] == 169 && o[1] == 254 {
        return true;
    }
    // 100.64.0.0/10 — CGNAT / shared address space (includes Alibaba 100.100.100.200)
    if o[0] == 100 && (o[1] & 0xc0) == 64 {
        return true;
    }
    // 0.0.0.0/8 — "this" network
    if o[0] == 0 {
        return true;
    }
    false
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();

    // ::1 — loopback
    if ip == Ipv6Addr::LOCALHOST {
        return true;
    }
    // fe80::/10 — link-local
    if (seg[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // fc00::/7 — ULA (unique local addresses)
    if (seg[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // ::ffff:0:0/96 — IPv4-mapped: check the embedded v4 address
    if seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0xffff {
        let v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            (seg[6] & 0xff) as u8,
            (seg[7] >> 8) as u8,
            (seg[7] & 0xff) as u8,
        );
        return is_blocked_v4(v4);
    }
    // 64:ff9b::/96 — IPv4/IPv6 translation (RFC 6052)
    if seg[0] == 0x0064 && seg[1] == 0xff9b && seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0 {
        let v4 = Ipv4Addr::new(
            (seg[6] >> 8) as u8,
            (seg[6] & 0xff) as u8,
            (seg[7] >> 8) as u8,
            (seg[7] & 0xff) as u8,
        );
        return is_blocked_v4(v4);
    }
    false
}

/// Check hostname-based denylist (not IP-literal hosts).
fn check_hostname_denylist(host: &str) -> Result<(), UrlGuardError> {
    let lower = host.to_ascii_lowercase();

    if lower == "localhost" {
        return Err(UrlGuardError::BlockedHostname(host.to_string()));
    }
    if lower.ends_with(".local") || lower == "local" {
        return Err(UrlGuardError::BlockedHostname(host.to_string()));
    }
    if lower == "metadata.google.internal" {
        return Err(UrlGuardError::BlockedHostname(host.to_string()));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Header filter
// ---------------------------------------------------------------------------

/// Header names that customers must not override via `extra_headers`.
///
/// - `authorization` — carries the API key set by the adapter
/// - `x-api-key`     — Anthropic's auth header
/// - `host`          — routing; overridable by reqwest/hyper but dangerous
/// - `content-type`  — set correctly by the adapter
/// - `anthropic-version` — must not be overridden
/// - Hop-by-hop headers per RFC 7230 §6.1 that must not be forwarded
const DENIED_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "host",
    "content-type",
    "anthropic-version",
    // Hop-by-hop (RFC 7230 §6.1)
    "connection",
    "proxy-authorization",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "keep-alive",
    "proxy-connection",
];

/// Filter `extra_headers`, dropping any header whose name is in the denylist.
///
/// Matching is case-insensitive. Returns a new `Vec` containing only the
/// allowed headers. A `warn!` log line is emitted for each dropped header.
pub fn filter_extra_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            if DENIED_HEADERS.contains(&lower.as_str()) {
                warn!(
                    header = %name,
                    "extra_headers: dropping denied header (authorization/host/hop-by-hop)"
                );
                None
            } else {
                Some((name.clone(), value.clone()))
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- validate_provider_url --

    #[test]
    fn accepts_normal_https_url() {
        assert!(validate_provider_url("https://api.openai.com/v1", false).is_ok());
        assert!(validate_provider_url("https://api.anthropic.com", false).is_ok());
        assert!(validate_provider_url("https://api.together.xyz/v1", false).is_ok());
    }

    #[test]
    fn rejects_http_for_non_local() {
        let err = validate_provider_url("http://api.example.com/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::InsecureScheme(_)));
    }

    #[test]
    fn allows_http_when_allow_local() {
        assert!(validate_provider_url("http://localhost:11434/v1", true).is_ok());
        assert!(validate_provider_url("http://127.0.0.1:8000/v1", true).is_ok());
    }

    #[test]
    fn rejects_cloud_metadata_ip() {
        let err = validate_provider_url("https://169.254.169.254/latest/meta-data/", false)
            .unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_alibaba_metadata_ip() {
        let err = validate_provider_url("https://100.100.100.200/meta-data/", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_loopback_ipv4() {
        let err =
            validate_provider_url("https://127.0.0.1/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_private_10_x() {
        let err = validate_provider_url("https://10.0.0.1/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_private_192_168() {
        let err = validate_provider_url("https://192.168.1.1/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_private_172_16() {
        let err = validate_provider_url("https://172.16.0.1/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_loopback_ipv6() {
        let err = validate_provider_url("https://[::1]/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_ula_ipv6_fc00() {
        let err = validate_provider_url("https://[fc00::1]/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_link_local_ipv6_fe80() {
        let err = validate_provider_url("https://[fe80::1]/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn rejects_localhost_hostname() {
        let err = validate_provider_url("https://localhost/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHostname(_)), "got: {err}");
    }

    #[test]
    fn rejects_dot_local_hostname() {
        let err = validate_provider_url("https://myhost.local/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHostname(_)), "got: {err}");
    }

    #[test]
    fn rejects_metadata_google_internal() {
        let err =
            validate_provider_url("https://metadata.google.internal/computeMetadata/v1/", false)
                .unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHostname(_)), "got: {err}");
    }

    #[test]
    fn allows_localhost_when_allow_local() {
        assert!(validate_provider_url("http://localhost:11434/v1", true).is_ok());
        assert!(validate_provider_url("https://localhost:11434/v1", true).is_ok());
    }

    #[test]
    fn rejects_invalid_url() {
        let err = validate_provider_url("not-a-url", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::InvalidUrl(_)), "got: {err}");
    }

    #[test]
    fn rejects_ftp_scheme() {
        let err = validate_provider_url("ftp://example.com/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::InsecureScheme(_)), "got: {err}");
    }

    // -- filter_extra_headers --

    #[test]
    fn drops_authorization_header() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer fake".to_string()),
            ("X-Custom".to_string(), "value".to_string()),
        ];
        let filtered = filter_extra_headers(&headers);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "X-Custom");
    }

    #[test]
    fn drops_host_header() {
        let headers = vec![
            ("Host".to_string(), "evil.internal".to_string()),
            ("X-Org-ID".to_string(), "abc".to_string()),
        ];
        let filtered = filter_extra_headers(&headers);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "X-Org-ID");
    }

    #[test]
    fn drops_hop_by_hop_headers() {
        let headers = vec![
            ("Connection".to_string(), "close".to_string()),
            ("Proxy-Authorization".to_string(), "Basic xyz".to_string()),
            ("Transfer-Encoding".to_string(), "chunked".to_string()),
            ("Keep-Alive".to_string(), "timeout=5".to_string()),
            ("X-Real-Header".to_string(), "ok".to_string()),
        ];
        let filtered = filter_extra_headers(&headers);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "X-Real-Header");
    }

    #[test]
    fn keeps_legitimate_extra_headers() {
        let headers = vec![
            ("X-Custom-Header".to_string(), "custom-value".to_string()),
            ("X-Org-Id".to_string(), "org-123".to_string()),
            ("Accept-Language".to_string(), "en".to_string()),
        ];
        let filtered = filter_extra_headers(&headers);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn filter_is_case_insensitive() {
        let headers = vec![
            ("AUTHORIZATION".to_string(), "Bearer x".to_string()),
            ("authorization".to_string(), "Bearer y".to_string()),
            ("Authorization".to_string(), "Bearer z".to_string()),
            ("x-api-key".to_string(), "sk-...".to_string()),
            ("X-API-KEY".to_string(), "sk-...".to_string()),
        ];
        let filtered = filter_extra_headers(&headers);
        assert!(filtered.is_empty(), "all auth headers should be dropped, got: {filtered:?}");
    }

    #[test]
    fn cgnat_range_is_blocked() {
        // 100.64.0.0/10 — CGNAT range that includes Alibaba metadata 100.100.100.200
        let err = validate_provider_url("https://100.64.0.1/v1", false).unwrap_err();
        assert!(matches!(err, UrlGuardError::BlockedHost(_)), "got: {err}");
    }

    #[test]
    fn is_blocked_v4_spot_checks() {
        assert!(is_blocked_v4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(is_blocked_v4(Ipv4Addr::new(169, 254, 169, 254)));
        assert!(is_blocked_v4(Ipv4Addr::new(100, 100, 100, 200)));
        assert!(is_blocked_v4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_blocked_v4(Ipv4Addr::new(192, 168, 0, 1)));
        assert!(is_blocked_v4(Ipv4Addr::new(172, 16, 0, 1)));
        assert!(is_blocked_v4(Ipv4Addr::new(172, 31, 255, 255)));
        assert!(!is_blocked_v4(Ipv4Addr::new(1, 1, 1, 1)));
        assert!(!is_blocked_v4(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(!is_blocked_v4(Ipv4Addr::new(172, 32, 0, 1))); // just outside 172.16/12
    }

    #[test]
    fn ipv6_mapped_v4_blocked() {
        // ::ffff:127.0.0.1 is the IPv4-mapped loopback
        let ip: Ipv6Addr = "::ffff:127.0.0.1".parse().unwrap();
        assert!(is_blocked_v6(ip));
    }

    #[test]
    fn public_ipv6_not_blocked() {
        let ip: Ipv6Addr = "2001:4860:4860::8888".parse().unwrap(); // Google DNS
        assert!(!is_blocked_v6(ip));
    }
}
