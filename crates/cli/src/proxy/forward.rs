//! Stream-forward an HTTP request to an upstream URL, returning the upstream
//! response shape (status, headers, body bytes-stream).

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ForwardError {
    #[error("reqwest: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("invalid upstream URL: {0}")]
    Url(String),
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
