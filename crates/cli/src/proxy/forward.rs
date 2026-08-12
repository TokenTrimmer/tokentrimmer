//! Stream-forward an HTTP request to an upstream URL, returning the upstream
//! response shape (status, headers, body bytes-stream).

use axum::body::Body;
use axum::http::{
    header::{HeaderValue, AUTHORIZATION},
    HeaderMap, StatusCode,
};
use thiserror::Error;

use crate::proxy::config::{CredentialPolicy, Mode};

#[derive(Debug, Error)]
pub enum ForwardError {
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("invalid upstream URL: {0}")]
    Url(String),
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("gateway mode requires a configured TokenTrimmer key")]
    MissingTokenTrimmerKey,
    #[error("configured TokenTrimmer key cannot be encoded as an HTTP bearer")]
    InvalidTokenTrimmerKey(#[from] axum::http::header::InvalidHeaderValue),
}

const CLIENT_CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "cookie",
];

/// Apply the selected mode's credential contract before any request leaves the
/// local listener. Hosted gateway mode must never receive a provider key or
/// subscription session; direct-provider and loopback BYOK modes preserve the
/// client credential byte-for-byte.
pub fn prepare_forward_headers(
    mode: Mode,
    mut headers: HeaderMap,
    tt_api_key: Option<&str>,
) -> Result<HeaderMap, CredentialError> {
    if mode.contract().credential_policy == CredentialPolicy::TokenTrimmer {
        for name in CLIENT_CREDENTIAL_HEADERS {
            headers.remove(*name);
        }
        let key = tt_api_key.ok_or(CredentialError::MissingTokenTrimmerKey)?;
        let authorization: HeaderValue = format!("Bearer {key}").parse()?;
        headers.insert(AUTHORIZATION, authorization);
    }
    Ok(headers)
}

pub struct ForwardedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: reqwest::Response,
}

pub async fn forward_post(
    client: &reqwest::Client,
    upstream_url: &str,
    headers: HeaderMap,
    body_bytes: bytes::Bytes,
) -> Result<ForwardedResponse, ForwardError> {
    let mut req = client.post(upstream_url).body(body_bytes);
    // Carry-over content-type, authorization, user-agent. Reqwest will set
    // host/length itself.
    for (k, v) in headers.iter() {
        if matches!(k.as_str(), "host" | "content-length") {
            continue;
        }
        if let Ok(s) = v.to_str() {
            req = req.header(k.as_str(), s);
        }
    }
    let resp = req.send().await?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut hm = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        hm.insert(k.clone(), v.clone());
    }
    Ok(ForwardedResponse {
        status,
        headers: hm,
        body: resp,
    })
}

/// Convert the upstream body stream into an Axum body. Streams chunks.
pub fn into_axum_body(resp: reqwest::Response) -> Body {
    Body::from_stream(resp.bytes_stream())
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[test]
    fn gateway_strips_client_credentials_and_injects_only_tokentrimmer_identity() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer provider-oauth".parse().unwrap());
        headers.insert("proxy-authorization", "Basic proxy-secret".parse().unwrap());
        headers.insert("x-api-key", "anthropic-secret".parse().unwrap());
        headers.insert("api-key", "azure-secret".parse().unwrap());
        headers.insert("x-goog-api-key", "gemini-secret".parse().unwrap());
        headers.insert("cookie", "session=provider-session".parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());

        let prepared =
            prepare_forward_headers(Mode::Gateway, headers, Some("tt_live_test")).unwrap();

        assert_eq!(prepared.get(AUTHORIZATION).unwrap(), "Bearer tt_live_test");
        for name in &CLIENT_CREDENTIAL_HEADERS[1..] {
            assert!(
                prepared.get(*name).is_none(),
                "gateway must strip client credential header {name}"
            );
        }
        assert_eq!(prepared.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn gateway_fails_closed_without_tokentrimmer_identity() {
        assert!(matches!(
            prepare_forward_headers(Mode::Gateway, HeaderMap::new(), None),
            Err(CredentialError::MissingTokenTrimmerKey)
        ));
    }

    #[test]
    fn direct_and_loopback_byok_modes_preserve_client_credentials() {
        for mode in [Mode::Bypass, Mode::Hybrid] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", "Bearer provider-oauth".parse().unwrap());
            headers.insert("x-api-key", "provider-key".parse().unwrap());
            let prepared =
                prepare_forward_headers(mode, headers.clone(), Some("ignored-tt-key")).unwrap();
            assert_eq!(prepared, headers, "{mode} must preserve client credentials");
        }
    }

    #[tokio::test]
    async fn forward_round_trips_body_and_status() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/messages").body("hello");
                then.status(200).body("upstream-resp");
            })
            .await;
        let client = reqwest::Client::new();
        let mut h = HeaderMap::new();
        h.insert("content-type", "text/plain".parse().unwrap());
        let resp = forward_post(
            &client,
            &format!("{}/v1/messages", server.base_url()),
            h,
            "hello".into(),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        let body_bytes = resp.body.bytes().await.unwrap();
        assert_eq!(&body_bytes[..], b"upstream-resp");
    }
}
