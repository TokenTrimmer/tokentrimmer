use std::{
    collections::BTreeSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::agent_policy::{NetworkDestination, NetworkScheme};

use super::{sanitize_terminal, BrokerError, BrokerState};

const MAX_REDIRECTS: u8 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NetworkFetchEvidence {
    pub sequence: u32,
    pub origin: String,
    pub url_sha256: String,
    pub resolved_addresses: Vec<String>,
    pub redirects: u8,
    pub status: u16,
    pub response_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FetchUrlArgs {
    url: String,
}

#[derive(Debug, Serialize)]
struct FetchUrlOutput {
    status: u16,
    content_type: Option<String>,
    body: String,
    redirects: u8,
}

impl BrokerState {
    pub(super) async fn fetch_url(&mut self, args: FetchUrlArgs) -> Result<String, BrokerError> {
        self.require_live()?;
        let initial_url = reqwest::Url::parse(&args.url).map_err(|error| {
            BrokerError::policy("fetch_url.url", format!("invalid URL: {error}"))
        })?;
        let initial_hash = hex::encode(Sha256::digest(initial_url.as_str().as_bytes()));
        let initial_origin = safe_origin(&initial_url)?;
        let mut current = initial_url;
        let mut redirects = 0u8;
        let mut all_addresses = BTreeSet::new();

        loop {
            let destination =
                authorize_url(&current, &self.policy.policy.network.allowed_destinations)?;
            let addresses =
                resolve_once(&current, &destination, self.remaining_wall_time()?).await?;
            all_addresses.extend(addresses.iter().map(ToString::to_string));
            let timeout = self.remaining_wall_time()?;
            let host = current
                .host_str()
                .ok_or_else(|| BrokerError::policy("fetch_url.url", "URL host is required"))?;
            let client = reqwest::Client::builder()
                .connect_timeout(timeout.min(Duration::from_secs(10)))
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .resolve_to_addrs(host, &addresses)
                .user_agent(concat!("tt-local-tool-broker/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(BrokerError::network)?;
            let response = client
                .get(current.clone())
                .send()
                .await
                .map_err(BrokerError::network)?;

            if response.status().is_redirection() {
                if !self.policy.policy.network.allow_redirects {
                    return Err(BrokerError::policy(
                        "network.allow_redirects",
                        format!(
                            "redirect response {} was refused",
                            response.status().as_u16()
                        ),
                    ));
                }
                if redirects >= MAX_REDIRECTS {
                    return Err(BrokerError::policy(
                        "network.allow_redirects",
                        format!("redirect count exceeded {MAX_REDIRECTS}"),
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        BrokerError::policy(
                            "network.allow_redirects",
                            "redirect response omitted a valid Location header",
                        )
                    })?;
                current = current.join(location).map_err(|error| {
                    BrokerError::policy(
                        "network.allow_redirects",
                        format!("invalid redirect target: {error}"),
                    )
                })?;
                redirects += 1;
                continue;
            }

            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let remaining_output = self
                .policy
                .policy
                .process
                .max_output_bytes
                .saturating_sub(self.output_bytes);
            if response
                .content_length()
                .is_some_and(|length| length > remaining_output)
            {
                return Err(BrokerError::policy(
                    "process.max_output_bytes",
                    format!(
                        "HTTP response exceeds remaining {remaining_output}-byte output ceiling"
                    ),
                ));
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(BrokerError::network)?;
                if body.len() as u64 + chunk.len() as u64 > remaining_output {
                    return Err(BrokerError::policy(
                        "process.max_output_bytes",
                        format!("HTTP response exceeds remaining {remaining_output}-byte output ceiling"),
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            let body_text = String::from_utf8(body).map_err(|_| {
                BrokerError::policy("fetch_url", "binary HTTP response bodies are refused")
            })?;
            let response_bytes = body_text.len() as u64;
            self.output_bytes = self.output_bytes.saturating_add(response_bytes);
            self.network_evidence.push(NetworkFetchEvidence {
                sequence: self.network_evidence.len() as u32 + 1,
                origin: initial_origin,
                url_sha256: initial_hash,
                resolved_addresses: all_addresses.into_iter().collect(),
                redirects,
                status,
                response_bytes,
            });
            return serde_json::to_string(&FetchUrlOutput {
                status,
                content_type,
                body: sanitize_terminal(body_text.as_bytes()),
                redirects,
            })
            .map_err(BrokerError::serialize);
        }
    }
}

fn authorize_url(
    url: &reqwest::Url,
    allowed: &[NetworkDestination],
) -> Result<NetworkDestination, BrokerError> {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(BrokerError::policy(
            "fetch_url.url",
            "URL userinfo and fragments are refused",
        ));
    }
    let scheme = match url.scheme() {
        "http" => NetworkScheme::Http,
        "https" => NetworkScheme::Https,
        _ => {
            return Err(BrokerError::policy(
                "fetch_url.url",
                "only HTTP and HTTPS are supported",
            ))
        }
    };
    let host = url
        .host_str()
        .ok_or_else(|| BrokerError::policy("fetch_url.url", "URL host is required"))?
        .to_ascii_lowercase();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| BrokerError::policy("fetch_url.url", "URL port is required"))?;
    let destination = NetworkDestination { scheme, host, port };
    if !allowed.contains(&destination) {
        return Err(BrokerError::policy(
            "network.allowed_destinations",
            format!("destination {destination:?} is not authorized"),
        ));
    }
    Ok(destination)
}

async fn resolve_once(
    url: &reqwest::Url,
    destination: &NetworkDestination,
    timeout: Duration,
) -> Result<Vec<SocketAddr>, BrokerError> {
    let host = url
        .host_str()
        .ok_or_else(|| BrokerError::policy("fetch_url.url", "URL host is required"))?;
    let mut addresses: Vec<_> = if let Ok(address) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(address, destination.port)]
    } else {
        tokio::time::timeout(timeout, tokio::net::lookup_host((host, destination.port)))
            .await
            .map_err(|_| BrokerError::WallTimeExceeded)?
            .map_err(BrokerError::dns)?
            .collect()
    };
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(BrokerError::policy(
            "network.allowed_destinations",
            "DNS returned no addresses",
        ));
    }

    let explicit_address = host.parse::<IpAddr>().is_ok();
    let explicit_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    for address in &addresses {
        let ip = address.ip();
        if forbidden_address(ip)
            || (ip.is_loopback() && !explicit_loopback)
            || (private_address(ip) && !explicit_address && !explicit_loopback)
        {
            return Err(BrokerError::policy(
                "network.allowed_destinations",
                format!("resolved address {ip} is forbidden for host {host:?}"),
            ));
        }
    }
    Ok(addresses)
}

fn forbidden_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_unspecified()
                || address.is_multicast()
                || address.is_link_local()
                || address.is_broadcast()
        }
        IpAddr::V6(address) => {
            address.is_unspecified()
                || address.is_multicast()
                || address.is_unicast_link_local()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| forbidden_address(IpAddr::V4(mapped)))
        }
    }
}

fn private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            address.is_private()
                || address.is_loopback()
                || octets[0] == 0
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || octets[0] >= 240
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unique_local()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| private_address(IpAddr::V4(mapped)))
        }
    }
}

fn safe_origin(url: &reqwest::Url) -> Result<String, BrokerError> {
    let host = url
        .host_str()
        .ok_or_else(|| BrokerError::policy("fetch_url.url", "URL host is required"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| BrokerError::policy("fetch_url.url", "URL port is required"))?;
    Ok(format!("{}://{host}:{port}", url.scheme()))
}
