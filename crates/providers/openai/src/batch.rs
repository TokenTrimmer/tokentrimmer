//! OpenAI Batch API client.
//!
//! A thin client capability layered onto [`OpenAiProvider`](crate::OpenAiProvider)
//! for the OpenAI Batch API (50% cheaper, 24h completion window):
//!
//! - [`OpenAiProvider::upload_batch_input`] — `POST /files` (multipart,
//!   `purpose=batch`) → file id.
//! - [`OpenAiProvider::create_batch`] — `POST /batches` → [`Batch`].
//! - [`OpenAiProvider::get_batch`] — `GET /batches/{id}` → [`Batch`].
//! - [`OpenAiProvider::cancel_batch`] — `POST /batches/{id}/cancel` → [`Batch`].
//! - [`OpenAiProvider::download_file_content`] — `GET /files/{id}/content` →
//!   the raw result/error JSONL bytes.
//!
//! All methods reuse the adapter's existing [`reqwest::Client`], bearer auth,
//! `base_url` resolution + SSRF validation, extra-header forwarding, and the
//! shared [`errors`](tt_provider_compat::errors) → [`ProviderError`] mapping, so
//! behaviour matches the chat/embeddings paths exactly. This module is purely a
//! client capability: it does **not** touch the gateway dispatch.

use bytes::Bytes;
use reqwest::multipart;
use serde::Deserialize;
use tracing::instrument;
use tt_shared::{filter_extra_headers, validate_provider_url, ProviderError, RequestContext};

use crate::{errors, OpenAiProvider};

/// Status of an OpenAI batch, mirroring the API's `status` field.
///
/// [`BatchStatus::Unknown`] is a forward-compatible fallback for any status the
/// API introduces that this client does not yet model — it preserves the raw
/// wire string so callers can still inspect it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    /// The input file is being validated before the batch can begin.
    Validating,
    /// The input file failed the validation process.
    Failed,
    /// The input file was validated and the batch is currently running.
    InProgress,
    /// The batch has completed and the results are being prepared.
    Finalizing,
    /// The batch has completed and the results are ready.
    Completed,
    /// The batch was not completed within the 24-hour window.
    Expired,
    /// The batch is being cancelled (may take up to 10 minutes).
    Cancelling,
    /// The batch was cancelled.
    Cancelled,
    /// Any status not yet modelled by this client; preserves the wire value.
    #[serde(other)]
    Unknown,
}

/// Per-request progress counters for a batch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BatchRequestCounts {
    /// Total number of requests in the batch.
    #[serde(default)]
    pub total: u64,
    /// Number of requests that have completed successfully.
    #[serde(default)]
    pub completed: u64,
    /// Number of requests that have failed.
    #[serde(default)]
    pub failed: u64,
}

/// A Batch object returned by the OpenAI Batch API.
///
/// Only the fields the gateway needs are modelled; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct Batch {
    /// The batch identifier (e.g. `batch_abc123`).
    pub id: String,
    /// Current status of the batch.
    pub status: BatchStatus,
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
    pub request_counts: BatchRequestCounts,
}

/// Response shape of `POST /files` — we only need the `id`.
#[derive(Debug, Deserialize)]
struct FileObject {
    id: String,
}

impl OpenAiProvider {
    /// Resolve + validate the base URL for a batch call, mirroring the chat path.
    fn batch_base_url<'a>(&self, ctx: &'a RequestContext) -> Result<&'a str, ProviderError> {
        let base_url = self.base_url(ctx);
        // Validate customer-supplied base_url overrides; skip when using the
        // compiled-in default (always safe) or when allow_local is set (tests).
        if ctx.credentials.base_url.is_some() {
            validate_provider_url(base_url, self.allow_local)
                .map_err(|e| ProviderError::InvalidRequest(format!("blocked provider URL: {e}")))?;
        }
        Ok(base_url)
    }

    /// Apply bearer auth + denylist-filtered extra headers, mirroring the chat path.
    fn apply_auth(
        &self,
        builder: reqwest::RequestBuilder,
        ctx: &RequestContext,
    ) -> reqwest::RequestBuilder {
        let mut builder = builder.header(
            "Authorization",
            format!("Bearer {}", ctx.credentials.api_key.expose()),
        );
        for (name, value) in &filter_extra_headers(&ctx.credentials.extra_headers) {
            builder = builder.header(name, value);
        }
        builder
    }

    /// Read the response status + `Retry-After`, returning the body text and
    /// mapping any HTTP ≥ 400 to a [`ProviderError`] — identical to the
    /// chat/embeddings paths.
    async fn read_response(response: reqwest::Response) -> Result<String, ProviderError> {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let response_text = response.text().await.map_err(errors::map_reqwest_error)?;

        if status >= 400 {
            return Err(errors::map_response_error(
                status,
                &response_text,
                retry_after.as_deref(),
            ));
        }
        Ok(response_text)
    }

    /// Upload a JSONL input file for a batch via `POST /files` (multipart,
    /// `purpose=batch`).
    ///
    /// Returns the uploaded file's id, which is then passed to
    /// [`create_batch`](Self::create_batch) as `input_file_id`.
    #[instrument(skip(self, jsonl_bytes, ctx), fields(provider = "openai", bytes = jsonl_bytes.len()))]
    pub async fn upload_batch_input(
        &self,
        jsonl_bytes: Vec<u8>,
        ctx: &RequestContext,
    ) -> Result<String, ProviderError> {
        let base_url = self.batch_base_url(ctx)?;
        let url = format!("{base_url}/files");

        let file_part = multipart::Part::bytes(jsonl_bytes)
            .file_name("batch.jsonl")
            .mime_str("application/jsonl")
            .map_err(errors::map_reqwest_error)?;
        let form = multipart::Form::new()
            .text("purpose", "batch")
            .part("file", file_part);

        let builder = self.apply_auth(self.client.post(&url), ctx).multipart(form);

        let response = builder.send().await.map_err(errors::map_reqwest_error)?;
        let body = Self::read_response(response).await?;

        let file: FileObject =
            serde_json::from_str(&body).map_err(|e| ProviderError::Deserialize(e.to_string()))?;
        Ok(file.id)
    }

    /// Create a batch via `POST /batches`.
    ///
    /// `endpoint` is the per-line endpoint the batch targets (e.g.
    /// `/v1/chat/completions`); `completion_window` is typically `"24h"`.
    #[instrument(skip(self, ctx), fields(provider = "openai", endpoint = endpoint, window = completion_window))]
    pub async fn create_batch(
        &self,
        input_file_id: &str,
        endpoint: &str,
        completion_window: &str,
        ctx: &RequestContext,
    ) -> Result<Batch, ProviderError> {
        let base_url = self.batch_base_url(ctx)?;
        let url = format!("{base_url}/batches");

        let body = serde_json::json!({
            "input_file_id": input_file_id,
            "endpoint": endpoint,
            "completion_window": completion_window,
        });

        let builder = self
            .apply_auth(self.client.post(&url), ctx)
            .header("Content-Type", "application/json")
            .json(&body);

        let response = builder.send().await.map_err(errors::map_reqwest_error)?;
        let resp_body = Self::read_response(response).await?;

        serde_json::from_str(&resp_body).map_err(|e| ProviderError::Deserialize(e.to_string()))
    }

    /// Retrieve a batch's current state via `GET /batches/{batch_id}`.
    ///
    /// The returned [`Batch`] carries the [`status`](Batch::status),
    /// `output_file_id`/`error_file_id` (once available), and
    /// [`request_counts`](Batch::request_counts).
    #[instrument(skip(self, ctx), fields(provider = "openai", batch_id = batch_id))]
    pub async fn get_batch(
        &self,
        batch_id: &str,
        ctx: &RequestContext,
    ) -> Result<Batch, ProviderError> {
        let base_url = self.batch_base_url(ctx)?;
        let url = format!("{base_url}/batches/{batch_id}");

        let builder = self.apply_auth(self.client.get(&url), ctx);

        let response = builder.send().await.map_err(errors::map_reqwest_error)?;
        let resp_body = Self::read_response(response).await?;

        serde_json::from_str(&resp_body).map_err(|e| ProviderError::Deserialize(e.to_string()))
    }

    /// Cancel a batch via `POST /batches/{batch_id}/cancel`.
    ///
    /// Returns the batch in (typically) the `cancelling` state.
    #[instrument(skip(self, ctx), fields(provider = "openai", batch_id = batch_id))]
    pub async fn cancel_batch(
        &self,
        batch_id: &str,
        ctx: &RequestContext,
    ) -> Result<Batch, ProviderError> {
        let base_url = self.batch_base_url(ctx)?;
        let url = format!("{base_url}/batches/{batch_id}/cancel");

        let builder = self.apply_auth(self.client.post(&url), ctx);

        let response = builder.send().await.map_err(errors::map_reqwest_error)?;
        let resp_body = Self::read_response(response).await?;

        serde_json::from_str(&resp_body).map_err(|e| ProviderError::Deserialize(e.to_string()))
    }

    /// Download the raw content of a file via `GET /files/{file_id}/content`.
    ///
    /// Used to fetch a batch's result/error JSONL (the `output_file_id` /
    /// `error_file_id` from a completed [`Batch`]). Returns the raw bytes;
    /// callers parse the JSONL.
    #[instrument(skip(self, ctx), fields(provider = "openai", file_id = file_id))]
    pub async fn download_file_content(
        &self,
        file_id: &str,
        ctx: &RequestContext,
    ) -> Result<Bytes, ProviderError> {
        let base_url = self.batch_base_url(ctx)?;
        let url = format!("{base_url}/files/{file_id}/content");

        let builder = self.apply_auth(self.client.get(&url), ctx);

        let response = builder.send().await.map_err(errors::map_reqwest_error)?;

        // File-content responses are not JSON; map HTTP errors (which *are*
        // JSON-enveloped) the same way, then return the raw body bytes.
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        if status >= 400 {
            let text = response.text().await.map_err(errors::map_reqwest_error)?;
            return Err(errors::map_response_error(
                status,
                &text,
                retry_after.as_deref(),
            ));
        }

        response.bytes().await.map_err(errors::map_reqwest_error)
    }
}
