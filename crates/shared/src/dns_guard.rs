//! Connect-time SSRF guard: a reqwest custom DNS resolver that drops any
//! resolved address in a blocked (private / loopback / link-local / CGNAT /
//! cloud-metadata / IPv6-ULA) range.
//!
//! # Why a resolver, not just URL validation
//!
//! [`crate::url_guard::validate_provider_url`] checks a customer-supplied
//! `base_url` at *validation* time by resolving it once. reqwest then resolves
//! the host **again, independently** at *connect* time, so a malicious DNS
//! server can return a public address for the validation probe and a private
//! address (e.g. `169.254.169.254`, an RFC-1918 host, or a Fly `*.internal`
//! ULA) for the real connection — the classic DNS-rebinding TOCTOU described in
//! the [`crate::url_guard`] module docs.
//!
//! Installing [`GuardedResolver`] as reqwest's DNS resolver closes that gap:
//! the SAME resolution reqwest connects to is filtered through
//! [`crate::url_guard`]'s `is_blocked_ip`, so the connection can never target a
//! blocked address regardless of what the DNS server returns. This is
//! connect-time enforcement; keep the validation-time check too (defense in
//! depth).
//!
//! Use [`with_guarded_dns`] to install the resolver on a
//! [`reqwest::ClientBuilder`].

use std::net::SocketAddr;
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use thiserror::Error;
use tracing::warn;

use crate::url_guard::is_blocked_ip;

/// Boxed error type used by reqwest's [`Resolve`] trait.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Error returned when DNS resolution yields no permitted address.
#[derive(Debug, Error)]
pub enum GuardedResolveError {
    /// Every resolved address for the host fell in a blocked range, so the
    /// request is refused rather than connecting to a private/internal target.
    #[error(
        "SSRF guard: all resolved addresses for '{host}' are in a blocked \
         (private/loopback/link-local/metadata) range"
    )]
    AllBlocked {
        /// The host whose addresses were all blocked.
        host: String,
    },
}

/// A reqwest [`Resolve`] implementation that filters out blocked IPs.
///
/// Resolves `name` via the system resolver, then removes every [`SocketAddr`]
/// whose IP is rejected by [`crate::url_guard`]'s `is_blocked_ip`. If no address
/// survives, resolution fails with [`GuardedResolveError::AllBlocked`] so the
/// HTTP request never connects to a blocked target — even when a malicious DNS
/// server rebinds the host between validation and connect time.
#[derive(Debug, Clone, Copy, Default)]
pub struct GuardedResolver;

impl GuardedResolver {
    /// Construct a new guarded resolver.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Resolve via the system resolver (getaddrinfo). The port is a
            // placeholder — reqwest overrides it with the URL's port — but
            // `lookup_host` requires a `host:port` target.
            let resolved = tokio::net::lookup_host((host.as_str(), 0_u16))
                .await
                .map_err(|e| Box::new(e) as BoxError)?;

            let (allowed, blocked) = partition_addrs(resolved);

            if allowed.is_empty() {
                warn!(
                    host = %host,
                    blocked,
                    "GuardedResolver: refusing connection — all resolved addresses are in a \
                     blocked range (possible DNS rebind / SSRF)"
                );
                return Err(Box::new(GuardedResolveError::AllBlocked { host }) as BoxError);
            }

            if blocked > 0 {
                warn!(
                    host = %host,
                    blocked,
                    allowed = allowed.len(),
                    "GuardedResolver: dropped blocked resolved address(es); proceeding with the \
                     allowed set only"
                );
            }

            let addrs: Addrs = Box::new(allowed.into_iter());
            Ok(addrs)
        })
    }
}

/// Partition resolved socket addresses into `(allowed, blocked_count)`.
///
/// `allowed` retains the original resolution order; `blocked_count` is the
/// number of addresses dropped because [`is_blocked_ip`] returned `true`.
fn partition_addrs(addrs: impl Iterator<Item = SocketAddr>) -> (Vec<SocketAddr>, usize) {
    let mut allowed = Vec::new();
    let mut blocked = 0usize;
    for sa in addrs {
        if is_blocked_ip(sa.ip()) {
            blocked += 1;
        } else {
            allowed.push(sa);
        }
    }
    (allowed, blocked)
}

/// Install [`GuardedResolver`] as the DNS resolver on a reqwest
/// [`reqwest::ClientBuilder`].
///
/// Call this when building any HTTP client that may connect to a
/// customer-supplied base URL, so connect-time resolution is filtered through
/// the SSRF guard (defeating DNS rebinding).
///
/// Do **not** use it for clients that must reach `localhost`/private endpoints
/// (the `local` provider targeting Ollama/vLLM/LM Studio): those addresses are
/// intentionally blocked here, exactly like [`crate::url_guard`]'s
/// validation-time check when `allow_local` is false.
pub fn with_guarded_dns(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder.dns_resolver(Arc::new(GuardedResolver::new()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn sa(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse::<IpAddr>().unwrap(), 443)
    }

    #[test]
    fn partition_drops_blocked_keeps_allowed_in_order() {
        let addrs = vec![
            sa("169.254.169.254"), // cloud metadata — blocked
            sa("1.1.1.1"),         // public — allowed
            sa("10.0.0.5"),        // RFC-1918 — blocked
            sa("8.8.8.8"),         // public — allowed
        ];
        let (allowed, blocked) = partition_addrs(addrs.into_iter());
        assert_eq!(blocked, 2);
        assert_eq!(allowed, vec![sa("1.1.1.1"), sa("8.8.8.8")]);
    }

    #[test]
    fn partition_all_blocked_yields_empty() {
        let addrs = vec![sa("127.0.0.1"), sa("192.168.1.1"), sa("172.16.0.1")];
        let (allowed, blocked) = partition_addrs(addrs.into_iter());
        assert!(allowed.is_empty());
        assert_eq!(blocked, 3);
    }

    #[test]
    fn partition_all_allowed_keeps_all() {
        let addrs = vec![sa("1.1.1.1"), sa("8.8.8.8")];
        let (allowed, blocked) = partition_addrs(addrs.into_iter());
        assert_eq!(blocked, 0);
        assert_eq!(allowed.len(), 2);
    }

    #[test]
    fn is_blocked_ip_covers_required_ranges() {
        // 169.254.0.0/16 link-local (incl. AWS/GCP metadata 169.254.169.254)
        assert!(is_blocked_ip("169.254.169.254".parse().unwrap()));
        assert!(is_blocked_ip("169.254.0.1".parse().unwrap()));
        // 10.0.0.0/8
        assert!(is_blocked_ip("10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("10.255.255.255".parse().unwrap()));
        // 172.16.0.0/12 (and not just outside)
        assert!(is_blocked_ip("172.16.0.1".parse().unwrap()));
        assert!(is_blocked_ip("172.31.255.255".parse().unwrap()));
        assert!(!is_blocked_ip("172.32.0.1".parse().unwrap()));
        // 192.168.0.0/16
        assert!(is_blocked_ip("192.168.0.1".parse().unwrap()));
        // ::1 loopback
        assert!(is_blocked_ip("::1".parse().unwrap()));
        // fc00::/7 ULA — Fly `*.internal` lives in fdaa::/16 ⊂ fc00::/7
        assert!(is_blocked_ip("fc00::1".parse().unwrap()));
        assert!(is_blocked_ip("fdaa:0:1::3".parse().unwrap()));
        // public addresses pass
        assert!(!is_blocked_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_blocked_ip("2001:4860:4860::8888".parse().unwrap()));
    }

    // The following resolver tests use literal IP "hostnames". `lookup_host`
    // resolves an IP literal to itself with no external DNS query, so these are
    // deterministic and network-free. A test that a public *hostname* resolves
    // through would require live DNS and is intentionally omitted (the
    // allowed-path filtering is fully covered by `partition_*` above).

    #[tokio::test]
    async fn resolve_refuses_cloud_metadata_ip() {
        let name: Name = "169.254.169.254".parse().unwrap();
        let res = GuardedResolver::new().resolve(name).await;
        assert!(res.is_err(), "metadata IP must be refused at connect time");
    }

    #[tokio::test]
    async fn resolve_refuses_loopback_ip() {
        let name: Name = "127.0.0.1".parse().unwrap();
        assert!(GuardedResolver::new().resolve(name).await.is_err());
    }

    #[tokio::test]
    async fn resolve_refuses_rfc1918_ip() {
        let name: Name = "10.0.0.1".parse().unwrap();
        assert!(GuardedResolver::new().resolve(name).await.is_err());
    }

    #[tokio::test]
    async fn resolve_allows_public_ip() {
        let name: Name = "1.1.1.1".parse().unwrap();
        let addrs = GuardedResolver::new()
            .resolve(name)
            .await
            .expect("public IP must resolve");
        let collected: Vec<SocketAddr> = addrs.collect();
        assert!(
            collected
                .iter()
                .any(|sa| sa.ip() == "1.1.1.1".parse::<IpAddr>().unwrap()),
            "expected 1.1.1.1 in resolved set, got {collected:?}"
        );
    }
}
