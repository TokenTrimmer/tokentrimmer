//! Batch Lane: the async, latency-insensitive surface (OpenAI-compatible Batch
//! API through the gateway). Upload a JSONL input file, create a batch over it,
//! poll its status, list/cancel batches, and download the result/error JSONL.
//!
//! These map 1:1 to the gateway's slice-2 endpoints (`crates/core/src/routes/
//! batches.rs`): `POST /v1/files`, `POST /v1/batches`,
//! `GET /v1/batches/{id}`, `GET /v1/batches`, `POST /v1/batches/{id}/cancel`,
//! `GET /v1/files/{id}/content`. The gateway returns OpenAI-compatible Batch
//! objects; [`Batch`] is a partial typed view of that shape (`#[serde(default)]`
//! on every optional, so a sparse gateway response still deserializes).

use bytes::{Bytes, BytesMut};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};

use crate::{Client, Error, Result};

const BATCH_ERROR_BODY_MAX_BYTES: usize = 64 * 1024;

/// Per-request progress counters for a batch (mirrors the gateway's
/// `request_counts` object).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct BatchCounts {
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub completed: u64,
    #[serde(default)]
    pub failed: u64,
}

/// A typed view of a gateway Batch object. Only the fields the gateway serializes
/// are modelled; every optional carries `#[serde(default)]` so a partial response
/// (e.g. a freshly-created batch with no `output_file_id` yet) deserializes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Batch {
    /// The batch id (e.g. `batch_abc123`).
    pub id: String,
    /// The current status (`validating` / `in_progress` / `completed` / …).
    #[serde(default)]
    pub status: String,
    /// The endpoint the batch targets (e.g. `/v1/chat/completions`).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The input file id the batch was created from.
    #[serde(default)]
    pub input_file_id: Option<String>,
    /// The file id of the successful-results JSONL (present once completed).
    #[serde(default)]
    pub output_file_id: Option<String>,
    /// The file id of the errors JSONL (present when some requests failed).
    #[serde(default)]
    pub error_file_id: Option<String>,
    /// The completion window the batch was created with (e.g. `24h`).
    #[serde(default)]
    pub completion_window: Option<String>,
    /// Per-request progress counters.
    #[serde(default)]
    pub request_counts: BatchCounts,
    /// Unix-seconds creation timestamp, when the gateway sends it.
    #[serde(default)]
    pub created_at: Option<i64>,
}

/// The `{"object":"list","data":[...]}` envelope `GET /v1/batches` returns.
#[derive(Debug, Deserialize)]
struct BatchList {
    #[serde(default)]
    data: Vec<Batch>,
}

/// The `POST /v1/files` response — we only need the `id`.
#[derive(Debug, Deserialize)]
struct FileObject {
    id: String,
}

/// Read a non-2xx response into [`Error::Status`] (carrying cost/trace
/// telemetry, consistent with the chat/embeddings paths). On success returns the
/// response untouched for the caller to decode.
async fn batch_ok(resp: reqwest::Response) -> Result<reqwest::Response> {
    let cost = crate::parse_cost(resp.headers());
    let status = resp.status();
    if !status.is_success() {
        let mut body = BytesMut::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                break;
            };
            let remaining = BATCH_ERROR_BODY_MAX_BYTES.saturating_sub(body.len());
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if remaining <= chunk.len() {
                break;
            }
        }
        return Err(Error::Status {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&body).into_owned(),
            cost: Box::new(cost),
        });
    }
    Ok(resp)
}

impl Client {
    /// Upload a JSONL batch input file (`POST /v1/files`, `purpose=batch`) and
    /// return the new file id. The bytes are sent as the `file` multipart part
    /// with filename `batch.jsonl`.
    ///
    /// # Errors
    /// [`Error::Request`] on transport failure, [`Error::Status`] on a non-2xx
    /// response, [`Error::Decode`] if the body isn't a File object.
    pub async fn upload_batch_input(&self, jsonl: Vec<u8>) -> Result<String> {
        let part = reqwest::multipart::Part::bytes(jsonl)
            .file_name("batch.jsonl")
            .mime_str("application/jsonl")
            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(Vec::new()));
        let form = reqwest::multipart::Form::new()
            .text("purpose", "batch")
            .part("file", part);
        let resp = self
            .http
            .post(format!("{}/v1/files", self.base))
            .bearer_auth(&self.key)
            .multipart(form)
            .send()
            .await
            .map_err(Error::Request)?;
        let resp = batch_ok(resp).await?;
        let file = resp.json::<FileObject>().await.map_err(Error::Decode)?;
        Ok(file.id)
    }

    /// Create a batch over a previously-uploaded `input_file_id`
    /// (`POST /v1/batches`).
    ///
    /// # Errors
    /// [`Error::Request`] / [`Error::Status`] / [`Error::Decode`].
    pub async fn create_batch(
        &self,
        input_file_id: &str,
        endpoint: &str,
        completion_window: &str,
    ) -> Result<Batch> {
        let body = serde_json::json!({
            "input_file_id": input_file_id,
            "endpoint": endpoint,
            "completion_window": completion_window,
        });
        let resp = self
            .http
            .post(format!("{}/v1/batches", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(Error::Request)?;
        let resp = batch_ok(resp).await?;
        resp.json::<Batch>().await.map_err(Error::Decode)
    }

    /// Fetch one batch by id (`GET /v1/batches/{id}`).
    ///
    /// # Errors
    /// [`Error::Request`] / [`Error::Status`] (404 for an unknown/foreign id) /
    /// [`Error::Decode`].
    pub async fn get_batch(&self, id: &str) -> Result<Batch> {
        let resp = self
            .http
            .get(format!("{}/v1/batches/{id}", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(Error::Request)?;
        let resp = batch_ok(resp).await?;
        resp.json::<Batch>().await.map_err(Error::Decode)
    }

    /// List the org's batches, newest-first (`GET /v1/batches`). Parses the
    /// `{"object":"list","data":[...]}` envelope into the inner Vec.
    ///
    /// # Errors
    /// [`Error::Request`] / [`Error::Status`] / [`Error::Decode`].
    pub async fn list_batches(&self) -> Result<Vec<Batch>> {
        let resp = self
            .http
            .get(format!("{}/v1/batches", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(Error::Request)?;
        let resp = batch_ok(resp).await?;
        let list = resp.json::<BatchList>().await.map_err(Error::Decode)?;
        Ok(list.data)
    }

    /// Cancel a batch by id (`POST /v1/batches/{id}/cancel`) and return the
    /// updated object.
    ///
    /// # Errors
    /// [`Error::Request`] / [`Error::Status`] / [`Error::Decode`].
    pub async fn cancel_batch(&self, id: &str) -> Result<Batch> {
        let resp = self
            .http
            .post(format!("{}/v1/batches/{id}/cancel", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(Error::Request)?;
        let resp = batch_ok(resp).await?;
        resp.json::<Batch>().await.map_err(Error::Decode)
    }

    /// Stream a file's raw bytes (`GET /v1/files/{id}/content`) — the
    /// result/error JSONL for a completed batch.
    ///
    /// # Errors
    /// [`Error::Request`] / [`Error::Status`] / [`Error::Decode`].
    pub async fn stream_file_content(
        &self,
        file_id: &str,
    ) -> Result<impl futures::Stream<Item = Result<Bytes>> + Send + 'static> {
        let resp = self
            .http
            .get(format!("{}/v1/files/{file_id}/content", self.base))
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(Error::Request)?;
        let resp = batch_ok(resp).await?;
        Ok(resp
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::Decode)))
    }

    /// Download a file into one byte buffer.
    ///
    /// This compatibility helper collects [`Self::stream_file_content`].
    /// Large-file consumers should use the streaming method directly.
    ///
    /// # Errors
    /// [`Error::Request`] / [`Error::Status`] / [`Error::Decode`].
    pub async fn download_file_content(&self, file_id: &str) -> Result<Bytes> {
        let stream = self.stream_file_content(file_id).await?;
        futures::pin_mut!(stream);
        let mut body = BytesMut::new();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(&chunk?);
        }
        Ok(body.freeze())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    fn batch_json(id: &str, status: &str) -> serde_json::Value {
        json!({
            "id": id,
            "object": "batch",
            "status": status,
            "endpoint": "/v1/chat/completions",
            "input_file_id": "file-in",
            "completion_window": "24h",
            "request_counts": { "total": 10, "completed": 3, "failed": 1 },
            "created_at": 1_700_000_000_i64,
        })
    }

    #[tokio::test]
    async fn upload_batch_input_returns_file_id() {
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/files")
                .header("authorization", "Bearer tt_live_test");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({ "id": "file-abc", "object": "file", "purpose": "batch" }));
        });
        let client = Client::new(server.base_url(), "tt_live_test");
        let id = client
            .upload_batch_input(b"{\"a\":1}\n".to_vec())
            .await
            .unwrap();
        m.assert();
        assert_eq!(id, "file-abc");
    }

    #[tokio::test]
    async fn create_batch_parses_batch() {
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST).path("/v1/batches").json_body(json!({
                "input_file_id": "file-in",
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            }));
            then.status(200)
                .header("content-type", "application/json")
                .json_body(batch_json("batch_1", "validating"));
        });
        let client = Client::new(server.base_url(), "k");
        let batch = client
            .create_batch("file-in", "/v1/chat/completions", "24h")
            .await
            .unwrap();
        m.assert();
        assert_eq!(batch.id, "batch_1");
        assert_eq!(batch.status, "validating");
        assert_eq!(batch.endpoint.as_deref(), Some("/v1/chat/completions"));
        assert_eq!(batch.request_counts.total, 10);
        assert_eq!(batch.request_counts.completed, 3);
        assert_eq!(batch.created_at, Some(1_700_000_000));
    }

    #[tokio::test]
    async fn get_batch_parses_batch() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/batches/batch_1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(batch_json("batch_1", "completed"));
        });
        let client = Client::new(server.base_url(), "k");
        let batch = client.get_batch("batch_1").await.unwrap();
        assert_eq!(batch.id, "batch_1");
        assert_eq!(batch.status, "completed");
    }

    #[tokio::test]
    async fn list_batches_parses_envelope() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/batches");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "object": "list",
                    "data": [batch_json("batch_2", "in_progress"), batch_json("batch_1", "completed")]
                }));
        });
        let client = Client::new(server.base_url(), "k");
        let batches = client.list_batches().await.unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].id, "batch_2");
        assert_eq!(batches[1].id, "batch_1");
    }

    #[tokio::test]
    async fn list_batches_handles_empty_data() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/batches");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({ "object": "list", "data": [] }));
        });
        let client = Client::new(server.base_url(), "k");
        assert!(client.list_batches().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancel_batch_parses_status() {
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST).path("/v1/batches/batch_1/cancel");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(batch_json("batch_1", "cancelling"));
        });
        let client = Client::new(server.base_url(), "k");
        let batch = client.cancel_batch("batch_1").await.unwrap();
        m.assert();
        assert_eq!(batch.status, "cancelling");
    }

    #[tokio::test]
    async fn download_file_content_returns_bytes() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/files/file-out/content");
            then.status(200)
                .header("content-type", "application/jsonl")
                .body("{\"id\":\"req-1\"}\n");
        });
        let client = Client::new(server.base_url(), "k");
        let bytes = client.download_file_content("file-out").await.unwrap();
        assert_eq!(&bytes[..], b"{\"id\":\"req-1\"}\n");
    }

    #[tokio::test]
    async fn file_content_error_body_is_bounded() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/files/file-out/content");
            then.status(502)
                .body("x".repeat(BATCH_ERROR_BODY_MAX_BYTES + 1));
        });
        let client = Client::new(server.base_url(), "k");
        let err = match client.stream_file_content("file-out").await {
            Err(err) => err,
            Ok(_) => panic!("expected a status error"),
        };
        match err {
            Error::Status { status, body, .. } => {
                assert_eq!(status, 502);
                assert_eq!(body.len(), BATCH_ERROR_BODY_MAX_BYTES);
            }
            other => panic!("expected bounded status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_batch_404_surfaces_as_status() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/batches/nope");
            then.status(404).body("no batch with id nope");
        });
        let client = Client::new(server.base_url(), "k");
        let err = client.get_batch("nope").await.unwrap_err();
        assert!(matches!(err, Error::Status { status: 404, .. }), "{err:?}");
    }
}
